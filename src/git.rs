//! Shared git probe helpers — the single canonical home for the cheap
//! `git`-shelling probes the rest of the tree relies on.
//!
//! Historically these probes lived privately in `worker.rs` (worktree layer) and
//! `tools.rs` (the push/commit guard); they are consolidated here as
//! `pub(crate)` helpers. Each is a thin, best-effort `git` invocation: any error
//! (not a repo, detached HEAD, git missing) degrades to a conservative `None` /
//! `false` rather than panicking. None of these mutate the working tree.

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

// ---------------------------------------------------------------------------
// repo_key — a stable, filesystem-safe identifier for a checkout.
// (Canonical home; `worker.rs` keeps its own separate copy for the worktree
// layout — not yet consolidated onto this one.)
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

/// The static `Repo:` line baked into the system prompt at session start.
/// `"Repo: owner/repo (branch main)\n"` when in a repo, else an empty string so
/// the placeholder collapses cleanly. Pure — the caller supplies `name`/`branch`.
pub(crate) fn repo_prompt_line(name: Option<&str>, branch: Option<&str>) -> String {
    match name {
        Some(n) => match branch {
            Some(b) => format!("Repo: {n} (branch {b})\n"),
            None => format!("Repo: {n}\n"),
        },
        None => String::new(),
    }
}

/// The suffix appended to a `change_dir` result describing the repo-context
/// transition, so the MODEL (which only sees tool results) notices a switch
/// between checkouts instead of silently operating on the wrong repo. `old`/`new`
/// are `owner/repo` names (`None` = not in a git repo); `branch` is the new
/// location's branch when known. Pure — no IO — so it's directly unit-tested.
pub(crate) fn repo_transition_note(
    old: Option<&str>,
    new: Option<&str>,
    branch: Option<&str>,
) -> String {
    let brs = branch
        .map(|b| format!(" on branch {b}"))
        .unwrap_or_default();
    match (old, new) {
        (None, None) => String::new(),
        (Some(o), None) => {
            format!("\nNote: left repo {o}; no git repo at this location")
        }
        (None, Some(n)) => format!("\nNote: now working in repo {n}{brs}"),
        (Some(o), Some(n)) if o != n => {
            format!("\nNote: repo context changed: {o} -> {n}{brs}")
        }
        (Some(_), Some(n)) => format!(" (repo: {n})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_prompt_line_formats_or_collapses() {
        assert_eq!(
            repo_prompt_line(Some("owner/repo"), Some("main")),
            "Repo: owner/repo (branch main)\n"
        );
        assert_eq!(
            repo_prompt_line(Some("owner/repo"), None),
            "Repo: owner/repo\n"
        );
        assert_eq!(repo_prompt_line(None, Some("main")), "");
        assert_eq!(repo_prompt_line(None, None), "");
    }

    #[test]
    fn repo_transition_note_covers_every_transition() {
        // Stayed outside any repo → silent.
        assert_eq!(repo_transition_note(None, None, None), "");
        // Entered a repo from a non-repo dir.
        assert_eq!(
            repo_transition_note(None, Some("o/r"), Some("main")),
            "\nNote: now working in repo o/r on branch main"
        );
        // Switched between two different repos.
        assert_eq!(
            repo_transition_note(Some("a/b"), Some("c/d"), Some("dev")),
            "\nNote: repo context changed: a/b -> c/d on branch dev"
        );
        // Same repo (e.g. cd into a subdir) → quiet inline tag, no warning.
        assert_eq!(
            repo_transition_note(Some("a/b"), Some("a/b"), Some("main")),
            " (repo: a/b)"
        );
        // Left a repo for a non-repo dir.
        assert_eq!(
            repo_transition_note(Some("a/b"), None, None),
            "\nNote: left repo a/b; no git repo at this location"
        );
        // Branch unknown but repo entered.
        assert_eq!(
            repo_transition_note(None, Some("o/r"), None),
            "\nNote: now working in repo o/r"
        );
    }

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
}
