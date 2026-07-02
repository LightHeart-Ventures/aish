//! Integration tests for the file-based plugin memory store (Phase 2).
//!
//! `aish` is a binary crate with no library target, so an integration test can't
//! `use aish::plugin_memory`. Instead we compile the module's source directly
//! into this test binary via `#[path]`. The module is deliberately
//! self-contained (std + serde_json only), so this Just Works.
//!
//! Run with the same gate CI uses:
//!   cargo test --no-default-features --locked plugin_memory
//!
//! Covers: get/set/append/delete/clear happy paths, nested keys, namespace
//! isolation (cross-plugin), path-traversal rejection, 0600 perms on the auth
//! namespace (create + write + read auto-recovery), non-auth namespaces are not
//! forced 0600, redaction, malformed-file handling, atomic-write durability,
//! and a full persist→reload lifecycle.

#[path = "../src/plugin_memory.rs"]
#[allow(dead_code)]
mod plugin_memory;

use plugin_memory::{MemoryError, MemoryNamespace, PluginMemory};
use serde_json::json;
use std::path::PathBuf;

/// A unique, dependency-free temp dir to act as the plugins root (no `tempfile`
/// crate needed).
fn temp_root(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = format!(
        "aish-plugin-memory-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    p.push(uniq);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn cleanup(root: &PathBuf) {
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---- happy paths -----------------------------------------------------------

#[test]
fn set_get_roundtrip() {
    let root = temp_root("roundtrip");
    let mem = PluginMemory::new(&root);

    mem.set("github", "prefs", "auto_sync", json!(true)).unwrap();
    assert_eq!(mem.get("github", "prefs", "auto_sync").unwrap(), json!(true));

    // overwrite
    mem.set("github", "prefs", "auto_sync", json!(false)).unwrap();
    assert_eq!(mem.get("github", "prefs", "auto_sync").unwrap(), json!(false));

    cleanup(&root);
}

#[test]
fn get_missing_key_is_not_found() {
    let root = temp_root("missing");
    let mem = PluginMemory::new(&root);
    // missing file entirely
    let err = mem.get("nope", "cache", "x").unwrap_err();
    assert!(matches!(err, MemoryError::NotFound { .. }));
    // present file, missing key
    mem.set("nope", "cache", "y", json!(1)).unwrap();
    let err = mem.get("nope", "cache", "x").unwrap_err();
    assert!(matches!(err, MemoryError::NotFound { .. }));
    cleanup(&root);
}

#[test]
fn nested_key_access() {
    let root = temp_root("nested");
    let mem = PluginMemory::new(&root);

    mem.set("github", "webhooks", "github.last_delivery_id", json!("12345"))
        .unwrap();
    mem.set(
        "github",
        "webhooks",
        "github.subscribed_events",
        json!(["push", "pull_request"]),
    )
    .unwrap();

    assert_eq!(
        mem.get("github", "webhooks", "github.last_delivery_id").unwrap(),
        json!("12345")
    );
    // parent object holds both children
    let gh = mem.get("github", "webhooks", "github").unwrap();
    assert_eq!(gh["last_delivery_id"], json!("12345"));
    assert_eq!(gh["subscribed_events"], json!(["push", "pull_request"]));

    cleanup(&root);
}

#[test]
fn append_to_array_creates_and_grows() {
    let root = temp_root("append");
    let mem = PluginMemory::new(&root);

    // missing key → new one-element array
    mem.append("github", "cache", "ts", json!(100)).unwrap();
    mem.append("github", "cache", "ts", json!(200)).unwrap();
    assert_eq!(mem.get("github", "cache", "ts").unwrap(), json!([100, 200]));

    // appending to a scalar is rejected
    mem.set("github", "cache", "scalar", json!(5)).unwrap();
    let err = mem.append("github", "cache", "scalar", json!(6)).unwrap_err();
    assert!(matches!(err, MemoryError::Validation(_)));

    cleanup(&root);
}

#[test]
fn delete_removes_key_and_nested() {
    let root = temp_root("delete");
    let mem = PluginMemory::new(&root);

    mem.set("github", "webhooks", "github.id", json!("x")).unwrap();
    mem.set("github", "webhooks", "top", json!(1)).unwrap();

    mem.delete("github", "webhooks", "github.id").unwrap();
    assert!(matches!(
        mem.get("github", "webhooks", "github.id").unwrap_err(),
        MemoryError::NotFound { .. }
    ));
    // sibling + parent survive
    assert_eq!(mem.get("github", "webhooks", "top").unwrap(), json!(1));
    assert!(mem.get("github", "webhooks", "github").is_ok());

    // deleting a missing key is NotFound
    assert!(matches!(
        mem.delete("github", "webhooks", "nope").unwrap_err(),
        MemoryError::NotFound { .. }
    ));

    cleanup(&root);
}

#[test]
fn clear_empties_namespace() {
    let root = temp_root("clear");
    let mem = PluginMemory::new(&root);

    mem.set("github", "prefs", "a", json!(1)).unwrap();
    mem.set("github", "prefs", "b", json!(2)).unwrap();
    assert_eq!(mem.key_count("github", MemoryNamespace::Prefs).unwrap(), 2);

    mem.clear("github", "prefs").unwrap();
    assert_eq!(mem.key_count("github", MemoryNamespace::Prefs).unwrap(), 0);
    // file still exists and reads as empty
    assert!(matches!(
        mem.get("github", "prefs", "a").unwrap_err(),
        MemoryError::NotFound { .. }
    ));

    cleanup(&root);
}

// ---- namespace isolation & access control ----------------------------------

#[test]
fn cross_plugin_isolation() {
    let root = temp_root("isolation");
    let mem = PluginMemory::new(&root);

    mem.set("plugin-a", "auth", "access_token", json!("A-secret"))
        .unwrap();
    mem.set("plugin-b", "auth", "access_token", json!("B-secret"))
        .unwrap();

    // Each sees only its own value.
    assert_eq!(
        mem.get("plugin-a", "auth", "access_token").unwrap(),
        json!("A-secret")
    );
    assert_eq!(
        mem.get("plugin-b", "auth", "access_token").unwrap(),
        json!("B-secret")
    );

    // Files are physically separate.
    assert!(root.join("plugin-a").join("memory").join("auth.json").exists());
    assert!(root.join("plugin-b").join("memory").join("auth.json").exists());

    cleanup(&root);
}

#[test]
fn path_traversal_is_rejected() {
    let root = temp_root("traversal");
    let mem = PluginMemory::new(&root);

    for bad in ["../victim", "..", ".", "a/b", "a\\b", "", "x..y"] {
        let err = mem.get(bad, "auth", "access_token").unwrap_err();
        assert!(
            matches!(err, MemoryError::PathTraversal(_)),
            "expected PathTraversal for `{bad}`, got {err:?}"
        );
        let err = mem.set(bad, "auth", "k", json!(1)).unwrap_err();
        assert!(matches!(err, MemoryError::PathTraversal(_)));
    }

    cleanup(&root);
}

#[test]
fn can_access_validates_id() {
    let root = temp_root("access");
    let mem = PluginMemory::new(&root);
    assert!(mem.can_access("github", MemoryNamespace::Auth).is_ok());
    assert!(mem.can_access("../evil", MemoryNamespace::Auth).is_err());
    cleanup(&root);
}

#[test]
fn invalid_namespace_rejected() {
    let root = temp_root("badns");
    let mem = PluginMemory::new(&root);
    let err = mem.get("github", "bogus", "k").unwrap_err();
    assert!(matches!(err, MemoryError::InvalidNamespace(_)));
    cleanup(&root);
}

// ---- file perms (auth namespace = 0600) ------------------------------------

#[cfg(unix)]
#[test]
fn auth_file_created_0600() {
    let root = temp_root("perms-create");
    let mem = PluginMemory::new(&root);

    mem.set("github", "auth", "access_token", json!("gho_x")).unwrap();
    let path = root.join("github").join("memory").join("auth.json");
    assert_eq!(mode_of(&path), 0o600, "auth.json must be 0600 on create");

    cleanup(&root);
}

#[cfg(unix)]
#[test]
fn auth_perms_reasserted_on_every_write() {
    let root = temp_root("perms-write");
    let mem = PluginMemory::new(&root);
    let path = root.join("github").join("memory").join("auth.json");

    mem.set("github", "auth", "t", json!("1")).unwrap();
    // Tamper: make it world-readable.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(mode_of(&path), 0o644);

    // A subsequent write must restore 0600.
    mem.set("github", "auth", "t2", json!("2")).unwrap();
    assert_eq!(mode_of(&path), 0o600);

    cleanup(&root);
}

#[cfg(unix)]
#[test]
fn auth_perms_autocorrected_on_read() {
    let root = temp_root("perms-read");
    let mem = PluginMemory::new(&root);
    let path = root.join("github").join("memory").join("auth.json");

    mem.set("github", "auth", "t", json!("1")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    // A read of the secret namespace should silently repair perms.
    let _ = mem.get("github", "auth", "t").unwrap();
    assert_eq!(mode_of(&path), 0o600, "read must auto-correct auth perms");

    cleanup(&root);
}

#[cfg(unix)]
#[test]
fn non_auth_namespaces_not_forced_0600() {
    let root = temp_root("perms-nonauth");
    let mem = PluginMemory::new(&root);

    mem.set("github", "prefs", "a", json!(1)).unwrap();
    let path = root.join("github").join("memory").join("prefs.json");
    assert_eq!(mode_of(&path), 0o644, "prefs.json should be 0644, not 0600");

    cleanup(&root);
}

// ---- redaction -------------------------------------------------------------

#[test]
fn auth_display_is_redacted() {
    let root = temp_root("redact");
    let mem = PluginMemory::new(&root);

    mem.set("github", "auth", "access_token", json!("gho_secret"))
        .unwrap();
    mem.set("github", "auth", "expires_at", json!("2026-07-02T10:00:00Z"))
        .unwrap();

    let shown = mem.display_namespace("github", MemoryNamespace::Auth).unwrap();
    assert_eq!(shown["access_token"], json!("***"));
    assert_eq!(shown["expires_at"], json!("***"));

    // non-secret namespace is shown verbatim
    mem.set("github", "prefs", "auto_sync", json!(true)).unwrap();
    let shown = mem.display_namespace("github", MemoryNamespace::Prefs).unwrap();
    assert_eq!(shown["auto_sync"], json!(true));

    cleanup(&root);
}

#[test]
fn writing_redaction_sentinel_into_auth_rejected() {
    let root = temp_root("sentinel");
    let mem = PluginMemory::new(&root);
    let err = mem.set("github", "auth", "access_token", json!("***")).unwrap_err();
    assert!(matches!(err, MemoryError::Validation(_)));
    // but fine in non-secret namespaces
    assert!(mem.set("github", "prefs", "note", json!("***")).is_ok());
    cleanup(&root);
}

// ---- robustness ------------------------------------------------------------

#[test]
fn malformed_file_is_reported_not_silently_empty() {
    let root = temp_root("malformed");
    let mem = PluginMemory::new(&root);

    let dir = root.join("github").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cache.json"), b"{ this is not json").unwrap();

    let err = mem.get("github", "cache", "x").unwrap_err();
    assert!(matches!(err, MemoryError::Malformed { .. }));

    cleanup(&root);
}

#[test]
fn non_object_root_is_malformed() {
    let root = temp_root("nonobject");
    let mem = PluginMemory::new(&root);
    let dir = root.join("github").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("prefs.json"), b"[1,2,3]").unwrap();
    assert!(matches!(
        mem.get("github", "prefs", "x").unwrap_err(),
        MemoryError::Malformed { .. }
    ));
    cleanup(&root);
}

#[test]
fn empty_file_reads_as_empty_object() {
    let root = temp_root("empty");
    let mem = PluginMemory::new(&root);
    let dir = root.join("github").join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("prefs.json"), b"").unwrap();
    assert_eq!(mem.key_count("github", MemoryNamespace::Prefs).unwrap(), 0);
    cleanup(&root);
}

// ---- full lifecycle --------------------------------------------------------

#[test]
fn full_lifecycle_persist_reload() {
    let root = temp_root("lifecycle");

    // "load 1": plugin writes memory.
    {
        let mem = PluginMemory::new(&root);
        mem.set("github", "auth", "access_token", json!("gho_1")).unwrap();
        mem.set("github", "webhooks", "github.last_delivery_id", json!("42"))
            .unwrap();
        mem.append("github", "cache", "ts", json!(1)).unwrap();
        mem.append("github", "cache", "ts", json!(2)).unwrap();
        mem.set("github", "prefs", "auto_sync", json!(true)).unwrap();
    }

    // "load 2": a fresh store over the same root sees persisted memory.
    {
        let mem = PluginMemory::new(&root);
        assert_eq!(mem.get("github", "auth", "access_token").unwrap(), json!("gho_1"));
        assert_eq!(
            mem.get("github", "webhooks", "github.last_delivery_id").unwrap(),
            json!("42")
        );
        assert_eq!(mem.get("github", "cache", "ts").unwrap(), json!([1, 2]));
        assert_eq!(mem.get("github", "prefs", "auto_sync").unwrap(), json!(true));

        // counts across namespaces
        assert_eq!(mem.key_count("github", MemoryNamespace::Auth).unwrap(), 1);
        assert_eq!(mem.key_count("github", MemoryNamespace::Cache).unwrap(), 1);
    }

    cleanup(&root);
}

#[test]
fn multi_plugin_lifecycle_isolated() {
    let root = temp_root("multi");
    let mem = PluginMemory::new(&root);

    for (id, tok) in [("plug-a", "AAA"), ("plug-b", "BBB")] {
        mem.set(id, "auth", "access_token", json!(tok)).unwrap();
        mem.set(id, "prefs", "id_marker", json!(id)).unwrap();
    }
    // reload
    let mem2 = PluginMemory::new(&root);
    assert_eq!(mem2.get("plug-a", "auth", "access_token").unwrap(), json!("AAA"));
    assert_eq!(mem2.get("plug-b", "auth", "access_token").unwrap(), json!("BBB"));
    assert_eq!(mem2.get("plug-a", "prefs", "id_marker").unwrap(), json!("plug-a"));

    cleanup(&root);
}
