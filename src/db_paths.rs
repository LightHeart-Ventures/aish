//! Centralized filesystem paths for aish's SQLite databases.
//!
//! Historically the main store (`aish.db`) and the plugin state store
//! (`plugins.db`) lived loose in the config home (`~/.aish/*.db`). As the number
//! of on-disk databases grows (coordinator journals, future stores), that flat
//! layout gets noisy. This module funnels every database file into a single
//! `~/.aish/database/` directory so the config home stays tidy and callers have
//! one canonical place to resolve a DB path.
//!
//! Layout:
//! ```text
//! ~/.aish/
//! └── database/
//!     ├── aish.db      (main history / memory / batch / coordinator store)
//!     └── plugins.db   (plugin-scoped key/value state)
//! ```
//!
//! There is **no auto-migration** from the old flat paths — see
//! `docs/DATABASE_PATHS.md`. Old `~/.aish/*.db` files can be removed by hand.

use std::path::PathBuf;

/// File name of the main history / memory / batch / coordinator store.
pub const MAIN_DB: &str = "aish.db";

/// File name of the plugin-scoped state store.
pub const PLUGIN_STATE_DB: &str = "plugins.db";

/// The config home, `~/.aish/`. Mirrors `main::aish_dir()` so this module has no
/// cross-module dependency and can be compiled directly into integration tests.
fn aish_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish")
}

/// The database directory, `~/.aish/database/`.
///
/// Best-effort creates the directory on every call via `fs::create_dir_all`
/// (idempotent), so callers can pass the returned path straight into a DB
/// `open` without a separate mkdir. A creation failure is swallowed here — the
/// subsequent DB open surfaces a precise, actionable error instead.
pub fn db_dir() -> PathBuf {
    let dir = aish_dir().join("database");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Full path to the main store: `~/.aish/database/aish.db`.
pub fn main_db_path() -> PathBuf {
    db_dir().join(MAIN_DB)
}

/// Full path to the plugin state store: `~/.aish/database/plugins.db`.
pub fn plugin_state_db_path() -> PathBuf {
    db_dir().join(PLUGIN_STATE_DB)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_live_under_db_dir() {
        let dir = db_dir();
        assert!(dir.ends_with("database"));
        assert_eq!(main_db_path(), dir.join("aish.db"));
        assert_eq!(plugin_state_db_path(), dir.join("plugins.db"));
    }

    #[test]
    fn db_dir_is_created() {
        // db_dir() must have materialized the directory on disk.
        assert!(db_dir().is_dir());
    }

    #[test]
    fn file_name_constants() {
        assert_eq!(MAIN_DB, "aish.db");
        assert_eq!(PLUGIN_STATE_DB, "plugins.db");
    }
}
