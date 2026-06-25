//! Skill registry integration — opt-in fetch & import of community skills, plus
//! a curated, binary-embedded catalog index for offline search.
//!
//! Skills are published in the same SKILL.md convention aish already uses
//! locally (YAML frontmatter with `name:`/`description:`, then a markdown body —
//! see src/skills.rs). This module is the OPT-IN bridge: nothing here runs
//! unless the user explicitly asks for a skill, with `aish --skill-fetch <ref>`
//! / `aish --skill-search <query>` or the interactive `:skill` commands.
//!
//! Flow: parse a ref → fetch the raw SKILL.md over HTTPS → validate it really
//! is a SKILL.md → write it under ~/.aish/skills/<name>/SKILL.md, where the
//! existing skills::load catalog picks it up on the next launch. The fetched
//! file is plain instructions (data, never code): aish never executes it, it
//! only advertises it to the model, so importing one can't run anything.
//!
//! Search defaults to a curated index embedded in the aish binary and written
//! to ~/.aish/registry/index.json on startup (see `initialize_registry`), so
//! the catalog is discoverable and searchable fully offline. Point
//! `AISH_SKILL_REGISTRY` at `https://skill.fish`, a self-hosted mirror, or
//! another `file://` index to use an alternate source.
//!
//! This mirrors the atum MCP server's `atum_import_skill` tool, which already
//! understands skill.fish refs — that path lets the *model* import a skill
//! mid-session; this CLI path lets a *user* import one without a backend or
//! any credentials. Both land a SKILL.md in the same local catalog.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The curated skill-registry index, embedded in the binary at compile time.
/// [`initialize_registry`] writes this to `~/.aish/registry/index.json` on
/// startup so the default `file://` registry always has a catalog to search,
/// even fully offline.
const EMBEDDED_INDEX: &str = include_str!("../registry/index.json");

/// The on-disk path of the local registry index: `~/.aish/registry/index.json`.
fn local_registry_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("registry")
        .join("index.json")
}

/// The default registry: a `file://` URI pointing at the local, binary-shipped
/// index written by [`initialize_registry`]. This makes skill *search* work
/// offline out of the box. Override with `AISH_SKILL_REGISTRY=scheme://host`
/// (e.g. `https://skill.fish`, a self-hosted mirror, or another `file://`
/// index) to use an alternate source — overriding is also what enables
/// *fetching* individual skills from their upstream origin.
fn default_registry() -> String {
    format!("file://{}", local_registry_path().display())
}

/// Resolve the active registry: the `AISH_SKILL_REGISTRY` override when set and
/// non-empty, else the local `file://` index default.
fn registry() -> String {
    std::env::var("AISH_SKILL_REGISTRY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(default_registry)
}

/// Write the curated, binary-embedded registry index to
/// `<aish_dir>/registry/index.json` so the default `file://` registry has a
/// catalog to search. Called once on startup; overwrites any prior copy so the
/// shipped index always matches this binary. Best-effort: the caller ignores
/// the error rather than aborting startup.
pub fn initialize_registry(aish_dir: &Path) -> std::io::Result<()> {
    let dir = aish_dir.join("registry");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("index.json"), EMBEDDED_INDEX)
}

/// A parsed reference to a skill on the registry.
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

/// The raw SKILL.md URL on `base` for this ref (no env lookup), so it stays a
/// pure, unit-testable function independent of the configured registry.
fn raw_url_on(base: &str, r: &SkillRef) -> String {
    let mut u = format!("{}/{}/{}/raw", base.trim_end_matches('/'), r.owner, r.name);
    if let Some(v) = &r.version {
        u.push_str(&format!("?version={v}"));
    }
    u
}

/// The raw SKILL.md URL on the configured registry for this ref.
pub fn raw_url(r: &SkillRef) -> String {
    raw_url_on(&registry(), r)
}

/// Refuse anything but HTTPS, except a loopback origin (for self-hosted mirrors
/// and the integration tests) or a file:// URI (for local mirrors).
fn check_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1") {
        return Ok(());
    }
    if url.starts_with("file://") {
        return Ok(());
    }
    bail!("refusing to fetch a skill over a non-HTTPS URL: {url}");
}

/// The shared reqwest client: an `aish/<version>` user-agent and a 20s timeout.
/// Reused by both the raw-SKILL fetch and the registry search so the two paths
/// present identically to the registry.
fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("aish/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?)
}

/// Detect Vercel's bot-protection challenge. skill.fish sits behind Vercel,
/// which answers automated requests with HTTP 429 plus an
/// `x-vercel-mitigated: challenge` header instead of the real payload. That
/// pairing is unmistakable, so we special-case it to give the user actionable
/// guidance rather than a bare "HTTP 429".
fn is_vercel_challenge(resp: &reqwest::Response) -> bool {
    resp.status().as_u16() == 429
        && resp
            .headers()
            .get("x-vercel-mitigated")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("challenge"))
            .unwrap_or(false)
}

/// Low-level fetch of a raw SKILL.md from an absolute URL (no env lookup), so
/// tests can point it at a loopback server without mutating process env.
/// Supports http://, https://, file://, and loopback origins.
pub async fn fetch_url(url: &str) -> Result<String> {
    check_url(url)?;
    
    // Handle file:// URIs locally
    if url.starts_with("file://") {
        let path = url_to_path(url)?;
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if body.trim().is_empty() {
            bail!("file {url} is empty");
        }
        return Ok(body);
    }
    
    // Handle http:// and https://
    let client = http_client()?;
    let resp = client.get(url).send().await.with_context(|| format!("fetching {url}"))?;
    if is_vercel_challenge(&resp) {
        bail!("{}", vercel_challenge_message());
    }
    if !resp.status().is_success() {
        bail!("skill.fish returned HTTP {} for {url}", resp.status().as_u16());
    }
    let body = resp.text().await.context("reading the skill body")?;
    if body.trim().is_empty() {
        bail!("skill.fish returned an empty body for {url}");
    }
    Ok(body)
}

/// Convert a file:// URI to a safe local path.
fn url_to_path(url: &str) -> Result<PathBuf> {
    let path = url.strip_prefix("file://")
        .context("not a file:// URI")?;
    let decoded = urlencoding::decode(path)
        .map(|s| s.into_owned())
        .context("URL decoding failed")?;
    Ok(PathBuf::from(decoded))
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

// ---------------------------------------------------------------------------
// Search — query the registry catalog (Phase 2)
// ---------------------------------------------------------------------------

/// One entry in a registry search response. Every field is optional on the
/// wire (the registry may omit a version or description), so each carries a
/// serde default; `author` also accepts the `owner` key and `reference` the
/// `ref`/`slug` keys, matching the shapes the registry has used in practice.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub name: String,
    #[serde(default, alias = "owner")]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, alias = "ref", alias = "slug")]
    pub reference: String,
}

impl SearchResult {
    /// The `owner/name` reference a user can paste into `--skill-fetch`. Prefers
    /// the explicit `reference` from the response, else composes `author/name`,
    /// else falls back to the bare name. Doubles as the dedup key.
    pub fn ref_or_synth(&self) -> String {
        let r = self.reference.trim();
        if !r.is_empty() {
            r.to_string()
        } else if !self.author.is_empty() && !self.name.is_empty() {
            format!("{}/{}", self.author, self.name)
        } else {
            self.name.clone()
        }
    }
}

/// Percent-encode a query value for a URL query string: the RFC 3986 unreserved
/// set passes through, everything else becomes `%XX`. Small and dependency-free
/// so the search URL is deterministic and unit-testable.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The search endpoint URL on `base` for `query`.
fn search_url_with_base(base: &str, query: &str) -> String {
    format!(
        "{}/api/v1/search?q={}&limit=50",
        base.trim_end_matches('/'),
        encode_query(query)
    )
}

/// Filter a locally-read catalog by a case-insensitive substring match on the
/// skill name, reference, author, or description — the offline equivalent of a
/// remote registry's `/api/v1/search?q=` filter. An empty query returns the
/// whole catalog.
fn filter_local(results: Vec<SearchResult>, query: &str) -> Vec<SearchResult> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return results;
    }
    results
        .into_iter()
        .filter(|r| {
            r.name.to_lowercase().contains(&q)
                || r.reference.to_lowercase().contains(&q)
                || r.author.to_lowercase().contains(&q)
                || r.description.to_lowercase().contains(&q)
        })
        .collect()
}

/// A helpful, multi-line error for the Vercel bot-challenge: skill.fish's search
/// endpoint sits behind Vercel's bot protection, which rejects automated clients
/// with HTTP 429 + `x-vercel-mitigated: challenge`. There's nothing aish can do
/// to clear the challenge from the CLI, so we point the user at the paths that
/// *do* work instead of failing with an opaque status code.
fn vercel_challenge_message() -> String {
    "skill.fish is behind a bot challenge right now (HTTP 429, x-vercel-mitigated: challenge) \
     and is refusing automated requests.\n\nTry one of these instead:\n  \
     • aish --skill-fetch <owner/name>   fetch a skill directly if you know its name\n  \
     • export AISH_SKILL_REGISTRY=<url>  point aish at a custom mirror of the registry\n  \
     • open https://skill.fish           browse and search in a browser as a last resort"
        .to_string()
}

/// Parse a search response body into a deduped list of results. Accepts either
/// a bare JSON array or an object wrapping the array under `results`/`skills`/
/// `data` (the registry shape isn't contractually fixed, so we're liberal).
/// Unparsable entries are skipped; duplicates (by reference) are dropped, keeping
/// the first. An empty list is a valid result, never an error.
fn parse_search_body(body: &str) -> Result<Vec<SearchResult>> {
    let v: serde_json::Value =
        serde_json::from_str(body).context("registry search response was not valid JSON")?;
    let arr = v
        .get("results")
        .or_else(|| v.get("skills"))
        .or_else(|| v.get("data"))
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in arr {
        let Ok(r) = serde_json::from_value::<SearchResult>(item) else {
            continue;
        };
        if seen.insert(r.ref_or_synth()) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Search `base`'s registry catalog for `query` (no env lookup), so tests can
/// point it at a loopback server without mutating process env. A `file://` base
/// is read directly as a local index.json catalog and filtered in-process;
/// http(s) bases hit the registry's `/api/v1/search` endpoint.
async fn search_with_base(base: &str, query: &str) -> Result<Vec<SearchResult>> {
    // A file:// base points directly at an index.json catalog — read & filter
    // it locally instead of hitting a `/api/v1/search` endpoint.
    if base.starts_with("file://") {
        check_url(base)?;
        let path = url_to_path(base)?;
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading local registry index {}", path.display()))?;
        return Ok(filter_local(parse_search_body(&body)?, query));
    }

    let url = search_url_with_base(base, query);
    check_url(&url)?;

    // Handle http:// and https://
    let client = http_client()?;
    let resp = client.get(&url).send().await.with_context(|| format!("searching {url}"))?;
    if is_vercel_challenge(&resp) {
        bail!("{}", vercel_challenge_message());
    }
    if !resp.status().is_success() {
        bail!("skill.fish returned HTTP {} for {url}", resp.status().as_u16());
    }
    let body = resp.text().await.context("reading the search response")?;
    parse_search_body(&body)
}

/// Search the configured registry catalog for `query`. An empty result list is
/// returned as `Ok(vec![])`, not an error.
pub async fn search(query: &str) -> Result<Vec<SearchResult>> {
    search_with_base(&registry(), query).await
}

/// Render search results as a plain, aligned text table (returned, not printed,
/// so it's unit-testable). Columns: the `owner/name` reference, the version, and
/// a truncated one-line description. An empty list renders the "no matches" line.
pub fn print_results_table(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("No skills found for {query:?}.");
    }
    const DESC_MAX: usize = 60;
    let refs: Vec<String> = results.iter().map(|r| r.ref_or_synth()).collect();
    let vers: Vec<String> = results
        .iter()
        .map(|r| if r.version.trim().is_empty() { "-".to_string() } else { r.version.clone() })
        .collect();
    let descs: Vec<String> = results.iter().map(|r| truncate(&r.description, DESC_MAX)).collect();

    let ref_w = refs
        .iter()
        .map(|s| s.chars().count())
        .chain(std::iter::once("SKILL".len()))
        .max()
        .unwrap_or(5);
    let ver_w = vers
        .iter()
        .map(|s| s.chars().count())
        .chain(std::iter::once("VERSION".len()))
        .max()
        .unwrap_or(7);

    let mut out = String::new();
    out.push_str(&format!("{:<ref_w$}  {:<ver_w$}  {}\n", "SKILL", "VERSION", "DESCRIPTION"));
    for i in 0..results.len() {
        out.push_str(&format!("{:<ref_w$}  {:<ver_w$}  {}\n", refs[i], vers[i], descs[i]));
    }
    out.push_str(&format!(
        "\n{} result(s) — fetch one with `aish --skill-fetch <skill>` or `:skill add <skill>`",
        results.len()
    ));
    out
}

/// Collapse newlines and cap a string at `max` display chars (ellipsis when cut).
fn truncate(s: &str, max: usize) -> String {
    let s = s.replace(['\n', '\r'], " ");
    if s.chars().count() <= max {
        return s;
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// CLI entry point for `aish --skill-search <query>`: search the registry and
/// print the result table.
pub async fn run_search(query: &str) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        bail!("empty search query — try `aish --skill-search <query>`");
    }
    println!("\x1b[2msearching {} for {query:?} …\x1b[0m", registry());
    let results = search(query).await?;
    println!("{}", print_results_table(query, &results));
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
        // Test the pure `raw_url_on` against a fixed base so the assertion is
        // independent of the (now file://) default registry — no env mutation.
        let r = SkillRef { owner: "a".into(), name: "b".into(), version: Some("2".into()) };
        assert_eq!(raw_url_on("https://skill.fish", &r), "https://skill.fish/a/b/raw?version=2");
        let r2 = SkillRef { owner: "a".into(), name: "b".into(), version: None };
        assert_eq!(raw_url_on("https://skill.fish", &r2), "https://skill.fish/a/b/raw");
        // A trailing slash on the base is normalized away.
        assert_eq!(raw_url_on("https://skill.fish/", &r2), "https://skill.fish/a/b/raw");
    }

    #[test]
    fn check_url_enforces_https_except_loopback_and_file() {
        assert!(check_url("https://skill.fish/a/b/raw").is_ok());
        assert!(check_url("http://127.0.0.1:8080/a/b/raw").is_ok());
        assert!(check_url("http://localhost/a/b/raw").is_ok());
        assert!(check_url("file:///tmp/index.json").is_ok());
        assert!(check_url("http://evil.example/a/b/raw").is_err());
        assert!(check_url("ftp://skill.fish/a/b").is_err());
    }

    #[test]
    fn default_registry_is_local_file_index() {
        // The default (no override) is a file:// URI pointing at the local
        // index.json under ~/.aish/registry. Pure function — no env mutation.
        let reg = default_registry();
        assert!(reg.starts_with("file://"), "got: {reg}");
        assert!(reg.ends_with("/registry/index.json"), "got: {reg}");
    }

    #[test]
    fn initialize_registry_writes_embedded_index() {
        let tmp = std::env::temp_dir().join(format!("aish-reg-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        initialize_registry(&tmp).unwrap();
        let written = std::fs::read_to_string(tmp.join("registry").join("index.json")).unwrap();
        assert_eq!(written, EMBEDDED_INDEX);
        // The embedded index parses as a non-empty catalog.
        let parsed = parse_search_body(&written).unwrap();
        assert!(!parsed.is_empty(), "embedded index parsed empty");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn search_reads_local_file_index() {
        // Write a small index, point a file:// base at it, and confirm search
        // reads + filters it without any network.
        let tmp = std::env::temp_dir().join(format!("aish-reg-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let idx = tmp.join("index.json");
        std::fs::write(
            &idx,
            r#"{"results":[
                {"name":"git-helper","author":"acme","reference":"acme/git-helper","description":"Helps with git."},
                {"name":"aws-s3","author":"acme","reference":"acme/aws-s3","description":"S3 buckets."}
            ]}"#,
        )
        .unwrap();
        let base = format!("file://{}", idx.display());
        let all = search_with_base(&base, "").await.unwrap();
        assert_eq!(all.len(), 2);
        let git = search_with_base(&base, "git").await.unwrap();
        assert_eq!(git.len(), 1);
        assert_eq!(git[0].ref_or_synth(), "acme/git-helper");
        let _ = std::fs::remove_dir_all(&tmp);
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
        let body = "---\nname: loopback-skill\ndescription: Served locally.\n---\nbody\n";
        let port = serve_once("200 OK", body.to_string()).await;

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

    // ---- Phase 2: search ------------------------------------------------

    /// One-shot loopback HTTP server: serves `status_line` (e.g. "200 OK") with
    /// `body` once, then closes. Returns the bound port. Shared by the fetch and
    /// search integration tests so neither touches the public registry or env.
    /// `extra_headers` are inserted verbatim into the response head (each must
    /// already end with its own CRLF), letting a test simulate e.g. the Vercel
    /// challenge header alongside a 429 status.
    async fn serve_once_with_headers(
        status_line: &str,
        extra_headers: &str,
        body: String,
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let status_line = status_line.to_string();
        let extra_headers = extra_headers.to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n{extra_headers}Connection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        port
    }

    /// Convenience wrapper: serve once with no extra headers.
    async fn serve_once(status_line: &str, body: String) -> u16 {
        serve_once_with_headers(status_line, "", body).await
    }

    #[tokio::test]
    async fn search_returns_parsed_results() {
        let body = r#"{"results":[{"name":"git-helper","author":"acme","description":"Helps with git.","version":"1.0.0","reference":"acme/git-helper"}]}"#;
        let port = serve_once("200 OK", body.to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        let got = search_with_base(&base, "git").await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "git-helper");
        assert_eq!(got[0].author, "acme");
        assert_eq!(got[0].version, "1.0.0");
        assert_eq!(got[0].ref_or_synth(), "acme/git-helper");
    }

    #[test]
    fn search_dedups_duplicate_refs() {
        // Two identical references collapse to one; a distinct one survives.
        let body = r#"[
            {"name":"a","author":"o","reference":"o/a"},
            {"name":"a","author":"o","reference":"o/a"},
            {"name":"b","author":"o","reference":"o/b"}
        ]"#;
        let got = parse_search_body(body).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ref_or_synth(), "o/a");
        assert_eq!(got[1].ref_or_synth(), "o/b");
    }

    #[test]
    fn search_synthesizes_reference_from_owner_name() {
        // `owner` alias fills author; reference is synthesized when absent.
        let body = r#"[{"name":"x","owner":"acme","description":"d"}]"#;
        let got = parse_search_body(body).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].author, "acme");
        assert_eq!(got[0].ref_or_synth(), "acme/x");
    }

    #[tokio::test]
    async fn search_empty_results_is_ok() {
        let port = serve_once("200 OK", "{\"results\":[]}".to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        let got = search_with_base(&base, "nonexistent").await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn search_http_error_propagates() {
        let port = serve_once("500 Internal Server Error", "oops".to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        assert!(search_with_base(&base, "x").await.is_err());
    }

    // A 429 carrying Vercel's `x-vercel-mitigated: challenge` header is detected
    // and surfaces the actionable guidance, not a bare HTTP status.
    #[tokio::test]
    async fn search_detects_vercel_challenge() {
        let port = serve_once_with_headers(
            "429 Too Many Requests",
            "x-vercel-mitigated: challenge\r\n",
            "blocked".to_string(),
        )
        .await;
        let base = format!("http://127.0.0.1:{port}");
        let err = search_with_base(&base, "github").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bot challenge"), "got: {msg}");
        assert!(msg.contains("--skill-fetch"), "got: {msg}");
        assert!(msg.contains("AISH_SKILL_REGISTRY"), "got: {msg}");
        assert!(msg.contains("https://skill.fish"), "got: {msg}");
    }

    // A plain 429 without the Vercel header takes the generic HTTP error path.
    #[tokio::test]
    async fn search_plain_429_is_generic_error() {
        let port = serve_once("429 Too Many Requests", "slow down".to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        let err = search_with_base(&base, "x").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("HTTP 429"), "got: {msg}");
        assert!(!msg.contains("bot challenge"), "got: {msg}");
    }

    #[test]
    fn search_query_is_url_encoded() {
        let u = search_url_with_base("https://skill.fish", "git helper & more/v2");
        assert!(u.starts_with("https://skill.fish/api/v1/search?q="), "got {u}");
        assert!(u.contains("git%20helper"), "space not encoded: {u}");
        assert!(u.contains("%26"), "& not encoded: {u}");
        assert!(u.contains("%2F"), "/ not encoded: {u}");
        assert!(u.ends_with("&limit=50"), "missing limit: {u}");
        // A trailing slash on the base is normalized away (no `//api`).
        assert_eq!(
            search_url_with_base("http://localhost:9/", "q"),
            "http://localhost:9/api/v1/search?q=q&limit=50"
        );
    }

    #[test]
    fn print_results_table_handles_empty() {
        assert_eq!(print_results_table("foo", &[]), "No skills found for \"foo\".");
    }

    #[test]
    fn print_results_table_lists_results() {
        let r = SearchResult {
            name: "git-helper".into(),
            author: "acme".into(),
            description: "Helps with git operations.".into(),
            version: "1.0.0".into(),
            reference: "acme/git-helper".into(),
        };
        let s = print_results_table("git", &[r]);
        assert!(s.contains("SKILL"));
        assert!(s.contains("VERSION"));
        assert!(s.contains("acme/git-helper"));
        assert!(s.contains("1.0.0"));
        assert!(s.contains("Helps with git operations."));
        assert!(s.contains("1 result(s)"));
    }

    #[test]
    fn truncate_caps_long_description() {
        let long = "x".repeat(80);
        let t = truncate(&long, 60);
        assert_eq!(t.chars().count(), 60);
        assert!(t.ends_with('…'));
        // Short strings pass through unchanged; newlines collapse to spaces.
        assert_eq!(truncate("short", 60), "short");
        assert_eq!(truncate("a\nb", 60), "a b");
    }
}
