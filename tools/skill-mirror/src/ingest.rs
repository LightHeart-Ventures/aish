//! `skill-mirror ingest` — fill a SKILL.md tree from an allowlisted set of
//! GitHub repositories (TASK-695).
//!
//! The upstream half of the mirror. `generate` (TASK-694) turns a local tree of
//! `{owner}/{dir}/SKILL.md` into `index.json`; `ingest` is what *produces* that
//! tree, and the two meet at exactly that contract — ingest writes
//! `SKILL.md` + a `stars` sidecar, which is precisely what `generate`'s
//! validator already reads. Neither step imports the other's internals.
//!
//! ```text
//!   allowlist.toml ──▶ ingest ──▶ {owner}/{dir}/{SKILL.md,stars,source.json}
//!                        ▲                         │
//!                   state.json (ETags)             ▼
//!                                              generate ──▶ index.json + raw tree
//! ```
//!
//! Operating principles, in priority order:
//!
//! 1. **Bounded by a human.** Only allowlisted repos are crawled, ever.
//! 2. **Cheap when nothing changed.** A stored ETag turns an unchanged repo
//!    into one conditional 304 — no `GET /repos`, no raw fetches.
//! 3. **Fail soft per repo, fail hard on the catalog.** A dead repo is a WARN
//!    and its previously-written skills stay on disk; a run that produced
//!    nothing (or fewer than `--min-skills`) exits non-zero.
//! 4. **Deterministic.** Sorted walks, sorted candidates, and a documented
//!    duplicate tiebreak, so the same upstream state produces the same tree.

use crate::allowlist::RepoEntry;
use crate::github::{GitHubClient, TreeResponse};
use crate::state::{RepoState, State, now_unix};
use aish::skill_contract::{parse_frontmatter, sanitize_dir_segment};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use tokio::sync::Semaphore;

#[derive(clap::Args)]
pub struct IngestArgs {
    /// Allowlist of repositories to crawl (`allowlist.toml`).
    #[arg(long)]
    pub allowlist: PathBuf,
    /// Output tree — the `--input` of a later `skill-mirror generate`.
    #[arg(long)]
    pub out: PathBuf,
    /// Incremental-crawl state (ETags + stars). Created if absent.
    #[arg(long, default_value = "ingest-state.json")]
    pub state: PathBuf,
    /// GitHub token. Falls back to `$GITHUB_TOKEN`. Unauthenticated runs are
    /// capped at 60 requests/hour and will not get far.
    #[arg(long)]
    pub token: Option<String>,
    /// GitHub REST base (overridable for testing).
    #[arg(long, default_value = "https://api.github.com")]
    pub api_base: String,
    /// Raw-content base (overridable for testing).
    #[arg(long, default_value = "https://raw.githubusercontent.com")]
    pub raw_base: String,
    /// Repos crawled in parallel.
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,
    /// Skip any SKILL.md larger than this many bytes (default 256 KiB).
    #[arg(long, default_value_t = 262_144)]
    pub max_size: u64,
    /// Refuse to take more than this many skills from any one repo.
    #[arg(long, default_value_t = 100)]
    pub max_per_repo: usize,
    /// Fail the run when fewer than this many skills end up in the tree.
    #[arg(long, default_value_t = 0)]
    pub min_skills: usize,
    /// Ignore stored ETags and recrawl everything.
    #[arg(long)]
    pub no_cache: bool,
}

/// One SKILL.md that survived discovery, with everything needed to write it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub owner: String,
    pub repo: String,
    pub git_ref: String,
    /// Path inside the repo.
    pub path: String,
    /// Frontmatter `name:` — the second half of the catalog reference.
    pub name: String,
    /// Filesystem-safe directory segment for the output tree.
    pub dir: String,
    pub stars: u64,
    pub tree_sha: String,
    pub bytes: Vec<u8>,
}

impl Candidate {
    /// `{owner}/{name}` — what a user types into `:skill add`.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
    /// `{owner}/{dir}` — the output directory, relative to `--out`.
    pub fn dir_key(&self) -> String {
        format!("{}/{}", self.owner, self.dir)
    }
    fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Is this tree path a SKILL.md we should consider?
///
/// Exact-case `SKILL.md` basename only — the registry convention is uppercase,
/// and accepting `skill.md` too would make the catalog ambiguous on
/// case-insensitive filesystems. An optional `prefix` from the allowlist
/// narrows a big repo to the subtree that actually holds skills.
pub fn is_skill_path(path: &str, prefix: Option<&str>) -> bool {
    if let Some(p) = prefix {
        let p = p.trim_matches('/');
        if !p.is_empty() && !path.starts_with(&format!("{p}/")) {
            return false;
        }
    }
    path.rsplit('/').next() == Some("SKILL.md")
}

/// Why a candidate lost a duplicate-reference contest.
#[derive(Debug, Clone)]
pub struct Dropped {
    pub reference: String,
    pub loser: String,
    pub winner: String,
}

/// Resolve duplicate `{owner}/{name}` references.
///
/// Two repos under one owner can easily publish the same frontmatter `name:`
/// (a fork, a rename, a monorepo plus its extracted copy), and the catalog can
/// only hold one row per reference. The tiebreak, in order:
///
/// 1. **more stars wins** — the popularity signal the registry already ranks by;
/// 2. **lexicographically smaller `owner/repo`**, then
/// 3. **lexicographically smaller path**
///
/// Steps 2–3 exist purely so the result is deterministic when stars tie: the
/// same upstream state must always produce the same catalog.
pub fn pick_winners(mut cands: Vec<Candidate>) -> (Vec<Candidate>, Vec<Dropped>) {
    cands.sort_by(|a, b| {
        a.reference()
            .cmp(&b.reference())
            .then(b.stars.cmp(&a.stars))
            .then(a.slug().cmp(&b.slug()))
            .then(a.path.cmp(&b.path))
    });
    let mut winners: Vec<Candidate> = Vec::with_capacity(cands.len());
    let mut dropped = Vec::new();
    for c in cands {
        match winners.last() {
            Some(prev) if prev.reference() == c.reference() => dropped.push(Dropped {
                reference: c.reference(),
                loser: format!("{}:{}", c.slug(), c.path),
                winner: format!("{}:{}", prev.slug(), prev.path),
            }),
            _ => winners.push(c),
        }
    }
    (winners, dropped)
}

/// Per-repo crawl outcome.
enum Outcome {
    /// 304 + an intact on-disk cache: nothing fetched, previous skills kept.
    Cached {
        slug: String,
        prev: RepoState,
    },
    Crawled {
        slug: String,
        stars: u64,
        git_ref: String,
        tree_sha: String,
        etag: Option<String>,
        candidates: Vec<Candidate>,
        skipped: usize,
    },
    Failed {
        slug: String,
        error: String,
    },
}

pub async fn run(args: IngestArgs) -> Result<ExitCode> {
    let text = std::fs::read_to_string(&args.allowlist)
        .with_context(|| format!("reading allowlist {}", args.allowlist.display()))?;
    let entries = crate::allowlist::parse(&text)
        .with_context(|| format!("parsing allowlist {}", args.allowlist.display()))?;

    let token = args
        .token
        .clone()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());
    let client = Arc::new(
        GitHubClient::new(&args.api_base, &args.raw_base, token)?
            .with_retry_limits(4, std::time::Duration::from_secs(60)),
    );
    eprintln!(
        "skill-mirror: ingesting {} repo(s) from {} ({})",
        entries.len(),
        args.allowlist.display(),
        if client.authenticated() {
            "authenticated"
        } else {
            "anonymous — 60 req/hr"
        }
    );

    let state = State::load(&args.state);
    let out = Arc::new(args.out.clone());
    let sem = Arc::new(Semaphore::new(args.concurrency.max(1)));
    let max_size = args.max_size;
    let max_per_repo = args.max_per_repo;
    let no_cache = args.no_cache;

    let mut tasks = tokio::task::JoinSet::new();
    for entry in entries {
        let prev = state.get(&entry.slug()).cloned();
        let (client, sem, out) = (client.clone(), sem.clone(), out.clone());
        tasks.spawn(async move {
            let _permit = sem
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let slug = entry.slug();
            match crawl_repo(
                &client,
                &entry,
                prev,
                &out,
                max_size,
                max_per_repo,
                no_cache,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => Outcome::Failed {
                    slug,
                    error: format!("{e:#}"),
                },
            }
        });
    }

    let mut cached = 0usize;
    let mut crawled = 0usize;
    let mut failed = 0usize;
    let mut skipped_files = 0usize;
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut next_state = State::default();
    // Directories kept alive by a cached or failed repo — never GC'd.
    let mut keep_dirs: BTreeSet<String> = BTreeSet::new();
    // Repos whose tree we actually re-read this run: only these may retire a
    // directory, because only for these do we know the current upstream truth.
    let mut crawled_slugs: BTreeSet<String> = BTreeSet::new();

    while let Some(joined) = tasks.join_next().await {
        match joined.context("ingest worker panicked")? {
            Outcome::Cached { slug, prev } => {
                cached += 1;
                keep_dirs.extend(prev.skills.iter().cloned());
                next_state.repos.insert(
                    slug,
                    RepoState {
                        last_seen_unix: now_unix(),
                        ..prev
                    },
                );
            }
            Outcome::Crawled {
                slug,
                stars,
                git_ref,
                tree_sha,
                etag,
                candidates: found,
                skipped,
            } => {
                crawled += 1;
                crawled_slugs.insert(slug.clone());
                skipped_files += skipped;
                candidates.extend(found);
                next_state.repos.insert(
                    slug,
                    RepoState {
                        etag,
                        tree_sha: Some(tree_sha),
                        git_ref: Some(git_ref),
                        stars,
                        // Filled in after the duplicate contest resolves.
                        skills: Vec::new(),
                        last_seen_unix: now_unix(),
                    },
                );
            }
            Outcome::Failed { slug, error } => {
                failed += 1;
                eprintln!("skill-mirror: WARN {slug}: {error}");
                // Keep the previous entry verbatim: its ETag is still valid and
                // its already-published skills must not be garbage-collected
                // just because upstream was briefly unreachable.
                if let Some(prev) = state.get(&slug) {
                    keep_dirs.extend(prev.skills.iter().cloned());
                    next_state.repos.insert(slug, prev.clone());
                }
            }
        }
    }

    let (winners, dropped) = pick_winners(candidates);
    for d in &dropped {
        eprintln!(
            "skill-mirror: WARN duplicate reference {} — kept {}, dropped {}",
            d.reference, d.winner, d.loser
        );
    }

    // ---- write phase ------------------------------------------------------
    let mut written: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for w in &winners {
        write_skill(&args.out, w)
            .with_context(|| format!("writing {} from {}", w.dir_key(), w.slug()))?;
        written.entry(w.slug()).or_default().push(w.dir_key());
    }
    for (slug, dirs) in &written {
        if let Some(st) = next_state.repos.get_mut(slug) {
            st.skills = dirs.clone();
        }
    }

    // GC: a directory a crawled repo no longer publishes is removed, unless a
    // cached/failed repo still vouches for it or another repo just wrote it.
    let live: BTreeSet<String> = winners
        .iter()
        .map(Candidate::dir_key)
        .chain(keep_dirs.iter().cloned())
        .collect();
    let mut gc = 0usize;
    for (slug, prev) in &state.repos {
        // Crawled ⇒ we know what it publishes now. Absent from next_state ⇒ it
        // left the allowlist. Cached/failed ⇒ leave its output alone.
        let retirable = crawled_slugs.contains(slug) || !next_state.repos.contains_key(slug);
        if !retirable {
            continue;
        }
        for dir in &prev.skills {
            if !live.contains(dir) {
                let p = args.out.join(dir);
                if p.is_dir() && std::fs::remove_dir_all(&p).is_ok() {
                    gc += 1;
                }
            }
        }
    }

    let total = live.len();
    println!(
        "ingest: {crawled} crawled, {cached} cached (304), {failed} failed; \
         {} skill(s) written, {} kept from cache, {} duplicate(s) dropped, \
         {skipped_files} file(s) skipped, {gc} stale dir(s) removed",
        winners.len(),
        keep_dirs.len(),
        dropped.len()
    );

    if winners.is_empty() && keep_dirs.is_empty() {
        eprintln!("skill-mirror: error: ingest produced no skills");
        return Ok(ExitCode::FAILURE);
    }
    if total < args.min_skills {
        eprintln!(
            "skill-mirror: error: {total} skill(s) is below --min-skills {}",
            args.min_skills
        );
        return Ok(ExitCode::FAILURE);
    }

    next_state.save(&args.state)?;
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
async fn crawl_repo(
    client: &GitHubClient,
    entry: &RepoEntry,
    prev: Option<RepoState>,
    out: &Path,
    max_size: u64,
    max_per_repo: usize,
    no_cache: bool,
) -> Result<Outcome> {
    let slug = entry.slug();
    let (owner, repo) = (entry.owner.as_str(), entry.repo.as_str());
    let probe_ref = entry
        .git_ref
        .clone()
        .or_else(|| prev.as_ref().and_then(|p| p.git_ref.clone()))
        .unwrap_or_else(|| "HEAD".to_string());

    // --- conditional probe --------------------------------------------------
    let cached_etag = if no_cache {
        None
    } else {
        prev.as_ref().and_then(|p| p.etag.clone())
    };
    let mut tree = client
        .tree(owner, repo, &probe_ref, cached_etag.as_deref())
        .await?;

    if matches!(tree, TreeResponse::NotModified) {
        let prev = prev.clone().unwrap_or_default();
        if cache_intact(out, &prev.skills) {
            return Ok(Outcome::Cached { slug, prev });
        }
        // The ETag says "unchanged" but the tree on disk disagrees — a cache
        // that lies is worse than no cache, so recrawl unconditionally.
        eprintln!("skill-mirror: WARN {slug}: 304 but output is missing; recrawling");
        tree = client.tree(owner, repo, &probe_ref, None).await?;
    }

    let TreeResponse::Tree {
        sha,
        etag,
        blobs,
        truncated,
    } = tree
    else {
        unreachable!("the NotModified branch above either returned or refetched");
    };
    if truncated {
        eprintln!("skill-mirror: WARN {slug}: tree is truncated; some skills may be missing");
    }

    // Stars + the real branch name for raw fetches (one call, only when changed).
    let meta = client.repo_meta(owner, repo).await?;
    let git_ref = entry.git_ref.clone().unwrap_or(meta.default_branch);

    let mut candidates = Vec::new();
    let mut skipped = 0usize;
    for blob in blobs
        .iter()
        .filter(|b| is_skill_path(&b.path, entry.path.as_deref()))
    {
        if candidates.len() >= max_per_repo {
            eprintln!("skill-mirror: WARN {slug}: hit --max-per-repo {max_per_repo}");
            break;
        }
        // The tree carries blob sizes, so an oversize file costs zero bandwidth.
        if blob.size > max_size {
            eprintln!(
                "skill-mirror: WARN {slug}:{} is {} bytes, over the {max_size}-byte cap",
                blob.path, blob.size
            );
            skipped += 1;
            continue;
        }
        let bytes = match client.raw(owner, repo, &git_ref, &blob.path).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skill-mirror: WARN {slug}:{}: {e:#}", blob.path);
                skipped += 1;
                continue;
            }
        };
        if bytes.len() as u64 > max_size {
            eprintln!(
                "skill-mirror: WARN {slug}:{} is {} bytes, over the {max_size}-byte cap",
                blob.path,
                bytes.len()
            );
            skipped += 1;
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            eprintln!("skill-mirror: WARN {slug}:{} is not UTF-8", blob.path);
            skipped += 1;
            continue;
        };
        // Enough parsing to key the output tree; `generate` does the real
        // validation with the client's rules so there is exactly one gate.
        let Some((name, _desc)) = parse_frontmatter(text) else {
            eprintln!(
                "skill-mirror: WARN {slug}:{} has no usable frontmatter",
                blob.path
            );
            skipped += 1;
            continue;
        };
        let Ok(dir) = sanitize_dir_segment(&name) else {
            eprintln!(
                "skill-mirror: WARN {slug}:{}: `name: {name}` has no filesystem-safe form",
                blob.path
            );
            skipped += 1;
            continue;
        };
        candidates.push(Candidate {
            owner: owner.to_string(),
            repo: repo.to_string(),
            git_ref: git_ref.clone(),
            path: blob.path.clone(),
            name,
            dir,
            stars: meta.stars,
            tree_sha: sha.clone(),
            bytes,
        });
    }

    Ok(Outcome::Crawled {
        slug,
        stars: meta.stars,
        git_ref,
        tree_sha: sha,
        etag,
        candidates,
        skipped,
    })
}

/// Every cached directory still has a SKILL.md on disk.
fn cache_intact(out: &Path, dirs: &[String]) -> bool {
    !dirs.is_empty() && dirs.iter().all(|d| out.join(d).join("SKILL.md").is_file())
}

/// Write one skill into the output tree: the verbatim SKILL.md, the `stars`
/// sidecar `generate` reads, and a `source.json` provenance record.
fn write_skill(out: &Path, c: &Candidate) -> Result<()> {
    let dir = out.join(&c.owner).join(&c.dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("SKILL.md"), &c.bytes)?;
    std::fs::write(dir.join("stars"), format!("{}\n", c.stars))?;
    let source = serde_json::json!({
        "repo": c.slug(),
        "ref": c.git_ref,
        "path": c.path,
        "tree_sha": c.tree_sha,
        "stars": c.stars,
        "reference": c.reference(),
        "fetched_unix": now_unix(),
    });
    let mut json = serde_json::to_string_pretty(&source)?;
    json.push('\n');
    std::fs::write(dir.join("source.json"), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(owner: &str, repo: &str, name: &str, path: &str, stars: u64) -> Candidate {
        Candidate {
            owner: owner.into(),
            repo: repo.into(),
            git_ref: "main".into(),
            path: path.into(),
            name: name.into(),
            dir: name.into(),
            stars,
            tree_sha: "sha".into(),
            bytes: b"---\nname: x\ndescription: y\n---\n".to_vec(),
        }
    }

    #[test]
    fn skill_paths_are_matched_exactly_and_under_the_prefix() {
        assert!(is_skill_path("SKILL.md", None));
        assert!(is_skill_path("skills/foo/SKILL.md", None));
        assert!(!is_skill_path("skills/foo/skill.md", None));
        assert!(!is_skill_path("docs/SKILL.md.bak", None));
        assert!(is_skill_path("skills/foo/SKILL.md", Some("skills")));
        assert!(is_skill_path("skills/foo/SKILL.md", Some("/skills/")));
        assert!(!is_skill_path("other/foo/SKILL.md", Some("skills")));
        assert!(!is_skill_path("SKILL.md", Some("skills")));
    }

    #[test]
    fn duplicate_references_are_broken_by_stars_then_deterministically() {
        let (winners, dropped) = pick_winners(vec![
            cand("acme", "low", "widget", "SKILL.md", 3),
            cand("acme", "high", "widget", "SKILL.md", 90),
            cand("acme", "other", "gadget", "SKILL.md", 1),
        ]);
        assert_eq!(winners.len(), 2);
        let widget = winners.iter().find(|c| c.name == "widget").unwrap();
        assert_eq!(widget.repo, "high", "more stars must win");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].reference, "acme/widget");

        // Stars tie ⇒ lexicographically smaller repo, then path. Run it with
        // the inputs reversed to prove the result does not depend on order.
        let a = cand("acme", "bbb", "dup", "SKILL.md", 5);
        let b = cand("acme", "aaa", "dup", "skills/dup/SKILL.md", 5);
        let (w1, _) = pick_winners(vec![a.clone(), b.clone()]);
        let (w2, _) = pick_winners(vec![b, a]);
        assert_eq!(w1[0].repo, "aaa");
        assert_eq!(w1[0].repo, w2[0].repo);
        assert_eq!(w1[0].path, w2[0].path);
    }

    #[test]
    fn same_name_under_different_owners_is_not_a_duplicate() {
        let (winners, dropped) = pick_winners(vec![
            cand("alice", "r", "review", "SKILL.md", 1),
            cand("bob", "r", "review", "SKILL.md", 99),
        ]);
        assert_eq!(winners.len(), 2);
        assert!(dropped.is_empty());
    }

    #[test]
    fn write_skill_emits_the_generate_contract() {
        let out = std::env::temp_dir().join(format!("skill-mirror-write-{}", now_unix()));
        let c = cand("acme", "repo", "widget", "skills/widget/SKILL.md", 7);
        write_skill(&out, &c).unwrap();

        let dir = out.join("acme").join("widget");
        assert_eq!(std::fs::read(dir.join("SKILL.md")).unwrap(), c.bytes);
        // `generate`'s read_stars parses exactly this file.
        assert_eq!(
            std::fs::read_to_string(dir.join("stars"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap(),
            7
        );
        let src: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("source.json")).unwrap())
                .unwrap();
        assert_eq!(src["repo"], "acme/repo");
        assert_eq!(src["reference"], "acme/widget");
        assert!(cache_intact(&out, &["acme/widget".to_string()]));
        assert!(!cache_intact(&out, &["acme/missing".to_string()]));
        assert!(!cache_intact(&out, &[]), "an empty cache is never intact");
        let _ = std::fs::remove_dir_all(&out);
    }
}
