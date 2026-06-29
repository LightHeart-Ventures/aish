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
// Removed Emulation import - using skillfish-cli UA instead

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

/// The `AISH_SKILL_REGISTRY` override, when set and non-empty (trailing slash
/// trimmed). `None` means "no override" — the caller picks the default source.
/// Search treats a present override as authoritative: it skips the dynamic
/// mcpmarket lookup and queries exactly what the user pointed it at.
fn registry_override() -> Option<String> {
    std::env::var("AISH_SKILL_REGISTRY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
}

/// Resolve the active registry: the `AISH_SKILL_REGISTRY` override when set and
/// non-empty, else the local `file://` index default.
fn registry() -> String {
    registry_override().unwrap_or_else(default_registry)
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

/// Read + parse the binary-shipped registry index (`~/.aish/registry/index.json`,
/// written by [`initialize_registry`] on startup) WITHOUT any network — the
/// source for the per-turn, offline skill-install recommendation
/// (`crate::skill_match::recommend_install`). Returns the full curated catalog
/// for the caller to rank in-process. Best-effort: a missing/unreadable/invalid
/// index yields an empty list, never an error, so the hot path can't fail on it.
pub fn local_index_catalog() -> Vec<SearchResult> {
    let Ok(body) = std::fs::read_to_string(local_registry_path()) else {
        return Vec::new();
    };
    parse_search_body(&body).unwrap_or_default()
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
    Ok(SkillRef {
        owner: owner.to_string(),
        name: name.to_string(),
        version,
    })
}

/// Reject path segments that could escape the skills dir or carry odd chars —
/// a SKILL.md `name:` becomes a directory name, so this is also a hard guard
/// against path traversal from an untrusted registry response.
fn validate_segment(s: &str) -> Result<()> {
    if s.is_empty() || s == "." || s == ".." || s.contains('/') || s.contains('\\') {
        bail!("unsafe path segment: {s:?}");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
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

/// The upstream origin used to FETCH an individual skill by its bare
/// `owner/name` ref. This is DISTINCT from [`registry`]: search defaults to the
/// local `file://` index (a catalog), but fetching one skill must hit a live
/// origin that serves `/{owner}/{name}/raw` — the file index is not a per-skill
/// file server, so reusing it as a fetch base yields a bogus
/// `file://…/index.json/{owner}/{name}/raw` path (ENOTDIR). An
/// `AISH_SKILL_REGISTRY` override is honored only when it's a real http(s)
/// mirror; a `file://` override (or no override) falls back to skill.fish.
fn fetch_origin() -> String {
    match registry_override() {
        Some(o) if !o.starts_with("file://") => o,
        _ => "https://skill.fish".to_string(),
    }
}

/// The raw SKILL.md URL on the configured fetch origin for this ref.
pub fn raw_url(r: &SkillRef) -> String {
    raw_url_on(&fetch_origin(), r)
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

/// The shared reqwest client and a 20s timeout.
/// Reused by both the raw-SKILL fetch and the registry search so the two paths
/// present identically to the registry.
/// Uses the skillfish-cli user-agent for compatibility with registries that have
/// allowlists for known tools (e.g. skill.fish / mcpmarket behind Vercel).
fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("skillfish-cli")
        .timeout(Duration::from_secs(20))
        .http1_only()
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
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if is_vercel_challenge(&resp) {
        bail!("{}", vercel_challenge_message());
    }
    if !resp.status().is_success() {
        bail!(
            "skill.fish returned HTTP {} for {url}",
            resp.status().as_u16()
        );
    }
    let body = resp.text().await.context("reading the skill body")?;
    if body.trim().is_empty() {
        bail!("skill.fish returned an empty body for {url}");
    }
    Ok(body)
}

/// Convert a file:// URI to a safe local path.
fn url_to_path(url: &str) -> Result<PathBuf> {
    let path = url.strip_prefix("file://").context("not a file:// URI")?;
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
/// Accepts a skill.fish `owner/name[@version]` ref, a `github:owner/repo[/path]`
/// spec, a `https://github.com/...` URL, or a `https://raw.githubusercontent.com/
/// owner/repo/<ref>/path/SKILL.md` raw URL. A GitHub spec that points at a repo
/// (rather than a single skill) imports every SKILL.md discovered under it.
pub async fn run_fetch(input: &str, skills_dir: &Path) -> Result<()> {
    let imported = add(input, skills_dir).await?;
    if imported.is_empty() {
        bail!("no SKILL.md found for {input:?}");
    }
    for sk in &imported {
        println!(
            "\x1b[32m✓\x1b[0m imported skill \x1b[1m{}\x1b[0m → {}",
            sk.name,
            sk.path.display()
        );
        if !sk.description.is_empty() {
            println!("  \x1b[2m{}\x1b[0m", sk.description);
        }
    }
    let n = imported.len();
    println!(
        "  {} skill{} in your catalog now — aish will use {} when a task matches.",
        n,
        if n == 1 { "" } else { "s" },
        if n == 1 { "it" } else { "them" }
    );
    Ok(())
}

/// A skill that was fetched and written to disk by [`add`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Unified import entry point used by both the CLI (`--skill-fetch`) and the
/// interactive `:skill add`. Detects the source from `input`:
///   * a `github:owner/repo[/path][@ref]` spec, a `https://github.com/...` URL,
///     or a `https://raw.githubusercontent.com/...` raw URL
///     → GitHub path resolution + (possibly multi-skill) discovery & import;
///   * anything else → a skill.fish `owner/name[@version]` ref, fetched and
///     imported as a single skill.
/// Returns every skill written, so the caller can reload its catalog and report.
pub async fn add(input: &str, skills_dir: &Path) -> Result<Vec<ImportedSkill>> {
    if let Some(gh) = parse_github_ref(input) {
        return add_github(&gh, skills_dir).await;
    }
    let r = parse_ref(input)?;
    match fetch(&r).await {
        Ok(text) => {
            let path = import(&text, skills_dir)?;
            let (name, description) =
                crate::skills::parse_frontmatter(&text).unwrap_or((r.name.clone(), String::new()));
            Ok(vec![ImportedSkill {
                name,
                description,
                path,
            }])
        }
        // The bare `owner/name` wasn't fetchable as a skill.fish skill. It may be
        // a SHORT NAME shown by `--skill-search` (whose rows are mcpmarket/GitHub
        // hits keyed by a long URL). Resolve the short name back to its real
        // fetchable reference and import that, so what the table prints is
        // exactly what `--skill-fetch` / `:skill add` accept.
        Err(skillfish_err) => match resolve_ref_via_search(input).await {
            Ok(reference) => {
                if let Some(gh) = parse_github_ref(&reference) {
                    return add_github(&gh, skills_dir).await;
                }
                let r2 = parse_ref(&reference)?;
                let text = fetch(&r2).await?;
                let path = import(&text, skills_dir)?;
                let (name, description) = crate::skills::parse_frontmatter(&text)
                    .unwrap_or((r2.name.clone(), String::new()));
                Ok(vec![ImportedSkill {
                    name,
                    description,
                    path,
                }])
            }
            // No registry match either — surface the original fetch error.
            Err(_) => Err(skillfish_err),
        },
    }
}

/// Resolve a short `owner/skill` name (as printed by `--skill-search`) back to a
/// fetchable reference by re-querying the registry and matching on
/// [`SearchResult::short_name`]. When several hits share the short name, the
/// most-starred wins. Errors when nothing matches, so the caller can fall back
/// to the original fetch error.
async fn resolve_ref_via_search(input: &str) -> Result<String> {
    let needle = input.trim().to_lowercase();
    // Query with the leaf skill name to get a focused candidate set.
    let leaf = needle.rsplit('/').next().unwrap_or(&needle).to_string();
    let results = search(&leaf).await?;
    let best = results
        .iter()
        .filter(|r| r.short_name().to_lowercase() == needle)
        .max_by_key(|r| r.stars);
    match best {
        Some(r) => Ok(r.ref_or_synth()),
        None => bail!(
            "no skill named {input:?} in the registry — run `aish --skill-search {leaf}` to see exact names"
        ),
    }
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
    #[serde(default, alias = "owner", alias = "publisher", alias = "namespace")]
    pub author: String,
    #[serde(default, alias = "summary", alias = "tagline")]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(
        default,
        alias = "ref",
        alias = "slug",
        alias = "full_name",
        alias = "fullName",
        alias = "id"
    )]
    pub reference: String,
    /// Popularity signal from the registry (mcpmarket's `github_stars`). 0 when
    /// the source doesn't report it. Surfaced as the STARS column and used to
    /// rank results most-popular-first.
    #[serde(default, alias = "github_stars", alias = "stars_count")]
    pub stars: u64,
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

    /// A short, human-readable `author/skill` label for the SKILL column —
    /// the readable counterpart to [`ref_or_synth`], which often holds a long
    /// `https://github.com/owner/repo/tree/<sha>/path/skill` URL that's painful
    /// to scan. Prefers the explicit `author` + `name`; when those are missing
    /// it distills a short name out of the reference: for a GitHub tree/blob URL
    /// it takes the repo owner and the leaf skill directory (e.g.
    /// `openhands/skills/tree/<sha>/skills/github` → `openhands/github`); for a
    /// bare `owner/name` ref it passes through unchanged.
    pub fn short_name(&self) -> String {
        let author = self.author.trim();
        let name = self.name.trim();
        if !author.is_empty() && !name.is_empty() {
            return format!("{author}/{name}");
        }
        short_name_from_ref(&self.ref_or_synth())
    }
}

/// Distill a compact `owner/skill` label from a registry reference. Handles a
/// full `github.com/<owner>/<repo>/tree|blob/<ref>/<path…>/<skill>` URL by
/// pairing the repo owner with the leaf path segment (the skill's own
/// directory), and leaves a short `owner/name` ref untouched. Pure + testable.
fn short_name_from_ref(reference: &str) -> String {
    let r = reference.trim();
    // Strip a known host prefix so we're left with path segments.
    let path = r
        .strip_prefix("https://github.com/")
        .or_else(|| r.strip_prefix("http://github.com/"))
        .or_else(|| r.strip_prefix("github.com/"))
        .or_else(|| r.strip_prefix("https://skill.fish/"))
        .unwrap_or(r)
        .trim_matches('/');
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        [] => r.to_string(),
        [only] => only.to_string(),
        [owner, rest @ ..] => {
            // Drop a `tree`/`blob` + ref marker and any trailing `SKILL.md`, then
            // take the leaf path segment as the skill name.
            let mut tail: Vec<&str> = rest.to_vec();
            if matches!(tail.first().copied(), Some("tree") | Some("blob")) && tail.len() >= 2 {
                tail.drain(0..2);
            }
            if tail.last().copied() == Some("SKILL.md") {
                tail.pop();
            }
            // Skip generic container directories so the leaf is the real skill.
            while tail.len() > 1
                && matches!(tail.last().copied(), Some("skills") | Some("skill"))
            {
                tail.pop();
            }
            match tail.last() {
                Some(skill) => format!("{owner}/{skill}"),
                None => owner.to_string(),
            }
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
        .or_else(|| v.get("items"))
        .or_else(|| v.get("hits"))
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
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("searching {url}"))?;
    if is_vercel_challenge(&resp) {
        bail!("{}", vercel_challenge_message());
    }
    if !resp.status().is_success() {
        bail!(
            "skill.fish returned HTTP {} for {url}",
            resp.status().as_u16()
        );
    }
    let body = resp.text().await.context("reading the search response")?;
    parse_search_body(&body)
}

/// Search for `query` across the available skill sources.
///
/// Source precedence (mirrors skillfish's `:skill search`):
///   1. An explicit `AISH_SKILL_REGISTRY` override always wins — when the user
///      points aish at a specific mirror we honor it exactly and do not
///      second-guess it with mcpmarket.
///   2. Otherwise, query mcpmarket.com's live skills API (the dynamic, primary
///      source). Non-empty results are merged with the curated, binary-embedded
///      index so the built-in catalog is always discoverable too.
///   3. If mcpmarket is unreachable or returns nothing, fall back to the local
///      embedded index alone — search keeps working fully offline.
///
/// An empty result list is returned as `Ok(vec![])`, never an error.
pub async fn search(query: &str) -> Result<Vec<SearchResult>> {
    // (1) An explicit registry override is authoritative.
    if let Some(base) = registry_override() {
        return search_with_base(&base, query).await;
    }

    // (3, prepared) The offline fallback: the curated, binary-embedded index.
    let local = search_with_base(&default_registry(), query)
        .await
        .unwrap_or_default();

    // (2) mcpmarket is the dynamic primary source. On any failure we degrade
    // gracefully to the embedded index rather than surfacing a network error.
    match search_mcpmarket(query, MCPMARKET_LIMIT).await {
        Ok(remote) if !remote.is_empty() => Ok(merge_results(remote, local)),
        _ => Ok(local),
    }
}

/// Merge two result lists, preferring `primary` order and dropping any
/// `fallback` entry whose `owner/name` reference already appeared in `primary`.
/// Keeps the dynamic mcpmarket hits first, then appends embedded-index entries
/// the live search didn't already cover.
fn merge_results(primary: Vec<SearchResult>, fallback: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut seen: HashSet<String> = primary.iter().map(|r| r.ref_or_synth()).collect();
    let mut out = primary;
    for r in fallback {
        if seen.insert(r.ref_or_synth()) {
            out.push(r);
        }
    }
    out
}

/// Get the local skills directory path: `~/.aish/skills`.
fn skills_dir_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("skills")
}

/// Render search results as a plain, aligned text table (returned, not printed,
/// so it's unit-testable). Columns: a short `author/skill` name, the GitHub
/// star count (popularity), a truncated one-line description, and an installed
/// marker. Results are ordered most-starred-first so the strongest candidate
/// leads. An empty list renders the "no matches" line.
pub fn print_results_table(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("No skills found for {query:?}.");
    }
    const DESC_MAX: usize = 56;

    // Rank most-popular-first while keeping the registry's relative order as the
    // stable tiebreaker (sort_by is stable), so 0-star ties preserve relevance.
    let mut ranked: Vec<&SearchResult> = results.iter().collect();
    ranked.sort_by(|a, b| b.stars.cmp(&a.stars));

    // Load locally installed skills to mark ones already in the catalog.
    let installed = crate::skills::load(&skills_dir_path());
    let installed_names: std::collections::HashSet<_> =
        installed.iter().map(|s| s.name.clone()).collect();

    let names: Vec<String> = ranked.iter().map(|r| r.short_name()).collect();
    let stars: Vec<String> = ranked
        .iter()
        .map(|r| if r.stars > 0 { format!("★ {}", r.stars) } else { "-".to_string() })
        .collect();
    let descs: Vec<String> = ranked
        .iter()
        .map(|r| truncate(&r.description, DESC_MAX))
        .collect();
    let statuses: Vec<String> = ranked
        .iter()
        .map(|r| {
            // A result counts as installed if either its short skill name or its
            // raw `name` field matches a locally-installed skill directory.
            let leaf = r.short_name();
            let leaf = leaf.rsplit('/').next().unwrap_or(&leaf);
            if installed_names.contains(&leaf.to_string())
                || (!r.name.is_empty() && installed_names.contains(&r.name))
            {
                "✓ installed".to_string()
            } else {
                String::new()
            }
        })
        .collect();

    let name_w = names
        .iter()
        .map(|s| s.chars().count())
        .chain(std::iter::once("SKILL".len()))
        .max()
        .unwrap_or(5);
    let star_w = stars
        .iter()
        .map(|s| s.chars().count())
        .chain(std::iter::once("STARS".len()))
        .max()
        .unwrap_or(5);
    let status_w = statuses
        .iter()
        .map(|s| s.chars().count())
        .chain(std::iter::once("STATUS".len()))
        .max()
        .unwrap_or(6);

    // Respect --no-color / NO_COLOR / a piped stdout so escape codes never leak
    // into a grep or a redirect (skill output is routinely piped).
    let color = crate::style::colors_enabled();
    let (bold, dim, green, yellow, reset) = if color {
        ("\x1b[1m", "\x1b[2m", "\x1b[32m", "\x1b[33m", "\x1b[0m")
    } else {
        ("", "", "", "", "")
    };

    let mut out = String::new();
    out.push_str(&format!(
        "{:<name_w$}  {:>star_w$}  {:<DESC_MAX$}  {:<status_w$}\n",
        "SKILL", "STARS", "DESCRIPTION", "STATUS"
    ));
    for i in 0..ranked.len() {
        let status_color = if statuses[i].is_empty() { dim } else { green };
        out.push_str(&format!(
            "{bold}{:<name_w$}{reset}  {yellow}{:>star_w$}{reset}  {:<DESC_MAX$}  {status_color}{:<status_w$}{reset}\n",
            names[i], stars[i], descs[i], statuses[i]
        ));
    }
    // Echo the top hit so the suggested fetch command is copy-paste ready.
    let top = names.first().cloned().unwrap_or_default();
    out.push_str(&format!(
        "\n{} result(s) — fetch one by name, e.g. `aish --skill-fetch {}` or `:skill add {}`",
        ranked.len(),
        top,
        top
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
    let line = format!("searching {} for {query:?} …", registry());
    println!("{}", crate::style::dim(&line));
    let results = search(query).await?;
    println!("{}", print_results_table(query, &results));
    Ok(())
}

// ---------------------------------------------------------------------------
// mcpmarket.com — the dynamic, primary search source (Phase 3)
// ---------------------------------------------------------------------------
//
// mcpmarket.com publishes a live, growing catalog of skills. We query it as the
// PRIMARY source for `:skill search` / `--skill-search`, merging its hits over
// the curated, binary-embedded index so the built-in catalog is still
// discoverable and search keeps working offline (see `search`).

/// The mcpmarket origin. Override with `AISH_MCPMARKET_BASE` (used by the tests
/// to point at a loopback server, and available as an escape hatch for a mirror).
const MCPMARKET_BASE: &str = "https://mcpmarket.com";

/// Whether to emit the verbose `[mcpmarket]` retry/transport trace to stderr.
/// Off by default — those lines were polluting a normal `--skill-search` run.
/// Set `AISH_SKILL_DEBUG=1` to turn the trace back on for troubleshooting.
fn skill_debug() -> bool {
    std::env::var("AISH_SKILL_DEBUG")
        .map(|v| v != "0" && !v.trim().is_empty())
        .unwrap_or(false)
}

/// How many skills to ask mcpmarket for. Matches the registry's own `limit=50`.
const MCPMARKET_LIMIT: usize = 50;

/// The active mcpmarket base: the `AISH_MCPMARKET_BASE` override when set and
/// non-empty (trailing slash trimmed), else the public origin.
fn mcpmarket_base() -> String {
    std::env::var("AISH_MCPMARKET_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| MCPMARKET_BASE.to_string())
}

/// The mcpmarket skills-search endpoint URL on `base` for `query`:
/// `GET {base}/api/search?q=<query>&type=skills&limit=<limit>`.
fn mcpmarket_search_url(base: &str, query: &str, limit: usize) -> String {
    format!(
        "{}/api/search?q={}&type=skills&limit={}",
        base.trim_end_matches('/'),
        encode_query(query),
        limit
    )
}

/// Retry delays in milliseconds, matching skillfish's exponential backoff.
const MCPMARKET_RETRY_DELAYS_MS: &[u64] = &[1000, 2000, 4000];

/// Query a specific mcpmarket `base` (no env lookup), so tests can point it at a
/// loopback server. Reuses [`parse_search_body`] — mcpmarket's per-skill fields
/// (`name`, `publisher`/`namespace`, `summary`, `full_name`/`slug`, …) are folded
/// onto [`SearchResult`] via the serde aliases on that struct.
///
/// Retries with exponential backoff on 429 responses (matching skillfish's behavior).
async fn search_mcpmarket_with_base(
    base: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let url = mcpmarket_search_url(base, query, limit);
    check_url(&url)?;
    
    let client = http_client()?;
    let mut last_error: Option<anyhow::Error> = None;
    
    let debug = skill_debug();
    // Retry up to 3 times with exponential backoff (matching skillfish)
    for attempt in 0..3 {
        if debug {
            eprintln!("[mcpmarket] attempt {} of 3...", attempt + 1);
        }
        let resp = match client
            .get(&url)
            .header("Referer", "https://mcpmarket.com/")
            .header("Accept", "application/json")
            .header("User-Agent", "skillfish-cli")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if debug {
                    eprintln!("[mcpmarket] request failed: {e}");
                }
                last_error = Some(e.into());
                if attempt < 2 {
                    if debug {
                        eprintln!("[mcpmarket] sleeping {}ms before retry...", MCPMARKET_RETRY_DELAYS_MS[attempt]);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        MCPMARKET_RETRY_DELAYS_MS[attempt],
                    ))
                    .await;
                    continue;
                }
                return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("request failed")));
            }
        };

        if debug {
            eprintln!("[mcpmarket] got response: {}", resp.status());
        }
        // 429 (Vercel challenge) → retry with backoff
        if resp.status().as_u16() == 429 {
            if attempt < 2 {
                if debug {
                    eprintln!("[mcpmarket] 429 on attempt {}, retrying in {}ms...", attempt + 1, MCPMARKET_RETRY_DELAYS_MS[attempt]);
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    MCPMARKET_RETRY_DELAYS_MS[attempt],
                ))
                .await;
                continue;
            }
            // Last attempt failed with 429
            if debug {
                eprintln!("[mcpmarket] 429 on final attempt {}", attempt + 1);
            }
            last_error = Some(anyhow::anyhow!("HTTP 429 after retries"));
            continue;
        }
        
        // Other success or client error → return immediately
        if !resp.status().is_success() {
            bail!("mcpmarket returned HTTP {} for {url}", resp.status().as_u16());
        }
        
        let body = resp
            .text()
            .await
            .context("reading the mcpmarket search response")?;
        return parse_mcpmarket_body(&body);
    }
    
    // All retries exhausted
    if let Some(e) = last_error {
        return Err(e);
    }
    bail!("mcpmarket search failed after retries")
}

/// Query mcpmarket's public skills API for `query`.
async fn search_mcpmarket(query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    search_mcpmarket_with_base(&mcpmarket_base(), query, limit).await
}

/// Parse an mcpmarket search response into [`SearchResult`]s.
///
/// mcpmarket's per-skill JSON shape differs from the generic registry shape that
/// [`parse_search_body`] handles, so it needs its own mapping:
///   * `owner` is a **nested object** `{name,url,avatar}` — not a bare string —
///     so the generic serde `alias = "owner"` on `author` fails to deserialize
///     and silently drops EVERY row (the bug this function fixes). We read
///     `owner.name` explicitly.
///   * the skill identifier lives in `skill_name`/`raw_name`/`slug`, not `name`
///     (which is a human display title like "Discord Doctor").
///   * the canonical, fetchable location is the `website` GitHub URL, which
///     `:skill add` / `--skill-fetch` resolve via the GitHub path. We surface
///     that as the `reference` so the value shown in the table actually fetches.
///
/// Hits are wrapped under `skills` (mcpmarket's key); we also accept the generic
/// wrappers for forward-compatibility. Duplicates (by reference) are dropped.
fn parse_mcpmarket_body(body: &str) -> Result<Vec<SearchResult>> {
    let v: serde_json::Value =
        serde_json::from_str(body).context("mcpmarket search response was not valid JSON")?;
    let arr = v
        .get("skills")
        .or_else(|| v.get("results"))
        .or_else(|| v.get("items"))
        .or_else(|| v.get("data"))
        .or_else(|| v.get("hits"))
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in arr {
        // First non-empty string among the given keys.
        let pick = |keys: &[&str]| -> String {
            for k in keys {
                if let Some(s) = item.get(*k).and_then(|x| x.as_str()) {
                    let s = s.trim();
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
            String::new()
        };

        // owner is usually a nested object {name,url,...}; tolerate a bare
        // string too, then fall back to the flat publisher/namespace fields.
        let author = item
            .get("owner")
            .and_then(|o| {
                o.get("name")
                    .and_then(|n| n.as_str())
                    .or_else(|| o.as_str())
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| pick(&["publisher", "namespace", "author"]));

        let name = pick(&["skill_name", "raw_name", "slug", "name"]);
        let description = pick(&["description", "summary", "tagline"]);
        let version = pick(&["version"]);
        // Popularity: mcpmarket reports `github_stars` as a number.
        let stars = ["github_stars", "stars", "stars_count"]
            .iter()
            .find_map(|k| item.get(*k).and_then(|x| x.as_u64()))
            .unwrap_or(0);

        // Canonical fetchable location: the GitHub website URL. Fall back to the
        // `github` short path, then a synthesized owner/name.
        let website = pick(&["website"]);
        let reference = if !website.is_empty() {
            website
        } else {
            let gh = pick(&["github"]);
            if !gh.is_empty() {
                gh
            } else if !author.is_empty() && !name.is_empty() {
                format!("{author}/{name}")
            } else {
                name.clone()
            }
        };

        if name.is_empty() && reference.is_empty() {
            continue;
        }
        let r = SearchResult {
            name,
            author,
            description,
            version,
            reference,
            stars,
        };
        if seen.insert(r.ref_or_synth()) {
            out.push(r);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// GitHub — fetch a SKILL.md (or discover many) from a repo (Phase 3)
// ---------------------------------------------------------------------------
//
// skillfish-style GitHub support. A `github:owner/repo[/path][@ref]` spec — or a
// plain `https://github.com/owner/repo[/tree/<ref>/path]` URL — resolves to one
// or more SKILL.md files:
//   * a spec that names a single SKILL.md (or a directory containing one) imports
//     that one skill;
//   * a spec that names a repo (or a sub-tree) is expanded via the GitHub trees
//     API into EVERY SKILL.md beneath it (multi-skill repos), each imported.
// Private repos work when `GITHUB_TOKEN` (or `GH_TOKEN`) is set — it's sent as a
// Bearer token on both the API and raw.githubusercontent.com requests.

/// The GitHub REST API origin (trees API). Override with `AISH_GITHUB_API_BASE`.
const GITHUB_API: &str = "https://api.github.com";

/// The raw file origin. Override with `AISH_GITHUB_RAW_BASE`.
const GITHUB_RAW: &str = "https://raw.githubusercontent.com";

/// The git ref used when a spec doesn't pin one. `HEAD` resolves to the repo's
/// default branch on both the trees API and raw.githubusercontent.com.
const GITHUB_DEFAULT_REF: &str = "HEAD";

/// A parsed reference to a skill (or a repo of skills) hosted on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRef {
    pub owner: String,
    pub repo: String,
    /// Branch, tag, or commit SHA. Defaults to [`GITHUB_DEFAULT_REF`] (`HEAD`).
    pub git_ref: String,
    /// Sub-path within the repo: a directory of skills, or a single SKILL.md.
    /// `None` means "the whole repo" — discovery walks every SKILL.md in it.
    pub path: Option<String>,
}

/// A single path segment is safe for a GitHub owner/repo/path component.
fn valid_github_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A git ref is safe: non-empty, no `..`, no whitespace; `/` is allowed so
/// branch names like `release/v2` pass through.
fn valid_github_ref(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("..")
        && !s.chars().any(|c| c.is_whitespace())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Validate a repo sub-path: every `/`-separated component must be a safe
/// segment. Returns the normalized path (no leading/trailing slash) or `None`.
fn normalize_github_path(path: &str) -> Option<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.split('/').all(valid_github_segment) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Parse a GitHub skill spec. Returns `None` when `input` is not a GitHub spec
/// (so the caller can fall back to the skill.fish `parse_ref` path). Accepts:
///   * `github:owner/repo`, `github:owner/repo/path/to/skill`, `…@ref`
///   * `gh:owner/repo[/path][@ref]` (short alias)
///   * `https://github.com/owner/repo`
///   * `https://github.com/owner/repo/tree/<ref>/path/to/skill`
///   * `https://github.com/owner/repo/blob/<ref>/path/to/skill/SKILL.md`
///   * `https://raw.githubusercontent.com/owner/repo/<ref>/path/to/SKILL.md`
///     (the "Raw" button URL / what `curl` fetches; ref is the 3rd segment)
/// A `.git` suffix on the repo is stripped. An unparsable/unsafe spec → `None`.
/// Parse a `raw.githubusercontent.com/<owner>/<repo>/<ref>/<path…>` URL body
/// (everything after the host) into a [`GithubRef`]. This is the "Raw" button
/// URL — and exactly what `curl`/`wget` fetch — so a user who copies the raw
/// link to a SKILL.md can paste it straight into `:skill add`. Unlike a
/// github.com tree/blob URL, the ref is a single POSITIONAL segment (branch,
/// tag, or commit SHA) with no `tree`/`blob` marker, so it needs its own parse.
/// A trailing `?…` query (e.g. a `?token=` on a private raw link) is stripped.
/// The remaining path is returned verbatim — for a direct raw link it ends in
/// `SKILL.md`, which [`resolve_github_skill_paths`] fetches as a single skill.
/// Returns `None` for an unsafe or too-short path (caller falls back to the
/// skill.fish `parse_ref` path).
fn parse_github_raw_ref(rest: &str) -> Option<GithubRef> {
    let body = rest.split('?').next().unwrap_or(rest).trim_matches('/');
    let segs: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
    // Need at least owner/repo/ref.
    if segs.len() < 3 {
        return None;
    }
    let owner = segs[0];
    let repo = segs[1].strip_suffix(".git").unwrap_or(segs[1]);
    let git_ref = segs[2];
    if !valid_github_segment(owner) || !valid_github_segment(repo) || !valid_github_ref(git_ref) {
        return None;
    }
    let path = if segs.len() > 3 {
        Some(normalize_github_path(&segs[3..].join("/"))?)
    } else {
        None
    };
    Some(GithubRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
        git_ref: git_ref.to_string(),
        path,
    })
}

pub fn parse_github_ref(input: &str) -> Option<GithubRef> {
    let s = input.trim();
    // raw.githubusercontent.com/<owner>/<repo>/<ref>/<path…> — the "Raw" button
    // URL (and what `curl`/`wget` fetch). Its ref is positional, not behind a
    // `tree`/`blob` marker, so it gets a dedicated parse before the github.com /
    // `github:` forms below.
    for raw_host in [
        "https://raw.githubusercontent.com/",
        "http://raw.githubusercontent.com/",
        "raw.githubusercontent.com/",
    ] {
        if let Some(rest) = s.strip_prefix(raw_host) {
            return parse_github_raw_ref(rest);
        }
    }
    // `url_form` specs carry the ref inside `/tree/<ref>/` or `/blob/<ref>/`;
    // `prefix` specs carry it as a trailing `@ref`.
    let (rest, url_form) = if let Some(r) = s.strip_prefix("https://github.com/") {
        (r, true)
    } else if let Some(r) = s.strip_prefix("http://github.com/") {
        (r, true)
    } else if let Some(r) = s.strip_prefix("github.com/") {
        (r, true)
    } else if let Some(r) = s.strip_prefix("github:") {
        (r, false)
    } else if let Some(r) = s.strip_prefix("gh:") {
        (r, false)
    } else {
        return None;
    };

    // A trailing `@ref` (prefix form only — a URL pins its ref via tree/blob).
    let (body, at_ref) = if !url_form {
        match rest.rsplit_once('@') {
            Some((b, r)) if !r.is_empty() => (b, Some(r.to_string())),
            _ => (rest, None),
        }
    } else {
        (rest, None)
    };

    let body = body.trim_matches('/');
    let segs: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None; // need at least owner/repo
    }
    let owner = segs[0];
    let repo = segs[1].strip_suffix(".git").unwrap_or(segs[1]);
    if !valid_github_segment(owner) || !valid_github_segment(repo) {
        return None;
    }

    // Resolve the ref + the in-repo path from whatever segments remain.
    let mut git_ref = at_ref.unwrap_or_else(|| GITHUB_DEFAULT_REF.to_string());
    let rest_segs = &segs[2..];
    let path_segs: &[&str] = if url_form
        && matches!(rest_segs.first().copied(), Some("tree") | Some("blob"))
    {
        // /tree/<ref>/<path…> or /blob/<ref>/<path…>
        match rest_segs.get(1) {
            Some(r) => {
                git_ref = (*r).to_string();
                &rest_segs[2..]
            }
            None => return None, // `/tree` with no ref is malformed
        }
    } else {
        rest_segs
    };

    if !valid_github_ref(&git_ref) {
        return None;
    }

    let path = if path_segs.is_empty() {
        None
    } else {
        Some(normalize_github_path(&path_segs.join("/"))?)
    };

    Some(GithubRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
        git_ref,
        path,
    })
}

/// `true` when a repo-relative path's basename is exactly `SKILL.md`.
fn is_skill_md_path(path: &str) -> bool {
    path.rsplit('/').next() == Some("SKILL.md")
}

/// `true` when `path` (a SKILL.md file path) sits at or beneath `prefix`. A
/// prefix that itself names a SKILL.md matches only that exact file; a directory
/// prefix matches `prefix/SKILL.md` and anything deeper (`prefix/sub/SKILL.md`).
fn path_under_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_matches('/');
    if is_skill_md_path(prefix) {
        return path == prefix;
    }
    path.starts_with(&format!("{prefix}/"))
}

/// The raw.githubusercontent.com URL for `file_path` in this ref's repo:
/// `{raw_base}/{owner}/{repo}/{ref}/{file_path}`.
fn github_raw_url_on(raw_base: &str, gh: &GithubRef, file_path: &str) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        raw_base.trim_end_matches('/'),
        gh.owner,
        gh.repo,
        gh.git_ref,
        file_path.trim_start_matches('/')
    )
}

/// The GitHub trees-API URL that lists every path in the repo at this ref:
/// `{api_base}/repos/{owner}/{repo}/git/trees/{ref}?recursive=1`.
fn trees_api_url_on(api_base: &str, gh: &GithubRef) -> String {
    format!(
        "{}/repos/{}/{}/git/trees/{}?recursive=1",
        api_base.trim_end_matches('/'),
        gh.owner,
        gh.repo,
        gh.git_ref
    )
}

/// Extract the repo-relative SKILL.md file paths from a GitHub trees-API
/// response body. Keeps only `blob` entries whose basename is `SKILL.md`,
/// optionally restricted to those under `path_prefix`. Sorted + deduped so
/// multi-skill discovery is deterministic.
fn parse_trees_skill_paths(body: &str, path_prefix: Option<&str>) -> Result<Vec<String>> {
    let v: serde_json::Value =
        serde_json::from_str(body).context("GitHub trees response was not valid JSON")?;
    let tree = v
        .get("tree")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for entry in tree {
        if entry.get("type").and_then(|t| t.as_str()) != Some("blob") {
            continue;
        }
        let Some(p) = entry.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        if !is_skill_md_path(p) {
            continue;
        }
        if let Some(prefix) = path_prefix {
            if !path_under_prefix(p, prefix) {
                continue;
            }
        }
        out.push(p.to_string());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The GitHub auth token, if present: `GITHUB_TOKEN` then `GH_TOKEN`. Sent as a
/// Bearer token so private repos and rate-limited public ones work.
fn github_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The active GitHub API base: `AISH_GITHUB_API_BASE` override else [`GITHUB_API`].
fn github_api_base() -> String {
    std::env::var("AISH_GITHUB_API_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| GITHUB_API.to_string())
}

/// The active raw file base: `AISH_GITHUB_RAW_BASE` override else [`GITHUB_RAW`].
fn github_raw_base() -> String {
    std::env::var("AISH_GITHUB_RAW_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| GITHUB_RAW.to_string())
}

/// A GET against GitHub with an optional Bearer token and Accept header. The
/// caller inspects the returned response's status.
async fn github_get(
    url: &str,
    token: Option<&str>,
    accept: Option<&str>,
) -> Result<reqwest::Response> {
    check_url(url)?;
    let client = http_client()?;
    let mut req = client.get(url);
    if let Some(a) = accept {
        req = req.header(reqwest::header::ACCEPT, a);
    }
    if let Some(t) = token {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    req.send()
        .await
        .with_context(|| format!("requesting {url}"))
}

/// Fetch a raw text body from GitHub, erroring on a non-success status or an
/// empty body.
async fn fetch_github_text(url: &str, token: Option<&str>) -> Result<String> {
    let resp = github_get(url, token, None).await?;
    if !resp.status().is_success() {
        bail!("GitHub returned HTTP {} for {url}", resp.status().as_u16());
    }
    let body = resp.text().await.context("reading the GitHub response body")?;
    if body.trim().is_empty() {
        bail!("GitHub returned an empty body for {url}");
    }
    Ok(body)
}

/// Discover every SKILL.md path in a repo (optionally under `gh.path`) via the
/// trees API against a specific `api_base` (no env lookup) — the testable core.
async fn discover_github_skills_on(
    api_base: &str,
    gh: &GithubRef,
    token: Option<&str>,
) -> Result<Vec<String>> {
    let url = trees_api_url_on(api_base, gh);
    let resp = github_get(&url, token, Some("application/vnd.github+json")).await?;
    if !resp.status().is_success() {
        bail!(
            "GitHub trees API returned HTTP {} for {url}",
            resp.status().as_u16()
        );
    }
    let body = resp
        .text()
        .await
        .context("reading the GitHub trees response")?;
    parse_trees_skill_paths(&body, gh.path.as_deref())
}

/// Resolve the set of SKILL.md file paths a GitHub spec refers to:
///   * an explicit `…/SKILL.md` path → just that file;
///   * a directory path → trees-API discovery under it, falling back to a direct
///     `<path>/SKILL.md` guess when the API is unavailable;
///   * no path → whole-repo discovery, falling back to a root `SKILL.md`.
async fn resolve_github_skill_paths(
    api_base: &str,
    gh: &GithubRef,
    token: Option<&str>,
) -> Result<Vec<String>> {
    if let Some(p) = &gh.path {
        if is_skill_md_path(p) {
            return Ok(vec![p.clone()]);
        }
        // Directory: prefer discovery (multi-skill), fall back to a direct guess.
        return match discover_github_skills_on(api_base, gh, token).await {
            Ok(found) if !found.is_empty() => Ok(found),
            _ => Ok(vec![format!("{}/SKILL.md", p.trim_end_matches('/'))]),
        };
    }
    // Whole repo: discovery is the path; fall back to a root SKILL.md on failure.
    match discover_github_skills_on(api_base, gh, token).await {
        Ok(found) if !found.is_empty() => Ok(found),
        _ => Ok(vec!["SKILL.md".to_string()]),
    }
}

/// Fetch + import every SKILL.md a GitHub spec resolves to, against explicit
/// bases (no env lookup) — the testable core of [`add_github`]. Skips any path
/// whose body isn't a valid SKILL.md; errors only when nothing valid imported.
async fn add_github_on(
    api_base: &str,
    raw_base: &str,
    gh: &GithubRef,
    token: Option<&str>,
    skills_dir: &Path,
) -> Result<Vec<ImportedSkill>> {
    let paths = resolve_github_skill_paths(api_base, gh, token).await?;
    let mut imported = Vec::new();
    let mut last_err: Option<anyhow::Error> = None;
    for fp in &paths {
        let url = github_raw_url_on(raw_base, gh, fp);
        let text = match fetch_github_text(&url, token).await {
            Ok(t) => t,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        // Only import bodies that are genuine SKILL.md files.
        let Some((name, description)) = crate::skills::parse_frontmatter(&text) else {
            continue;
        };
        let path = import(&text, skills_dir)?;
        imported.push(ImportedSkill {
            name,
            description,
            path,
        });
    }
    if imported.is_empty() {
        if let Some(e) = last_err {
            return Err(e.context(format!(
                "no importable SKILL.md found in github:{}/{}",
                gh.owner, gh.repo
            )));
        }
        bail!(
            "no importable SKILL.md found in github:{}/{}",
            gh.owner,
            gh.repo
        );
    }
    Ok(imported)
}

/// Fetch + import a GitHub skill spec into `skills_dir`, reading the API/raw
/// bases and the auth token from the environment.
pub async fn add_github(gh: &GithubRef, skills_dir: &Path) -> Result<Vec<ImportedSkill>> {
    add_github_on(
        &github_api_base(),
        &github_raw_base(),
        gh,
        github_token().as_deref(),
        skills_dir,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_and_shorthand() {
        let want = SkillRef {
            owner: "acme".into(),
            name: "git-helper".into(),
            version: None,
        };
        assert_eq!(
            parse_ref("https://skill.fish/acme/git-helper").unwrap(),
            want
        );
        assert_eq!(parse_ref("skill.fish/acme/git-helper").unwrap(), want);
        assert_eq!(parse_ref("acme/git-helper").unwrap(), want);
        assert_eq!(
            parse_ref("https://skill.fish/acme/git-helper/raw").unwrap(),
            want
        );
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
        let r = SkillRef {
            owner: "a".into(),
            name: "b".into(),
            version: Some("2".into()),
        };
        assert_eq!(
            raw_url_on("https://skill.fish", &r),
            "https://skill.fish/a/b/raw?version=2"
        );
        let r2 = SkillRef {
            owner: "a".into(),
            name: "b".into(),
            version: None,
        };
        assert_eq!(
            raw_url_on("https://skill.fish", &r2),
            "https://skill.fish/a/b/raw"
        );
        // A trailing slash on the base is normalized away.
        assert_eq!(
            raw_url_on("https://skill.fish/", &r2),
            "https://skill.fish/a/b/raw"
        );
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
    fn fetch_origin_ignores_file_override_uses_skillfish() {
        // No override (or a file:// override) must NOT be used as a fetch base —
        // the file index can't serve `/{owner}/{name}/raw`, so fetch falls back
        // to the live skill.fish origin. An http(s) override IS honored.
        // SAFETY: single-threaded test; var restored before returning.
        let prev = std::env::var("AISH_SKILL_REGISTRY").ok();
        unsafe { std::env::remove_var("AISH_SKILL_REGISTRY") };
        assert_eq!(fetch_origin(), "https://skill.fish");

        unsafe { std::env::set_var("AISH_SKILL_REGISTRY", "file:///tmp/index.json") };
        assert_eq!(fetch_origin(), "https://skill.fish");

        unsafe { std::env::set_var("AISH_SKILL_REGISTRY", "https://mirror.example") };
        assert_eq!(fetch_origin(), "https://mirror.example");

        match prev {
            Some(v) => unsafe { std::env::set_var("AISH_SKILL_REGISTRY", v) },
            None => unsafe { std::env::remove_var("AISH_SKILL_REGISTRY") },
        }
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
    async fn serve_once_with_headers(status_line: &str, extra_headers: &str, body: String) -> u16 {
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
        assert!(
            u.starts_with("https://skill.fish/api/v1/search?q="),
            "got {u}"
        );
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
        assert_eq!(
            print_results_table("foo", &[]),
            "No skills found for \"foo\"."
        );
    }

    #[test]
    fn print_results_table_lists_results() {
        let r = SearchResult {
            name: "git-helper".into(),
            author: "acme".into(),
            description: "Helps with git operations.".into(),
            version: "1.0.0".into(),
            reference: "acme/git-helper".into(),
            stars: 42,
        };
        let s = print_results_table("git", &[r]);
        assert!(s.contains("SKILL"));
        // The popularity column replaces the old VERSION column.
        assert!(s.contains("STARS"));
        assert!(s.contains("★ 42"));
        // The SKILL column shows the short author/name, not a long URL.
        assert!(s.contains("acme/git-helper"));
        assert!(s.contains("Helps with git operations."));
        assert!(s.contains("1 result(s)"));
        // The footer echoes the top hit as a copy-paste-ready fetch target.
        assert!(s.contains("--skill-fetch acme/git-helper"), "got: {s}");
    }

    #[test]
    fn print_results_table_uses_short_name_and_ranks_by_stars() {
        // A long GitHub URL reference collapses to `owner/skill`; the more-starred
        // row sorts first regardless of input order.
        let low = SearchResult {
            name: String::new(),
            author: String::new(),
            description: "Low stars.".into(),
            version: String::new(),
            reference:
                "https://github.com/openhands/skills/tree/abc123/skills/github".into(),
            stars: 3,
        };
        let high = SearchResult {
            name: String::new(),
            author: String::new(),
            description: "High stars.".into(),
            version: String::new(),
            reference:
                "https://github.com/clawdbot/clawdbot/tree/def456/skills/github".into(),
            stars: 99,
        };
        let s = print_results_table("github", &[low, high]);
        // Short names are derived from the URLs.
        assert!(s.contains("openhands/github"), "got: {s}");
        assert!(s.contains("clawdbot/github"), "got: {s}");
        // The 99-star row appears before the 3-star row (ranked by stars).
        let hi = s.find("clawdbot/github").unwrap();
        let lo = s.find("openhands/github").unwrap();
        assert!(hi < lo, "high-star row should lead:\n{s}");
    }

    #[test]
    fn short_name_from_url_distills_owner_and_skill() {
        // tree URL with a skills/ container → owner + leaf skill dir.
        assert_eq!(
            short_name_from_ref(
                "https://github.com/openhands/skills/tree/f5b98/skills/github"
            ),
            "openhands/github"
        );
        // blob URL ending in SKILL.md → strip the filename, take the dir.
        assert_eq!(
            short_name_from_ref(
                "https://github.com/acme/repo/blob/main/packs/git/SKILL.md"
            ),
            "acme/git"
        );
        // A bare owner/name ref passes through untouched.
        assert_eq!(short_name_from_ref("anthropic/github-pr"), "anthropic/github-pr");
        // A scheme-less github path also works.
        assert_eq!(
            short_name_from_ref("github.com/foo/bar/tree/main/skills/baz"),
            "foo/baz"
        );
    }

    #[test]
    fn search_result_short_name_prefers_author_and_name() {
        // When author + name are present they win over reference distillation.
        let r = SearchResult {
            name: "github".into(),
            author: "lycfyi".into(),
            description: String::new(),
            version: String::new(),
            reference: "https://github.com/lycfyi/repo/tree/abc/skills/github".into(),
            stars: 7,
        };
        assert_eq!(r.short_name(), "lycfyi/github");
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

    // ---- Phase 3: mcpmarket search --------------------------------------

    #[test]
    fn mcpmarket_search_url_is_well_formed() {
        let u = mcpmarket_search_url("https://mcpmarket.com", "git helper", 25);
        assert_eq!(
            u,
            "https://mcpmarket.com/api/search?q=git%20helper&type=skills&limit=25"
        );
        // A trailing slash on the base is normalized away (no `//api`).
        assert_eq!(
            mcpmarket_search_url("http://localhost:9/", "x", 50),
            "http://localhost:9/api/search?q=x&type=skills&limit=50"
        );
    }

    #[tokio::test]
    async fn mcpmarket_parses_real_skills_shape() {
        // The REAL mcpmarket shape: hits under `skills`, a NESTED `owner` object
        // (not a bare string), the identifier in `skill_name`/`slug`, and the
        // fetchable location in the `website` GitHub URL. The generic
        // parse_search_body silently dropped every such row because the nested
        // owner object failed to deserialize into the String `author` field —
        // this is the regression parse_mcpmarket_body fixes.
        let body = r#"{"skills":[
            {"id":29062,"name":"Discord Doctor","slug":"discord-doctor",
             "owner":{"url":"https://github.com/lycfyi","name":"lycfyi"},
             "github":"lycfyi/community-agent-plugin/discord-connector/discord-doctor",
             "website":"https://github.com/lycfyi/community-agent-plugin/tree/abc123/plugins/discord-connector/skills/discord-doctor",
             "raw_name":"discord-doctor","skill_name":"discord-doctor",
             "description":"Diagnoses Discord configuration issues."}
        ]}"#;
        let port = serve_once("200 OK", body.to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        let got = search_mcpmarket_with_base(&base, "discord", 50)
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "nested-owner row was dropped");
        assert_eq!(got[0].name, "discord-doctor");
        assert_eq!(got[0].author, "lycfyi");
        assert_eq!(got[0].description, "Diagnoses Discord configuration issues.");
        // The fetchable GitHub website URL becomes the reference.
        assert_eq!(
            got[0].ref_or_synth(),
            "https://github.com/lycfyi/community-agent-plugin/tree/abc123/plugins/discord-connector/skills/discord-doctor"
        );
    }

    #[tokio::test]
    async fn mcpmarket_parses_github_stars() {
        // The popularity signal `github_stars` is read onto SearchResult.stars.
        let body = r#"{"skills":[
            {"skill_name":"github","owner":{"name":"clawdbot"},
             "website":"https://github.com/clawdbot/clawdbot/tree/abc/skills/github",
             "description":"GitHub ops.","github_stars":123}
        ]}"#;
        let port = serve_once("200 OK", body.to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        let got = search_mcpmarket_with_base(&base, "github", 50).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].stars, 123);
        assert_eq!(got[0].short_name(), "clawdbot/github");
    }

    #[tokio::test]
    async fn mcpmarket_falls_back_to_flat_fields() {
        // Forward-compat: a flat shape (publisher/summary, no owner object, no
        // website) still maps, synthesizing the reference from author/name.
        let body = r#"{"results":[
            {"skill_name":"git-helper","publisher":"acme","summary":"Helps with git.","version":"2.0.0"}
        ]}"#;
        let port = serve_once("200 OK", body.to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        let got = search_mcpmarket_with_base(&base, "git", 50).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].author, "acme");
        assert_eq!(got[0].description, "Helps with git.");
        assert_eq!(got[0].ref_or_synth(), "acme/git-helper");
        assert_eq!(got[0].version, "2.0.0");
    }

    #[tokio::test]
    async fn mcpmarket_http_error_propagates() {
        let port = serve_once("503 Service Unavailable", "down".to_string()).await;
        let base = format!("http://127.0.0.1:{port}");
        let err = search_mcpmarket_with_base(&base, "x", 50)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("mcpmarket"), "{err}");
    }

    #[test]
    fn merge_results_dedups_fallback_against_primary() {
        let mk = |r: &str| SearchResult {
            name: String::new(),
            author: String::new(),
            description: String::new(),
            version: String::new(),
            reference: r.into(),
            stars: 0,
        };
        let primary = vec![mk("a/one"), mk("a/two")];
        let fallback = vec![mk("a/two"), mk("a/three")];
        let merged = merge_results(primary, fallback);
        let refs: Vec<String> = merged.iter().map(|r| r.ref_or_synth()).collect();
        // Primary order preserved; only the new fallback ref is appended.
        assert_eq!(refs, vec!["a/one", "a/two", "a/three"]);
    }

    // ---- Phase 3: GitHub spec parsing -----------------------------------

    #[test]
    fn parses_github_prefix_specs() {
        // bare repo
        assert_eq!(
            parse_github_ref("github:acme/skills").unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "HEAD".into(),
                path: None,
            }
        );
        // gh: alias + sub-path
        assert_eq!(
            parse_github_ref("gh:acme/skills/packs/git").unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "HEAD".into(),
                path: Some("packs/git".into()),
            }
        );
        // @ref pin + .git suffix stripped
        assert_eq!(
            parse_github_ref("github:acme/skills.git/packs/git@v1.2.0").unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "v1.2.0".into(),
                path: Some("packs/git".into()),
            }
        );
        // branch name with a slash survives via @ref
        assert_eq!(
            parse_github_ref("github:acme/skills@release/v2").unwrap().git_ref,
            "release/v2"
        );
    }

    #[test]
    fn parses_github_url_specs() {
        // plain repo URL
        assert_eq!(
            parse_github_ref("https://github.com/acme/skills").unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "HEAD".into(),
                path: None,
            }
        );
        // /tree/<ref>/<path>
        assert_eq!(
            parse_github_ref("https://github.com/acme/skills/tree/main/packs/git").unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "main".into(),
                path: Some("packs/git".into()),
            }
        );
        // /blob/<ref>/<path>/SKILL.md
        assert_eq!(
            parse_github_ref("https://github.com/acme/skills/blob/main/packs/git/SKILL.md")
                .unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "main".into(),
                path: Some("packs/git/SKILL.md".into()),
            }
        );
        // scheme-less github.com/...
        assert_eq!(
            parse_github_ref("github.com/acme/skills").unwrap().repo,
            "skills"
        );
    }

    #[test]
    fn parses_raw_githubusercontent_urls() {
        // The "Raw" button URL / what `curl` fetches: the ref is the 3rd path
        // segment (no tree/blob marker) and the remainder is the file path.
        assert_eq!(
            parse_github_ref(
                "https://raw.githubusercontent.com/acme/skills/main/packs/git/SKILL.md"
            )
            .unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "main".into(),
                path: Some("packs/git/SKILL.md".into()),
            }
        );
        // A 40-char commit SHA as the ref + a deep path (the omniclaude shape).
        assert_eq!(
            parse_github_ref(
                "https://raw.githubusercontent.com/omninode-ai/omniclaude/b60e95ca064e0008987791f29c5cf730cb5dbf21/plugins/onex/skills/unstick_queue/SKILL.md"
            )
            .unwrap(),
            GithubRef {
                owner: "omninode-ai".into(),
                repo: "omniclaude".into(),
                git_ref: "b60e95ca064e0008987791f29c5cf730cb5dbf21".into(),
                path: Some("plugins/onex/skills/unstick_queue/SKILL.md".into()),
            }
        );
        // scheme-less host + a `?token=…` query is stripped.
        assert_eq!(
            parse_github_ref("raw.githubusercontent.com/acme/skills/v1/SKILL.md?token=abc")
                .unwrap(),
            GithubRef {
                owner: "acme".into(),
                repo: "skills".into(),
                git_ref: "v1".into(),
                path: Some("SKILL.md".into()),
            }
        );
        // Too short (no ref) and traversal in the path are rejected.
        assert!(parse_github_ref("https://raw.githubusercontent.com/acme/skills").is_none());
        assert!(
            parse_github_ref("https://raw.githubusercontent.com/acme/skills/main/../etc/x")
                .is_none()
        );
    }

    #[test]
    fn non_github_and_unsafe_specs_return_none() {
        // Not a GitHub spec → None (caller falls back to skill.fish parse_ref).
        assert!(parse_github_ref("acme/git-helper").is_none());
        assert!(parse_github_ref("https://skill.fish/acme/git").is_none());
        // Missing repo segment.
        assert!(parse_github_ref("github:acme").is_none());
        // Traversal / unsafe segments are rejected.
        assert!(parse_github_ref("github:acme/skills/../etc").is_none());
        assert!(parse_github_ref("github:../evil/skills").is_none());
        // `/tree` with no ref is malformed.
        assert!(parse_github_ref("https://github.com/acme/skills/tree").is_none());
    }

    #[test]
    fn github_url_builders_are_well_formed() {
        let gh = GithubRef {
            owner: "acme".into(),
            repo: "skills".into(),
            git_ref: "main".into(),
            path: Some("packs/git".into()),
        };
        assert_eq!(
            github_raw_url_on("https://raw.githubusercontent.com", &gh, "packs/git/SKILL.md"),
            "https://raw.githubusercontent.com/acme/skills/main/packs/git/SKILL.md"
        );
        assert_eq!(
            trees_api_url_on("https://api.github.com", &gh),
            "https://api.github.com/repos/acme/skills/git/trees/main?recursive=1"
        );
        // Trailing slashes on the base are normalized away.
        assert_eq!(
            github_raw_url_on("https://raw.githubusercontent.com/", &gh, "/a/SKILL.md"),
            "https://raw.githubusercontent.com/acme/skills/main/a/SKILL.md"
        );
    }

    #[test]
    fn skill_md_path_helpers() {
        assert!(is_skill_md_path("SKILL.md"));
        assert!(is_skill_md_path("packs/git/SKILL.md"));
        assert!(!is_skill_md_path("packs/git/README.md"));
        assert!(!is_skill_md_path("SKILL.md.bak"));
        // A directory prefix matches its own SKILL.md and deeper ones.
        assert!(path_under_prefix("packs/git/SKILL.md", "packs/git"));
        assert!(path_under_prefix("packs/git/sub/SKILL.md", "packs/git"));
        assert!(!path_under_prefix("packs/other/SKILL.md", "packs/git"));
        // A SKILL.md prefix matches only that exact file.
        assert!(path_under_prefix("packs/git/SKILL.md", "packs/git/SKILL.md"));
        assert!(!path_under_prefix("packs/git/sub/SKILL.md", "packs/git/SKILL.md"));
    }

    #[test]
    fn parse_trees_extracts_and_filters_skill_paths() {
        // A realistic trees-API body: two skills, a non-skill blob, a tree node.
        let body = r#"{
            "tree": [
                {"path":"README.md","type":"blob"},
                {"path":"packs","type":"tree"},
                {"path":"packs/alpha/SKILL.md","type":"blob"},
                {"path":"packs/beta/SKILL.md","type":"blob"},
                {"path":"packs/beta/reference.md","type":"blob"}
            ],
            "truncated": false
        }"#;
        // No prefix → both skills, sorted.
        let all = parse_trees_skill_paths(body, None).unwrap();
        assert_eq!(all, vec!["packs/alpha/SKILL.md", "packs/beta/SKILL.md"]);
        // Prefix narrows to one.
        let one = parse_trees_skill_paths(body, Some("packs/alpha")).unwrap();
        assert_eq!(one, vec!["packs/alpha/SKILL.md"]);
    }

    // ---- Phase 3: GitHub discovery + import (loopback) -------------------

    /// A multi-request loopback HTTP server. Each accepted connection is matched
    /// against `routes` by the request's path (query string stripped); the first
    /// route whose key equals the path wins. Unmatched paths get a 404. Runs for
    /// the lifetime of the test (the task is dropped when the test returns).
    /// Returns the bound port. Captures the Authorization header of the LAST
    /// request into `auth_sink` so a test can assert the token was forwarded.
    async fn serve_router(
        routes: Vec<(String, String)>,
        auth_sink: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                let auth_sink = auth_sink.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let target = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .split('?')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    // Record any Authorization header seen (header names are
                    // case-insensitive; hyper emits them lower-cased on the wire).
                    for line in req.lines() {
                        if let Some((name, value)) = line.split_once(':') {
                            if name.trim().eq_ignore_ascii_case("authorization") {
                                auth_sink.lock().unwrap().push(value.trim().to_string());
                            }
                        }
                    }
                    let (status, body) = match routes.iter().find(|(p, _)| *p == target) {
                        Some((_, b)) => ("200 OK", b.clone()),
                        None => ("404 Not Found", String::new()),
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        port
    }

    fn skill_md(name: &str) -> String {
        format!("---\nname: {name}\ndescription: GitHub skill {name}.\n---\nBody for {name}.\n")
    }

    #[tokio::test]
    async fn github_multi_skill_discovery_imports_all() {
        // The repo has two skills; whole-repo discovery imports both. The API
        // base and the raw base point at the SAME loopback router, distinguished
        // by path (`/repos/...` vs `/owner/repo/ref/...`).
        let trees = r#"{"tree":[
            {"path":"README.md","type":"blob"},
            {"path":"packs/alpha/SKILL.md","type":"blob"},
            {"path":"packs/beta/SKILL.md","type":"blob"}
        ]}"#;
        let routes = vec![
            (
                "/repos/acme/skills/git/trees/HEAD".to_string(),
                trees.to_string(),
            ),
            (
                "/acme/skills/HEAD/packs/alpha/SKILL.md".to_string(),
                skill_md("alpha"),
            ),
            (
                "/acme/skills/HEAD/packs/beta/SKILL.md".to_string(),
                skill_md("beta"),
            ),
        ];
        let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = serve_router(routes, auth.clone()).await;
        let base = format!("http://127.0.0.1:{port}");
        let gh = GithubRef {
            owner: "acme".into(),
            repo: "skills".into(),
            git_ref: "HEAD".into(),
            path: None,
        };
        let tmp = std::env::temp_dir().join(format!("aish-gh-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let imported = add_github_on(&base, &base, &gh, Some("secret-token"), &tmp)
            .await
            .unwrap();
        let mut names: Vec<String> = imported.iter().map(|s| s.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
        // Both skills landed in the catalog.
        assert_eq!(crate::skills::load(&tmp).len(), 2);
        // The Bearer token was forwarded on the GitHub requests.
        let seen = auth.lock().unwrap().clone();
        assert!(
            seen.iter().any(|h| h == "Bearer secret-token"),
            "token not forwarded: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn github_path_resolves_single_skill_md() {
        // A spec that names a SKILL.md directly skips the API and fetches it.
        let routes = vec![(
            "/acme/skills/main/packs/git/SKILL.md".to_string(),
            skill_md("git-pack"),
        )];
        let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = serve_router(routes, auth).await;
        let base = format!("http://127.0.0.1:{port}");
        let gh = parse_github_ref("https://github.com/acme/skills/blob/main/packs/git/SKILL.md")
            .unwrap();
        let tmp = std::env::temp_dir().join(format!("aish-gh-single-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // No token needed for a public file.
        let imported = add_github_on(&base, &base, &gh, None, &tmp).await.unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].name, "git-pack");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn github_path_prefix_filters_discovery() {
        // Discovery under a directory prefix imports only that sub-tree.
        let trees = r#"{"tree":[
            {"path":"packs/alpha/SKILL.md","type":"blob"},
            {"path":"packs/beta/SKILL.md","type":"blob"}
        ]}"#;
        let routes = vec![
            (
                "/repos/acme/skills/git/trees/HEAD".to_string(),
                trees.to_string(),
            ),
            (
                "/acme/skills/HEAD/packs/alpha/SKILL.md".to_string(),
                skill_md("alpha"),
            ),
            (
                "/acme/skills/HEAD/packs/beta/SKILL.md".to_string(),
                skill_md("beta"),
            ),
        ];
        let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = serve_router(routes, auth).await;
        let base = format!("http://127.0.0.1:{port}");
        let gh = GithubRef {
            owner: "acme".into(),
            repo: "skills".into(),
            git_ref: "HEAD".into(),
            path: Some("packs/alpha".into()),
        };
        let tmp = std::env::temp_dir().join(format!("aish-gh-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let imported = add_github_on(&base, &base, &gh, None, &tmp).await.unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].name, "alpha");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn github_directory_falls_back_to_direct_when_api_unavailable() {
        // The trees API 404s (no route), so a directory spec falls back to a
        // direct `<path>/SKILL.md` raw fetch.
        let routes = vec![(
            "/acme/skills/HEAD/packs/git/SKILL.md".to_string(),
            skill_md("git-pack"),
        )];
        let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = serve_router(routes, auth).await;
        let base = format!("http://127.0.0.1:{port}");
        let gh = GithubRef {
            owner: "acme".into(),
            repo: "skills".into(),
            git_ref: "HEAD".into(),
            path: Some("packs/git".into()),
        };
        let tmp = std::env::temp_dir().join(format!("aish-gh-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let imported = add_github_on(&base, &base, &gh, None, &tmp).await.unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].name, "git-pack");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn github_no_skill_found_errors() {
        // An empty repo (no SKILL.md anywhere, root fallback 404s) errors.
        let trees = r#"{"tree":[{"path":"README.md","type":"blob"}]}"#;
        let routes = vec![(
            "/repos/acme/empty/git/trees/HEAD".to_string(),
            trees.to_string(),
        )];
        let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = serve_router(routes, auth).await;
        let base = format!("http://127.0.0.1:{port}");
        let gh = GithubRef {
            owner: "acme".into(),
            repo: "empty".into(),
            git_ref: "HEAD".into(),
            path: None,
        };
        let tmp = std::env::temp_dir().join(format!("aish-gh-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        assert!(add_github_on(&base, &base, &gh, None, &tmp).await.is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn add_routes_github_specs_through_discovery() {
        // The unified `add()` entry point: a github: spec resolves via the
        // GitHub path (env-overridable bases), not skill.fish parse_ref.
        let trees = r#"{"tree":[{"path":"SKILL.md","type":"blob"}]}"#;
        let routes = vec![
            (
                "/repos/acme/solo/git/trees/HEAD".to_string(),
                trees.to_string(),
            ),
            ("/acme/solo/HEAD/SKILL.md".to_string(), skill_md("solo")),
        ];
        let auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = serve_router(routes, auth).await;
        let base = format!("http://127.0.0.1:{port}");

        // SAFETY: this test is the only one that sets these vars; serialize via
        // the dedicated names so it doesn't collide with the base-taking tests.
        // SAFETY: single-threaded within this test; restored at the end.
        unsafe {
            std::env::set_var("AISH_GITHUB_API_BASE", &base);
            std::env::set_var("AISH_GITHUB_RAW_BASE", &base);
        }
        let tmp = std::env::temp_dir().join(format!("aish-gh-add-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let imported = add("github:acme/solo", &tmp).await.unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].name, "solo");

        unsafe {
            std::env::remove_var("AISH_GITHUB_API_BASE");
            std::env::remove_var("AISH_GITHUB_RAW_BASE");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
