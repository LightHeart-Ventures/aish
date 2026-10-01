//! Artifact emission — write the two things the client demands:
//!
//!   * `index.json` — a **bare** `SearchResult[]`. The client's
//!     `parse_search_body` accepts either a wrapper object or a bare array
//!     (`v.as_array()` is its last fallback); the bare array is chosen for the
//!     smallest possible payload. Rows arrive already sorted by `reference`, so
//!     the bytes are stable across reruns.
//!   * `{owner}/{name}/raw` — the verbatim SKILL.md bytes, at exactly the path
//!     the client's `raw_url_on` builds (`{base}/{owner}/{name}/raw`).
//!
//! Nothing here re-orders or re-serializes a row: the ordering contract is
//! established in [`crate::catalog::scan`] and simply preserved.

use crate::validate::ValidatedSkill;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// What was written, for the run summary.
#[derive(Debug)]
pub struct Emitted {
    pub index: PathBuf,
    pub index_bytes: usize,
    pub raw_objects: usize,
}

/// Serialize the catalog rows to the exact `index.json` bytes that will be
/// written. Split out from [`emit`] so the determinism test can compare bytes
/// without touching the filesystem twice.
///
/// A trailing newline is always appended — it makes the artifact a well-formed
/// text file and keeps `git diff` on a published mirror clean.
pub fn index_bytes(rows: &[ValidatedSkill], pretty: bool) -> Result<Vec<u8>> {
    let rows_view: Vec<_> = rows.iter().map(|r| &r.row).collect();
    let mut s = if pretty {
        serde_json::to_string_pretty(&rows_view).context("serializing index.json")?
    } else {
        serde_json::to_string(&rows_view).context("serializing index.json")?
    };
    s.push('\n');
    Ok(s.into_bytes())
}

/// Write `index.json` plus one `{owner}/{name}/raw` object per row under `out`.
pub fn emit(out: &Path, rows: &[ValidatedSkill], pretty: bool) -> Result<Emitted> {
    std::fs::create_dir_all(out)
        .with_context(|| format!("creating output dir {}", out.display()))?;

    let bytes = index_bytes(rows, pretty)?;
    let index = out.join("index.json");
    std::fs::write(&index, &bytes).with_context(|| format!("writing {}", index.display()))?;

    for row in rows {
        // Both segments passed `validate_segment`, so neither can contain a
        // separator or `..` — the join cannot escape `out`.
        let dir = out.join(&row.owner).join(&row.row.name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating raw dir {}", dir.display()))?;
        let raw = dir.join("raw");
        std::fs::write(&raw, &row.raw).with_context(|| format!("writing {}", raw.display()))?;
    }

    Ok(Emitted {
        index,
        index_bytes: bytes.len(),
        raw_objects: rows.len(),
    })
}
