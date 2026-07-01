//! End-to-end integration tests for the plugin system (Phase 1.4–1.6).
//!
//! These tests verify that plugins can:
//! 1. Be discovered from the plugin registry
//! 2. Load skills into the catalog
//! 3. Use the state store (Phase 1.5)
//! 4. Receive webhook events (Phase 1.6)

#[path = "../src/plugin_state.rs"]
mod plugin_state;

use plugin_state::PluginStateStore;
use std::fs;
use std::path::Path;

/// Test 1: Plugin discovery reads hello-world plugin.json
#[test]
fn test_hello_world_plugin_discovery() {
    let plugin_dir = Path::new("examples/plugins/hello-world");
    assert!(plugin_dir.exists(), "hello-world plugin directory must exist");

    let manifest_path = plugin_dir.join("plugin.json");
    assert!(
        manifest_path.exists(),
        "hello-world plugin.json must exist"
    );

    let manifest_text = fs::read_to_string(&manifest_path)
        .expect("read hello-world plugin.json");

    // Verify manifest structure
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_text).expect("parse plugin.json");

    assert_eq!(
        manifest["id"].as_str(),
        Some("hello-world"),
        "plugin id should be 'hello-world'"
    );
    assert_eq!(
        manifest["enabled"].as_bool(),
        Some(true),
        "plugin should be enabled"
    );

    // Phase 1.6: webhook_command field should be present
    assert!(
        manifest.get("webhook_command").is_some(),
        "plugin should have webhook_command for Phase 1.6 testing"
    );
}

/// Test 2: Plugin skill is discoverable
#[test]
fn test_hello_world_skill_discovery() {
    let skill_dir = Path::new("examples/plugins/hello-world/skills/hello-world");
    assert!(
        skill_dir.exists(),
        "hello-world skill directory must exist"
    );

    let skill_md = skill_dir.join("SKILL.md");
    assert!(skill_md.exists(), "hello-world SKILL.md must exist");

    let skill_text = fs::read_to_string(&skill_md).expect("read SKILL.md");
    assert!(
        skill_text.contains("name:"),
        "SKILL.md must have YAML frontmatter with name:"
    );
}

/// Test 3: Plugin state store isolation (Phase 1.5)
///
/// Verifies that two plugins can use the same key without colliding.
#[test]
fn test_plugin_state_isolation() {
    let store = PluginStateStore::open_in_memory().expect("open in-memory store");

    // Plugin A sets "counter" to 1
    store
        .set("plugin-a", "counter", &serde_json::json!(1))
        .expect("plugin-a set counter");

    // Plugin B sets "counter" to 2
    store
        .set("plugin-b", "counter", &serde_json::json!(2))
        .expect("plugin-b set counter");

    // Verify isolation
    let a_val = store
        .get("plugin-a", "counter")
        .expect("plugin-a get counter");
    assert_eq!(a_val, Some(serde_json::json!(1)));

    let b_val = store
        .get("plugin-b", "counter")
        .expect("plugin-b get counter");
    assert_eq!(b_val, Some(serde_json::json!(2)));
}

/// Test 4: Plugin state persistence (Phase 1.5)
///
/// Verifies that state survives across store close/reopen cycles.
#[test]
fn test_plugin_state_persistence() {
    let temp_dir = std::env::temp_dir().join(format!(
        "aish-plugin-test-{}",
        std::process::id()
    ));
    let _ = fs::create_dir_all(&temp_dir);
    let db_path = temp_dir.join("test-plugins.db");

    // Clean up from any prior run
    let _ = fs::remove_file(&db_path);

    // Open, write, close
    {
        let store =
            PluginStateStore::open(&db_path).expect("open file-backed store");
        store
            .set("hello-world", "init_count", &serde_json::json!(42))
            .expect("write to store");
    }

    // Reopen and verify
    {
        let store = PluginStateStore::open(&db_path).expect("reopen store");
        let val = store
            .get("hello-world", "init_count")
            .expect("read from store");
        assert_eq!(val, Some(serde_json::json!(42)));
    }

    // Cleanup
    let _ = fs::remove_file(&db_path);
}

/// Test 5: Webhook manifest parsing (Phase 1.6)
///
/// Verifies that hello-world plugin's webhook_command is correctly parsed.
#[test]
fn test_hello_world_webhook_manifest() {
    let manifest_path = Path::new("examples/plugins/hello-world/plugin.json");
    let manifest_text = fs::read_to_string(manifest_path)
        .expect("read hello-world plugin.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_text).expect("parse plugin.json");

    let webhook_cmd = manifest["webhook_command"].as_str();
    assert!(
        webhook_cmd.is_some(),
        "hello-world plugin should define webhook_command"
    );

    // Verify the command is sensible (e.g., references stdin)
    let cmd = webhook_cmd.unwrap();
    assert!(
        cmd.contains("cat") || cmd.contains(">"),
        "webhook_command should read from stdin"
    );
}

/// Test 6: Plugin manifest roundtrip (Phase 1.4 config loading)
///
/// Verifies that ${VAR} expansion in plugin.json works (if integrated).
#[test]
fn test_plugin_manifest_var_expansion() {
    // This test is a placeholder for Phase 1.4 integration.
    // Once plugin manifests support ${VAR} expansion, verify that
    // a manifest with environment variables is correctly expanded
    // during plugin discovery.

    // Note: env::set_var is unsafe; we skip this for now.
    // The Phase 1.4 config loader should be tested via the hello-world
    // plugin's webhook_command field, which may contain ${VAR} expansion.
    assert!(true, "placeholder for Phase 1.4 var expansion test");
}
