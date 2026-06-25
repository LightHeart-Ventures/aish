# Git repo cache + branch-guarded sync — design

Status: **design** (this doc). Implementation lands behind it as a reviewable,
test-first add — see "Build plan" at the end.

## Problem

aish already runs git probes all over the tree — `is_git_repo`, `repo_key`,
`trunk_branch`, `current_git_branch`, `git_head` — but it does so **ad hoc, per
call, with no record of what it found**. Two concrete gaps follow from that:

1. **No cached repo state.** Every consumer (the worker worktree layer in
   `worker.rs`, the default-branch guard in `tools.rs`, future agent-context
   gathering) re-shells `git rev-parse …` on demand. There's no single place
   that knows "this checkout is repo X, on branch Y, at sha Z, clean/dirty, as
   of time T". Agent context assembly (`context.rs` neighbourhood) has nothing
   structured to attach.

2. **No branch guard at sync time.** When something records "the repo state",
   nothing verifies the checkout is on the trunk. An agent sitting on a feature
   branch can capture stale/incomplete state and present it as "the repo", and a
   later main-assuming operation (a release, a merge, a "what's on main" query)
   silently acts on the wrong baseline. The `tools.rs` guard only fires at *push
   /commit* time; there is no equivalent at *read/sync* time.

This design adds a small, cached, branch-aware **`GitRepoCache`** so a repo's
state is resolved once, stored, queryable, and — crucially — gated on the
trunk-branch check with an explicit strict/permissive choice at the call site.

## Goals / non-goals

**Goals**
- One typed `sync_git_repo(...)` entry point that resolves a checkout's identity
  + state and persists it.
- A `require_main` switch: strict callers fail fast off-trunk; permissive
  callers sync anyway and record the off-trunk fact for later inspection.
- A typed `GitError` so callers branch on *why* a sync failed (not-a-repo,
  detached, off-trunk, dirty, IO).
- Cheap, queryable storage of the last-synced state, keyed by the existing
  `repo_key`, so `query_main_repos()` etc. answer without re-shelling git.
- Reuse the git plumbing already in `worker.rs` — do **not** invent a second set
  of git probes.

**Non-goals**
- Replacing the push/commit guard in `tools.rs` (that's a *mutation* gate; this
  is a *read/sync* gate — complementary, see "Relationship to the existing
  guard").
- Auto-fetching / auto-switching branches. Sync **observes**; it never mutates
  the working tree.
- A long-lived in-process watcher. Sync is pull-based, called at well-defined
  moments (shell init, pre-release, context gathering).

## Trunk, not literally "main"

The repo's trunk is **not** hard-coded to `main`. `worker.rs::trunk_branch`
already resolves it correctly: `origin/HEAD` when set, else whichever of
`main`/`master` exists, else `main`. The guard checks "current branch ==
resolved trunk", so a `master`-trunk repo (or a repo whose default is renamed)
is handled. The `GitError` variant is named `NotOnTrunk` (with a `main`-friendly
message) rather than `NotOnMain` to avoid baking the literal in.

## API

A new module `src/git_repo.rs`. The cache owns a dedicated SQLite connection
(same pattern as `BatchStore` / `CoordinatorStore` in `db.rs`: own connection,
same `aish.db`, WAL-safe), so a background task can sync without sharing the main
`Db` connection.

```rust
/// Identity + last-observed state of one git checkout. Persisted by repo_key.
pub struct RepoState {
    pub repo_key: String,        // worker.rs::repo_key — owner--repo or basename+hash
    pub root: PathBuf,           // worktree root (git rev-parse --show-toplevel)
    pub remote_url: Option<String>,
    pub trunk_branch: String,    // resolved trunk (worker.rs::trunk_branch)
    pub current_branch: Option<String>, // None on detached HEAD
    pub on_trunk: bool,          // current_branch == trunk_branch
    pub head_sha: String,
    pub dirty: bool,             // git status --porcelain non-empty
    pub synced_at: String,       // SQLite current_timestamp
}

pub enum GitError {
    NotAGitRepo,
    DetachedHead,
    NotOnTrunk { current_branch: String, trunk: String },
    Dirty(DirtyDetails),         // only surfaced when the caller asks (require_clean)
    Io(std::io::Error),
}

pub struct DirtyDetails {
    pub changed_paths: usize,    // count from `git status --porcelain`
    pub sample: Vec<String>,     // first few entries, for the message
}

#[derive(Clone, Copy)]
pub struct SyncOptions {
    /// Fail with NotOnTrunk when the checkout isn't on the resolved trunk.
    pub require_trunk: bool,      // default true
    /// Fail with Dirty when the working tree has uncommitted changes.
    pub require_clean: bool,      // default false
}
impl Default for SyncOptions { /* require_trunk: true, require_clean: false */ }

pub struct GitRepoCache { /* Arc<Mutex<Connection>> */ }

impl GitRepoCache {
    pub fn open(path: &Path) -> Result<Self>;

    /// Resolve `path`'s repo identity + state and persist it.
    ///
    /// Strict (require_trunk): returns Err(NotOnTrunk{..}) WITHOUT writing —
    /// a strict sync that fails the guard records nothing, so the stored state
    /// is never the off-trunk one.
    /// Permissive (require_trunk=false): always resolves + persists, with
    /// on_trunk recorded so a later query can warn "this is feat/foo, not main".
    pub fn sync(&self, path: &Path, opts: SyncOptions) -> Result<RepoState, GitError>;

    /// Last-synced state for a repo_key, or None if never synced.
    pub fn get(&self, repo_key: &str) -> Result<Option<RepoState>>;

    /// Every repo whose last sync observed it on its trunk (on_trunk = true).
    pub fn query_trunk_repos(&self) -> Result<Vec<RepoState>>;

    /// Every repo last seen OFF its trunk — the "careful, this is a branch" set.
    pub fn query_off_trunk_repos(&self) -> Result<Vec<RepoState>>;
}
```

### Usage

```rust
// Shell init / pre-release — fail fast if not on trunk:
match cache.sync(&cwd, SyncOptions::default()) {
    Ok(state) => { /* state.on_trunk == true here */ }
    Err(GitError::NotOnTrunk { current_branch, trunk }) => {
        // surface: "you're on `current_branch`, not `trunk` — refusing to sync"
    }
    Err(GitError::NotAGitRepo) => { /* skip — not every cwd is a repo */ }
    Err(e) => { /* log + continue */ }
}

// Agent context gathering — permissive: record the branch, don't fail:
let state = cache.sync(&cwd, SyncOptions { require_trunk: false, ..Default::default() })?;
if !state.on_trunk {
    // attach to context: "working on branch `feat/foo`, not trunk"
}
```

### Multi-repo (optional convenience, can ship later)

```rust
/// Sync many checkouts. With require_trunk, returns the FIRST off-trunk repo as
/// the error so a release flow can refuse to ship if ANY repo is off trunk.
pub fn sync_all(&self, paths: &[PathBuf], opts: SyncOptions)
    -> Result<Vec<RepoState>, (PathBuf, GitError)>;
```

## Storage

One table, created the same way the other stores create theirs in `db.rs`
(`CREATE TABLE IF NOT EXISTS`, WAL, idempotent `ALTER … ADD COLUMN` for
back-compat). Keyed by `repo_key` so it dedups across re-syncs of the same
checkout and joins cleanly with the worker/worktree layer that already speaks
`repo_key`.

```sql
CREATE TABLE IF NOT EXISTS repo_state (
    repo_key       TEXT PRIMARY KEY,
    root           TEXT NOT NULL,
    remote_url     TEXT,
    trunk_branch   TEXT NOT NULL,
    current_branch TEXT,                 -- NULL on detached HEAD
    on_trunk       INTEGER NOT NULL,     -- 0/1 cached guard result
    head_sha       TEXT NOT NULL,
    dirty          INTEGER NOT NULL,     -- 0/1
    synced_at      TEXT NOT NULL DEFAULT current_timestamp
);
```

Upsert on `repo_key` (`ON CONFLICT(repo_key) DO UPDATE SET …`), so the row is
always the latest observation. `query_trunk_repos` is `WHERE on_trunk = 1`;
`query_off_trunk_repos` is `WHERE on_trunk = 0`.

## Where the git plumbing comes from (reuse, don't reinvent)

Every probe `sync` needs already exists in `worker.rs` — the implementation
**lifts them to `pub(crate)`** rather than writing new ones:

| Need | Existing fn (`worker.rs`) |
|---|---|
| Is this a repo? | `is_git_repo` |
| Stable key | `repo_key` (→ `repo_key_from_remote` / `fallback_repo_key`) |
| Trunk name | `trunk_branch` |
| HEAD sha | `git_head` |
| Trimmed git output | `git_out` / `git_ok` |
| Current branch (None on detached) | `current_git_branch` (`tools.rs`) — move to a shared `git` helper module |

`sync` is then a thin, pure-ish composition: probe → build `RepoState` → apply
`SyncOptions` guard → (persist or return Err). The guard decision itself
(`current_branch` vs `trunk` → `on_trunk` / `NotOnTrunk`) is a **pure function**
(`evaluate_guard`) so it's unit-testable with zero IO, mirroring how
`protected_git_mutation`, `should_sweep`, and `compaction_split` are split out
for testing across the codebase.

## Relationship to the existing default-branch guard

`tools.rs::git_default_branch_guard` / `protected_git_mutation` stop an agent
*writing* to the default branch (push/commit). This cache guards *reading/syncing*
the wrong branch's state. They're complementary and share the trunk-detection
idea but not code paths:

- **Mutation guard** (exists): "don't let the agent push/commit on main."
- **Sync guard** (this doc): "don't record a feature branch's state as the repo's
  authoritative state when the caller asked for trunk."

Both should ultimately agree on "what is the trunk", so the shared
`current_git_branch` + `trunk_branch` helpers are the natural consolidation
point — but this design does **not** change the mutation guard's behaviour.

## Failure semantics

| Condition | `require_trunk=true` | `require_trunk=false` |
|---|---|---|
| Not a git repo | `Err(NotAGitRepo)`, no write | `Err(NotAGitRepo)`, no write |
| Detached HEAD | `Err(DetachedHead)`, no write | sync, `current_branch=None`, `on_trunk=false` |
| On a feature branch | `Err(NotOnTrunk{..})`, **no write** | sync, `on_trunk=false`, recorded |
| On trunk, dirty, `require_clean=false` | sync, `dirty=true` | sync, `dirty=true` |
| On trunk, dirty, `require_clean=true` | `Err(Dirty(..))`, no write | `Err(Dirty(..))`, no write |
| On trunk, clean | sync, `on_trunk=true` | sync, `on_trunk=true` |

Invariant: a **strict** sync that fails its guard **persists nothing** — the
stored state is never the off-trunk/dirty one a strict caller rejected. A
permissive sync always persists (that's its whole point: capture the branch fact
for later).

## Testing

Pure, IO-free unit tests for the guard core (`evaluate_guard`), exactly like the
existing `protected_git_mutation` / `should_sweep` tests:

- on-trunk clean → `on_trunk=true`, no error
- on feature branch + `require_trunk` → `NotOnTrunk`
- on feature branch + permissive → `on_trunk=false`, ok
- detached + `require_trunk` → `DetachedHead`; permissive → `current_branch=None`
- dirty + `require_clean` → `Dirty`
- trunk resolution honours `master` / renamed default

Store round-trip tests mirroring `coordinator_store_roundtrip_and_resume`:
upsert replaces the row, `get` returns latest, `query_trunk_repos` /
`query_off_trunk_repos` partition by `on_trunk`, reopening the same file sees
the persisted state.

A couple of integration tests over a throwaway `git init` temp repo (create a
branch, sync strict → `NotOnTrunk`; checkout trunk, sync → ok) — gated the same
way the existing `is_git_repo`-touching tests are.

## Build plan (for the implementation PR)

1. Extract the shared git helpers (`is_git_repo`, `repo_key`, `trunk_branch`,
   `git_head`, `git_out`, `git_ok`, `current_git_branch`) into a small
   `src/git.rs` (or `pub(crate)` re-exports) so both `worker.rs` and the new
   cache use one set. No behaviour change.
2. Add `src/git_repo.rs`: `RepoState`, `GitError`, `SyncOptions`, the pure
   `evaluate_guard`, and `GitRepoCache` (open/sync/get/query_*), with the
   `repo_state` table.
3. Unit-test the pure guard + the store round-trip; add the temp-repo integration
   tests.
4. Wire one real caller (shell init → permissive sync, so context gathering can
   read `get(repo_key)`); leave strict callers (pre-release) as a follow-up so
   this PR stays a reviewable add with no behaviour change to existing flows.

## Out of scope (follow-ups)

- Strict-mode wiring into a release/merge command.
- Consolidating the `tools.rs` mutation guard onto the shared trunk helper.
- `sync_all` multi-repo enforcement in any real flow.
- Auto-fetch to freshen `head_sha` against the remote before recording.
