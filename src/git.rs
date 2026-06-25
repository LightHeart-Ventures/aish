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

#[cfg(test)]
mod tests {
    use super::*;

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
}
