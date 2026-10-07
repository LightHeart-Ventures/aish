//! Docs-completeness gate for the plugin developer guides (TASK-275, Phase 12.6).
//!
//! The manifest structs are the source of truth for what `plugin.json` accepts.
//! These tests read the Rust sources, extract every deserialized field of the
//! manifest structs, and assert each one is documented — so adding a manifest
//! key without documenting it fails CI. They also assert every `:plugin`
//! subcommand is documented and that relative links in the guides resolve.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The serde-visible field names of `pub struct <name> { … }` in `src`:
/// `pub <field>:` lines, honoring `#[serde(rename = "…")]` and skipping
/// `#[serde(skip…)]` fields.
fn struct_fields(src: &str, name: &str) -> Vec<String> {
    let header = format!("pub struct {name} {{");
    let start = src
        .find(&header)
        .unwrap_or_else(|| panic!("struct {name} not found"));
    let body = &src[start + header.len()..];
    let end = body.find("\n}").expect("struct body terminates");
    let mut fields = Vec::new();
    let mut rename: Option<String> = None;
    let mut skip = false;
    for line in body[..end].lines() {
        let t = line.trim();
        if t.starts_with("#[serde(") {
            if let Some(i) = t.find("rename = \"") {
                let rest = &t[i + "rename = \"".len()..];
                rename = rest.split('"').next().map(str::to_string);
            }
            if t.contains("skip") {
                skip = true;
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("pub ") {
            if let Some((field, _)) = rest.split_once(':') {
                if !skip {
                    fields.push(rename.take().unwrap_or_else(|| field.trim().to_string()));
                }
            }
            rename = None;
            skip = false;
        }
    }
    assert!(!fields.is_empty(), "struct {name} has no fields?");
    fields
}

fn assert_documented(doc_rel: &str, src_rel: &str, structs: &[&str]) {
    let doc = read(doc_rel);
    let src = read(src_rel);
    let mut missing = Vec::new();
    for s in structs {
        for f in struct_fields(&src, s) {
            if !doc.contains(&format!("`{f}`")) {
                missing.push(format!("{s}.{f}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{doc_rel} does not document these manifest fields (add them as `field`): {missing:?}"
    );
}

#[test]
fn every_shell_manifest_field_is_documented() {
    assert_documented(
        "docs/PLUGIN_DEVELOPER.md",
        "src/plugins.rs",
        &[
            "PluginManifest",
            "Provides",
            "PluginStatusline",
            "PluginTimer",
            "SkillSource",
        ],
    );
}

#[test]
fn every_webhook_manifest_field_is_documented() {
    let src = "crates/aish-webhook-client/src/dispatcher.rs";
    assert_documented("docs/WEBHOOK_HANDLERS.md", src, &["WebhookHandler"]);
    assert_documented(
        "docs/PLUGIN_DEVELOPER.md",
        src,
        &["PluginManifest", "WebhookHandler"],
    );
}

#[test]
fn every_plugin_subcommand_is_documented() {
    let repl = read("src/repl.rs");
    let start = repl
        .find("fn handle_plugin(")
        .expect("handle_plugin exists");
    let body = &repl[start..];
    let end = body[1..].find("\nfn ").map(|i| i + 1).unwrap_or(body.len());
    let body = &body[..end];
    let doc = read("docs/PLUGIN_DEVELOPER.md");
    let mut missing = Vec::new();
    for line in body.lines() {
        let t = line.trim_start();
        // Match-arm heads only: `Some("verb"…) =>` at the subcommand level.
        let Some(rest) = t.strip_prefix("Some(\"") else {
            continue;
        };
        if !t.contains("=>") {
            continue;
        }
        let verb = rest.split('"').next().unwrap_or("");
        if verb.is_empty()
            || verb.starts_with('-')
            || verb == "list" && doc.contains(":plugin list")
        {
            continue;
        }
        if !doc.contains(&format!(":plugin {verb}")) {
            missing.push(verb.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "docs/PLUGIN_DEVELOPER.md does not document `:plugin` subcommands: {missing:?}"
    );
}

/// Relative markdown links in the new guides must point at files that exist.
#[test]
fn guide_relative_links_resolve() {
    for doc in [
        "docs/PLUGIN_DEVELOPER.md",
        "docs/WEBHOOK_HANDLERS.md",
        "plugins/github/README.md",
    ] {
        let text = read(doc);
        let base = root().join(doc);
        let base = base.parent().unwrap();
        let mut broken = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("](") {
            rest = &rest[i + 2..];
            let Some(close) = rest.find(')') else { break };
            let target = &rest[..close];
            rest = &rest[close..];
            if target.starts_with("http") || target.starts_with('#') || target.contains(' ') {
                continue;
            }
            let path = target.split('#').next().unwrap();
            if !Path::new(&base.join(path)).exists() {
                broken.push(target.to_string());
            }
        }
        assert!(broken.is_empty(), "{doc}: broken relative links {broken:?}");
    }
}
