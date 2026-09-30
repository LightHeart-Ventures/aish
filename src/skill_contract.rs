//! Skill-catalog **contract** — the single source of truth for the rules a
//! SKILL.md and a registry `index.json` must satisfy, shared by the aish client
//! and the out-of-tree mirror generator (`tools/skill-mirror`).
//!
//! Why this module exists (TASK-694): the mirror that *publishes* a catalog and
//! the client that *consumes* it must apply byte-identical rules, otherwise the
//! mirror ships skills the client then refuses to install. Reimplementing the
//! frontmatter parser or the path-segment hardening in a second crate
//! guarantees that drift. So the rules live here, in the aish crate's `lib`
//! target, and both sides call the same functions:
//!
//!   * `src/skills.rs` and `src/skill_provider.rs` re-export / delegate to them,
//!     so the binary's behaviour is unchanged.
//!   * `tools/skill-mirror` depends on the `aish` lib and calls them directly.
//!
//! Everything here is pure (no I/O beyond an explicit file read in
//! [`search_file_index`], no env lookups, no network), which is what makes it
//! safe to expose from a lib target without dragging the rest of the crate in.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::Path;

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

/// Pull `name:` and `description:` out of a `---`-fenced frontmatter block.
/// Single-line values only — that's what the convention uses in practice.
/// Shared with the skill.fish importer, which validates fetched SKILL.md files,
/// and with the mirror generator, which builds catalog rows from the same pair.
pub fn parse_frontmatter(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let mut name = None;
    let mut description = None;
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().to_string());
        }
    }
    Some((name?, description?))
}

/// Pull an arbitrary single-line scalar field out of the same `---`-fenced
/// frontmatter block [`parse_frontmatter`] reads, e.g. `version:`. Returns
/// `None` when there is no frontmatter or the key is absent; the value is
/// trimmed and a single pair of surrounding quotes is stripped.
///
/// This is the generalisation of the two hard-coded keys above — the catalog
/// generator needs `version:` and it must obey exactly the same fence rules, so
/// the convention stays in one file rather than being re-derived downstream.
pub fn frontmatter_field(text: &str, key: &str) -> Option<String> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let prefix = format!("{key}:");
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix(&prefix) {
            let t = v.trim();
            let t = t
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .or_else(|| t.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
                .unwrap_or(t);
            return Some(t.trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Path-segment hardening
// ---------------------------------------------------------------------------

/// Turn a SKILL.md frontmatter `name:` into a filesystem-safe directory
/// segment. The on-disk directory name is just storage — the catalog displays
/// the real `name:` from the frontmatter (see `skills::load`, which re-parses
/// each SKILL.md), so a human-readable name with spaces or punctuation (e.g.
/// `Neon Automation`) need not be a valid path itself: we slugify it. Every
/// char outside `[A-Za-z0-9._-]` becomes `-`, runs of `-` collapse, and
/// leading/trailing `-`/`.` are trimmed. This doubles as a hard path-traversal
/// guard — `/` and `\` can't survive the slug, so the result can never escape
/// the skills dir. Errors only when nothing usable remains (an empty slug, or
/// `.`/`..`).
pub fn sanitize_dir_segment(name: &str) -> Result<String> {
    let mut slug = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.') {
            slug.push(c);
            prev_dash = false;
        } else if !prev_dash {
            // Any other char (space, '/', '\\', punctuation, non-ASCII) becomes
            // a single '-', collapsing consecutive separators.
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches(|c| c == '-' || c == '.').to_string();
    if slug.is_empty() || slug == "." || slug == ".." {
        bail!("SKILL.md frontmatter `name:` has no filesystem-safe characters: {name:?}");
    }
    Ok(slug)
}

/// Reject path segments that could escape the skills dir or carry odd chars —
/// used to validate `owner`/`name` parsed from an untrusted registry ref, where
/// the value must be exact (not slugified). A SKILL.md frontmatter `name:` that
/// becomes a directory goes through [`sanitize_dir_segment`] instead.
pub fn validate_segment(s: &str) -> Result<()> {
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

// ---------------------------------------------------------------------------
// Catalog rows
// ---------------------------------------------------------------------------

/// One entry in a registry search response. Every field is optional on the
/// wire (the registry may omit a version or description), so each carries a
/// serde default; `author` also accepts the `owner` key and `reference` the
/// `ref`/`slug` keys, matching the shapes the registry has used in practice.
///
/// `Serialize` is derived as well so the mirror generator emits *this* type —
/// the row the client will parse back — rather than a hand-rolled look-alike.
/// Serialization is field-order stable (declaration order) and emits every
/// field unconditionally, which is what makes a generated `index.json`
/// byte-stable across reruns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// the readable counterpart to [`SearchResult::ref_or_synth`], which often
    /// holds a long `https://github.com/owner/repo/tree/<sha>/path/skill` URL
    /// that's painful to scan. Prefers the explicit `author` + `name`; when
    /// those are missing it distills a short name out of the reference: for a
    /// GitHub tree/blob URL it takes the repo owner and the leaf skill directory
    /// (e.g. `openhands/skills/tree/<sha>/skills/github` → `openhands/github`);
    /// for a bare `owner/name` ref it passes through unchanged.
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
pub fn short_name_from_ref(reference: &str) -> String {
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
            while tail.len() > 1 && matches!(tail.last().copied(), Some("skills") | Some("skill")) {
                tail.pop();
            }
            match tail.last() {
                Some(skill) => format!("{owner}/{skill}"),
                None => owner.to_string(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Index parsing / filtering — the `file://` registry read path
// ---------------------------------------------------------------------------

/// Parse a search response body into a deduped list of results. Accepts either
/// a bare JSON array or an object wrapping the array under `results`/`skills`/
/// `data` (the registry shape isn't contractually fixed, so we're liberal).
/// Unparsable entries are skipped; duplicates (by reference) are dropped, keeping
/// the first. An empty list is a valid result, never an error.
pub fn parse_search_body(body: &str) -> Result<Vec<SearchResult>> {
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

/// Filter a locally-read catalog by a case-insensitive substring match on the
/// skill name, reference, author, or description — the offline equivalent of a
/// remote registry's `/api/v1/search?q=` filter. An empty query returns the
/// whole catalog.
pub fn filter_local(results: Vec<SearchResult>, query: &str) -> Vec<SearchResult> {
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

/// Read a local `index.json` catalog and filter it for `query` — the exact
/// in-process body of `skill_provider::search_with_base` when the configured
/// registry base is a `file://` URI. `search_with_base` delegates here after
/// converting the URL to a path, so a caller that exercises this function is
/// exercising the real client read path (that is what makes a generated
/// `index.json` conformance-testable without standing up a server).
pub fn search_file_index(path: &Path, query: &str) -> Result<Vec<SearchResult>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading local registry index {}", path.display()))?;
    Ok(filter_local(parse_search_body(&body)?, query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_field_reads_version_and_strips_quotes() {
        let md = "---\nname: demo\ndescription: d\nversion: \"1.2.3\"\n---\nbody\n";
        assert_eq!(frontmatter_field(md, "version").as_deref(), Some("1.2.3"));
        assert_eq!(frontmatter_field(md, "nope"), None);
        assert_eq!(frontmatter_field("no frontmatter", "version"), None);
    }

    #[test]
    fn validate_segment_rejects_traversal() {
        assert!(validate_segment("good-name_1.0").is_ok());
        assert!(validate_segment("..").is_err());
        assert!(validate_segment("../../etc/passwd").is_err());
        assert!(validate_segment("").is_err());
    }

    #[test]
    fn search_result_round_trips_through_json() {
        let row = SearchResult {
            name: "demo".into(),
            author: "acme".into(),
            description: "a demo".into(),
            version: String::new(),
            reference: "acme/demo".into(),
            stars: 0,
        };
        let body = serde_json::to_string(&std::slice::from_ref(&row)).unwrap();
        let back = parse_search_body(&body).unwrap();
        assert_eq!(back, vec![row]);
    }
}
