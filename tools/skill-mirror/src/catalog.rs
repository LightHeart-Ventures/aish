//! Catalog assembly — walk an input tree of `{owner}/{dir}/SKILL.md`, validate
//! each file, and produce the deduped, `reference`-sorted row set that becomes
//! `index.json`.
//!
//! Determinism is a hard requirement (nightly rebuilds must produce byte-stable
//! diffs when nothing changed), so directory entries are sorted before they are
//! visited and the final rows are sorted by `reference`. `read_dir` order is
//! filesystem-dependent and is never relied on.
//!
//! Per-file failures are collected, never fatal: a bad skill is skipped with a
//! reason so the run still publishes the good ones. Catalog-level policy (zero
//! rows, `--min-rows`) is enforced by the caller in `main.rs`.

use crate::validate::{ValidatedSkill, validate_skill};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One skipped input plus why it was skipped — rendered as a WARN line.
#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// The outcome of a scan: accepted rows plus the full accounting needed for the
/// `skipped N of M` stderr summary.
#[derive(Debug, Default)]
pub struct Catalog {
    /// Accepted skills, deduped and sorted by `reference`.
    pub rows: Vec<ValidatedSkill>,
    /// Every candidate SKILL.md considered (accepted + skipped).
    pub considered: usize,
    pub skipped: Vec<Skipped>,
}

impl Catalog {
    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }
}

/// Scan `input` for `{owner}/{dir}/SKILL.md` files and build the catalog.
///
/// The second path segment is provenance only — the row's `name` (and therefore
/// the second half of its `reference`) comes from the frontmatter `name:`, which
/// is what the client displays and what `parse_ref` must round-trip. That is why
/// a directory may be named `alpha-skill` while its reference is `acme/alpha`.
pub fn scan(input: &Path, max_size: u64) -> Result<Catalog> {
    let mut cat = Catalog::default();

    for owner_dir in
        sorted_subdirs(input).with_context(|| format!("reading input tree {}", input.display()))?
    {
        let owner = file_name(&owner_dir);
        for skill_dir in sorted_subdirs(&owner_dir)
            .with_context(|| format!("reading owner dir {}", owner_dir.display()))?
        {
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.is_file() {
                // Not a skill directory at all — not a validation failure, so it
                // is not counted against the skipped/considered totals.
                continue;
            }
            cat.considered += 1;
            match validate_skill(&owner, &skill_md, max_size) {
                Ok(v) => cat.rows.push(v),
                Err(e) => cat.skipped.push(Skipped {
                    path: skill_md,
                    reason: format!("{e:#}"),
                }),
            }
        }
    }

    // Stable order first, THEN dedupe, so which duplicate wins is deterministic
    // (the one whose source path sorts first) rather than filesystem-dependent.
    cat.rows.sort_by(|a, b| a.reference().cmp(b.reference()));
    let mut seen: HashSet<String> = HashSet::new();
    let mut deduped = Vec::with_capacity(cat.rows.len());
    for row in std::mem::take(&mut cat.rows) {
        if seen.insert(row.reference().to_string()) {
            deduped.push(row);
        } else {
            let reference = row.reference().to_string();
            cat.skipped.push(Skipped {
                path: PathBuf::from(format!("{}/{}", row.owner, row.row.name)),
                reason: format!("duplicate reference {reference:?} — keeping the first"),
            });
        }
    }
    cat.rows = deduped;
    Ok(cat)
}

/// Immediate subdirectories of `dir`, sorted by file name for determinism.
/// A missing directory is an error (the caller pointed us at nothing).
fn sorted_subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}
