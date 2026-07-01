//! Integration tests for the plugin-scoped state store (Phase 1.5).
//!
//! `aish` is a binary crate with no library target, so an integration test can't
//! `use aish::plugin_state`. Instead we compile the module's source directly
//! into this test binary via `#[path]`. The module is deliberately
//! self-contained (std + rusqlite + serde_json only), so this Just Works.
//!
//! Run with the same gate CI uses:
//!   cargo test --no-default-features --locked plugin_state

#[path = "../src/plugin_state.rs"]
#[allow(dead_code)]
mod plugin_state;

use plugin_state::PluginStateStore;
use serde_json::json;
use std::path::PathBuf;

/// A unique, dependency-free temp file path (no `tempfile` crate needed). The
/// file itself is created by SQLite on open; we just pick a collision-free name.
fn temp_db_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = format!(
        "aish-plugin-state-{tag}-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    p.push(uniq);
    p
}

/// Best-effort cleanup of a file-backed test DB and its WAL/SHM sidecars.
fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

#[test]
fn test_init_db_creates_schema() {
    // Opening the store must create the `plugin_state` table. Prove it by
    // performing a write+read round-trip against a fresh in-memory DB — that
    // only succeeds if init ran the DDL.
    let store = PluginStateStore::open_in_memory().expect("open in-memory store");
    store
        .set("p", "k", &json!("v"))
        .expect("write after init must succeed (schema exists)");
    assert_eq!(store.get("p", "k").unwrap(), Some(json!("v")));

    // And re-opening a file-backed DB is idempotent (schema already present).
    let path = temp_db_path("init");
    {
        let s1 = PluginStateStore::open(&path).expect("first open");
        s1.set("p", "k", &json!(1)).unwrap();
    }
    {
        let s2 = PluginStateStore::open(&path).expect("second open — must not error on existing schema");
        assert_eq!(s2.get("p", "k").unwrap(), Some(json!(1)));
    }
    cleanup(&path);
}

#[test]
fn test_set_and_get_value() {
    let store = PluginStateStore::open_in_memory().unwrap();

    // Missing key → None.
    assert_eq!(store.get("plug", "missing").unwrap(), None);

    // Round-trip a variety of JSON shapes.
    store.set("plug", "str", &json!("hello")).unwrap();
    store.set("plug", "num", &json!(42)).unwrap();
    store
        .set("plug", "obj", &json!({"a": 1, "b": [true, null]}))
        .unwrap();

    assert_eq!(store.get("plug", "str").unwrap(), Some(json!("hello")));
    assert_eq!(store.get("plug", "num").unwrap(), Some(json!(42)));
    assert_eq!(
        store.get("plug", "obj").unwrap(),
        Some(json!({"a": 1, "b": [true, null]}))
    );

    // Overwrite updates the value in place (upsert).
    store.set("plug", "str", &json!("world")).unwrap();
    assert_eq!(store.get("plug", "str").unwrap(), Some(json!("world")));
}

#[test]
fn test_namespace_isolation() {
    let store = PluginStateStore::open_in_memory().unwrap();

    // Same key, two plugins — values must not collide.
    store.set("plugin-a", "shared", &json!("A")).unwrap();
    store.set("plugin-b", "shared", &json!("B")).unwrap();

    assert_eq!(store.get("plugin-a", "shared").unwrap(), Some(json!("A")));
    assert_eq!(store.get("plugin-b", "shared").unwrap(), Some(json!("B")));

    // Deleting one namespace's key leaves the other intact.
    store.delete("plugin-a", "shared").unwrap();
    assert_eq!(store.get("plugin-a", "shared").unwrap(), None);
    assert_eq!(store.get("plugin-b", "shared").unwrap(), Some(json!("B")));
}

#[test]
fn test_delete_value() {
    let store = PluginStateStore::open_in_memory().unwrap();

    store.set("p", "k", &json!("v")).unwrap();
    assert_eq!(store.get("p", "k").unwrap(), Some(json!("v")));

    store.delete("p", "k").unwrap();
    assert_eq!(store.get("p", "k").unwrap(), None);

    // Deleting an already-absent key is a no-op, not an error.
    store.delete("p", "k").expect("deleting a missing key is Ok");
    store
        .delete("nonexistent", "nope")
        .expect("deleting from an unseen plugin is Ok");
}

#[test]
fn test_list_for_plugin() {
    let store = PluginStateStore::open_in_memory().unwrap();

    // Empty for an unseen plugin.
    assert!(store.list_for_plugin("empty").unwrap().is_empty());

    store.set("p", "beta", &json!(2)).unwrap();
    store.set("p", "alpha", &json!(1)).unwrap();
    store.set("p", "gamma", &json!(3)).unwrap();
    // A different plugin's keys must not appear.
    store.set("other", "zeta", &json!(99)).unwrap();

    let listed = store.list_for_plugin("p").unwrap();
    // Ordered by key.
    assert_eq!(
        listed,
        vec![
            ("alpha".to_string(), json!(1)),
            ("beta".to_string(), json!(2)),
            ("gamma".to_string(), json!(3)),
        ]
    );

    let other = store.list_for_plugin("other").unwrap();
    assert_eq!(other, vec![("zeta".to_string(), json!(99))]);
}

/// Three tokio tasks each open their OWN connection to the SAME file-backed DB
/// and write concurrently. WAL + busy_timeout let the independent writers
/// interleave without corruption or spurious SQLITE_BUSY; all 300 rows must
/// survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_writes() {
    let path = temp_db_path("concurrent");
    // Ensure the schema exists before the writers race on it.
    PluginStateStore::open(&path).unwrap();

    let mut handles = Vec::new();
    for t in 0..3u32 {
        let p = path.clone();
        handles.push(tokio::spawn(async move {
            // Each task gets its own connection to the shared file.
            let store = PluginStateStore::open(&p).expect("task open");
            for i in 0..100u32 {
                store
                    .set("shared", &format!("t{t}-k{i}"), &json!(i))
                    .expect("concurrent set must succeed");
            }
        }));
    }
    for h in handles {
        h.await.expect("task must not panic");
    }

    let store = PluginStateStore::open(&path).unwrap();
    let rows = store.list_for_plugin("shared").unwrap();
    assert_eq!(rows.len(), 300, "all 3x100 concurrent writes must persist");

    // Spot-check a value from each writer.
    assert_eq!(store.get("shared", "t0-k0").unwrap(), Some(json!(0)));
    assert_eq!(store.get("shared", "t2-k99").unwrap(), Some(json!(99)));

    cleanup(&path);
}
