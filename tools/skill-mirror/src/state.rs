//! The incremental-crawl state file (TASK-695).
//!
//! One JSON document, one entry per allowlisted repo, holding the three things
//! that let the nightly job cost ~1 API call per unchanged repo instead of
//! `1 + n_skills`:
//!
//!   * `etag` — replayed as `If-None-Match` on the tree request, so an
//!     unchanged repo answers **304 Not Modified** and is skipped wholesale.
//!     GitHub does not bill a conditional 304 against the rate limit.
//!   * `stars` — the popularity signal, so the 304 path still knows how to rank
//!     the repo's skills without spending a `GET /repos` call.
//!   * `skills` — the output directories this repo wrote last time. Two jobs:
//!     verifying the cache is actually still on disk (if it is not, we ignore
//!     the 304 and refetch — a cache that lies is worse than no cache), and
//!     garbage-collecting directories a repo no longer publishes.
//!
//! A missing or corrupt state file is **not** an error: it degrades to a full
//! crawl. The file is a cost optimization, never a correctness dependency.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bumped when the on-disk shape changes incompatibly; a mismatch is treated as
/// "no state" rather than a parse error, so a rollout never needs a migration.
pub const STATE_VERSION: u32 = 1;

/// What ingest remembers about one repo between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoState {
    /// Tree-request ETag, replayed as `If-None-Match`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Tree SHA the ETag corresponds to (provenance + human debugging).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_sha: Option<String>,
    /// Ref the raw objects were fetched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Last known star count — the ranking signal on the 304 path.
    #[serde(default)]
    pub stars: u64,
    /// `{owner}/{dir}` output directories this repo produced last run.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Unix seconds of the last successful crawl (304 counts as success).
    #[serde(default)]
    pub last_seen_unix: u64,
}

/// The whole state document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoState>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            repos: BTreeMap::new(),
        }
    }
}

impl State {
    /// Load state, degrading to [`State::default`] (with a WARN) on anything
    /// unreadable, unparsable, or version-mismatched.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(s) if s.version == STATE_VERSION => s,
            Ok(s) => {
                eprintln!(
                    "skill-mirror: WARN state {} is version {} (expected {STATE_VERSION}); doing a full crawl",
                    path.display(),
                    s.version
                );
                Self::default()
            }
            Err(e) => {
                eprintln!(
                    "skill-mirror: WARN state {} is unreadable ({e}); doing a full crawl",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Previous state for one repo slug.
    pub fn get(&self, slug: &str) -> Option<&RepoState> {
        self.repos.get(slug)
    }

    /// Write the state file (pretty, sorted — it lands in diffs and caches).
    ///
    /// Written to a sibling temp file and renamed, so an interrupted run cannot
    /// leave a half-written state that the next run would then discard.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut json = serde_json::to_string_pretty(self).context("serializing state")?;
        json.push('\n');
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Unix seconds, or 0 if the clock is before the epoch (it is not).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("skill-mirror-state-{}", now_unix()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let mut s = State::default();
        s.repos.insert(
            "a/b".into(),
            RepoState {
                etag: Some("\"abc\"".into()),
                tree_sha: Some("deadbeef".into()),
                git_ref: Some("main".into()),
                stars: 42,
                skills: vec!["a/skill-one".into()],
                last_seen_unix: 1700000000,
            },
        );
        s.save(&path).unwrap();

        let back = State::load(&path);
        assert_eq!(back.version, STATE_VERSION);
        assert_eq!(back.get("a/b").unwrap().stars, 42);
        assert_eq!(back.get("a/b").unwrap().etag.as_deref(), Some("\"abc\""));
        assert!(back.get("nope").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_corrupt_state_degrade_to_empty() {
        assert!(
            State::load(Path::new("/nonexistent/state.json"))
                .repos
                .is_empty()
        );

        let dir = std::env::temp_dir().join(format!("skill-mirror-bad-{}", now_unix()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(State::load(&path).repos.is_empty());

        std::fs::write(&path, r#"{"version":999,"repos":{"a/b":{}}}"#).unwrap();
        assert!(State::load(&path).repos.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
