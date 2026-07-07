//! Integration test: exercise the IO probes against a real, throwaway git repo
//! created in a temp dir. Skips gracefully when `git` isn't on PATH so the suite
//! never hard-fails in a git-less sandbox.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unique_tmp(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    p.push(format!("git-discover-it-{tag}-{}-{nanos}", std::process::id()));
    p
}

#[test]
fn discovers_a_freshly_initialized_repo() {
    if !git_available() {
        eprintln!("git not available — skipping integration test");
        return;
    }

    let dir = unique_tmp("init");
    std::fs::create_dir_all(&dir).unwrap();

    // `-b main` isn't universally supported on ancient gits; fall back to a
    // rename if needed.
    let ok = git(&dir, &["init", "-q", "-b", "main"]) || git(&dir, &["init", "-q"]);
    assert!(ok, "git init failed");
    let _ = git(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let _ = git(&dir, &["remote", "add", "origin", "git@github.com:acme/widget.git"]);

    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    assert!(git(&dir, &["add", "."]), "git add failed");
    assert!(git(&dir, &["commit", "-q", "-m", "init"]), "git commit failed");

    let info = git_discover::discover(&dir).expect("should discover the repo");

    // Remote-derived identity.
    assert_eq!(info.host.as_deref(), Some("github.com"));
    assert_eq!(info.owner.as_deref(), Some("acme"));
    assert_eq!(info.repo.as_deref(), Some("widget"));
    assert_eq!(info.slug.as_deref(), Some("acme/widget"));
    assert_eq!(info.repo_key, "acme--widget");

    // State.
    assert_eq!(info.branch.as_deref(), Some("main"));
    assert!(!info.detached);
    assert_eq!(info.trunk, "main");
    assert!(info.on_trunk);
    assert!(!info.dirty, "freshly committed tree is clean");
    assert!(info.head.is_some());
    assert!(!info.is_linked_worktree);

    // Dirty detection.
    std::fs::write(dir.join("scratch.txt"), "wip\n").unwrap();
    let dirty = git_discover::discover(&dir).unwrap();
    assert!(dirty.dirty, "untracked file makes the tree dirty");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn linked_worktree_is_flagged() {
    if !git_available() {
        return;
    }
    let dir = unique_tmp("wt-main");
    std::fs::create_dir_all(&dir).unwrap();
    let ok = git(&dir, &["init", "-q", "-b", "main"]) || git(&dir, &["init", "-q"]);
    assert!(ok);
    let _ = git(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::write(dir.join("f"), "x\n").unwrap();
    assert!(git(&dir, &["add", "."]));
    assert!(git(&dir, &["commit", "-q", "-m", "c0"]));

    let wt = unique_tmp("wt-linked");
    let added = git(
        &dir,
        &["worktree", "add", "-q", "-b", "feature", wt.to_str().unwrap()],
    );
    if added {
        let main_info = git_discover::discover(&dir).unwrap();
        assert!(!main_info.is_linked_worktree, "primary tree is not linked");

        let linked = git_discover::discover(&wt).unwrap();
        assert!(linked.is_linked_worktree, "added worktree is linked");
        assert_eq!(linked.branch.as_deref(), Some("feature"));
        assert!(!linked.on_trunk);

        let _ = git(&dir, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
    }
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&dir);
}
