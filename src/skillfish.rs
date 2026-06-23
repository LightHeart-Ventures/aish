//! skill.fish integration — opt-in fetch & import of community skills.
//!
//! skill.fish (https://skill.fish) is a community marketplace for AI-agent
//! skills published in the same SKILL.md convention aish already uses locally
//! (YAML frontmatter with `name:`/`description:`, then a markdown body — see
//! src/skills.rs). This module is the OPT-IN bridge: nothing here runs unless
//! the user explicitly asks for a skill, with `aish --skill-fetch <ref>`.
//!
//! Flow: parse a ref → fetch the raw SKILL.md over HTTPS → validate it really
//! is a SKILL.md → write it under ~/.aish/skills/<name>/SKILL.md, where the
//! existing skills::load catalog picks it up on the next launch. The fetched
//! file is plain instructions (data, never code): aish never executes it, it
//! only advertises it to the model, so importing one can't run anything.
//!
//! This mirrors the atum MCP server's `atum_import_skill` tool, which already
//! understands skill.fish refs — that path lets the *model* import a skill
//! mid-session; this CLI path lets a *user* import one without a backend or
//! any credentials. Both land a SKILL.md in the same local catalog.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default registry origin. Override with `AISH_SKILLFISH_REGISTRY=scheme://host`
/// for self-hosted mirrors or tests (a loopback `http://` origin is allowed).
const DEFAULT_REGISTRY: &str = "https://skill.fish";

fn registry() -> String {
    std::env::var("AISH_SKILLFISH_REGISTRY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

/// A parsed reference to a skill on skill.fish.
#[derive(Debug, PartialEq, Eq)]
pub struct SkillRef {
    pub owner: String,
    pub name: String,
    pub version: Option<String>,
}

/// Parse a skill reference. Accepts a full URL
/// (`https://skill.fish/owner/name[@version]`) or the bare `owner/name[@version]`
/// shorthand. Extra path segments after the name (e.g. `/raw`) are ignored.
pub fn parse_ref(input: &str) -> Result<SkillRef> {
    let s = input.trim();
    let path = s
        .strip_prefix("https://skill.fish/")
        .or_else(|| s.strip_prefix("http://skill.fish/"))
        .or_else(|| s.strip_prefix("skill.fish/"))
        .unwrap_or(s)
        .trim_matches('/');
    let mut parts = path.splitn(2, '/');
    let owner = parts
        .next()
        .filter(|p| !p.is_empty())
        .context("missing skill owner — expected owner/name, e.g. acme/git-helper")?;
    let rest = parts
        .next()
        .context("missing skill name — expected owner/name, e.g. acme/git-helper")?;
    let name_seg = rest.split('/').next().unwrap_or(rest);
    let (name, version) = match name_seg.split_once('@') {
        Some((n, v)) if !v.is_empty() => (n, Some(v.to_string())),
        _ => (name_seg.trim_end_matches('@'), None),
    };
    validate_segment(owner).context("invalid owner")?;
    validate_segment(name).context("invalid skill name")?;
    Ok(SkillRef { owner: owner.to_string(), name: name.to_string(), version })
}

/// Reject path segments that could escape the skills dir or carry odd chars —
/// a SKILL.md `name:` becomes a directory name, so this is also a hard guard
/// against path traversal from an untrusted registry response.
fn validate_segment(s: &str) -> Result<()> {
    if s.is_empty() || s == "." || s == ".." || s.contains('/') || s.contains('\\') {
        bail!("unsafe path segment: {s:?}");
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        bail!("only [A-Za-z0-9._-] allowed, got: {s:?}");
    }
    Ok(())
}

/// The raw SKILL.md URL on the registry for this ref.
pub fn raw_url(r: &SkillRef) -> String {
    let mut u = format!("{}/{}/{}/raw", registry(), r.owner, r.name);
    if let Some(v) = &r.version {
        u.push_str(&format!("?version={v}"));
    }
    u
}

/// Refuse anything but HTTPS, except a loopback origin (for self-hosted mirrors
/// and the integration tests). A skill is fetched in the clear otherwise.
fn check_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1") {
        return Ok(());
    }
    bail!("refusing to fetch a skill over a non-HTTPS URL: {url}");
}

/// Low-level fetch of a raw SKILL.md from an absolute URL (no env lookup), so
/// tests can point it at a loopback server without mutating process env.
pub async fn fetch_url(url: &str) -> Result<String> {
    check_url(url)?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("aish/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?;
    let resp = client.get(url).send().await.with_context(|| format!("fetching {url}"))?;
    if !resp.status().is_success() {
        bail!("skill.fish returned HTTP {} for {url}", resp.status().as_u16());
    }
    let body = resp.text().await.context("reading the skill body")?;
    if body.trim().is_empty() {
        bail!("skill.fish returned an empty body for {url}");
    }
    Ok(body)
}

/// Fetch the raw SKILL.md for a parsed ref from the configured registry.
pub async fn fetch(r: &SkillRef) -> Result<String> {
    fetch_url(&raw_url(r)).await
}

/// Validate a fetched SKILL.md and write it to `skills_dir/<name>/SKILL.md`.
/// The on-disk directory name comes from the file's own frontmatter `name:`,
/// so the local catalog stays consistent with what the model will read.
pub fn import(text: &str, skills_dir: &Path) -> Result<PathBuf> {
    let (name, _desc) = crate::skills::parse_frontmatter(text).context(
        "fetched content is not a valid SKILL.md (needs `name:`/`description:` frontmatter)",
    )?;
    validate_segment(&name).context("SKILL.md frontmatter `name:` is unsafe as a directory")?;
    let dir = skills_dir.join(&name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// CLI entry point for `aish --skill-fetch <ref>`: parse, fetch, import, report.
pub async fn run_fetch(input: &str, skills_dir: &Path) -> Result<()> {
    let r = parse_ref(input)?;
    let ver = r.version.as_deref().map(|v| format!("@{v}")).unwrap_or_default();
    println!("\x1b[2mfetching {}/{}{ver} from {} …\x1b[0m", r.owner, r.name, registry());
    let text = fetch(&r).await?;
    let path = import(&text, skills_dir)?;
    let (name, desc) =
        crate::skills::parse_frontmatter(&text).unwrap_or((r.name.clone(), String::new()));
    println!("\x1b[32m✓\x1b[0m imported skill \x1b[1m{name}\x1b[0m → {}", path.display());
    if !desc.is_empty() {
        println!("  \x1b[2m{desc}\x1b[0m");
    }
    println!("  It's in your skills catalog now — aish will use it when a task matches.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_and_shorthand() {
        let want = SkillRef { owner: "acme".into(), name: "git-helper".into(), version: None };
        assert_eq!(parse_ref("https://skill.fish/acme/git-helper").unwrap(), want);
        assert_eq!(parse_ref("skill.fish/acme/git-helper").unwrap(), want);
        assert_eq!(parse_ref("acme/git-helper").unwrap(), want);
        assert_eq!(parse_ref("https://skill.fish/acme/git-helper/raw").unwrap(), want);
    }

    #[test]
    fn parses_version_pin() {
        let r = parse_ref("acme/git-helper@1.2.0").unwrap();
        assert_eq!(r.version.as_deref(), Some("1.2.0"));
        // a bare trailing @ is just "no version"
        assert_eq!(parse_ref("acme/git-helper@").unwrap().version, None);
    }

    #[test]
    fn rejects_bad_refs() {
        assert!(parse_ref("just-a-name").is_err());
        assert!(parse_ref("acme/..").is_err());
        assert!(parse_ref("../etc/passwd").is_err());
        assert!(parse_ref("acme/bad name").is_err());
        assert!(parse_ref("ac me/x").is_err());
    }

    #[test]
    fn raw_url_includes_version_query() {
        let r = SkillRef { owner: "a".into(), name: "b".into(), version: Some("2".into()) };
        assert_eq!(raw_url(&r), "https://skill.fish/a/b/raw?version=2");
        let r2 = SkillRef { owner: "a".into(), name: "b".into(), version: None };
        assert_eq!(raw_url(&r2), "https://skill.fish/a/b/raw");
    }

    #[test]
    fn check_url_enforces_https_except_loopback() {
        assert!(check_url("https://skill.fish/a/b/raw").is_ok());
        assert!(check_url("http://127.0.0.1:8080/a/b/raw").is_ok());
        assert!(check_url("http://localhost/a/b/raw").is_ok());
        assert!(check_url("http://evil.example/a/b/raw").is_err());
        assert!(check_url("ftp://skill.fish/a/b").is_err());
    }

    #[test]
    fn import_writes_under_frontmatter_name() {
        let tmp = std::env::temp_dir().join(format!("aish-sf-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let md = "---\nname: demo\ndescription: A demo skill.\n---\nDo the thing.\n";
        let path = import(md, &tmp).unwrap();
        assert_eq!(path, tmp.join("demo").join("SKILL.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), md);
        // the local catalog now sees it
        let loaded = crate::skills::load(&tmp);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "demo");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn import_rejects_non_skill_content() {
        let tmp = std::env::temp_dir().join(format!("aish-sf-bad-{}", std::process::id()));
        assert!(import("not a skill", &tmp).is_err());
    }

    // End-to-end fetch → import against a one-shot loopback HTTP server. Covers
    // the real reqwest path without touching the public registry (which sits
    // behind a bot challenge) and without mutating process-global env.
    #[tokio::test]
    async fn fetch_and_import_flow_over_loopback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = "---\nname: loopback-skill\ndescription: Served locally.\n---\nbody\n";
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/markdown\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });

        let url = format!("http://127.0.0.1:{port}/acme/loopback-skill/raw");
        let fetched = fetch_url(&url).await.unwrap();
        assert!(fetched.contains("name: loopback-skill"));

        let tmp = std::env::temp_dir().join(format!("aish-sf-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let path = import(&fetched, &tmp).unwrap();
        assert!(path.ends_with("loopback-skill/SKILL.md"));
        assert_eq!(crate::skills::load(&tmp).len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
