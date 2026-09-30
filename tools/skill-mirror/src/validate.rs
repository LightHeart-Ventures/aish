//! Per-skill validation — every rule a SKILL.md must satisfy to earn a catalog
//! row, applied with the **client's own functions** so the mirror can never
//! publish a skill `:skill add` would then reject.
//!
//! | Rule | Source of truth |
//! |---|---|
//! | frontmatter present, `name:` + `description:` non-empty | `aish::skill_contract::parse_frontmatter` |
//! | `owner` / `name` are safe, exact path segments | `aish::skill_contract::validate_segment` |
//! | `name:` yields a usable on-disk directory segment | `aish::skill_contract::sanitize_dir_segment` |
//! | file ≤ `--max-size` (default 256 KiB) | this module (DoS / cost bound) |
//! | valid UTF-8 | this module (serde requirement) |
//!
//! `reference` uniqueness is a catalog-level rule and lives in [`crate::catalog`].

use aish::skill_contract::{
    SearchResult, frontmatter_field, parse_frontmatter, sanitize_dir_segment, validate_segment,
};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// A SKILL.md that passed every rule, plus everything needed to emit it.
#[derive(Debug, Clone)]
pub struct ValidatedSkill {
    /// Repo owner — the `{owner}` path segment of the input tree. Becomes the
    /// row's `author` and the first segment of `reference`.
    pub owner: String,
    /// The catalog row the client will parse back out of `index.json`.
    pub row: SearchResult,
    /// Verbatim SKILL.md bytes, emitted unchanged at `{owner}/{name}/raw`.
    pub raw: Vec<u8>,
}

impl ValidatedSkill {
    /// `{owner}/{name}` — the `reference` a user pastes into `:skill add`, and
    /// the path prefix of the raw object. Round-trips through the client's ref
    /// parser because both segments passed [`validate_segment`].
    pub fn reference(&self) -> &str {
        &self.row.reference
    }
}

/// Validate one SKILL.md found at `path` under repo owner `owner`.
///
/// `stars` is the popularity signal handed over by the ingest step (TASK-695).
/// Until ingest exists, the seam is an optional plain-text `stars` file next to
/// the SKILL.md; absent or unparsable ⇒ 0, per the spec's "else 0".
pub fn validate_skill(owner: &str, path: &Path, max_size: u64) -> Result<ValidatedSkill> {
    // --- size cap (checked before reading, so a huge file is never buffered) --
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() > max_size {
        bail!(
            "SKILL.md is {} bytes, over the {max_size}-byte cap",
            meta.len()
        );
    }

    // --- bytes + UTF-8 -----------------------------------------------------
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = std::str::from_utf8(&raw).context("SKILL.md is not valid UTF-8")?;

    // --- frontmatter (client's parser) -------------------------------------
    let Some((name, description)) = parse_frontmatter(text) else {
        bail!("no `---`-fenced frontmatter with both `name:` and `description:`");
    };
    if name.trim().is_empty() {
        bail!("frontmatter `name:` is empty");
    }
    if description.trim().is_empty() {
        bail!("frontmatter `description:` is empty");
    }

    // --- path-segment hardening (client's validators) -----------------------
    // `owner` and `name` are used EXACTLY in the reference, so they must pass
    // the strict, non-slugifying validator the client applies to a parsed ref.
    validate_segment(owner).context("repo owner is not a safe path segment")?;
    validate_segment(&name).context("frontmatter `name:` is not a safe path segment")?;
    // ...and the client will also turn `name:` into a directory on install, so
    // confirm that step can't fail downstream either.
    sanitize_dir_segment(&name).context("frontmatter `name:` has no filesystem-safe form")?;

    let version = frontmatter_field(text, "version").unwrap_or_default();
    let stars = read_stars(path);

    let reference = format!("{owner}/{name}");
    Ok(ValidatedSkill {
        owner: owner.to_string(),
        row: SearchResult {
            name,
            author: owner.to_string(),
            description,
            version,
            reference,
            stars,
        },
        raw,
    })
}

/// Read the optional `stars` sidecar next to a SKILL.md (the TASK-695 ingest
/// hand-off). Missing, unreadable, or non-numeric ⇒ 0.
fn read_stars(skill_md: &Path) -> u64 {
    skill_md
        .parent()
        .map(|d| d.join("stars"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}
