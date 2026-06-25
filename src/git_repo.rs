//! Cached, branch-aware git repo state — `docs/git-repo-cache.md`.
//!
//! aish shells `git` probes ad hoc all over the tree but never records what it
//! found, and never gates a *read/sync* on the trunk-branch check. This module
//! resolves a checkout's identity + state ONCE, applies an explicit
//! strict/permissive trunk guard, and persists the result keyed by `repo_key`
//! so later queries answer without re-shelling git.
//!
//! It complements — does not replace — the push/commit mutation guard in
//! `tools.rs`: that stops an agent *writing* the default branch; this stops an
//! agent *recording a feature branch's state as the repo's authoritative state*
//! when the caller asked for trunk.
//!
//! `sync` OBSERVES only. It never checks out, fetches, or commits.
// Skeleton: the cache type + API have no in-tree caller yet (wired to a real
// consumer in a follow-up per docs/git-repo-cache.md). Allow until then.
#![allow(dead_code)]

use crate::git;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How many porcelain entries to keep as a sample in a [`DirtyDetails`].
const DIRTY_SAMPLE: usize = 5;

/// Identity + last-observed state of one git checkout. Persisted by `repo_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    /// Stable key (`owner--repo` or basename+hash) — `git::repo_key`.
    pub repo_key: String,
    /// Worktree root (`git rev-parse --show-toplevel`).
    pub root: PathBuf,
    pub remote_url: Option<String>,
    /// Resolved trunk (`git::trunk_branch`), never hard-coded `main`.
    pub trunk_branch: String,
    /// Checked-out branch; `None` on a detached HEAD.
    pub current_branch: Option<String>,
    /// `current_branch == Some(trunk_branch)` — the cached guard result.
    pub on_trunk: bool,
    pub head_sha: String,
    pub dirty: bool,
    /// SQLite `current_timestamp` string written at persist time. Empty on a
    /// freshly-built (not-yet-loaded) state.
    pub synced_at: String,
}

/// Uncommitted-change detail, surfaced only when a caller sets `require_clean`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyDetails {
    pub changed_paths: usize,
    /// First few `git status --porcelain` entries, for the message.
    pub sample: Vec<String>,
}

/// Why a sync failed. Callers branch on the variant to react precisely.
#[derive(Debug)]
pub enum GitError {
    NotAGitRepo,
    DetachedHead,
    NotOnTrunk {
        current_branch: String,
        trunk: String,
    },
    Dirty(DirtyDetails),
    Io(std::io::Error),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::NotAGitRepo => write!(f, "not a git repository"),
            GitError::DetachedHead => write!(f, "detached HEAD — no current branch to check"),
            GitError::NotOnTrunk {
                current_branch,
                trunk,
            } => write!(
                f,
                "on branch `{current_branch}`, not the trunk `{trunk}` — refusing to sync repo state"
            ),
            GitError::Dirty(d) => {
                write!(
                    f,
                    "working tree has {} uncommitted change(s)",
                    d.changed_paths
                )
            }
            GitError::Io(e) => write!(f, "git io error: {e}"),
        }
    }
}

impl std::error::Error for GitError {}

/// Strict/permissive knobs for a sync. `Default` is the strict trunk gate that a
/// shell-init / pre-release caller wants.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    /// Fail with `NotOnTrunk` / `DetachedHead` when the checkout isn't on the
    /// resolved trunk. A strict sync that fails this guard persists NOTHING.
    pub require_trunk: bool,
    /// Fail with `Dirty` when the working tree has uncommitted changes.
    pub require_clean: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            require_trunk: true,
            require_clean: false,
        }
    }
}

impl SyncOptions {
    /// Permissive: never fail the guard — resolve + persist, recording the
    /// off-trunk fact. The mode agent-context gathering uses.
    pub fn permissive() -> Self {
        Self {
            require_trunk: false,
            require_clean: false,
        }
    }
}

/// The PURE guard core (no IO): given observed facts + options, decide whether
/// the sync passes. Split out so every row of the failure-semantics table is
/// unit-testable without spawning git, mirroring `tools::protected_git_mutation`
/// and `worker::should_sweep`.
///
/// `detached` ⇒ `current_branch` is `None`. `dirty_details` is `Some` exactly
/// when the tree is dirty. Returns `Ok(())` when the sync may persist.
pub(crate) fn evaluate_guard(
    detached: bool,
    current_branch: Option<&str>,
    trunk: &str,
    on_trunk: bool,
    dirty_details: Option<&DirtyDetails>,
    opts: SyncOptions,
) -> Result<(), GitError> {
    if opts.require_trunk {
        if detached {
            return Err(GitError::DetachedHead);
        }
        if !on_trunk {
            return Err(GitError::NotOnTrunk {
                current_branch: current_branch.unwrap_or("").to_string(),
                trunk: trunk.to_string(),
            });
        }
    }
    if opts.require_clean {
        if let Some(d) = dirty_details {
            return Err(GitError::Dirty(d.clone()));
        }
    }
    Ok(())
}

/// Cached repo state, in its OWN SQLite connection (same pattern as
/// `BatchStore` / `CoordinatorStore`): points at the same `aish.db`, WAL makes
/// the concurrent connections safe, and a background task can sync without
/// sharing the main `Db` connection. Cloneable.
#[derive(Clone)]
pub struct GitRepoCache {
    conn: Arc<Mutex<Connection>>,
}

impl GitRepoCache {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("can't open git repo cache at {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS repo_state (
                 repo_key       TEXT PRIMARY KEY,
                 root           TEXT NOT NULL,
                 remote_url     TEXT,
                 trunk_branch   TEXT NOT NULL,
                 current_branch TEXT,
                 on_trunk       INTEGER NOT NULL,
                 head_sha       TEXT NOT NULL,
                 dirty          INTEGER NOT NULL,
                 synced_at      TEXT NOT NULL DEFAULT current_timestamp
             );",
        )
        .context("repo_state schema init failed")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Resolve `path`'s repo identity + state and persist it.
    ///
    /// Strict (`require_trunk`): returns `Err(NotOnTrunk{..})` / `DetachedHead`
    /// WITHOUT writing — a strict sync that fails the guard records nothing, so
    /// the stored state is never the off-trunk one. Permissive: always resolves
    /// + persists, with `on_trunk` recorded so a later query can warn "this is
    /// `feat/foo`, not `main`".
    pub fn sync(&self, path: &Path, opts: SyncOptions) -> Result<RepoState, GitError> {
        if !git::is_git_repo(path) {
            return Err(GitError::NotAGitRepo);
        }
        // Observe — never mutate.
        let current_branch = git::current_branch(path);
        let detached = current_branch.is_none();
        let trunk = git::trunk_branch(path);
        let on_trunk = current_branch.as_deref() == Some(trunk.as_str());
        let head_sha = git::git_head(path).unwrap_or_default();
        let root = git::toplevel(path)
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf());
        let remote_url = git::origin_url(path);
        let porcelain = git::dirty_porcelain(path).unwrap_or_default();
        let dirty = !porcelain.is_empty();
        let dirty_details = dirty.then(|| DirtyDetails {
            changed_paths: porcelain.len(),
            sample: porcelain.iter().take(DIRTY_SAMPLE).cloned().collect(),
        });

        // Apply the guard. A failing strict guard returns here, before any write.
        evaluate_guard(
            detached,
            current_branch.as_deref(),
            &trunk,
            on_trunk,
            dirty_details.as_ref(),
            opts,
        )?;

        let repo_key = git::repo_key(path);
        let mut state = RepoState {
            repo_key,
            root,
            remote_url,
            trunk_branch: trunk,
            current_branch,
            on_trunk,
            head_sha,
            dirty,
            synced_at: String::new(),
        };
        let synced_at = self.persist(&state).map_err(|e| {
            GitError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))
        })?;
        state.synced_at = synced_at;
        Ok(state)
    }

    /// Upsert the row and return the `synced_at` timestamp the DB stamped.
    fn persist(&self, s: &RepoState) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO repo_state
                 (repo_key, root, remote_url, trunk_branch, current_branch, on_trunk, head_sha, dirty, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, current_timestamp)
             ON CONFLICT(repo_key) DO UPDATE SET
                 root           = excluded.root,
                 remote_url     = excluded.remote_url,
                 trunk_branch   = excluded.trunk_branch,
                 current_branch = excluded.current_branch,
                 on_trunk       = excluded.on_trunk,
                 head_sha       = excluded.head_sha,
                 dirty          = excluded.dirty,
                 synced_at      = current_timestamp",
            rusqlite::params![
                s.repo_key,
                s.root.to_string_lossy(),
                s.remote_url,
                s.trunk_branch,
                s.current_branch,
                s.on_trunk as i64,
                s.head_sha,
                s.dirty as i64,
            ],
        )?;
        Ok(conn.query_row(
            "SELECT synced_at FROM repo_state WHERE repo_key = ?1",
            [&s.repo_key],
            |r| r.get(0),
        )?)
    }

    /// Last-synced state for a `repo_key`, or `None` if never synced.
    pub fn get(&self, repo_key: &str) -> Result<Option<RepoState>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT repo_key, root, remote_url, trunk_branch, current_branch, on_trunk, head_sha, dirty, synced_at
                 FROM repo_state WHERE repo_key = ?1",
                [repo_key],
                row_to_state,
            )
            .optional()?)
    }

    /// Every repo whose last sync observed it on its trunk (`on_trunk = 1`).
    pub fn query_trunk_repos(&self) -> Result<Vec<RepoState>> {
        self.query_by_on_trunk(true)
    }

    /// Every repo last seen OFF its trunk (`on_trunk = 0`) — the
    /// "careful, this is a branch" set.
    pub fn query_off_trunk_repos(&self) -> Result<Vec<RepoState>> {
        self.query_by_on_trunk(false)
    }

    fn query_by_on_trunk(&self, on_trunk: bool) -> Result<Vec<RepoState>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT repo_key, root, remote_url, trunk_branch, current_branch, on_trunk, head_sha, dirty, synced_at
             FROM repo_state WHERE on_trunk = ?1 ORDER BY repo_key",
        )?;
        let rows = stmt.query_map([on_trunk as i64], row_to_state)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }
}

/// Materialize a `RepoState` from a `repo_state` row (column order fixed by the
/// SELECTs above).
fn row_to_state(r: &rusqlite::Row<'_>) -> rusqlite::Result<RepoState> {
    Ok(RepoState {
        repo_key: r.get(0)?,
        root: PathBuf::from(r.get::<_, String>(1)?),
        remote_url: r.get(2)?,
        trunk_branch: r.get(3)?,
        current_branch: r.get(4)?,
        on_trunk: r.get::<_, i64>(5)? != 0,
        head_sha: r.get(6)?,
        dirty: r.get::<_, i64>(7)? != 0,
        synced_at: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(n: usize) -> DirtyDetails {
        DirtyDetails {
            changed_paths: n,
            sample: vec![" M src/x.rs".into()],
        }
    }

    // ---- pure guard: every row of the failure-semantics table ----

    #[test]
    fn on_trunk_clean_passes_both_modes() {
        assert!(
            evaluate_guard(
                false,
                Some("main"),
                "main",
                true,
                None,
                SyncOptions::default()
            )
            .is_ok()
        );
        assert!(
            evaluate_guard(
                false,
                Some("main"),
                "main",
                true,
                None,
                SyncOptions::permissive()
            )
            .is_ok()
        );
    }

    #[test]
    fn feature_branch_strict_is_not_on_trunk() {
        let e = evaluate_guard(
            false,
            Some("feat/foo"),
            "main",
            false,
            None,
            SyncOptions::default(),
        )
        .unwrap_err();
        match e {
            GitError::NotOnTrunk {
                current_branch,
                trunk,
            } => {
                assert_eq!(current_branch, "feat/foo");
                assert_eq!(trunk, "main");
            }
            other => panic!("expected NotOnTrunk, got {other:?}"),
        }
    }

    #[test]
    fn feature_branch_permissive_passes() {
        assert!(
            evaluate_guard(
                false,
                Some("feat/foo"),
                "main",
                false,
                None,
                SyncOptions::permissive()
            )
            .is_ok()
        );
    }

    #[test]
    fn detached_strict_is_detached_permissive_passes() {
        assert!(matches!(
            evaluate_guard(true, None, "main", false, None, SyncOptions::default()).unwrap_err(),
            GitError::DetachedHead
        ));
        assert!(evaluate_guard(true, None, "main", false, None, SyncOptions::permissive()).is_ok());
    }

    #[test]
    fn dirty_only_fails_when_require_clean() {
        // require_clean off → dirty is fine in both trunk modes.
        assert!(
            evaluate_guard(
                false,
                Some("main"),
                "main",
                true,
                Some(&details(2)),
                SyncOptions::default()
            )
            .is_ok()
        );
        // require_clean on → Dirty with the detail.
        let opts = SyncOptions {
            require_trunk: true,
            require_clean: true,
        };
        match evaluate_guard(false, Some("main"), "main", true, Some(&details(3)), opts)
            .unwrap_err()
        {
            GitError::Dirty(d) => assert_eq!(d.changed_paths, 3),
            other => panic!("expected Dirty, got {other:?}"),
        }
        // clean + require_clean → ok.
        assert!(evaluate_guard(false, Some("main"), "main", true, None, opts).is_ok());
    }

    #[test]
    fn master_trunk_is_honoured() {
        // A repo whose resolved trunk is `master`: on it → ok; on main → NotOnTrunk.
        assert!(
            evaluate_guard(
                false,
                Some("master"),
                "master",
                true,
                None,
                SyncOptions::default()
            )
            .is_ok()
        );
        assert!(matches!(
            evaluate_guard(
                false,
                Some("main"),
                "master",
                false,
                None,
                SyncOptions::default()
            )
            .unwrap_err(),
            GitError::NotOnTrunk { .. }
        ));
    }

    // ---- store round-trip ----

    fn temp_cache(name: &str) -> (GitRepoCache, PathBuf) {
        let path =
            std::env::temp_dir().join(format!("aish_repocache_{name}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (GitRepoCache::open(&path).unwrap(), path)
    }

    fn mk_state(key: &str, on_trunk: bool, branch: Option<&str>) -> RepoState {
        RepoState {
            repo_key: key.into(),
            root: PathBuf::from(format!("/repos/{key}")),
            remote_url: Some(format!("https://github.com/o/{key}.git")),
            trunk_branch: "main".into(),
            current_branch: branch.map(String::from),
            on_trunk,
            head_sha: "deadbeef".into(),
            dirty: false,
            synced_at: String::new(),
        }
    }

    #[test]
    fn upsert_replaces_and_get_returns_latest() {
        let (cache, path) = temp_cache("upsert");
        assert!(cache.get("o--a").unwrap().is_none());

        let s1 = mk_state("o--a", false, Some("feat/x"));
        let ts1 = cache.persist(&s1).unwrap();
        assert!(!ts1.is_empty());
        let got = cache.get("o--a").unwrap().unwrap();
        assert_eq!(got.current_branch.as_deref(), Some("feat/x"));
        assert!(!got.on_trunk);

        // Re-sync the SAME repo_key on trunk now — upsert replaces, no 2nd row.
        let s2 = mk_state("o--a", true, Some("main"));
        cache.persist(&s2).unwrap();
        let got = cache.get("o--a").unwrap().unwrap();
        assert_eq!(got.current_branch.as_deref(), Some("main"));
        assert!(got.on_trunk);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn queries_partition_by_on_trunk_and_persist_across_reopen() {
        let (cache, path) = temp_cache("partition");
        cache
            .persist(&mk_state("o--ontrunk", true, Some("main")))
            .unwrap();
        cache
            .persist(&mk_state("o--branch", false, Some("feat/y")))
            .unwrap();
        cache
            .persist(&mk_state("o--detached", false, None))
            .unwrap();

        let on = cache.query_trunk_repos().unwrap();
        assert_eq!(
            on.iter().map(|s| s.repo_key.as_str()).collect::<Vec<_>>(),
            vec!["o--ontrunk"]
        );
        let off = cache.query_off_trunk_repos().unwrap();
        assert_eq!(
            off.iter().map(|s| s.repo_key.as_str()).collect::<Vec<_>>(),
            vec!["o--branch", "o--detached"]
        );
        // Detached row preserved its NULL current_branch.
        assert!(
            off.iter()
                .find(|s| s.repo_key == "o--detached")
                .unwrap()
                .current_branch
                .is_none()
        );

        // Reopen the same file — persisted state survives the restart.
        let reopened = GitRepoCache::open(&path).unwrap();
        assert_eq!(reopened.query_trunk_repos().unwrap().len(), 1);
        assert_eq!(reopened.query_off_trunk_repos().unwrap().len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    // ---- integration over a throwaway git repo ----

    /// Best-effort `git` in `dir`; returns true on success. Skips the test body
    /// (returns false) if git isn't available so CI without git doesn't fail.
    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn sync_strict_rejects_feature_branch_then_accepts_trunk() {
        let dir = std::env::temp_dir().join(format!("aish_repocache_int_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if !git(&dir, &["init", "-b", "main"]) {
            eprintln!("skipping: git unavailable");
            return;
        }
        std::fs::write(dir.join("f.txt"), b"hi").unwrap();
        assert!(git(&dir, &["add", "."]));
        assert!(git(&dir, &["commit", "-m", "init"]));

        let (cache, dbpath) = temp_cache("integration");

        // On trunk (main) → strict sync succeeds and records on_trunk.
        let st = cache
            .sync(&dir, SyncOptions::default())
            .expect("trunk sync ok");
        assert!(st.on_trunk);
        assert_eq!(st.current_branch.as_deref(), Some("main"));
        assert!(!st.head_sha.is_empty());

        // Switch to a feature branch → strict sync is rejected and writes nothing.
        assert!(git(&dir, &["checkout", "-b", "feat/z"]));
        let key = st.repo_key.clone();
        let err = cache.sync(&dir, SyncOptions::default()).unwrap_err();
        assert!(matches!(err, GitError::NotOnTrunk { .. }), "got {err:?}");
        // Invariant: the strict failure persisted NOTHING — stored state is still
        // the on-trunk one from before.
        assert!(
            cache.get(&key).unwrap().unwrap().on_trunk,
            "strict failure must not overwrite"
        );

        // Permissive sync on the feature branch DOES record it (on_trunk=false).
        let st2 = cache
            .sync(&dir, SyncOptions::permissive())
            .expect("permissive ok");
        assert!(!st2.on_trunk);
        assert_eq!(st2.current_branch.as_deref(), Some("feat/z"));
        assert!(!cache.get(&key).unwrap().unwrap().on_trunk);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dbpath);
    }

    #[test]
    fn sync_non_repo_is_not_a_git_repo() {
        let dir =
            std::env::temp_dir().join(format!("aish_repocache_norepo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (cache, dbpath) = temp_cache("norepo");
        assert!(matches!(
            cache.sync(&dir, SyncOptions::default()).unwrap_err(),
            GitError::NotAGitRepo
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dbpath);
    }
}
