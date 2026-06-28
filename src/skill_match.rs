//! Skill-awareness — per-turn relevance matching of the user's task against the
//! INSTALLED local skill catalog (`~/.aish/skills/<name>/SKILL.md`).
//!
//! The installed skills are already advertised, once, in the system prompt (see
//! `skills::render_prompt_section`), but that static list grows with every skill
//! the user adds and a relevant one is easy to overlook on a given task. This
//! module closes that gap with a light, per-turn nudge: it scores the user's
//! input against the catalog by keyword overlap and, when a skill clearly fits,
//! prepends a short `[aish skill-awareness] …` note to that turn's input
//! pointing the model at the matching SKILL.md.
//!
//! The note is injected into the *turn input* (alongside `engine::seed_context`),
//! NOT into the cached system prompt — so the prompt-cache prefix stays
//! byte-stable and the hint is contextual to the task at hand. It's a hint, not
//! a command: the model still decides whether to read and follow the playbook.
//!
//! Scope: this module covers BOTH halves of the skill-awareness design.
//!   * "YES, an installed skill matches" → [`hint`] surfaces the local SKILL.md
//!     so the model reads and follows it (the engine prepends it to the turn).
//!   * "NO installed skill matches a substantial task" → [`recommend_install`]
//!     names an installable skill from the registry catalog so the model can
//!     RECOMMEND it (`:skill add <ref>`) instead of faking or hand-rolling it.
//!     To keep the per-turn hot path fast and offline, the recommendation reads
//!     the binary-shipped registry index (`skill_provider::local_index_catalog`)
//!     — NO network round-trip per prompt. A full live search across mcpmarket /
//!     skill.fish stays explicit via `:skill search <query>` / `--skill-search`
//!     (see `skill_provider`).

use crate::skills::Skill;
use std::collections::HashSet;

/// A name-token match is worth this much more than a description-token match: a
/// hit on the skill's NAME (e.g. "reviewer", "incident", "profiler") is a much
/// stronger signal of relevance than a hit somewhere in its prose description.
const NAME_WEIGHT: usize = 3;

/// Minimum relevance score for a skill to be surfaced as a hint. Equal to
/// [`NAME_WEIGHT`], so a SINGLE name-token match qualifies on its own, while a
/// description-only match needs three overlapping significant words — a high
/// enough bar that an incidental word or two never trips a false nudge.
const MIN_SCORE: usize = NAME_WEIGHT;

/// At most this many skills are named in one hint, so the note stays a glance,
/// not a wall. The highest-scoring matches win (see [`rank`]).
const MAX_HINTS: usize = 2;

/// Common English / shell filler dropped before scoring — matching one of these
/// must never contribute relevance. Kept small and high-frequency; the `len < 3`
/// floor in [`tokens`] already removes most noise (`a`, `to`, `of`, `is`, …).
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "your", "are", "was", "were", "this", "that", "with", "from",
    "into", "out", "can", "could", "would", "should", "will", "what", "when", "where", "why",
    "how", "who", "please", "help", "need", "want", "use", "using", "get", "got", "let", "any",
    "all", "now", "new", "see", "show", "tell", "make", "made", "did", "does", "done", "run",
    "running", "ran", "have", "has", "had", "about", "some", "they", "them", "then", "than",
    "here", "there", "been", "being", "our", "their", "its",
];

/// Tokenize free text into lowercased significant words: split on every
/// non-alphanumeric boundary (so `pr-reviewer`, `P99`, `incident_response` all
/// split cleanly), drop tokens shorter than three chars, and drop stopwords.
fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Whether two significant tokens are "the same word" for matching purposes:
/// exactly equal, or one is a prefix of the other with the shorter at least four
/// chars long. The prefix rule is a cheap stand-in for stemming — it lets
/// `review` match `reviewer`, `deploy` match `deployment`, `test` match
/// `testing` — without pulling in a stemmer dependency. The four-char floor
/// keeps short, generic stems from over-matching.
fn token_matches(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 4 && long.starts_with(short)
}

/// Relevance score of a `(name, description)` pair for a tokenized task: for
/// each DISTINCT task token, [`NAME_WEIGHT`] if it matches a token in the name,
/// else 1 if it matches a token in the description, else nothing. A token is
/// counted once, at the higher (name) weight when it matches both. Shared by the
/// installed-skill nudge ([`relevance`]) and the registry recommendation
/// ([`recommend_install`]).
fn relevance_named(task_tokens: &[String], name: &str, description: &str) -> usize {
    let name_toks = tokens(name);
    let desc_toks = tokens(description);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut score = 0;
    for t in task_tokens {
        if !seen.insert(t.as_str()) {
            continue; // a repeated task word contributes only once
        }
        if name_toks.iter().any(|n| token_matches(t, n)) {
            score += NAME_WEIGHT;
        } else if desc_toks.iter().any(|d| token_matches(t, d)) {
            score += 1;
        }
    }
    score
}

/// Relevance score of an installed `skill` for a tokenized task — a thin wrapper
/// over [`relevance_named`] on the skill's name + description.
pub fn relevance(task_tokens: &[String], skill: &Skill) -> usize {
    relevance_named(task_tokens, &skill.name, &skill.description)
}

/// One ranked match: the skill and its relevance score (always `>= MIN_SCORE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match<'a> {
    pub skill: &'a Skill,
    pub score: usize,
}

/// Rank the installed skills by relevance to `task`, keeping only those at or
/// above [`MIN_SCORE`], highest score first. Ties break by skill name so the
/// order is stable (and so a tie renders deterministically in the hint).
pub fn rank<'a>(task: &str, skills: &'a [Skill]) -> Vec<Match<'a>> {
    let task_tokens = tokens(task);
    if task_tokens.is_empty() {
        return Vec::new();
    }
    let mut matches: Vec<Match<'a>> = skills
        .iter()
        .map(|skill| Match {
            skill,
            score: relevance(&task_tokens, skill),
        })
        .filter(|m| m.score >= MIN_SCORE)
        .collect();
    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.skill.name.cmp(&b.skill.name)));
    matches
}

/// The per-turn skill-awareness note for `task`, or `None` when no installed
/// skill clears the bar. Names up to [`MAX_HINTS`] top matches and points the
/// model at each one's SKILL.md. Pure + unit-tested; the engine does the prepend.
pub fn hint(task: &str, skills: &[Skill]) -> Option<String> {
    let matches = rank(task, skills);
    if matches.is_empty() {
        return None;
    }
    let top = &matches[..matches.len().min(MAX_HINTS)];
    if top.len() == 1 {
        let m = &top[0];
        Some(format!(
            "[aish skill-awareness] Your installed `{}` skill fits this task. USING a skill just \
means reading its SKILL.md and carrying out its steps yourself with your normal tools — there is \
no separate command to \"invoke\" it. Read it FIRST with read_file(\"{}\") and follow it, BEFORE \
attempting the task manually ({}).",
            m.skill.name,
            m.skill.path.display(),
            m.skill.description.trim_end_matches('.'),
        ))
    } else {
        let mut s = String::from(
            "[aish skill-awareness] These installed skills look relevant to this task — \
read the best-fitting one FIRST with read_file and follow it:",
        );
        for m in top {
            s.push_str(&format!(
                "\n- `{}` ({}): {}",
                m.skill.name,
                m.skill.path.display(),
                m.skill.description,
            ));
        }
        Some(s)
    }
}

/// Minimum number of SIGNIFICANT tokens (after stopword/short-word filtering) a
/// task must carry to be "skill-worthy" — substantial enough that recommending
/// an installable skill is worth the interruption. A one- or two-word command
/// (`ls /tmp`, `git status`) falls below the bar; a real task ("resolve the
/// merge conflicts on this branch") clears it.
const SKILL_WORTHY_MIN_TOKENS: usize = 4;

/// Whether `task` is substantial enough to warrant an offline registry
/// recommendation when no installed skill matched. Trivial commands never trip
/// a "you could install …" nudge.
pub fn is_skill_worthy(task: &str) -> bool {
    tokens(task).len() >= SKILL_WORTHY_MIN_TOKENS
}

/// A recommended-but-not-installed skill from the registry catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// The `owner/name` (or GitHub URL) ref to pass to `:skill add` /
    /// `--skill-fetch`. Doubles as the per-session dedup key.
    pub reference: String,
    /// The pre-rendered `[aish skill-awareness] …` note to fold into the turn.
    pub note: String,
}

/// Recommend an installable skill from `catalog` (a registry index / search
/// result set) for `task`, when NONE of the `installed` skills already fits.
/// Pure + unit-tested: the caller supplies the catalog (loaded offline from the
/// binary-shipped index — see `skill_provider::local_index_catalog`).
///
/// Returns `None` unless the task is skill-worthy AND a catalog entry clears the
/// same name-level relevance bar ([`MIN_SCORE`]) the local nudge uses AND that
/// entry isn't already installed (matched by skill name). The note tells the
/// model to RECOMMEND the install to the user (`:skill add <ref>`) rather than
/// fake, or manually re-implement, a skill that isn't installed.
pub fn recommend_install(
    task: &str,
    installed: &[Skill],
    catalog: &[crate::skill_provider::SearchResult],
) -> Option<Recommendation> {
    if !is_skill_worthy(task) {
        return None;
    }
    let task_tokens = tokens(task);
    if task_tokens.is_empty() {
        return None;
    }
    let installed_names: HashSet<&str> = installed.iter().map(|s| s.name.as_str()).collect();
    // Rank catalog entries by the same name/description relevance the local nudge
    // uses; skip anything already installed and anything below the bar. Highest
    // score wins; ties break by reference so the pick is deterministic.
    let mut best: Option<(&crate::skill_provider::SearchResult, usize)> = None;
    for entry in catalog {
        let name = entry.name.trim();
        if name.is_empty() || installed_names.contains(name) {
            continue;
        }
        let score = relevance_named(&task_tokens, &entry.name, &entry.description);
        if score < MIN_SCORE {
            continue;
        }
        let better = match &best {
            None => true,
            Some((cur, cur_score)) => {
                score > *cur_score
                    || (score == *cur_score && entry.ref_or_synth() < cur.ref_or_synth())
            }
        };
        if better {
            best = Some((entry, score));
        }
    }
    let (entry, _score) = best?;
    let reference = entry.ref_or_synth();
    let desc = entry.description.trim().trim_end_matches('.');
    let suffix = if desc.is_empty() {
        String::new()
    } else {
        format!(" ({desc})")
    };
    let note = format!(
        "[aish skill-awareness] No installed skill fits this task, but the skill registry has \
`{reference}`{suffix} — it looks relevant. RECOMMEND it to the user: they can install it with \
`:skill add {reference}`, after which you'd read its SKILL.md and follow it. Do NOT pretend to \
run, or silently hand-roll, a skill that isn't installed — surface the recommendation instead."
    );
    Some(Recommendation { reference, note })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        }
    }

    /// A small catalog modeled on the real installed skills.
    fn catalog() -> Vec<Skill> {
        vec![
            skill(
                "pr-reviewer",
                "Review a pull request or diff against engineering best practices — correctness, security, tests, style, and risk.",
            ),
            skill(
                "incident-responder",
                "Triage and mitigate incidents — stuck tasks, failed agent runs, error spikes, broken deploys.",
            ),
            skill(
                "performance-profiler",
                "Identify performance bottlenecks from logs, traces, and profiles — slow endpoints, hot paths, P99 latency, N+1 queries.",
            ),
            skill(
                "dependency-audit",
                "Audit project dependencies for known vulnerabilities, outdated versions, and license compliance.",
            ),
        ]
    }

    #[test]
    fn tokens_split_filter_and_lowercase() {
        let t = tokens("Review the PR-diff for P99 latency!");
        // hyphen/space/punct all split; <3-char and stopwords dropped; lowercased.
        assert!(t.contains(&"review".to_string()));
        assert!(t.contains(&"diff".to_string()));
        assert!(t.contains(&"p99".to_string()));
        assert!(t.contains(&"latency".to_string()));
        assert!(!t.contains(&"the".to_string())); // stopword
        assert!(!t.contains(&"pr".to_string())); // < 3 chars
        assert!(!t.contains(&"for".to_string())); // stopword
    }

    #[test]
    fn token_matches_handles_prefix_stems() {
        assert!(token_matches("review", "review"));
        assert!(token_matches("review", "reviewer")); // prefix stem
        assert!(token_matches("deploy", "deployment"));
        assert!(token_matches("testing", "test"));
        // Too short a shared stem doesn't match.
        assert!(!token_matches("cat", "category"));
        // Unrelated words don't match.
        assert!(!token_matches("review", "deploy"));
    }

    #[test]
    fn name_match_alone_qualifies() {
        let skills = catalog();
        // "reviewer" is a name token → single match, weight >= MIN_SCORE.
        let h = hint("can you act as a reviewer on my change", &skills).unwrap();
        assert!(h.contains("pr-reviewer"), "{h}");
        assert!(h.contains("/skills/pr-reviewer/SKILL.md"), "{h}");
        assert!(h.starts_with("[aish skill-awareness]"), "{h}");
    }

    #[test]
    fn description_keywords_can_qualify_without_name() {
        // None of these words are in a NAME, but three hit the pr-reviewer
        // description (correctness, security, style) → score 3 == MIN_SCORE.
        let skills = catalog();
        let h = hint("check this for correctness, security, and style", &skills);
        let h = h.expect("three description hits should qualify");
        assert!(h.contains("pr-reviewer"), "{h}");
    }

    #[test]
    fn weak_overlap_does_not_trigger() {
        // A single incidental description word (one point) is below MIN_SCORE.
        let skills = catalog();
        assert_eq!(hint("update the project license header", &skills), None);
    }

    #[test]
    fn plain_prose_with_no_skill_returns_none() {
        let skills = catalog();
        assert_eq!(hint("what is the capital of texas", &skills), None);
        assert_eq!(hint("list the files in this directory", &skills), None);
    }

    #[test]
    fn empty_catalog_or_empty_task_is_none() {
        assert_eq!(hint("review this PR for security", &[]), None);
        assert_eq!(hint("", &catalog()), None);
        assert_eq!(hint("   ", &catalog()), None);
    }

    #[test]
    fn ranking_orders_by_score_then_name() {
        let skills = catalog();
        // "performance bottleneck latency" hits the profiler name + description
        // strongly; nothing else should rank.
        let ranked = rank("find the performance bottleneck causing P99 latency", &skills);
        assert_eq!(ranked[0].skill.name, "performance-profiler");
        assert!(ranked[0].score >= NAME_WEIGHT);
    }

    #[test]
    fn multiple_matches_render_a_list_capped_at_two() {
        // Craft a task that hits two skills by name: "review" (pr-reviewer) and
        // "incident" (incident-responder).
        let skills = catalog();
        let h = hint("review the incident from the failed deploy", &skills).unwrap();
        assert!(h.contains("pr-reviewer"), "{h}");
        assert!(h.contains("incident-responder"), "{h}");
        // Rendered as a bullet list, not the single-skill sentence.
        assert!(h.contains("\n- `"), "{h}");
        // Never more than MAX_HINTS bullets.
        assert!(h.matches("\n- `").count() <= MAX_HINTS, "{h}");
    }

    // ---- registry recommendation (no installed skill matched) -----------

    fn search_result(name: &str, description: &str, reference: &str) -> crate::skill_provider::SearchResult {
        crate::skill_provider::SearchResult {
            name: name.into(),
            author: "anthropic".into(),
            description: description.into(),
            version: "1.0.0".into(),
            reference: reference.into(),
        }
    }

    /// A small registry catalog modeled on the binary-shipped index.json.
    fn registry() -> Vec<crate::skill_provider::SearchResult> {
        vec![
            search_result("git-rebase", "Rebase and squash git commits interactively.", "anthropic/git-rebase"),
            search_result("kubernetes-deploy", "Deploy applications to Kubernetes clusters.", "anthropic/kubernetes-deploy"),
            search_result("terraform-plan", "Plan and apply Terraform infrastructure changes.", "anthropic/terraform-plan"),
        ]
    }

    #[test]
    fn skill_worthy_gate_filters_trivial_tasks() {
        // Substantial multi-word tasks clear the bar; short commands don't.
        assert!(is_skill_worthy("deploy this application to a kubernetes cluster"));
        assert!(!is_skill_worthy("ls /tmp"));
        assert!(!is_skill_worthy("git status"));
        assert!(!is_skill_worthy(""));
    }

    #[test]
    fn recommends_a_relevant_registry_skill_when_none_installed() {
        let rec = recommend_install(
            "deploy the service to our kubernetes cluster",
            &[],
            &registry(),
        )
        .expect("a kubernetes task should match the kubernetes-deploy skill");
        assert_eq!(rec.reference, "anthropic/kubernetes-deploy");
        assert!(rec.note.starts_with("[aish skill-awareness]"), "{}", rec.note);
        assert!(rec.note.contains(":skill add anthropic/kubernetes-deploy"), "{}", rec.note);
        // The note steers away from faking/hand-rolling the skill.
        assert!(rec.note.to_lowercase().contains("not pretend"), "{}", rec.note);
    }

    #[test]
    fn no_recommendation_when_the_skill_is_already_installed() {
        // The matching skill is installed → the local nudge handles it; the
        // registry path must NOT also recommend installing it again.
        let installed = vec![skill(
            "kubernetes-deploy",
            "Deploy applications to Kubernetes clusters.",
        )];
        assert_eq!(
            recommend_install(
                "deploy the service to our kubernetes cluster",
                &installed,
                &registry(),
            ),
            None
        );
    }

    #[test]
    fn no_recommendation_for_trivial_or_unmatched_tasks() {
        // Below the skill-worthy token bar.
        assert_eq!(recommend_install("git status", &[], &registry()), None);
        // Substantial, but nothing in the catalog is relevant.
        assert_eq!(
            recommend_install("what is the capital of france today please", &[], &registry()),
            None
        );
    }
}
