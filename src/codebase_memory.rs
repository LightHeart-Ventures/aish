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
}
