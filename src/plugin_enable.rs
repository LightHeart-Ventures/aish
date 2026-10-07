//! User-level plugin enable/disable state (TASK-272, SPR-104).
//!
//! `:plugin enable|disable <id>` persists the operator's choice in a small JSON
//! file that sits NEXT TO the plugins directory — for the default
//! `~/.aish/plugins` that is `~/.aish/plugins.state.json`:
//!
//! ```json
//! { "version": 1, "plugins": { "github": { "enabled": false } } }
//! ```
//!
//! The file lives outside every `<plugins>/<id>/` directory on purpose, so a
//! `:plugin add` reinstall (which rewrites the plugin directory, including its
//! `plugin.json`) never clobbers the user's choice.
//!
//! **Precedence:** a `plugins.<id>.enabled` entry here overrides the manifest's
//! own `"enabled"` field; with no entry, the manifest decides (default `true`).
//! See [`effective_enabled`].
//!
//! Forward compatibility: every per-plugin entry keeps unknown keys verbatim
//! (`extra`), so later phases (TASK-274 error auto-disable: a reason/timestamp)
//! can add fields without this module dropping them on the next write.
//!
//! The module is deliberately self-contained (std + serde only) so the
//! `#[path]`-included test crates (`tests/plugin_dispatcher_tests.rs`) can pull
//! it in alongside `plugin_dispatcher.rs`. The webhook client crate
//! (`crates/aish-webhook-client`) carries a minimal read-only mirror of
//! [`state_path`] + [`enabled_override`]; keep the two in sync.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Current on-disk schema version.
pub const STATE_VERSION: u32 = 1;

/// One plugin's persisted state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginStateEntry {
    /// Operator override of the manifest's `enabled`. `None` → manifest decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Unknown keys preserved verbatim (room for TASK-274 and later phases).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The whole state file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginStateFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginStateEntry>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for PluginStateFile {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            plugins: BTreeMap::new(),
        }
    }
}

/// The state file for `plugins_dir`: a sibling named `<dirname>.state.json`
/// (`~/.aish/plugins` → `~/.aish/plugins.state.json`). Deriving it from the
/// plugins dir keeps every caller — discovery, the Phase 1.6 dispatcher, the
/// webhook client — agreeing on one path without extra plumbing, and keeps
/// tests that use a private temp plugins dir hermetic.
pub fn state_path(plugins_dir: &Path) -> PathBuf {
    match (plugins_dir.parent(), plugins_dir.file_name()) {
        (Some(parent), Some(name)) => parent.join(format!("{}.state.json", name.to_string_lossy())),
        _ => plugins_dir.join(".plugins.state.json"),
    }
}

/// Load the state for `plugins_dir`. Forgiving: a missing or malformed file
/// yields the empty default (a corrupt state file must never block startup).
pub fn load(plugins_dir: &Path) -> PluginStateFile {
    std::fs::read_to_string(state_path(plugins_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Persist `state` for `plugins_dir` atomically (`.tmp` + rename).
pub fn save(plugins_dir: &Path, state: &PluginStateFile) -> std::io::Result<()> {
    let path = state_path(plugins_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{body}\n"))?;
    std::fs::rename(&tmp, &path)
}

/// The persisted override for plugin `id`, if any.
pub fn enabled_override(plugins_dir: &Path, id: &str) -> Option<bool> {
    load(plugins_dir).plugins.get(id).and_then(|e| e.enabled)
}

/// Effective enabled state: the persisted override wins, else the manifest's
/// own `enabled` field, else `true`.
pub fn effective_enabled(plugins_dir: &Path, id: &str, manifest_enabled: Option<bool>) -> bool {
    resolve(enabled_override(plugins_dir, id), manifest_enabled)
}

/// Pure precedence rule behind [`effective_enabled`].
pub fn resolve(state_override: Option<bool>, manifest_enabled: Option<bool>) -> bool {
    state_override.or(manifest_enabled).unwrap_or(true)
}

/// Persist `enabled` as the override for plugin `id` (other keys kept).
pub fn set_enabled(plugins_dir: &Path, id: &str, enabled: bool) -> std::io::Result<()> {
    let mut state = load(plugins_dir);
    state.version = STATE_VERSION;
    state.plugins.entry(id.to_string()).or_default().enabled = Some(enabled);
    save(plugins_dir, &state)
}

/// Drop every persisted entry for plugin `id` (used by `:plugin remove`). A
/// no-op (no write) when nothing is stored for it.
pub fn forget(plugins_dir: &Path, id: &str) -> std::io::Result<()> {
    let mut state = load(plugins_dir);
    if state.plugins.remove(id).is_none() {
        return Ok(());
    }
    save(plugins_dir, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "aish-plugin-enable-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn state_path_is_sibling_of_plugins_dir() {
        assert_eq!(
            state_path(Path::new("/home/u/.aish/plugins")),
            PathBuf::from("/home/u/.aish/plugins.state.json")
        );
        assert_eq!(
            state_path(Path::new("/home/u/.aish/plugins/")),
            PathBuf::from("/home/u/.aish/plugins.state.json")
        );
    }

    #[test]
    fn precedence_override_then_manifest_then_default() {
        assert!(resolve(None, None));
        assert!(!resolve(None, Some(false)));
        assert!(resolve(Some(true), Some(false)));
        assert!(!resolve(Some(false), Some(true)));
        assert!(!resolve(Some(false), None));
    }

    #[test]
    fn set_enabled_round_trips_and_persists() {
        let dir = tempdir().join("plugins");
        assert_eq!(enabled_override(&dir, "gh"), None);
        set_enabled(&dir, "gh", false).unwrap();
        assert_eq!(enabled_override(&dir, "gh"), Some(false));
        assert!(!effective_enabled(&dir, "gh", Some(true)));
        set_enabled(&dir, "gh", true).unwrap();
        assert!(effective_enabled(&dir, "gh", Some(false)));
        let raw = std::fs::read_to_string(state_path(&dir)).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["plugins"]["gh"]["enabled"], true);
    }

    #[test]
    fn unknown_keys_survive_a_write() {
        let dir = tempdir().join("plugins");
        std::fs::write(
            state_path(&dir),
            r#"{"version":1,"plugins":{"gh":{"enabled":true,"disabled_reason":"x"}}}"#,
        )
        .unwrap();
        set_enabled(&dir, "gh", false).unwrap();
        let st = load(&dir);
        assert_eq!(st.plugins["gh"].enabled, Some(false));
        assert_eq!(st.plugins["gh"].extra["disabled_reason"], "x");
    }

    #[test]
    fn malformed_file_is_ignored_and_forget_clears() {
        let dir = tempdir().join("plugins");
        std::fs::write(state_path(&dir), "{ not json").unwrap();
        assert_eq!(load(&dir), PluginStateFile::default());
        set_enabled(&dir, "a", false).unwrap();
        set_enabled(&dir, "b", false).unwrap();
        forget(&dir, "a").unwrap();
        assert_eq!(enabled_override(&dir, "a"), None);
        assert_eq!(enabled_override(&dir, "b"), Some(false));
    }
}
