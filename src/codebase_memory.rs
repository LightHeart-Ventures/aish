//! Native enrollment for the DeusData/`codebase-memory-mcp` server (TASK-404,
//! SPR-071 — "Codebase-Memory Integration").
//!
//! The 14 code-intelligence tools of `codebase-memory-mcp` (MIT, DeusData) only
//! reach the model once aish (a) has the platform-matched static binary on disk
//! and (b) writes a `codebase-memory` stdio server entry into the user-scope
//! `~/.aish/.mcp.json`. This module is the pure, offline-testable core of that
//! enrollment: platform→asset mapping, the release download URL, the idempotent
//! `.mcp.json` merge, and graceful-absence probes. The `:codebase` REPL command
//! (see `repl::handle_codebase`) does the filesystem/network IO on top of these.
//!
//! Everything here is dependency-light (std + `serde_json`, both already in the
//! tree) and free of network/filesystem side effects except the two tiny probe
//! helpers (`binary_present`), so the unit tests below run with no network under
//! `cargo test --no-default-features --locked`.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The stable name of the server as it appears under `mcpServers` in
/// `.mcp.json` and in `:mcp` listings. Also the on-disk binary stem.
pub const SERVER_NAME: &str = "codebase-memory";

/// GitHub `owner/repo` the release assets are published under.
pub const RELEASE_REPO: &str = "DeusData/codebase-memory-mcp";

/// Release tag this build of aish pins enrollment to. Bumping aish's support for
/// a newer server is a one-line change here plus a checksum refresh.
pub const PINNED_VERSION: &str = "v0.1.0";

/// SPDX license of the enrolled server, recorded for attribution (AC: "MIT
/// attribution recorded").
pub const LICENSE: &str = "MIT";

/// Outcome of merging the server entry into a `.mcp.json` root — lets the caller
/// report precisely and proves idempotency in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The server was not present and has been added.
    Added,
    /// The server was already present with an identical spec — no write needed.
    Unchanged,
    /// The server was present but with a different spec, now replaced.
    Updated,
}

/// The config home, `~/.aish/`. Mirrors `main::aish_dir()` / `db_paths::aish_dir`
/// so this module has no cross-module dependency.
pub fn aish_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish")
}

/// Directory aish keeps enrolled server binaries in: `~/.aish/bin/`.
pub fn install_dir(home: &Path) -> PathBuf {
    home.join("bin")
}

/// Absolute path of the enrolled binary: `~/.aish/bin/codebase-memory-mcp`
/// (`.exe` on Windows).
pub fn binary_path(home: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "codebase-memory-mcp.exe"
    } else {
        "codebase-memory-mcp"
    };
    install_dir(home).join(name)
}

/// Whether the enrolled binary exists on disk. The whole point of the graceful-
/// absence path (TASK-411): enrollment can register the `.mcp.json` entry before
/// the binary lands, and the server simply stays dormant until it does.
pub fn binary_present(path: &Path) -> bool {
    path.is_file()
}

/// The platform-matched release asset filename, or `None` when this
/// OS/arch has no prebuilt asset (caller degrades gracefully with a
/// build-from-source hint).
pub fn platform_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("codebase-memory-mcp-x86_64-unknown-linux-gnu.tar.gz"),
        ("linux", "aarch64") => Some("codebase-memory-mcp-aarch64-unknown-linux-gnu.tar.gz"),
        ("macos", "x86_64") => Some("codebase-memory-mcp-x86_64-apple-darwin.tar.gz"),
        ("macos", "aarch64") => Some("codebase-memory-mcp-aarch64-apple-darwin.tar.gz"),
        ("windows", "x86_64") => Some("codebase-memory-mcp-x86_64-pc-windows-msvc.zip"),
        _ => None,
    }
}

/// The GitHub release download URL for a given version tag + asset filename.
pub fn asset_url(version: &str, asset: &str) -> String {
    format!("https://github.com/{RELEASE_REPO}/releases/download/{version}/{asset}")
}

/// The stdio `.mcp.json` server spec pointing at the on-disk binary — the same
/// `{ "command": …, "args": … }` shape `:mcp add` persists.
pub fn server_spec(binary: &Path) -> Value {
    json!({
        "command": binary.to_string_lossy(),
        "args": [],
    })
}

/// True when `name` is already registered under `mcpServers` in `root`.
pub fn is_enrolled(root: &Value, name: &str) -> bool {
    root.get("mcpServers")
        .and_then(|m| m.get(name))
        .is_some()
}

/// Idempotently merge a `codebase-memory` server entry into a parsed `.mcp.json`
/// root, in place. Ensures `mcpServers` is an object, preserves every sibling
/// server, and never clobbers an identical existing entry (returns `Unchanged`
/// so the caller can skip the disk write). Re-running with the same spec is a
/// no-op — this is the idempotency the AC requires.
pub fn merge_server_entry(root: &mut Value, name: &str, spec: Value) -> MergeOutcome {
    if !root.get("mcpServers").map(Value::is_object).unwrap_or(false) {
        root["mcpServers"] = json!({});
    }
    let servers = root["mcpServers"]
        .as_object_mut()
        .expect("mcpServers coerced to object above");
    match servers.get(name) {
        Some(existing) if *existing == spec => MergeOutcome::Unchanged,
        Some(_) => {
            servers.insert(name.to_string(), spec);
            MergeOutcome::Updated
        }
        None => {
            servers.insert(name.to_string(), spec);
            MergeOutcome::Added
        }
    }
}

// ---------------------------------------------------------------------------
// TASK-407 (SPR-071): Repo-open auto-index handoff.
//
// When aish enters a repo carrying the `.repospec.json` habit-marker and the
// `codebase-memory` server is enrolled + connected, warm its structural index
// ONCE per repo-open so the graph is ready before the first coordinator query.
// The filesystem / MCP IO lives in `engine::maybe_auto_index_repo`; the pieces
// below are the pure, offline-testable core (tool id, args, config gate, and the
// combined fire/no-op predicate).
// ---------------------------------------------------------------------------

/// The `codebase-memory` tool that (re)builds the structural index for a repo.
/// The fully-qualified id advertised to the model is
/// `mcp__codebase-memory__index_repository` (see [`index_tool_qualified`]).
pub const INDEX_TOOL: &str = "index_repository";

/// Environment variable that opts a session out of repo-open auto-indexing.
/// Set `AISH_CODEBASE_AUTO_INDEX=0` (or `false`/`off`/`no`) to disable.
pub const AUTO_INDEX_ENV: &str = "AISH_CODEBASE_AUTO_INDEX";

/// The fully-qualified MCP tool id for the index/refresh call, in the
/// `mcp__<server>__<tool>` shape [`crate::mcp::McpHost::call`] expects.
pub fn index_tool_qualified() -> String {
    format!("mcp__{SERVER_NAME}__{INDEX_TOOL}")
}

/// The JSON arguments for an index/refresh over `repo_root` — the repo whose
/// structural graph should be warmed.
pub fn index_args(repo_root: &Path) -> Value {
    json!({ "path": repo_root.to_string_lossy() })
}

/// Whether repo-open auto-indexing is enabled — the AC's `auto_index` config
/// gate. Priority: an explicit env override wins (`AISH_CODEBASE_AUTO_INDEX`),
/// otherwise the optional `{"codebaseMemory":{"autoIndex":false}}` key in
/// `.mcp.json`, otherwise ON by default. An empty env value is treated as unset.
pub fn auto_index_enabled(root: &Value, env: Option<&str>) -> bool {
    match env
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        Some(v) => !matches!(v.as_str(), "0" | "false" | "off" | "no"),
        None => root
            .get("codebaseMemory")
            .and_then(|c| c.get("autoIndex"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
    }
}

/// The combined decision: fire a repo-open auto-index only when the server is
/// enrolled AND connected (binary present + tools live), the `auto_index` gate
/// is on, and this repo root has not already been indexed this session
/// (dedup → at most once per repo-open). Pure so the fire/no-op matrix is fully
/// unit-testable without touching the filesystem or an MCP server.
pub fn should_auto_index(enrolled: bool, connected: bool, gate_on: bool, already: bool) -> bool {
    enrolled && connected && gate_on && !already
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec_for(cmd: &str) -> Value {
        server_spec(&PathBuf::from(cmd))
    }

    #[test]
    fn merge_into_empty_root_adds_entry() {
        let mut root = json!({});
        let out = merge_server_entry(&mut root, SERVER_NAME, spec_for("/bin/cbm"));
        assert_eq!(out, MergeOutcome::Added);
        assert!(is_enrolled(&root, SERVER_NAME));
        assert_eq!(root["mcpServers"][SERVER_NAME]["command"], "/bin/cbm");
    }

    #[test]
    fn merge_is_idempotent_on_identical_spec() {
        let mut root = json!({ "mcpServers": {} });
        let first = merge_server_entry(&mut root, SERVER_NAME, spec_for("/bin/cbm"));
        let second = merge_server_entry(&mut root, SERVER_NAME, spec_for("/bin/cbm"));
        assert_eq!(first, MergeOutcome::Added);
        assert_eq!(second, MergeOutcome::Unchanged);
    }

    #[test]
    fn merge_updates_when_spec_differs() {
        let mut root = json!({ "mcpServers": { SERVER_NAME: { "command": "/old", "args": [] } } });
        let out = merge_server_entry(&mut root, SERVER_NAME, spec_for("/new"));
        assert_eq!(out, MergeOutcome::Updated);
        assert_eq!(root["mcpServers"][SERVER_NAME]["command"], "/new");
    }

    #[test]
    fn merge_preserves_sibling_servers() {
        let mut root = json!({ "mcpServers": { "atum": { "type": "http", "url": "x" } } });
        merge_server_entry(&mut root, SERVER_NAME, spec_for("/bin/cbm"));
        // The pre-existing server must survive the merge untouched.
        assert_eq!(root["mcpServers"]["atum"]["url"], "x");
        assert!(is_enrolled(&root, SERVER_NAME));
    }

    #[test]
    fn merge_coerces_non_object_mcpservers() {
        // A malformed file with a string mcpServers must not panic or lose the merge.
        let mut root = json!({ "mcpServers": "oops" });
        let out = merge_server_entry(&mut root, SERVER_NAME, spec_for("/bin/cbm"));
        assert_eq!(out, MergeOutcome::Added);
        assert!(root["mcpServers"].is_object());
    }

    #[test]
    fn platform_asset_resolves_for_supported_targets() {
        // On every CI target we run (linux/macos x86_64/aarch64) an asset exists.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(platform_asset().is_some());
        // Whatever it is, it carries the project name.
        if let Some(a) = platform_asset() {
            assert!(a.starts_with("codebase-memory-mcp-"));
        }
    }

    #[test]
    fn asset_url_is_a_github_release_download() {
        let url = asset_url(PINNED_VERSION, "codebase-memory-mcp-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(
            url,
            "https://github.com/DeusData/codebase-memory-mcp/releases/download/v0.1.0/codebase-memory-mcp-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn binary_present_reflects_disk_state() {
        let dir = std::env::temp_dir().join(format!("cbm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = binary_path(&dir);
        assert!(!binary_present(&bin), "absent before creation");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"#!/bin/true\n").unwrap();
        assert!(binary_present(&bin), "present after creation");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_enrolled_false_when_absent() {
        assert!(!is_enrolled(&json!({ "mcpServers": {} }), SERVER_NAME));
        assert!(!is_enrolled(&json!({}), SERVER_NAME));
    }

    #[test]
    fn binary_path_lives_under_bin() {
        let home = PathBuf::from("/home/x/.aish");
        assert!(binary_path(&home).starts_with(home.join("bin")));
    }

    // --- TASK-407: repo-open auto-index core -------------------------------

    #[test]
    fn index_tool_is_fully_qualified_mcp_id() {
        assert_eq!(INDEX_TOOL, "index_repository");
        assert_eq!(
            index_tool_qualified(),
            format!("mcp__{SERVER_NAME}__index_repository")
        );
        // Round-trips through the `mcp__<server>__<tool>` shape McpHost::call parses.
        let q = index_tool_qualified();
        let rest = q.strip_prefix("mcp__").unwrap();
        let (server, tool) = rest.split_once("__").unwrap();
        assert_eq!(server, SERVER_NAME);
        assert_eq!(tool, INDEX_TOOL);
    }

    #[test]
    fn index_args_carry_the_repo_root_path() {
        let args = index_args(&PathBuf::from("/home/x/proj"));
        assert_eq!(args["path"], "/home/x/proj");
    }

    #[test]
    fn auto_index_env_override_wins_over_config() {
        // Config says off, but an explicit env "1" forces it on.
        let cfg_off = json!({ "codebaseMemory": { "autoIndex": false } });
        assert!(auto_index_enabled(&cfg_off, Some("1")));
        // Config says on (default), but env off wins.
        let cfg_empty = json!({});
        for off in ["0", "false", "off", "no", "FALSE", " Off "] {
            assert!(!auto_index_enabled(&cfg_empty, Some(off)), "{off:?}");
        }
        for on in ["1", "true", "yes", "anything"] {
            assert!(auto_index_enabled(&cfg_empty, Some(on)), "{on:?}");
        }
    }

    #[test]
    fn auto_index_empty_env_falls_back_to_config() {
        // An empty/whitespace env value is treated as unset → config decides.
        let cfg_off = json!({ "codebaseMemory": { "autoIndex": false } });
        assert!(!auto_index_enabled(&cfg_off, Some("")));
        assert!(!auto_index_enabled(&cfg_off, Some("   ")));
        assert!(!auto_index_enabled(&cfg_off, None));
    }

    #[test]
    fn auto_index_defaults_on_when_unconfigured() {
        assert!(auto_index_enabled(&json!({}), None));
        assert!(auto_index_enabled(&json!({ "mcpServers": {} }), None));
        // Explicit true in config also enables.
        assert!(auto_index_enabled(
            &json!({ "codebaseMemory": { "autoIndex": true } }),
            None
        ));
    }

    #[test]
    fn should_auto_index_only_when_all_gates_pass() {
        // The one firing combination: enrolled + connected + gate on + not-yet-done.
        assert!(should_auto_index(true, true, true, false));
        // Any single failing precondition suppresses the fire.
        assert!(!should_auto_index(false, true, true, false), "not enrolled");
        assert!(!should_auto_index(true, false, true, false), "not connected");
        assert!(!should_auto_index(true, true, false, false), "gate off");
        assert!(!should_auto_index(true, true, true, true), "already indexed");
    }
}
