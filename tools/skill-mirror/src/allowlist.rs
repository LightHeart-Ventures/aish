//! The allowlist — the human-reviewed set of GitHub repositories ingest is
//! permitted to crawl (TASK-695).
//!
//! Ingest **never** discovers repos on its own. Every crawled repo is listed in
//! a checked-in `allowlist.toml`, so adding a source is a reviewable pull
//! request and the nightly job's API budget is bounded by a number a human
//! chose. That is the whole security model of the mirror: a compromised or
//! spammy repo cannot enter the catalog without a merge.
//!
//! Two spellings, both deterministic, both order-preserving:
//!
//! ```toml
//! # 1. compact — one slug per line, for the common "just crawl HEAD" case
//! seeds = [
//!   "anthropics/skills",
//!   "obra/superpowers",
//! ]
//!
//! # 2. table — when a repo needs a pinned ref or a path prefix
//! [[repo]]
//! owner = "hyperb1iss"
//! repo  = "hyperskills"
//! ref   = "main"      # optional, default: the repo's default branch
//! path  = "skills"    # optional, only look under this prefix
//! ```
//!
//! This is a deliberately small hand-rolled parser for exactly that subset —
//! the crate does not take a TOML dependency for one config file, and a strict
//! parser that rejects anything it does not understand is safer here than a
//! permissive one that silently ignores a typo'd key.

use anyhow::{Result, bail};
use std::collections::BTreeSet;

/// One allowlisted repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEntry {
    /// GitHub owner (user or org).
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Optional pinned git ref. `None` ⇒ the repo's default branch.
    pub git_ref: Option<String>,
    /// Optional path prefix: only SKILL.md files under it are considered.
    pub path: Option<String>,
}

impl RepoEntry {
    /// `owner/repo` — the state-file key and log identity.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn from_slug(slug: &str) -> Result<Self> {
        let mut parts = slug.splitn(2, '/');
        let owner = parts.next().unwrap_or_default().trim();
        let repo = parts.next().unwrap_or_default().trim();
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            bail!("`{slug}` is not an `owner/repo` slug");
        }
        Ok(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            git_ref: None,
            path: None,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.owner.is_empty() {
            bail!("repo entry is missing `owner`");
        }
        if self.repo.is_empty() {
            bail!("repo entry `{}` is missing `repo`", self.owner);
        }
        for (field, v) in [("owner", &self.owner), ("repo", &self.repo)] {
            if v.contains('/') || v.contains("..") || v.starts_with('.') {
                bail!("`{field} = \"{v}\"` is not a single safe path segment");
            }
        }
        if let Some(p) = &self.path
            && (p.starts_with('/') || p.contains(".."))
        {
            bail!("`path = \"{p}\"` must be a relative prefix without `..`");
        }
        Ok(())
    }
}

/// Parse an allowlist document.
///
/// Returns entries in file order with duplicate slugs rejected — a duplicate is
/// a review mistake, and silently collapsing it would hide the fact that two
/// reviewers added the same source with different refs.
pub fn parse(text: &str) -> Result<Vec<RepoEntry>> {
    let mut entries: Vec<RepoEntry> = Vec::new();
    let mut current: Option<RepoEntry> = None;
    // Multi-line `seeds = [ … ]` accumulation.
    let mut in_seeds = false;

    for (lineno, raw_line) in text.lines().enumerate() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        let ctx = |e: anyhow::Error| anyhow::anyhow!("allowlist line {}: {e:#}", lineno + 1);

        if in_seeds {
            for slug in quoted_strings(line) {
                entries.push(RepoEntry::from_slug(&slug).map_err(ctx)?);
            }
            if line.contains(']') {
                in_seeds = false;
            }
            continue;
        }

        if line == "[[repo]]" {
            if let Some(e) = current.take() {
                e.validate().map_err(&ctx)?;
                entries.push(e);
            }
            current = Some(RepoEntry {
                owner: String::new(),
                repo: String::new(),
                git_ref: None,
                path: None,
            });
            continue;
        }
        if line.starts_with('[') {
            bail!("allowlist line {}: unknown table `{line}`", lineno + 1);
        }

        let Some((key, rest)) = line.split_once('=') else {
            bail!("allowlist line {}: expected `key = value`", lineno + 1);
        };
        let key = key.trim();
        let rest = rest.trim();

        if key == "seeds" {
            if current.is_some() {
                bail!(
                    "allowlist line {}: `seeds` must appear before any [[repo]] table",
                    lineno + 1
                );
            }
            for slug in quoted_strings(rest) {
                entries.push(RepoEntry::from_slug(&slug).map_err(&ctx)?);
            }
            // Open array: keep consuming until the closing bracket.
            in_seeds = rest.contains('[') && !rest.contains(']');
            continue;
        }

        let Some(entry) = current.as_mut() else {
            bail!(
                "allowlist line {}: `{key}` outside a [[repo]] table",
                lineno + 1
            );
        };
        let value = unquote(rest).ok_or_else(|| {
            anyhow::anyhow!(
                "allowlist line {}: `{key}` value must be a quoted string",
                lineno + 1
            )
        })?;
        match key {
            "owner" => entry.owner = value,
            "repo" => entry.repo = value,
            "ref" => entry.git_ref = Some(value),
            "path" => entry.path = Some(value.trim_matches('/').to_string()),
            // `notes` is for reviewers; anything else is a typo we refuse to ignore.
            "notes" => {}
            other => bail!("allowlist line {}: unknown key `{other}`", lineno + 1),
        }
    }

    if let Some(e) = current.take() {
        e.validate()?;
        entries.push(e);
    }
    if entries.is_empty() {
        bail!("allowlist contains no repositories");
    }

    let mut seen = BTreeSet::new();
    for e in &entries {
        if !seen.insert(e.slug()) {
            bail!("allowlist lists `{}` twice", e.slug());
        }
    }
    Ok(entries)
}

/// Drop a trailing `#` comment, ignoring `#` inside double quotes.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Every double-quoted string in `s`, in order.
fn quoted_strings(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    for c in s.chars() {
        match (c, &mut cur) {
            ('"', None) => cur = Some(String::new()),
            ('"', Some(_)) => {
                if let Some(v) = cur.take() {
                    let v = v.trim().to_string();
                    if !v.is_empty() {
                        out.push(v);
                    }
                }
            }
            (c, Some(buf)) => buf.push(c),
            _ => {}
        }
    }
    out
}

/// `"value"` ⇒ `value`; anything unquoted ⇒ `None`.
fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seeds_array_inline_and_multiline() {
        let doc = r#"
# a comment
seeds = [
  "anthropics/skills",   # hub
  "obra/superpowers",
]
"#;
        let got = parse(doc).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].slug(), "anthropics/skills");
        assert_eq!(got[1].slug(), "obra/superpowers");
        assert!(got[0].git_ref.is_none());

        let inline = parse(r#"seeds = ["a/b", "c/d"]"#).unwrap();
        assert_eq!(inline.len(), 2);
        assert_eq!(inline[1].slug(), "c/d");
    }

    #[test]
    fn parses_repo_tables_with_ref_and_path() {
        let doc = r#"
[[repo]]
owner = "hyperb1iss"
repo  = "hyperskills"
ref   = "main"
path  = "/skills/"
notes = "ignored by the parser"

[[repo]]
owner = "anthropics"
repo = "skills"
"#;
        let got = parse(doc).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].git_ref.as_deref(), Some("main"));
        assert_eq!(got[0].path.as_deref(), Some("skills"));
        assert_eq!(got[1].slug(), "anthropics/skills");
        assert!(got[1].path.is_none());
    }

    #[test]
    fn rejects_bad_documents() {
        // unknown key — a typo must not be silently ignored
        assert!(parse("[[repo]]\nowner=\"a\"\nrepo=\"b\"\nbranch=\"main\"").is_err());
        // key outside a table
        assert!(parse("owner = \"a\"").is_err());
        // duplicate slug
        assert!(parse("seeds = [\"a/b\", \"a/b\"]").is_err());
        // not a slug
        assert!(parse("seeds = [\"nope\"]").is_err());
        // path traversal
        assert!(parse("[[repo]]\nowner=\"a\"\nrepo=\"b\"\npath=\"../x\"").is_err());
        // empty document
        assert!(parse("# nothing here\n").is_err());
        // unquoted value
        assert!(parse("[[repo]]\nowner = a").is_err());
    }

    #[test]
    fn comments_inside_quotes_survive() {
        let got = parse("[[repo]]\nowner = \"a\"\nrepo = \"b#c\"\n").unwrap();
        assert_eq!(got[0].repo, "b#c");
    }
}
