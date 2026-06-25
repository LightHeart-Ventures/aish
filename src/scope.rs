//! Session-scoped job filtering — the pure logic behind "status of X".
//!
//! `background_status` (see `tools.rs`) lists background jobs the host can see:
//! this session's in-memory coordinators plus every session's durable
//! coordinator runs and Anthropic batches from the shared `aish.db`. That
//! cross-session firehose is right for "show me everything" but wrong for the
//! common `status` — which means *my* jobs in *this* shell.
//!
//! This module is the SKELETON of the scope filter described in
//! `docs/session-scoped-jobs.md`. It is deliberately pure (no I/O, no
//! `Session`) so the parse + match rules are unit-testable in isolation; the
//! caller threads in the live session id and per-job facts. Wiring the durable
//! `repo_key` column and flipping the `background_status` default to `Session`
//! are follow-ups (see `docs/session-scoped-jobs-implementation.md`).

/// What slice of the background-job table a "status" query asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobScope {
    /// Jobs owned by the current session — the eventual default for a bare
    /// `status`. (The skeleton keeps `background_status`'s default at `All` to
    /// avoid a behaviour change before review; see the module docs.)
    Session,
    /// Every job across every session — today's `background_status` behaviour,
    /// reached explicitly with "status of all sessions".
    All,
    /// Jobs whose repo-key matches, e.g. "status of aish". NOTE: the durable
    /// rows don't carry a repo-key yet, so the caller currently can't satisfy
    /// this against `aish.db` — it's parsed here so the grammar is complete and
    /// the follow-up only has to populate the column (Phase 1).
    Repo(String),
    /// A single job by id or id-prefix, e.g. "status of w_a7k3m2pQ".
    Job(String),
}

/// The few per-job facts the scope filter needs, borrowed from whatever row
/// type the caller is iterating (worker handle, `CoordinatorRow`, `BatchRow`).
/// Keeping this a thin view means `matches` never depends on a concrete store.
pub struct JobRef<'a> {
    /// The owning session's uuid, or `None` for a legacy row written before
    /// ownership tracking existed. A `None` owner never matches `Session`.
    pub owner_session_id: Option<&'a str>,
    /// The job's repo-key (`owner--repo` or a local fallback), or `None` when
    /// untracked. A `None` repo-key never matches `Repo`.
    pub repo_key: Option<&'a str>,
    /// The job's own id (e.g. `w_a7k3m2pQ`, `run_…`, a batch local id).
    pub id: &'a str,
}

impl JobScope {
    /// Parse the free-text the agent extracts from "status of X" into a scope.
    ///
    /// `None`/empty and the "me/this session" synonyms → [`JobScope::Session`];
    /// the "everything" synonyms → [`JobScope::All`]; a `job:`/`repo:` prefix or
    /// a token shaped like a job id routes to [`JobScope::Job`], and anything
    /// else is treated as a repo name ([`JobScope::Repo`]). Case-insensitive for
    /// the keyword set; the `Repo`/`Job` payload preserves the caller's casing
    /// (repo-keys and ids can be mixed-case).
    pub fn parse(raw: Option<&str>) -> JobScope {
        let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return JobScope::Session;
        };
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "session" | "mine" | "me" | "this" | "this session" | "local" => JobScope::Session,
            "all" | "all sessions" | "everything" | "every" | "any" | "*" => JobScope::All,
            _ => {
                if let Some(rest) = trimmed.strip_prefix("job:") {
                    return JobScope::Job(rest.trim().to_string());
                }
                if let Some(rest) = trimmed.strip_prefix("repo:") {
                    return JobScope::Repo(rest.trim().to_string());
                }
                if looks_like_job_id(trimmed) {
                    return JobScope::Job(trimmed.to_string());
                }
                JobScope::Repo(trimmed.to_string())
            }
        }
    }

    /// Does `job` belong in this scope, for a shell whose session id is
    /// `current_session_id`? Pure — the single source of truth for the filter.
    pub fn matches(&self, job: &JobRef<'_>, current_session_id: &str) -> bool {
        match self {
            JobScope::All => true,
            JobScope::Session => job.owner_session_id == Some(current_session_id),
            JobScope::Job(q) => job.id == q || job.id.starts_with(q.as_str()),
            JobScope::Repo(key) => job
                .repo_key
                .is_some_and(|rk| rk.eq_ignore_ascii_case(key) || contains_ci(rk, key)),
        }
    }
}

/// Heuristic: does this token look like a background-job id rather than a repo
/// name? Job ids carry a known machine prefix (`w_`/`worker_` workers, `run_`
/// coordinator runs, `batch`/`msgbatch` batches). Anything else is treated as a
/// repo name. Pure.
fn looks_like_job_id(token: &str) -> bool {
    const JOB_PREFIXES: &[&str] = &["w_", "worker_", "run_", "batch", "msgbatch"];
    JOB_PREFIXES.iter().any(|p| token.starts_with(p))
}

/// Case-insensitive substring test (ASCII). Lets `status of aish` match a
/// `LightHeart-Ventures--aish` repo-key without forcing an exact equality.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job<'a>(owner: Option<&'a str>, repo: Option<&'a str>, id: &'a str) -> JobRef<'a> {
        JobRef {
            owner_session_id: owner,
            repo_key: repo,
            id,
        }
    }

    #[test]
    fn parse_maps_session_synonyms_and_default() {
        assert_eq!(JobScope::parse(None), JobScope::Session);
        assert_eq!(JobScope::parse(Some("")), JobScope::Session);
        assert_eq!(JobScope::parse(Some("   ")), JobScope::Session);
        for s in ["session", "Mine", "ME", "this", "this session", "local"] {
            assert_eq!(JobScope::parse(Some(s)), JobScope::Session, "{s}");
        }
    }

    #[test]
    fn parse_maps_all_synonyms() {
        for s in ["all", "All Sessions", "EVERYTHING", "every", "any", "*"] {
            assert_eq!(JobScope::parse(Some(s)), JobScope::All, "{s}");
        }
    }

    #[test]
    fn parse_routes_job_ids_and_prefixes() {
        assert_eq!(
            JobScope::parse(Some("w_a7k3m2pQ")),
            JobScope::Job("w_a7k3m2pQ".into())
        );
        assert_eq!(
            JobScope::parse(Some("worker_123")),
            JobScope::Job("worker_123".into())
        );
        assert_eq!(
            JobScope::parse(Some("run_abc")),
            JobScope::Job("run_abc".into())
        );
        // Explicit job: prefix wins, payload trimmed.
        assert_eq!(
            JobScope::parse(Some("job: w_xy ")),
            JobScope::Job("w_xy".into())
        );
        // Explicit repo: prefix forces a repo scope even for a job-shaped token.
        assert_eq!(
            JobScope::parse(Some("repo:w_xy")),
            JobScope::Repo("w_xy".into())
        );
    }

    #[test]
    fn parse_treats_other_tokens_as_repo_preserving_case() {
        assert_eq!(
            JobScope::parse(Some("LightHeart-Ventures--aish")),
            JobScope::Repo("LightHeart-Ventures--aish".into())
        );
        assert_eq!(JobScope::parse(Some("aish")), JobScope::Repo("aish".into()));
    }

    #[test]
    fn matches_session_scope_excludes_others_and_legacy_nulls() {
        let s = JobScope::Session;
        // Owned by me → in scope.
        assert!(s.matches(&job(Some("A"), None, "w_1"), "A"));
        // Owned by another session → out.
        assert!(!s.matches(&job(Some("B"), None, "w_2"), "A"));
        // Legacy null owner → out (not provably mine) — the back-compat rule.
        assert!(!s.matches(&job(None, None, "w_3"), "A"));
    }

    #[test]
    fn matches_all_scope_includes_everything() {
        let a = JobScope::All;
        assert!(a.matches(&job(Some("A"), None, "w_1"), "A"));
        assert!(a.matches(&job(Some("B"), None, "w_2"), "A"));
        assert!(a.matches(&job(None, None, "w_3"), "A")); // legacy null still shown under All
    }

    #[test]
    fn matches_job_scope_by_exact_and_prefix() {
        let exact = JobScope::Job("w_a7k3m2pQ".into());
        assert!(exact.matches(&job(Some("A"), None, "w_a7k3m2pQ"), "A"));
        let prefix = JobScope::Job("w_a7".into());
        assert!(prefix.matches(&job(Some("B"), None, "w_a7k3m2pQ"), "A")); // owner-agnostic
        assert!(!prefix.matches(&job(Some("A"), None, "w_zz00"), "A"));
    }

    #[test]
    fn matches_repo_scope_ci_eq_contains_and_legacy_null() {
        let r = JobScope::Repo("aish".into());
        // Substring, case-insensitive.
        assert!(r.matches(
            &job(Some("A"), Some("LightHeart-Ventures--aish"), "w_1"),
            "A"
        ));
        // Exact, case-insensitive.
        assert!(JobScope::Repo("AISH".into()).matches(&job(None, Some("aish"), "w_2"), "A"));
        // Unrelated repo → no.
        assert!(!r.matches(&job(Some("A"), Some("other--repo"), "w_3"), "A"));
        // Legacy null repo-key → never matches a repo query.
        assert!(!r.matches(&job(Some("A"), None, "w_4"), "A"));
    }
}
