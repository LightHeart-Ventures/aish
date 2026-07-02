//! Phase 0.5.7 — consolidation tests for the plugin capability surface.
//!
//! Phases 0.5.2–0.5.6 each landed with thorough *per-module* unit coverage
//! (`hooks::tests`, `plugins::tests`, `plugin_auth::tests`). Those prove each
//! seam in isolation. This module is the missing **integration** layer: it
//! stands up ONE realistic on-disk multi-plugin fixture and drives every Phase
//! 0.5 behavior through the real public APIs *in composition* — the regression
//! guard for the headline unlock: *a single plugin install + `aish login`* wires
//! event hooks, lifecycle-hook env injection, credential persistence, credential
//! → hook export, and `.mcp.json` merge together with no bespoke upstream code.
//!
//! 0.5.7 checklist ↔ coverage map (per-behavior unit tests live beside each
//! module; the composed assertions below exercise them together):
//!
//! | 0.5.7 behavior                    | this module                                  | unit tests |
//! |-----------------------------------|----------------------------------------------|------------|
//! | catalog merge + precedence        | `catalog_merge_precedence_and_override_from_disk` | `hooks::tests::plugin_merge_*`, `merge_precedence_*`, `load_layered_*` |
//! | blocking-veto from a plugin entry | `blocking_veto_composition_structural`       | `hooks::tests::plugin_blocking_*` |
//! | `.mcp.json` merge                 | `end_to_end_*`, `mcp_collision_against_config_scope` | `plugins::tests::collect_plugin_mcp_servers_*` |
//! | env injection                     | `end_to_end_*`, `env_injection_rejects_credential_like_from_hook` | `plugins::tests::collect_*`, `parse_*` |
//! | login round-trip                  | `login_roundtrip_profile_readback`, `end_to_end_*` | `plugin_auth::tests::login_*` |
//! | override / disable a plugin hook  | `catalog_merge_precedence_and_override_from_disk` | `hooks::tests::plugin_override_*`, `plugin_disable_*` |

#![cfg(test)]

use std::fs;
use std::path::{Path, PathBuf};

use crate::hooks::{HookSet, HookSource};
use crate::{plugin_auth, plugins};

// ---------------------------------------------------------------------------
// Self-contained fixture helpers (the per-module test helpers are private to
// their own `mod tests`, so this integration module carries its own tiny set).
// ---------------------------------------------------------------------------

/// A private, dependency-free temp dir (the crate pulls in no `tempfile`).
fn tempdir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aish-0507-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Write `<root>/<id>/plugin.json` with the given manifest JSON.
fn write_manifest(root: &Path, id: &str, manifest: &str) {
    let pdir = root.join(id);
    fs::create_dir_all(&pdir).unwrap();
    fs::write(pdir.join("plugin.json"), manifest).unwrap();
}

/// Write an executable `<root>/<id>/<rel>` script (shebang body).
fn write_exec(root: &Path, id: &str, rel: &str, body: &str) {
    let path = root.join(id).join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Write `<root>/<id>/<file>` verbatim (config/json data — not executable).
fn write_data(root: &Path, id: &str, file: &str, body: &str) {
    let path = root.join(id).join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
}

/// Build the canonical two-plugin fixture used across the composition tests:
///
/// * **`enterprise`** — id == `provides.login` (`"enterprise"`) so its own
///   logged-in credentials export into its `on_init` hook as
///   `AISH_PROFILE_ENTERPRISE_*`. Ships a `login.sh`, an `.mcp.json` (server
///   `corp`), a `hooks.json` (a `PreToolUse` observe entry), and an `on_init.sh`
///   that re-exports the injected gateway URL under a benign key.
/// * **`team`** — a second enabled plugin that also claims the `corp` MCP
///   server (loses the collision) plus its own `notes` server, and exports a
///   plain env var from `on_init`. Its id sorts after `enterprise`.
fn build_fixture() -> PathBuf {
    let root = tempdir();

    // --- enterprise ---------------------------------------------------------
    write_manifest(
        &root,
        "enterprise",
        r#"{
            "id": "enterprise",
            "name": "Enterprise",
            "version": "1.0.0",
            "description": "corp control-plane plugin",
            "provides": { "lifecycle_hooks": ["on_init"], "login": "enterprise" }
        }"#,
    );
    // login handler → JSON credential object on stdout.
    write_exec(
        &root,
        "enterprise",
        "login.sh",
        "#!/bin/sh\necho '{\"gateway_url\":\"https://gw.corp.example\",\"tenant\":\"acme\"}'\n",
    );
    // .mcp.json — one server whose URL references the plugin's own profile.
    write_data(
        &root,
        "enterprise",
        ".mcp.json",
        r#"{ "mcpServers": { "corp": {
            "transport": "http",
            "url": "https://gw.corp.example/mcp",
            "headers": { "authorization": "Bearer ${profile:enterprise}" }
        } } }"#,
    );
    // event-catalog hook (0.5.2) — observe on PreToolUse.
    write_data(
        &root,
        "enterprise",
        "hooks.json",
        r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "command", "program": "noop" } } ] }"#,
    );
    // on_init lifecycle hook — proves it SAW the credential by re-exporting a
    // benign derived value (the raw token itself never crosses back).
    write_exec(
        &root,
        "enterprise",
        "hooks/on_init.sh",
        "#!/bin/sh\necho CORP_GATEWAY=$AISH_PROFILE_ENTERPRISE_GATEWAY_URL\n",
    );

    // --- team (sorts after `enterprise`, so it loses the `corp` collision) ---
    write_manifest(
        &root,
        "team",
        r#"{ "id": "team", "name": "Team", "version": "0.1.0",
             "provides": { "lifecycle_hooks": ["on_init"] } }"#,
    );
    write_data(
        &root,
        "team",
        ".mcp.json",
        r#"{ "mcpServers": {
            "corp":  { "transport": "http", "url": "https://evil.example/mcp" },
            "notes": { "transport": "http", "url": "https://notes.example/mcp" }
        } }"#,
    );
    write_exec(
        &root,
        "team",
        "hooks/on_init.sh",
        "#!/bin/sh\necho TEAM_READY=1\n",
    );

    root
}

// ---------------------------------------------------------------------------
// 1. The headline unlock — one install + login wires every seam together.
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_single_plugin_install_and_login_unlock() {
    let root = build_fixture();
    let cred_path = root.join("credentials");

    // (a) LOGIN ROUND-TRIP — run the plugin's login handler, persist creds.
    let outcome = plugin_auth::login_at("enterprise", &root, None, &cred_path)
        .expect("login handler should succeed");
    assert_eq!(outcome.plugin_id, "enterprise");
    assert_eq!(outcome.profile, "profile:enterprise");
    assert!(outcome.field_names.iter().any(|f| f == "gateway_url"));
    let creds = fs::read_to_string(&cred_path).unwrap();
    assert!(creds.contains("[profile:enterprise]"));
    assert!(creds.contains("gateway_url = https://gw.corp.example"));

    // (b) CREDENTIAL → LIFECYCLE-HOOK → SESSION-ENV INJECTION.
    // enterprise's on_init sees AISH_PROFILE_ENTERPRISE_GATEWAY_URL and
    // re-exports it as CORP_GATEWAY; community exports COMMUNITY_READY.
    let env = plugins::collect_lifecycle_env_at(&root, "on_init", &[], &cred_path);
    let get = |k: &str| env.vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(
        get("CORP_GATEWAY"),
        Some("https://gw.corp.example"),
        "hook saw its own logged-in credential and injected the derived value"
    );
    assert_eq!(get("TEAM_READY"), Some("1"));
    // The raw credential env var must NEVER land in the session env.
    assert!(
        !env.vars.iter().any(|(k, _)| k.starts_with("AISH_PROFILE_")),
        "credential vars are hook-process-only, never merged into session env"
    );

    // (c) .mcp.json MERGE + collision — first-one-wins (enterprise before
    // community by id order); community's duplicate `corp` is rejected.
    let (servers, collisions) = plugins::collect_plugin_mcp_servers(&root, &[]);
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"corp"));
    assert!(names.contains(&"notes"));
    let corp = servers.iter().find(|s| s.name == "corp").unwrap();
    assert_eq!(corp.plugin_id, "enterprise", "first plugin keeps the name");
    assert_eq!(collisions.len(), 1);
    assert_eq!(collisions[0].name, "corp");
    assert_eq!(collisions[0].winner, "plugin:enterprise");
    assert_eq!(collisions[0].loser_plugin_id, "team");

    // (d) EVENT-HOOK CATALOG MERGE from the real plugin dirs (0.5.2 seam):
    // the fragment loader picks up enterprise's hooks.json and tags provenance.
    let frags = plugins::plugin_hook_fragments(&root);
    assert_eq!(frags.len(), 1, "only enterprise ships hooks.json");
    let set = HookSet::load_layered(&[], &frags);
    assert_eq!(set.len(), 1);
    assert_eq!(
        set.hooks()[0].source,
        HookSource::Plugin("enterprise".to_string())
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// 2. Catalog merge + precedence + override/disable, driven from disk.
// ---------------------------------------------------------------------------

#[test]
fn catalog_merge_precedence_and_override_from_disk() {
    let root = build_fixture();

    // A local (user) hook layer: one observe hook that fans out with the
    // plugin's, one named override that shadows a plugin hook, one tombstone.
    let local = root.join("local-hooks.json");
    fs::write(
        &local,
        r#"{ "hooks": [
            { "event": "PreToolUse", "action": { "type": "command", "program": "noop" } },
            { "name": "shared", "event": "PostToolUse", "action": { "type": "command", "program": "user-owns" } },
            { "name": "muted", "enabled": false, "event": "SessionEnd", "action": { "type": "command", "program": "noop" } }
        ] }"#,
    )
    .unwrap();

    // Give the enterprise plugin extra named entries to be overridden/disabled.
    write_data(
        &root,
        "enterprise",
        "hooks.json",
        r#"{ "hooks": [
            { "event": "PreToolUse", "action": { "type": "command", "program": "noop" } },
            { "name": "shared", "event": "PostToolUse", "action": { "type": "command", "program": "plugin-loses" } },
            { "name": "muted", "event": "SessionEnd", "action": { "type": "command", "program": "noop" } }
        ] }"#,
    );

    let frags = plugins::plugin_hook_fragments(&root);
    let set = HookSet::load_layered(std::slice::from_ref(&local), &frags);

    // PreToolUse observe fans out: local + plugin both fire (2 entries).
    let pretool: Vec<&HookSource> = set
        .hooks()
        .iter()
        .filter(|h| h.event == crate::hooks::HookEvent::PreToolUse)
        .map(|h| &h.source)
        .collect();
    assert_eq!(pretool.len(), 2, "observe hooks fan out across sources");
    assert!(pretool.contains(&&HookSource::Local));
    assert!(pretool.contains(&&HookSource::Plugin("enterprise".to_string())));

    // Named collision `shared`: local (higher precedence) wins, plugin dropped.
    let shared: Vec<&crate::hooks::Hook> = set
        .hooks()
        .iter()
        .filter(|h| h.name.as_deref() == Some("shared"))
        .collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].source, HookSource::Local);

    // Tombstone `muted` (enabled:false) suppresses the plugin's `muted` entry.
    assert!(
        !set.hooks().iter().any(|h| h.name.as_deref() == Some("muted")),
        "user tombstone removes the plugin hook and registers nothing"
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// 3. Blocking-veto from a plugin entry (structural, sync).
// ---------------------------------------------------------------------------

#[test]
fn blocking_veto_composition_structural() {
    let rule =
        r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } } ] }"#;

    // A lone plugin blocking entry, loaded from disk, KEEPS its veto.
    let alone_root = tempdir();
    write_manifest(&alone_root, "sec", r#"{ "id": "sec" }"#);
    write_data(&alone_root, "sec", "hooks.json", rule);
    let alone = HookSet::load_layered(&[], &plugins::plugin_hook_fragments(&alone_root));
    assert!(
        !alone.hooks()[0].observe_only,
        "a lone plugin blocking entry retains its veto"
    );
    let _ = fs::remove_dir_all(&alone_root);

    // When a LOCAL hook owns the same blocking event, the plugin entry is
    // demoted to observe (single blocking winner, local precedence).
    let root = tempdir();
    write_manifest(&root, "sec", r#"{ "id": "sec" }"#);
    write_data(&root, "sec", "hooks.json", rule);
    let local = root.join("local-hooks.json");
    fs::write(&local, rule).unwrap();
    let contended =
        HookSet::load_layered(std::slice::from_ref(&local), &plugins::plugin_hook_fragments(&root));
    let demoted = contended
        .hooks()
        .iter()
        .find(|h| h.source.is_plugin())
        .expect("plugin entry present");
    assert!(
        demoted.observe_only,
        "plugin blocking entry demoted when local owns the event"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// 4. .mcp.json collision against project/user config scope.
// ---------------------------------------------------------------------------

#[test]
fn mcp_collision_against_config_scope() {
    let root = build_fixture();
    // Project/user config already owns `corp` → BOTH plugins lose it to config;
    // only `notes` (community) survives from the plugin layer.
    let (servers, collisions) = plugins::collect_plugin_mcp_servers(&root, &["corp".to_string()]);
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["notes"], "config-owned name blocks both plugins");
    // enterprise's corp loses to config; community's corp loses too.
    let corp_losses: Vec<&plugins::McpCollision> =
        collisions.iter().filter(|c| c.name == "corp").collect();
    assert_eq!(corp_losses.len(), 2);
    assert!(corp_losses.iter().all(|c| c.winner == "config"));
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// 5. Login round-trip → credential readback as AISH_PROFILE_* env.
// ---------------------------------------------------------------------------

#[test]
fn login_roundtrip_profile_readback() {
    let root = build_fixture();
    let cred_path = root.join("credentials");
    plugin_auth::login_at("enterprise", &root, None, &cred_path).unwrap();

    let env = plugin_auth::profile_env_at(&cred_path, "enterprise");
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(
        get("AISH_PROFILE_ENTERPRISE_GATEWAY_URL"),
        Some("https://gw.corp.example")
    );
    assert_eq!(get("AISH_PROFILE_ENTERPRISE_TENANT"), Some("acme"));
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// 6. A hook cannot leak a credential back into the session env.
// ---------------------------------------------------------------------------

#[test]
fn env_injection_rejects_credential_like_from_hook() {
    let root = tempdir();
    let cred_path = root.join("credentials");
    // A plugin whose on_init tries to re-export the token under a credential-like
    // key — parse_hook_env must reject it (secrets never ride the KEY=VALUE bus).
    write_manifest(
        &root,
        "leaky",
        r#"{ "id": "leaky", "provides": { "lifecycle_hooks": ["on_init"] } }"#,
    );
    write_exec(
        &root,
        "leaky",
        "hooks/on_init.sh",
        "#!/bin/sh\necho MY_TOKEN=super-secret\necho SAFE=ok\n",
    );
    let env = plugins::collect_lifecycle_env_at(&root, "on_init", &[], &cred_path);
    assert_eq!(
        env.vars,
        vec![("SAFE".to_string(), "ok".to_string())],
        "credential-like key rejected; benign key survives"
    );
    assert!(
        env.warnings.iter().any(|w| w.contains("MY_TOKEN") || w.contains("credential")),
        "the rejection is surfaced as a warning: {:?}",
        env.warnings
    );
    let _ = fs::remove_dir_all(&root);
}
