//! Opt-in reclamation of the PROVABLY-SAFE subset of leaked worker worktrees,
//! plus a purely informational disk-pressure warning.
//!
//! ## Why this module exists
//!
//! Measured on a live install: `~/.aish/worktrees` held **96 GB** across 51
//! live trees, and the `worktree_lifecycle` ledger had **176 rows, 91 of them
//! still open** (`cleaned_up_at IS NULL`) with **`cleanup_failed = 0`**. That
//! last number is the tell: cleanup is not FAILING, it is never ATTEMPTED.
//! Isolated-worker trees and their Rust `target/` dirs accumulate forever.
//!
//! ## Why this module does NOT just delete them
//!
//! [`crate::coordinator`]'s `report_leaked_worktrees` (ISS-409757) deliberately
//! never deletes anything: "removal is irreversible and an operator's unmerged
//! branch is exactly what's at stake." That decision stands — this module does
//! not change it, does not lower its 24-hour leak threshold, and does not run
//! on startup. It adds a NARROW, EXPLICIT, default-OFF path that reclaims only
//! trees whose safety can be PROVEN, and leaves everything else to the report.
//!
//! ## The gates (all must hold; anything unknown means SKIP)
//!
//! 1. The ledger row is still open and older than [`LEAK_AFTER_HOURS`].
//! 2. The tree is CLEAN — [`crate::worker::worktree_holds_work`] says no
//!    uncommitted changes and no commits ahead of trunk. That helper reads any
//!    git error as "holds work", so an unreadable tree is skipped for free.
//! 3. The branch is fully merged into trunk, proven with `git merge-base
//!    --is-ancestor`. Not-merged → skip. Can't tell → skip.
//! 4. Detached HEAD, a missing trunk ref, a git failure, a vanished path: skip.
//!
//! The asymmetry is deliberate. A skipped tree costs disk, which the operator
//! can reclaim on the next run; a wrongly-removed tree costs work that cannot
//! be recovered. So every uncertain branch resolves to SKIP.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::coordinator_store::CoordinatorStore;

/// Mirror of `coordinator::WORKTREE_LEAK_AFTER_HOURS` (24). Kept as a separate
/// constant so this module never has to touch `coordinator.rs` — the two are
/// compared in a unit test rather than coupled by an import, because the
/// 1900-1990 region of that file is under concurrent edit.
pub const LEAK_AFTER_HOURS: i64 = 24;

/// Default disk-pressure threshold for the worktree root, in GiB. Purely
/// informational: crossing it prints one dim line, nothing else.
pub const DEFAULT_PRESSURE_GIB: u64 = 20;

/// Env override for [`DEFAULT_PRESSURE_GIB`]. `0` disables the warning.
const PRESSURE_GIB_ENV: &str = "AISH_WORKTREE_PRESSURE_GIB";

/// Why a candidate was or was not selected. `Skip` carries the human reason so
/// `:worktrees gc` can explain itself — an unexplained skip looks like a bug and
/// trains operators to reach for `rm -rf` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every gate passed: clean, merged, aged, unambiguous.
    Reclaim { trunk: String, branch: String },
    /// At least one gate did not pass (or could not be evaluated).
    Skip(String),
}

/// One open ledger row plus the verdict for its tree.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub path: PathBuf,
    pub verdict: Verdict,
}

/// What a sweep did (or, in dry-run, would do).
#[derive(Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    /// `(id, path)` pairs removed and whose ledger row was closed.
    pub reclaimed: Vec<(String, PathBuf)>,
    /// `(id, path, error)` — removal failed; row left OPEN and stamped
    /// `cleanup_failed` so the leak report keeps surfacing it.
    pub failed: Vec<(String, PathBuf, String)>,
    /// `(id, path, reason)` — gated out.
    pub skipped: Vec<(String, PathBuf, String)>,
}

// ---------------------------------------------------------------------------
// git plumbing (all best-effort; every failure degrades to "unknown" → skip)
// ---------------------------------------------------------------------------

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Exit code of a git invocation, or `None` when git could not be run at all.
/// `merge-base --is-ancestor` answers via exit status (0 = yes, 1 = no), so the
/// distinction between "1" and "could not run" is load-bearing here.
fn git_code(dir: &Path, args: &[&str]) -> Option<i32> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .code()
}

/// The checked-out branch of `leaf`, or `None` for a detached HEAD / git error.
fn current_branch(leaf: &Path) -> Option<String> {
    let name = git_out(leaf, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    (!name.is_empty() && name != "HEAD").then_some(name)
}

/// First trunk ref that actually resolves in `leaf`, preferring the remote (what
/// isolated workers branch from). `None` → we cannot judge merged-ness at all.
fn trunk_ref(leaf: &Path) -> Option<String> {
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|r| git_out(leaf, &["rev-parse", "--verify", "--quiet", r]).is_some())
        .map(str::to_string)
}

/// The worktree's MAIN repo checkout, needed because `git worktree remove` must
/// be issued from outside the tree being removed.
fn main_repo_of(leaf: &Path) -> Option<PathBuf> {
    let common = git_out(
        leaf,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Path::new(&common).parent().map(Path::to_path_buf)
}

// ---------------------------------------------------------------------------
// gating
// ---------------------------------------------------------------------------

/// Decide a single tree's fate. Pure with respect to the filesystem — it only
/// READS. Every uncertain answer becomes [`Verdict::Skip`].
pub fn classify(leaf: &Path) -> Verdict {
    if !leaf.exists() {
        return Verdict::Skip("path is already gone — nothing to remove".to_string());
    }
    // Gate 2 first: it is the cheapest conclusive "no", and it already folds in
    // git errors (the helper reads any failure as "holds work").
    if crate::worker::worktree_holds_work(leaf) {
        return Verdict::Skip(
            "holds work — uncommitted changes or commits ahead of trunk".to_string(),
        );
    }
    let Some(branch) = current_branch(leaf) else {
        return Verdict::Skip("detached HEAD or unreadable branch".to_string());
    };
    let Some(trunk) = trunk_ref(leaf) else {
        return Verdict::Skip("no trunk ref (origin/main, main, …) resolves here".to_string());
    };
    match git_code(leaf, &["merge-base", "--is-ancestor", &branch, &trunk]) {
        Some(0) => Verdict::Reclaim { trunk, branch },
        Some(1) => Verdict::Skip(format!("branch `{branch}` is not merged into `{trunk}`")),
        Some(code) => Verdict::Skip(format!(
            "merge check for `{branch}` was inconclusive (git exit {code})"
        )),
        None => Verdict::Skip(format!("merge check for `{branch}` could not run")),
    }
}

/// Classify every open ledger row older than `hours`. Read-only.
pub fn plan(store: &CoordinatorStore, hours: i64) -> Vec<Candidate> {
    let rows = store.list_orphaned_worktrees(hours).unwrap_or_default();
    rows.into_iter()
        .map(|(id, path, _run_id)| {
            let path = PathBuf::from(path);
            let verdict = classify(&path);
            Candidate { id, path, verdict }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// removal
// ---------------------------------------------------------------------------

/// Remove a worktree leaf (and therefore its `target/` build artifacts).
/// `Ok(())` ONLY when the path is verifiably gone afterwards.
fn remove_tree(leaf: &Path, branch: &str) -> Result<(), String> {
    if let Some(main) = main_repo_of(leaf) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "remove", "--force"])
            .arg(leaf)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if leaf.exists() {
            // Registration may be stale; fall back to the filesystem, then let
            // git reconcile its metadata.
            let _ = std::fs::remove_dir_all(leaf);
            let _ = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(["worktree", "prune"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // The branch is proven merged by gate 3, so deleting it loses nothing.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["branch", "-D", branch])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        let _ = std::fs::remove_dir_all(leaf);
    }
    if leaf.exists() {
        Err(format!("{} is still on disk after removal", leaf.display()))
    } else {
        Ok(())
    }
}

/// Core sweep, with removal injected so the failure path is testable without
/// having to manufacture a real `git worktree remove` failure.
///
/// `apply == false` (the default everywhere) classifies and reports but touches
/// nothing — not the filesystem, not the ledger.
pub fn sweep_with<R>(store: &CoordinatorStore, hours: i64, apply: bool, remove: R) -> Report
where
    R: Fn(&Path, &str) -> Result<(), String>,
{
    let mut report = Report {
        dry_run: !apply,
        ..Default::default()
    };
    for c in plan(store, hours) {
        let (trunk_branch, _trunk) = match &c.verdict {
            Verdict::Reclaim { branch, trunk } => (branch.clone(), trunk.clone()),
            Verdict::Skip(reason) => {
                report.skipped.push((c.id, c.path, reason.clone()));
                continue;
            }
        };
        if !apply {
            report.reclaimed.push((c.id, c.path));
            continue;
        }
        match remove(&c.path, &trunk_branch) {
            Ok(()) => {
                if let Err(e) = store.record_worktree_cleaned_up(&c.id) {
                    eprintln!("aish: worktree {} removed but ledger not closed: {e}", c.id);
                }
                report.reclaimed.push((c.id, c.path));
            }
            Err(e) => {
                // Keep the designed behaviour: the row stays OPEN and stamped,
                // so the leak report keeps surfacing it.
                if let Err(e2) = store.record_worktree_cleanup_failed(&c.id, &e) {
                    eprintln!("aish: could not stamp cleanup failure for {}: {e2}", c.id);
                }
                report.failed.push((c.id, c.path, e));
            }
        }
    }
    report
}

/// [`sweep_with`] using the real remover.
pub fn sweep(store: &CoordinatorStore, hours: i64, apply: bool) -> Report {
    sweep_with(store, hours, apply, remove_tree)
}

// NOTE: no automatic sweep is wired, by design. Reclamation happens ONLY when
// an operator types `:worktrees gc --apply`. Startup stays report-only, so an
// upgrade can never silently delete a tree.

// ---------------------------------------------------------------------------
// operator surface: `:worktrees gc [--apply]`
// ---------------------------------------------------------------------------

/// Handle `:worktrees [gc] [--apply|-y]`. Dry-run unless `--apply` is passed.
/// Prints what would be / was removed and WHY each survivor was skipped.
pub fn command(args: &[&str]) -> String {
    let mut apply = false;
    for a in args {
        match *a {
            "gc" | "list" => {}
            "--apply" | "-y" | "--yes" => apply = true,
            other => {
                return format!("usage: :worktrees gc [--apply]   (unknown arg `{other}`)");
            }
        }
    }
    let store = match CoordinatorStore::open(&crate::db_paths::main_db_path()) {
        Ok(s) => s,
        Err(e) => return format!("worktrees: cannot open the coordinator store: {e}"),
    };
    let report = sweep(&store, LEAK_AFTER_HOURS, apply);
    render(&report)
}

/// Human rendering of a [`Report`].
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let verb = if report.dry_run {
        "would remove"
    } else {
        "removed"
    };
    if report.reclaimed.is_empty() && report.failed.is_empty() && report.skipped.is_empty() {
        return format!(
            "worktrees: no ledger rows older than {LEAK_AFTER_HOURS}h are still open — nothing to reclaim"
        );
    }
    out.push_str(&format!(
        "worktrees: {} {verb}, {} skipped",
        report.reclaimed.len(),
        report.skipped.len()
    ));
    if !report.failed.is_empty() {
        out.push_str(&format!(", {} failed", report.failed.len()));
    }
    out.push('\n');
    for (id, path) in &report.reclaimed {
        out.push_str(&format!("  \x1b[32m✓\x1b[0m {id}  {}\n", path.display()));
    }
    for (id, path, err) in &report.failed {
        out.push_str(&format!(
            "  \x1b[31m✗\x1b[0m {id}  {}  — {err}\n",
            path.display()
        ));
    }
    for (id, path, why) in &report.skipped {
        out.push_str(&format!(
            "  \x1b[2m·\x1b[0m {id}  {}  \x1b[2m— {why}\x1b[0m\n",
            path.display()
        ));
    }
    if report.dry_run && !report.reclaimed.is_empty() {
        out.push_str("\x1b[2mdry run — nothing was removed. re-run with `:worktrees gc --apply` to reclaim.\x1b[0m\n");
    }
    out.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// disk-pressure warning (informational, always on, never blocking)
// ---------------------------------------------------------------------------

/// Configured pressure threshold in GiB; `None` when disabled (`0`).
fn pressure_threshold_gib() -> Option<u64> {
    let gib = std::env::var(PRESSURE_GIB_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_PRESSURE_GIB);
    (gib > 0).then_some(gib)
}

/// Pure threshold policy, so the decision is testable without touching disk.
fn over_pressure(total_kib: u64, threshold_gib: u64) -> bool {
    total_kib / (1024 * 1024) >= threshold_gib
}

/// Total size of `root` in KiB via `du -sk`, or `None` on any failure.
fn du_kib(root: &Path) -> Option<u64> {
    let out = Command::new("du")
        .args(["-sk"])
        .arg(root)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// One-shot, non-blocking, best-effort: if the worktree root is over threshold,
/// print a single dim line pointing at `:worktrees gc`.
///
/// Called from the worktree-creation path, so it must never block, panic, or
/// fail a worker spawn. The size probe therefore runs on a detached thread and
/// every error is swallowed — a missing warning is free, a stalled spawn is not.
pub fn warn_if_disk_pressure() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(threshold) = pressure_threshold_gib() else {
            return;
        };
        let root = crate::worker::worktree_root();
        std::thread::spawn(move || {
            if !root.exists() {
                return;
            }
            let Some(kib) = du_kib(&root) else {
                return; // slow/erroring probe: stay silent
            };
            if over_pressure(kib, threshold) {
                let gib = kib / (1024 * 1024);
                eprintln!(
                    "\x1b[2maish: {} is using {gib} GiB (threshold {threshold} GiB) — \
                     run `:worktrees gc` to see what is safely reclaimable \
                     (set {PRESSURE_GIB_ENV}=0 to silence)\x1b[0m",
                    root.display()
                );
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- git fixtures (real git, temp dirs — never the live machine) ----

    fn run_git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs in tests");
        assert!(st.success(), "git {args:?} failed in {}", dir.display());
    }

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aish_wtgc_{tag}_{}_{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A repo with one commit on `trunk` (default `main`).
    fn init_repo(root: &Path, trunk: &str) -> PathBuf {
        let repo = root.join("main");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", trunk]);
        run_git(&repo, &["config", "user.email", "t@example.com"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "hi\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-qm", "init"]);
        repo
    }

    /// Add a worktree at `<root>/<id>` on a new branch off trunk.
    fn add_worktree(repo: &Path, root: &Path, id: &str) -> PathBuf {
        let leaf = root.join(id);
        run_git(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("aish/{id}"),
                leaf.to_str().unwrap(),
            ],
        );
        run_git(&leaf, &["config", "user.email", "t@example.com"]);
        run_git(&leaf, &["config", "user.name", "t"]);
        leaf
    }

    fn store_for(tag: &str) -> (CoordinatorStore, PathBuf) {
        let path = scratch(tag).join("store.db");
        let store = CoordinatorStore::open(&path).unwrap();
        (store, path)
    }

    /// Open an AGED ledger row for `leaf`.
    fn open_aged_row(store: &CoordinatorStore, id: &str, leaf: &Path) {
        store
            .record_worktree_created(id, leaf, &format!("run_{id}"))
            .unwrap();
        store.age_worktree_row_for_test(id, 48);
    }

    // ---- gate tests: the danger lives here ----

    #[test]
    fn clean_merged_aged_tree_is_selected() {
        let root = scratch("ok");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_ok");
        // Branch at trunk with no new commits => merged ancestor of main.
        assert!(
            matches!(classify(&leaf), Verdict::Reclaim { .. }),
            "{:?}",
            classify(&leaf)
        );
    }

    #[test]
    fn dirty_tree_is_never_selected() {
        let root = scratch("dirty");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_dirty");
        std::fs::write(leaf.join("scratch.txt"), "uncommitted\n").unwrap();
        match classify(&leaf) {
            Verdict::Skip(why) => assert!(why.contains("holds work"), "{why}"),
            v => panic!("dirty tree selected: {v:?}"),
        }
    }

    #[test]
    fn tree_with_commits_ahead_is_never_selected() {
        let root = scratch("ahead");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_ahead");
        std::fs::write(leaf.join("feature.txt"), "work\n").unwrap();
        run_git(&leaf, &["add", "."]);
        run_git(&leaf, &["commit", "-qm", "feature"]);
        match classify(&leaf) {
            Verdict::Skip(why) => assert!(why.contains("holds work"), "{why}"),
            v => panic!("commits-ahead tree selected: {v:?}"),
        }
    }

    /// An unmerged branch whose commits are hidden from the "ahead" check (the
    /// branch is committed AND merged nowhere) must still be refused by gate 3.
    #[test]
    fn unmerged_branch_is_never_selected() {
        let root = scratch("unmerged");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_unmerged");
        std::fs::write(leaf.join("f.txt"), "x\n").unwrap();
        run_git(&leaf, &["add", "."]);
        run_git(&leaf, &["commit", "-qm", "unmerged work"]);
        // Prove gate 3 independently: the branch is not an ancestor of main.
        let branch = current_branch(&leaf).unwrap();
        assert_eq!(
            git_code(&leaf, &["merge-base", "--is-ancestor", &branch, "main"]),
            Some(1),
            "fixture should be unmerged"
        );
        assert!(matches!(classify(&leaf), Verdict::Skip(_)));
    }

    #[test]
    fn missing_trunk_ref_skips_as_unknown() {
        let root = scratch("notrunk");
        // Trunk is neither main nor master, so no trunk ref resolves.
        let repo = init_repo(&root, "trunk");
        let leaf = add_worktree(&repo, &root, "w_notrunk");
        match classify(&leaf) {
            Verdict::Skip(_) => {}
            v => panic!("unknown trunk selected: {v:?}"),
        }
    }

    #[test]
    fn vanished_path_skips() {
        let root = scratch("gone");
        match classify(&root.join("nope")) {
            Verdict::Skip(why) => assert!(why.contains("already gone"), "{why}"),
            v => panic!("{v:?}"),
        }
    }

    // ---- sweep tests ----

    #[test]
    fn dry_run_selects_but_removes_nothing() {
        let root = scratch("dry");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_dry");
        let (store, _db) = store_for("dry");
        open_aged_row(&store, "w_dry", &leaf);

        let called = std::sync::atomic::AtomicUsize::new(0);
        let report = sweep_with(&store, LEAK_AFTER_HOURS, false, |_, _| {
            called.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        assert_eq!(report.reclaimed.len(), 1);
        assert!(report.dry_run);
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(leaf.exists(), "dry run must not touch the filesystem");
        // Ledger untouched: the row is still open.
        assert_eq!(
            store
                .list_orphaned_worktrees(LEAK_AFTER_HOURS)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn fresh_row_is_not_selected() {
        let root = scratch("fresh");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_fresh");
        let (store, _db) = store_for("fresh");
        // No back-dating: the row is younger than the leak threshold.
        store
            .record_worktree_created("w_fresh", &leaf, "run_fresh")
            .unwrap();
        let report = sweep_with(&store, LEAK_AFTER_HOURS, true, |_, _| Ok(()));
        assert!(report.reclaimed.is_empty());
        assert!(report.skipped.is_empty());
        assert!(leaf.exists());
    }

    #[test]
    fn apply_removes_tree_and_closes_ledger_row() {
        let root = scratch("apply");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_apply");
        let (store, _db) = store_for("apply");
        open_aged_row(&store, "w_apply", &leaf);

        let report = sweep(&store, LEAK_AFTER_HOURS, true);
        assert_eq!(report.reclaimed.len(), 1, "{report:?}");
        assert!(!leaf.exists(), "tree should be gone");
        assert!(
            store
                .list_orphaned_worktrees(LEAK_AFTER_HOURS)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.worktree_cleanup_failed_for_test("w_apply"),
            Some(false)
        );
    }

    #[test]
    fn cleanup_failure_stamps_failed_and_leaves_row_open() {
        let root = scratch("fail");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_fail");
        let (store, _db) = store_for("fail");
        open_aged_row(&store, "w_fail", &leaf);

        let report = sweep_with(&store, LEAK_AFTER_HOURS, true, |_, _| {
            Err("simulated removal failure".to_string())
        });
        assert_eq!(report.failed.len(), 1);
        assert!(report.reclaimed.is_empty());
        assert_eq!(store.worktree_cleanup_failed_for_test("w_fail"), Some(true));
        // Still reported — a failed cleanup is exactly the leak to keep seeing.
        assert_eq!(
            store
                .list_orphaned_worktrees(LEAK_AFTER_HOURS)
                .unwrap()
                .len(),
            1
        );
        assert!(leaf.exists());
    }

    #[test]
    fn dirty_tree_survives_an_apply_sweep() {
        let root = scratch("safe");
        let repo = init_repo(&root, "main");
        let leaf = add_worktree(&repo, &root, "w_safe");
        std::fs::write(leaf.join("precious.txt"), "operator work\n").unwrap();
        let (store, _db) = store_for("safe");
        open_aged_row(&store, "w_safe", &leaf);

        let report = sweep(&store, LEAK_AFTER_HOURS, true);
        assert!(report.reclaimed.is_empty(), "{report:?}");
        assert_eq!(report.skipped.len(), 1);
        assert!(leaf.join("precious.txt").exists());
    }

    // ---- policy / config ----

    #[test]
    fn leak_threshold_is_not_lowered() {
        assert_eq!(LEAK_AFTER_HOURS, 24);
    }

    #[test]
    fn pressure_policy_is_a_plain_threshold() {
        let gib = 1024 * 1024;
        assert!(!over_pressure(19 * gib, 20));
        assert!(over_pressure(20 * gib, 20));
        assert!(over_pressure(96 * gib, 20));
    }

    #[test]
    fn unknown_arg_is_rejected_without_touching_anything() {
        let out = command(&["--force"]);
        assert!(out.starts_with("usage:"), "{out}");
    }
}
