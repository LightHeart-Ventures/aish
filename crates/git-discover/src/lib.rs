//! `git-discover` — detect the git repository a directory currently belongs to.
//!
//! aish needs to answer "what repo am I working with right now?" in a lot of
//! places (session naming, agent context, worktree bookkeeping, the `:repo`
//! surface). Historically each site re-shelled its own `git rev-parse …`. This
//! crate is the single, self-contained, **zero-dependency** answer: point
//! [`discover`] at a directory and it returns a [`RepoInfo`] describing the
//! checkout — or `None` when the directory isn't inside a git working tree.
//!
//! Every probe is a thin, best-effort `git` invocation run with `git -C <dir>`.
//! Any failure (not a repo, detached HEAD, `git` missing, non-UTF8 output)
//! degrades to a conservative `None`/`false` instead of panicking. Nothing here
//! mutates the working tree — discovery only *observes*.
//!
//! ```no_run
//! if let Some(info) = git_discover::discover_here() {
//!     println!("repo: {}", info.repo_key);
//!     println!("branch: {:?} (trunk {})", info.branch, info.trunk);
//! }
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A parsed git remote: the forge `host`, the `owner` (namespace), and the
/// `repo` name. Produced by [`parse_remote`] from any of the common remote URL
/// shapes (https, ssh, `git://`, and the scp-like `git@host:owner/repo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// Forge hostname, e.g. `github.com`, `gitlab.com`, `bitbucket.org`. User
    /// (`git@`) and any `:port` are stripped.
    pub host: String,
    /// Repository owner / namespace, e.g. `LightHeart-Ventures`. For a nested
    /// GitLab group path (`group/subgroup/repo`) this is the *last* namespace
    /// segment (`subgroup`).
    pub owner: String,
    /// Repository name with any trailing `.git` removed, e.g. `aish`.
    pub repo: String,
}

impl Remote {
    /// `owner/repo` slug.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Identity + last-observed state of one git checkout — the answer to
/// "what repo is this directory in, right now?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Worktree root (`git rev-parse --show-toplevel`).
    pub root: PathBuf,
    /// The `origin` remote URL, when configured.
    pub remote_url: Option<String>,
    /// Forge host parsed from `remote_url` (e.g. `github.com`).
    pub host: Option<String>,
    /// Repo owner / namespace parsed from `remote_url`.
    pub owner: Option<String>,
    /// Repo name parsed from `remote_url`.
    pub repo: Option<String>,
    /// `owner/repo` slug when both were parseable.
    pub slug: Option<String>,
    /// Stable, filesystem- and branch-safe key: `owner--repo` from the remote
    /// when parseable, else the root basename plus a short path hash. Always
    /// present.
    pub repo_key: String,
    /// Checked-out branch; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// True when HEAD is detached (no current branch).
    pub detached: bool,
    /// Full HEAD commit sha.
    pub head: Option<String>,
    /// Abbreviated (short) HEAD sha.
    pub short_head: Option<String>,
    /// Resolved trunk branch — `origin/HEAD` when set, else whichever of
    /// `main`/`master` exists, else `main`. Never hard-coded.
    pub trunk: String,
    /// `branch == Some(trunk)` — is the checkout sitting on its trunk?
    pub on_trunk: bool,
    /// Working tree has uncommitted/untracked changes
    /// (`git status --porcelain` non-empty).
    pub dirty: bool,
    /// True when this is a *linked* worktree (`git worktree add`), i.e. its
    /// per-worktree git dir differs from the shared common git dir.
    pub is_linked_worktree: bool,
}

impl RepoInfo {
    /// Serialize to a compact JSON object using only std (no serde).
    pub fn to_json(&self) -> String {
        fn s(v: &Option<String>) -> String {
            match v {
                Some(x) => json_str(x),
                None => "null".to_string(),
            }
        }
        format!(
            concat!(
                "{{\"root\":{},\"remote_url\":{},\"host\":{},\"owner\":{},",
                "\"repo\":{},\"slug\":{},\"repo_key\":{},\"branch\":{},",
                "\"detached\":{},\"head\":{},\"short_head\":{},\"trunk\":{},",
                "\"on_trunk\":{},\"dirty\":{},\"is_linked_worktree\":{}}}"
            ),
            json_str(&self.root.to_string_lossy()),
            s(&self.remote_url),
            s(&self.host),
            s(&self.owner),
            s(&self.repo),
            s(&self.slug),
            json_str(&self.repo_key),
            s(&self.branch),
            self.detached,
            s(&self.head),
            s(&self.short_head),
            json_str(&self.trunk),
            self.on_trunk,
            self.dirty,
            self.is_linked_worktree,
        )
    }

    /// A one-line human summary, e.g.
    /// `LightHeart-Ventures/aish on aish/w_x (trunk main) — dirty`.
    pub fn summary(&self) -> String {
        let ident = self
            .slug
            .clone()
            .unwrap_or_else(|| self.repo_key.clone());
        let branch = match (&self.branch, self.detached) {
            (Some(b), _) => format!("on {b}"),
            (None, true) => "detached HEAD".to_string(),
            (None, false) => "unknown branch".to_string(),
        };
        let trunk = if self.on_trunk {
            format!("trunk {}, on trunk", self.trunk)
        } else {
            format!("trunk {}", self.trunk)
        };
        let dirt = if self.dirty { " — dirty" } else { " — clean" };
        let wt = if self.is_linked_worktree {
            " [linked worktree]"
        } else {
            ""
        };
        format!("{ident} {branch} ({trunk}){dirt}{wt}")
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Discover the repository that `dir` belongs to. Returns `None` when `dir`
/// isn't inside a git working tree (or `git` is unavailable).
pub fn discover(dir: &Path) -> Option<RepoInfo> {
    if !is_git_repo(dir) {
        return None;
    }
    let root = toplevel(dir).map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf());
    let remote_url = origin_url(dir);
    let remote = remote_url.as_deref().and_then(parse_remote);
    let (host, owner, repo, slug) = match &remote {
        Some(r) => (
            Some(r.host.clone()),
            Some(r.owner.clone()),
            Some(r.repo.clone()),
            Some(r.slug()),
        ),
        None => (None, None, None, None),
    };
    let repo_key = remote_url
        .as_deref()
        .and_then(repo_key_from_remote)
        .unwrap_or_else(|| fallback_repo_key(&root));
    let branch = current_branch(dir);
    let detached = branch.is_none() && is_git_repo(dir);
    let head = git_head(dir);
    let short_head = head.as_deref().map(|h| h.chars().take(12).collect());
    let trunk = trunk_branch(dir);
    let on_trunk = branch.as_deref() == Some(trunk.as_str());
    let dirty = is_dirty(dir);
    let is_linked_worktree = is_linked_worktree(dir);
    Some(RepoInfo {
        root,
        remote_url,
        host,
        owner,
        repo,
        slug,
        repo_key,
        branch,
        detached,
        head,
        short_head,
        trunk,
        on_trunk,
        dirty,
        is_linked_worktree,
    })
}

/// Discover the repository for the current working directory.
pub fn discover_here() -> Option<RepoInfo> {
    let cwd = std::env::current_dir().ok()?;
    discover(&cwd)
}

// ---------------------------------------------------------------------------
// Pure parsers (no IO) — unit-tested without spawning git.
// ---------------------------------------------------------------------------

/// Parse a git remote URL into its [`Remote`] parts, or `None` when it isn't a
/// recognizable `host/owner/repo` remote. Pure. Handles:
/// - `https://host/owner/repo(.git)` and `http://…`, `git://…`
/// - `ssh://[user@]host[:port]/owner/repo(.git)`
/// - scp-like `git@host:owner/repo(.git)` (and `[user@]host:path`)
///
/// Nested namespaces (e.g. a GitLab `group/subgroup/repo`) collapse to the last
/// two path segments (`subgroup` as owner, `repo` as repo).
pub fn parse_remote(url: &str) -> Option<Remote> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    let (authority, path) = if let Some(rest) = url
        .strip_prefix("ssh://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        // scheme://[user@]host[:port]/owner/repo…
        let (authority, path) = rest.split_once('/')?;
        (authority.to_string(), path.to_string())
    } else if !url.contains("://") && url.contains(':') {
        // scp-like: [user@]host:owner/repo(.git)
        let (authority, path) = url.split_once(':')?;
        // A drive-letter or bare `host:` with no path isn't a remote.
        (authority.to_string(), path.to_string())
    } else {
        return None;
    };

    // Clean host: drop any `user@` prefix and any `:port` suffix.
    let host = authority.rsplit('@').next().unwrap_or(&authority);
    let host = host.split(':').next().unwrap_or(host).trim().to_string();
    if host.is_empty() {
        return None;
    }

    // Clean path → owner/repo.
    let path = path.trim().trim_start_matches('/');
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    let repo = segs.pop()?.to_string();
    let owner = segs.pop()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(Remote { host, owner, repo })
}

/// Parse a remote URL into a stable `owner--repo` [`repo_key`](repo_key). Pure.
/// Generalizes across forges (any parseable `host/owner/repo`), unlike a
/// GitHub-only variant.
pub fn repo_key_from_remote(url: &str) -> Option<String> {
    parse_remote(url).map(|r| sanitize_repo_key(&r.slug()))
}

/// Map an `owner/repo` string to a filesystem- and branch-safe key: `/` → `--`,
/// any other char outside `[A-Za-z0-9._-]` → `-`. Pure.
pub fn sanitize_repo_key(s: &str) -> String {
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

/// Fallback repo-key for a local-only / unparseable-remote repo: the root dir's
/// basename plus a short FNV-1a hash of its absolute path (so two same-named
/// checkouts in different places don't collide). Pure given `root`.
pub fn fallback_repo_key(root: &Path) -> String {
    let base = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in root.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!(
        "{}-{:08x}",
        sanitize_repo_key(&base),
        (hash & 0xffff_ffff) as u32
    )
}

/// Minimal JSON string escaper for [`RepoInfo::to_json`]. Pure.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Low-level git probes (IO). Best-effort: any failure → None / false.
// ---------------------------------------------------------------------------

/// Run a git command in `dir`, returning trimmed stdout on success, `None` on
/// any failure (non-zero exit, git missing, non-UTF8 handled lossily).
fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
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
fn git_ok(dir: &Path, args: &[&str]) -> bool {
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

/// True when `dir` is inside a git working tree.
pub fn is_git_repo(dir: &Path) -> bool {
    git_ok(dir, &["rev-parse", "--is-inside-work-tree"])
}

/// Worktree root (`git rev-parse --show-toplevel`), or `None` outside a repo.
pub fn toplevel(dir: &Path) -> Option<String> {
    git_out(dir, &["rev-parse", "--show-toplevel"])
}

/// The `origin` remote URL, or `None` when there is no such remote.
pub fn origin_url(dir: &Path) -> Option<String> {
    git_out(dir, &["remote", "get-url", "origin"])
}

/// Full HEAD commit sha, or `None` on error / empty repo.
pub fn git_head(dir: &Path) -> Option<String> {
    git_out(dir, &["rev-parse", "HEAD"])
}

/// The currently checked-out branch, or `None` when it can't be told (not a
/// repo, detached HEAD, git missing). A detached HEAD reports literal `HEAD`,
/// which is filtered out.
pub fn current_branch(dir: &Path) -> Option<String> {
    let b = git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    (!b.is_empty() && b != "HEAD").then_some(b)
}

/// The repo's trunk branch — `origin/HEAD` when set (e.g. `main`/`master`),
/// else whichever of `main`/`master` exists locally or on the remote, else
/// `main`. Resolved, never hard-coded.
pub fn trunk_branch(dir: &Path) -> String {
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

/// True when the working tree has uncommitted/untracked changes.
pub fn is_dirty(dir: &Path) -> bool {
    match git_out(dir, &["status", "--porcelain"]) {
        Some(s) => !s.trim().is_empty(),
        None => false,
    }
}

/// True when `dir` is inside a *linked* worktree (`git worktree add …`), as
/// opposed to the primary working tree. Detected by comparing the per-worktree
/// git dir against the shared common git dir. False on any error.
pub fn is_linked_worktree(dir: &Path) -> bool {
    let git_dir = git_out(dir, &["rev-parse", "--absolute-git-dir"]);
    let common = git_out(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    match (git_dir, common) {
        (Some(g), Some(c)) => !g.is_empty() && !c.is_empty() && g != c,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests — the pure parsers need no git; the probes get a temp-repo harness.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_handles_https_ssh_scp_and_git() {
        let cases = [
            (
                "https://github.com/LightHeart-Ventures/aish.git",
                ("github.com", "LightHeart-Ventures", "aish"),
            ),
            (
                "https://github.com/owner/repo",
                ("github.com", "owner", "repo"),
            ),
            ("git@github.com:owner/repo.git", ("github.com", "owner", "repo")),
            (
                "ssh://git@github.com/owner/repo.git",
                ("github.com", "owner", "repo"),
            ),
            (
                "ssh://git@github.com:22/owner/repo.git",
                ("github.com", "owner", "repo"),
            ),
            (
                "https://gitlab.com/group/subgroup/repo.git",
                ("gitlab.com", "subgroup", "repo"),
            ),
            (
                "git@bitbucket.org:team/svc.git",
                ("bitbucket.org", "team", "svc"),
            ),
            ("git://example.com/o/r.git", ("example.com", "o", "r")),
        ];
        for (url, (host, owner, repo)) in cases {
            let r = parse_remote(url).unwrap_or_else(|| panic!("failed to parse {url}"));
            assert_eq!(r.host, host, "host for {url}");
            assert_eq!(r.owner, owner, "owner for {url}");
            assert_eq!(r.repo, repo, "repo for {url}");
        }
    }

    #[test]
    fn parse_remote_rejects_non_remotes() {
        assert_eq!(parse_remote(""), None);
        assert_eq!(parse_remote("   "), None);
        assert_eq!(parse_remote("https://github.com/owner"), None); // no repo
        assert_eq!(parse_remote("just-a-string"), None);
        assert_eq!(parse_remote("https://github.com/"), None);
    }

    #[test]
    fn repo_key_from_remote_is_forge_agnostic() {
        assert_eq!(
            repo_key_from_remote("https://github.com/LightHeart-Ventures/aish.git").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        assert_eq!(
            repo_key_from_remote("git@gitlab.com:owner/repo.git").as_deref(),
            Some("owner--repo")
        );
        assert_eq!(repo_key_from_remote("not-a-url"), None);
    }

    #[test]
    fn sanitize_and_fallback_keys_are_stable() {
        assert_eq!(sanitize_repo_key("owner/repo"), "owner--repo");
        assert_eq!(sanitize_repo_key("a b/c:d"), "a-b--c-d");
        let a = fallback_repo_key(Path::new("/home/me/aish"));
        let b = fallback_repo_key(Path::new("/home/me/aish"));
        assert_eq!(a, b, "fallback key is deterministic");
        assert!(a.starts_with("aish-"));
        assert_ne!(
            fallback_repo_key(Path::new("/a/aish")),
            fallback_repo_key(Path::new("/b/aish")),
            "same basename in different paths must differ"
        );
    }

    #[test]
    fn to_json_escapes_and_roundtrips_shape() {
        let info = RepoInfo {
            root: PathBuf::from("/tmp/x\"y"),
            remote_url: Some("https://github.com/o/r.git".into()),
            host: Some("github.com".into()),
            owner: Some("o".into()),
            repo: Some("r".into()),
            slug: Some("o/r".into()),
            repo_key: "o--r".into(),
            branch: Some("main".into()),
            detached: false,
            head: Some("abc123".into()),
            short_head: Some("abc123".into()),
            trunk: "main".into(),
            on_trunk: true,
            dirty: false,
            is_linked_worktree: false,
        };
        let j = info.to_json();
        assert!(j.contains("\"repo_key\":\"o--r\""));
        assert!(j.contains("\"on_trunk\":true"));
        assert!(j.contains("\"dirty\":false"));
        // The `"` in the root path must be escaped.
        assert!(j.contains("/tmp/x\\\"y"));
    }

    #[test]
    fn non_repo_dir_discovers_nothing() {
        // A temp dir that is definitely not a git repo.
        let tmp = std::env::temp_dir().join(format!("git-discover-nonrepo-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        assert!(discover(&tmp).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
