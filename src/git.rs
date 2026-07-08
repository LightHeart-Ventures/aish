//! Shared git probe helpers — the single canonical home for the cheap
//! `git`-shelling probes the rest of the tree relies on.
//!
//! Historically these probes lived privately in `worker.rs` (worktree layer) and
//! `tools.rs` (the push/commit guard). `git_repo.rs` (the cached, branch-aware
//! repo state) needs the same set, so they are consolidated here as
//! `pub(crate)` helpers. Each is a thin, best-effort `git` invocation: any error
//! (not a repo, detached HEAD, git missing) degrades to a conservative `None` /
//! `false` rather than panicking. None of these mutate the working tree.
// Skeleton: several helpers have no in-tree caller yet (the cache is wired to a
// real consumer in a follow-up per docs/git-repo-cache.md). Allow until then.
#![allow(dead_code)]

use crate::git_repo::{DiffStatus, FileDiff};
use std::path::Path;
use std::process::{Command, Stdio};

/// Run a git command in `dir`, returning trimmed stdout on success, `None` on
/// any failure (non-zero exit, git missing, non-UTF8 handled lossily).
pub(crate) fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// True when a git command in `dir` exits 0 (output discarded).
pub(crate) fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True when `dir` is inside a git working tree. Cheap `git rev-parse` probe;
/// false on any error (not a repo, git missing, …).
pub(crate) fn is_git_repo(dir: &Path) -> bool {
    git_ok(dir, &["rev-parse", "--is-inside-work-tree"])
}

/// The current HEAD commit sha of a repo/worktree, or `None` on error.
pub(crate) fn git_head(dir: &Path) -> Option<String> {
    git_out(dir, &["rev-parse", "HEAD"])
}

/// The worktree root (`git rev-parse --show-toplevel`), or `None` outside a repo.
pub(crate) fn toplevel(dir: &Path) -> Option<String> {
    git_out(dir, &["rev-parse", "--show-toplevel"])
}

/// The `origin` remote URL, or `None` when there is no such remote.
pub(crate) fn origin_url(dir: &Path) -> Option<String> {
    git_out(dir, &["remote", "get-url", "origin"])
}

/// The currently checked-out branch in `dir`, or `None` when it can't be told
/// (not a repo, detached HEAD, git missing). Cheap `git rev-parse` probe.
/// A detached HEAD reports the literal `HEAD`, which is filtered out.
pub(crate) fn current_branch(dir: &Path) -> Option<String> {
    let b = git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    (!b.is_empty() && b != "HEAD").then_some(b)
}

/// The repo's trunk branch name — `origin/HEAD` when set (e.g. `main`/`master`),
/// else whichever of `main`/`master` exists locally or on the remote, else
/// `main`. Resolved, never hard-coded, so a `master`-trunk or renamed-default
/// repo is handled.
pub(crate) fn trunk_branch(dir: &Path) -> String {
    if let Some(s) = git_out(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(name) = s.strip_prefix("origin/") {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    for cand in ["main", "master"] {
        if git_ok(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{cand}"),
            ],
        ) || git_ok(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/origin/{cand}"),
            ],
        ) {
            return cand.to_string();
        }
    }
    "main".to_string()
}

/// True when the working tree has uncommitted/untracked changes
/// (`git status --porcelain` non-empty). Conservative: a git error reads as
/// "not dirty" only when we genuinely got clean output — callers that need the
/// detail use [`dirty_porcelain`].
pub(crate) fn is_dirty(dir: &Path) -> bool {
    match git_out(dir, &["status", "--porcelain"]) {
        Some(s) => !s.trim().is_empty(),
        None => false,
    }
}

/// The raw `git status --porcelain` lines (empty when clean), or `None` on a git
/// error. Backs the dirty-detail report.
pub(crate) fn dirty_porcelain(dir: &Path) -> Option<Vec<String>> {
    let out = git_out(dir, &["status", "--porcelain"])?;
    Ok::<_, ()>(
        out.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
    .ok()
}

// ---------------------------------------------------------------------------
// Diff-vs-trunk probes — "how does this checkout differ from the trunk?"
// All resolve the trunk via `trunk_branch` (caller passes it in) and use the
// merge-base `<trunk>...HEAD` range so a stale trunk doesn't inflate the count.
// Pure parsers are split out so the line-munging is unit-testable with zero IO,
// mirroring the `evaluate_guard` split in `git_repo.rs`.
// ---------------------------------------------------------------------------

/// Commits `(ahead, behind)` of `HEAD` relative to `trunk` — `ahead` is the
/// number of commits on HEAD not yet on trunk, `behind` the number on trunk not
/// on HEAD. `None` on any git error (unknown trunk ref, not a repo, …).
pub(crate) fn commits_ahead_behind(dir: &Path, trunk: &str) -> Option<(usize, usize)> {
    let range = format!("{trunk}...HEAD");
    let out = git_out(dir, &["rev-list", "--left-right", "--count", &range])?;
    parse_ahead_behind(&out)
}

/// File-level diff of `HEAD` vs the `<trunk>...HEAD` merge-base, as `FileDiff`
/// rows carrying insertion/deletion counts. Status is filled as `Modified` here
/// and corrected by overlaying [`diff_name_status`] (numstat carries counts, not
/// the A/D/R classification). Empty on any git error.
pub(crate) fn diff_numstat(dir: &Path, trunk: &str) -> Vec<FileDiff> {
    let range = format!("{trunk}...HEAD");
    git_out(dir, &["diff", "--numstat", &range])
        .map(|s| parse_numstat(&s))
        .unwrap_or_default()
}

/// `(status, path)` pairs for the `<trunk>...HEAD` diff, classifying each path
/// as Added/Deleted/Renamed/Modified/Conflict. Empty on any git error.
pub(crate) fn diff_name_status(dir: &Path, trunk: &str) -> Vec<(DiffStatus, String)> {
    let range = format!("{trunk}...HEAD");
    git_out(dir, &["diff", "--name-status", &range])
        .map(|s| parse_name_status(&s))
        .unwrap_or_default()
}

/// True when `trunk` is an ancestor of `HEAD` — i.e. HEAD is strictly ahead and
/// could fast-forward onto trunk with no merge. False on any git error.
pub(crate) fn can_fastforward(dir: &Path, trunk: &str) -> bool {
    git_ok(dir, &["merge-base", "--is-ancestor", trunk, "HEAD"])
}

/// Paths currently in a merge-conflict (`git diff --diff-filter=U`). Empty when
/// there's no in-progress conflicted merge or on any git error.
pub(crate) fn conflicted_paths(dir: &Path) -> Vec<String> {
    git_out(dir, &["diff", "--name-only", "--diff-filter=U"])
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `git rev-list --left-right --count <trunk>...HEAD` line — a single
/// `"<left>\t<right>"` pair where `left` = commits on trunk not on HEAD
/// (behind) and `right` = commits on HEAD not on trunk (ahead). Returns
/// `(ahead, behind)`. Pure; `None` when the line isn't two integers.
pub(crate) fn parse_ahead_behind(s: &str) -> Option<(usize, usize)> {
    let line = s.lines().next()?.trim();
    let mut it = line.split_whitespace();
    let behind: usize = it.next()?.parse().ok()?;
    let ahead: usize = it.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// Parse `git diff --numstat` output into [`FileDiff`] rows (status defaulted to
/// `Modified` — overlay [`parse_name_status`] for the real classification).
/// Handles binary rows (`-\t-\tpath` → 0/0 counts) and rename rows
/// (`ins\tdel\told => new`, including the `pre/{old => new}/post` brace form →
/// the new path). Pure.
pub(crate) fn parse_numstat(s: &str) -> Vec<FileDiff> {
    s.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.is_empty() {
                return None;
            }
            let mut it = line.splitn(3, '\t');
            let ins = it.next()?;
            let del = it.next()?;
            let path = it.next()?;
            // Binary files report `-` for both counts.
            let insertions = ins.parse::<usize>().unwrap_or(0);
            let deletions = del.parse::<usize>().unwrap_or(0);
            Some(FileDiff {
                path: numstat_rename_target(path),
                status: DiffStatus::Modified,
                insertions,
                deletions,
            })
        })
        .collect()
}

/// Resolve a numstat path field to its post-rename target. Plain paths pass
/// through; `old => new` yields `new`; the compact `pre/{old => new}/post`
/// brace form is rebuilt as `pre/new/post`. Pure.
fn numstat_rename_target(raw: &str) -> String {
    let Some(arrow) = raw.find(" => ") else {
        return raw.to_string();
    };
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}'))
        && open < arrow
        && arrow < close
    {
        let prefix = &raw[..open];
        let new_part = &raw[arrow + 4..close];
        let suffix = &raw[close + 1..];
        return format!("{prefix}{new_part}{suffix}");
    }
    raw[arrow + 4..].to_string()
}

/// Parse `git diff --name-status` output into `(status, path)` pairs. The first
/// column's leading letter classifies the change (`A`dded / `D`eleted /
/// `R`enamed / `U`nmerged→Conflict / else Modified); for rename/copy rows
/// (`R100\told\tnew`) the *target* (last field) is taken. Pure.
pub(crate) fn parse_name_status(s: &str) -> Vec<(DiffStatus, String)> {
    s.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.is_empty() {
                return None;
            }
            let mut it = line.split('\t');
            let code = it.next()?;
            let status = match code.chars().next()? {
                'A' => DiffStatus::Added,
                'D' => DiffStatus::Deleted,
                'R' => DiffStatus::Renamed,
                'U' => DiffStatus::Conflict,
                _ => DiffStatus::Modified,
            };
            // Rename/copy rows carry old+new; the target is the last field.
            let path = it.next_back()?.trim();
            (!path.is_empty()).then(|| (status, path.to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// repo_key — a stable, filesystem-safe identifier for a checkout.
// (Canonical home; `worker.rs` keeps its own copies for the worktree layout and
// is consolidated onto these in a follow-up — see docs/git-repo-cache.md.)
// ---------------------------------------------------------------------------

/// Parse a GitHub remote URL into a stable `owner--repo` key, or `None` when the
/// URL isn't a parseable GitHub remote. Pure. Handles
/// `https://github.com/owner/repo(.git)`, `git@github.com:owner/repo.git`, and
/// `ssh://git@github.com/owner/repo.git`.
pub(crate) fn repo_key_from_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(sanitize_repo_key(&format!("{owner}/{repo}")))
}

/// Map an `owner/repo` string to a filesystem- and branch-safe key: `/` → `--`,
/// any other char outside `[A-Za-z0-9._-]` → `-`. Pure.
pub(crate) fn sanitize_repo_key(s: &str) -> String {
    s.replace('/', "--")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Fallback repo-key for a local-only / non-GitHub repo: the source dir's
/// basename plus a short FNV-1a hash of its absolute path. Pure given `src`.
pub(crate) fn fallback_repo_key(src: &Path) -> String {
    let base = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in src.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!(
        "{}-{:08x}",
        sanitize_repo_key(&base),
        (hash & 0xffff_ffff) as u32
    )
}

/// Resolve the `{repo-key}` for `src` (IO — reads the git `origin` remote):
/// `owner--repo` from the GitHub remote when parseable, else the
/// basename+shorthash fallback.
pub(crate) fn repo_key(src: &Path) -> String {
    origin_url(src)
        .and_then(|url| repo_key_from_remote(&url))
        .unwrap_or_else(|| fallback_repo_key(src))
}

/// Parse a GitHub remote URL into a human-readable `owner/repo` name (the slash
/// KEPT, unlike [`repo_key_from_remote`] which sanitizes it to `--`). `None`
/// when the URL isn't a parseable GitHub remote. Pure.
pub(crate) fn repo_name_from_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// A human-readable name for the repo containing `dir`: `owner/repo` from the
/// GitHub `origin` remote when parseable, else the worktree-root basename. IO —
/// shells `git`. `None` when `dir` isn't inside a git repo.
pub(crate) fn repo_name(dir: &Path) -> Option<String> {
    if !is_git_repo(dir) {
        return None;
    }
    if let Some(name) = origin_url(dir).and_then(|url| repo_name_from_remote(&url)) {
        return Some(name);
    }
    toplevel(dir).and_then(|t| {
        Path::new(&t)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_from_remote_keeps_owner_slash_repo() {
        assert_eq!(
            repo_name_from_remote("https://github.com/LightHeart-Ventures/aish.git").as_deref(),
            Some("LightHeart-Ventures/aish")
        );
        assert_eq!(
            repo_name_from_remote("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            repo_name_from_remote("ssh://git@github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            repo_name_from_remote("https://github.com/owner/repo/").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(repo_name_from_remote("https://gitlab.com/a/b.git"), None);
        assert_eq!(repo_name_from_remote("git@github.com:owner"), None);
    }

    #[test]
    fn repo_key_from_remote_parses_https_and_ssh() {
        assert_eq!(
            repo_key_from_remote("https://github.com/LightHeart-Ventures/aish.git").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        assert_eq!(
            repo_key_from_remote("git@github.com:owner/repo.git").as_deref(),
            Some("owner--repo")
        );
        assert_eq!(
            repo_key_from_remote("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner--repo")
        );
        assert_eq!(
            repo_key_from_remote("https://gitlab.com/owner/repo.git"),
            None
        );
        assert_eq!(repo_key_from_remote("https://github.com/owner"), None);
    }

    #[test]
    fn sanitize_and_fallback_keys_are_stable() {
        assert_eq!(sanitize_repo_key("owner/repo"), "owner--repo");
        assert_eq!(sanitize_repo_key("a b/c:d"), "a-b--c-d");
        let a = fallback_repo_key(Path::new("/home/me/aish"));
        let b = fallback_repo_key(Path::new("/home/me/aish"));
        assert_eq!(a, b);
        assert!(a.starts_with("aish-"));
        assert_ne!(a, fallback_repo_key(Path::new("/tmp/aish")));
    }

    // ---- pure diff parsers (zero IO) ----

    #[test]
    fn parse_ahead_behind_reads_left_behind_right_ahead() {
        // `rev-list --left-right --count trunk...HEAD` → "<behind>\t<ahead>".
        assert_eq!(parse_ahead_behind("0\t1"), Some((1, 0)));
        assert_eq!(parse_ahead_behind("3\t2"), Some((2, 3)));
        assert_eq!(parse_ahead_behind("0\t0\n"), Some((0, 0)));
        // Whitespace-separated variants are tolerated.
        assert_eq!(parse_ahead_behind("4 5"), Some((5, 4)));
        // Garbage → None.
        assert_eq!(parse_ahead_behind(""), None);
        assert_eq!(parse_ahead_behind("nope"), None);
    }

    #[test]
    fn parse_numstat_handles_text_binary_and_rename() {
        let out = "\
1\t0\tsrc/a.rs
12\t3\tsrc/b.rs
-\t-\tassets/logo.png
0\t0\tsrc/old.rs => src/new.rs
5\t1\tcrate/{old => new}/mod.rs";
        let diffs = parse_numstat(out);
        assert_eq!(diffs.len(), 5);

        assert_eq!(diffs[0].path, "src/a.rs");
        assert_eq!((diffs[0].insertions, diffs[0].deletions), (1, 0));

        assert_eq!((diffs[1].insertions, diffs[1].deletions), (12, 3));

        // Binary row: `-`/`-` → 0/0, path preserved.
        assert_eq!(diffs[2].path, "assets/logo.png");
        assert_eq!((diffs[2].insertions, diffs[2].deletions), (0, 0));

        // Rename: target path is taken.
        assert_eq!(diffs[3].path, "src/new.rs");
        // Brace-compacted rename rebuilds the full target path.
        assert_eq!(diffs[4].path, "crate/new/mod.rs");

        // Parser defaults status to Modified; name-status overlays the truth.
        assert!(diffs.iter().all(|d| d.status == DiffStatus::Modified));
    }

    #[test]
    fn parse_name_status_classifies_including_rename() {
        let out = "\
M\tsrc/a.rs
A\tsrc/new.rs
D\tsrc/gone.rs
R100\tsrc/old.rs\tsrc/renamed.rs
U\tsrc/conflict.rs";
        let got = parse_name_status(out);
        assert_eq!(
            got,
            vec![
                (DiffStatus::Modified, "src/a.rs".to_string()),
                (DiffStatus::Added, "src/new.rs".to_string()),
                (DiffStatus::Deleted, "src/gone.rs".to_string()),
                // Rename takes the target (new) path.
                (DiffStatus::Renamed, "src/renamed.rs".to_string()),
                (DiffStatus::Conflict, "src/conflict.rs".to_string()),
            ]
        );
    }
}
