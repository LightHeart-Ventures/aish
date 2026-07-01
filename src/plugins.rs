//! Plugin discovery — the minimal, shipping slice of the plugin system.
//!
//! Plugins live in `~/.aish/plugins/<plugin-id>/` and, for now, can contribute
//! **skills** that expand the shell's skill registry. A plugin is any
//! subdirectory holding a readable, parseable `plugin.json`; its skills live in
//! `<plugin>/skills/<skill-name>/SKILL.md` — the exact on-disk layout
//! [`crate::skills::load`] already understands, so a plugin's skills flow into
//! the same catalog the agent sees for `~/.aish/skills`.
//!
//! This is deliberately the smallest useful piece of the broader design in
//! `docs/PLUGIN_SYSTEM_DESIGN.md` (webhooks, hooks, MCP servers, tools, memory,
//! schemas). Everything not needed to expand the skill registry is ignored —
//! unknown `plugin.json` keys parse and are dropped — so a richer manifest still
//! loads today and future phases can grow into it without breaking existing
//! plugins.

use crate::skills::Skill;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Minimal `plugin.json` manifest. Only the fields the skill-expansion slice
/// needs are parsed; every other key in the design doc (webhooks, hooks,
/// config_schema, provides, …) is ignored via serde's default
/// unknown-field-dropping so a fuller manifest still deserializes.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // name/version/description are parsed manifest surface, consumed by later plugin phases (docs/PLUGIN_SYSTEM_DESIGN.md)
pub struct PluginManifest {
    /// Stable plugin identifier, e.g. `"hello-world"`.
    pub id: String,
    /// Human-facing name (defaults to `id` when omitted).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Plugins are enabled unless the manifest explicitly sets `false`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Phase 1.6 webhook opt-in: an HTTP endpoint lifecycle events are POSTed
    /// to. Consumed by [`crate::plugin_dispatcher`]; parsed here so the field is
    /// part of the canonical manifest surface (not silently dropped).
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Phase 1.6 webhook opt-in: a shell command run on each lifecycle event
    /// (event JSON on stdin). Consumed by [`crate::plugin_dispatcher`].
    #[serde(default)]
    pub webhook_command: Option<String>,
}

impl PluginManifest {
    /// A plugin ships enabled unless it opts out with `"enabled": false`.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// A discovered plugin: its parsed manifest, on-disk directory, and the skills
/// it contributes to the registry.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `dir` is retained for later phases (hooks/tools/webhooks resolve paths relative to it)
pub struct Plugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    pub skills: Vec<Skill>,
}

/// The default plugins directory: `~/.aish/plugins`. Mirrors the skills
/// directory's HOME-derived resolution so startup and reload agree. Retained as
/// the canonical resolver for later phases and callers that don't derive the
/// path from a sibling skills dir.
#[allow(dead_code)]
pub fn default_plugins_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("plugins")
}

/// Discover every valid, enabled plugin under `dir`. Missing dir → no plugins;
/// a subdirectory without a readable/parseable `plugin.json`, or one explicitly
/// disabled, is skipped silently (mirrors [`crate::skills::load`]'s forgiving
/// contract — a malformed plugin never blocks startup). Result is sorted by
/// plugin id for deterministic ordering.
pub fn discover(dir: &Path) -> Vec<Plugin> {
    let mut plugins = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return plugins;
    };
    for entry in entries.flatten() {
        let pdir = entry.path();
        if !pdir.is_dir() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(pdir.join("plugin.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<PluginManifest>(&text) else {
            continue;
        };
        if !manifest.is_enabled() {
            continue;
        }
        let skills = crate::skills::load(&pdir.join("skills"));
        plugins.push(Plugin {
            manifest,
            dir: pdir,
            skills,
        });
    }
    plugins.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    plugins
}

/// Flatten every discovered plugin's skills into one list — the skill-registry
/// expansion consumed by [`crate::skills::load_catalog`]. Ordered by plugin id
/// then skill name (each plugin's `skills::load` already pre-sorts by name).
pub fn plugin_skills(dir: &Path) -> Vec<Skill> {
    discover(dir).into_iter().flat_map(|p| p.skills).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write a plugin at `<root>/<id>/` with the given manifest JSON and,
    /// optionally, one skill named `skill_name`.
    fn write_plugin(root: &Path, id: &str, manifest: &str, skill: Option<(&str, &str)>) {
        let pdir = root.join(id);
        fs::create_dir_all(&pdir).unwrap();
        fs::write(pdir.join("plugin.json"), manifest).unwrap();
        if let Some((skill_name, desc)) = skill {
            let sdir = pdir.join("skills").join(skill_name);
            fs::create_dir_all(&sdir).unwrap();
            fs::write(
                sdir.join("SKILL.md"),
                format!("---\nname: {skill_name}\ndescription: {desc}\n---\nbody\n"),
            )
            .unwrap();
        }
    }

    #[test]
    fn discovers_plugin_and_expands_skills() {
        let tmp = tempdir();
        write_plugin(
            &tmp,
            "hello-world",
            r#"{"id":"hello-world","name":"Hello World","version":"0.1.0"}"#,
            Some(("hello-world", "Greet the world.")),
        );
        let plugins = discover(&tmp);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.id, "hello-world");
        assert_eq!(plugins[0].skills.len(), 1);

        let skills = plugin_skills(&tmp);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "hello-world");
        assert_eq!(skills[0].description, "Greet the world.");
    }

    #[test]
    fn disabled_plugin_is_skipped() {
        let tmp = tempdir();
        write_plugin(
            &tmp,
            "off",
            r#"{"id":"off","enabled":false}"#,
            Some(("off-skill", "Should not load.")),
        );
        assert!(discover(&tmp).is_empty());
        assert!(plugin_skills(&tmp).is_empty());
    }

    #[test]
    fn malformed_or_missing_manifest_is_skipped() {
        let tmp = tempdir();
        // Directory with no plugin.json.
        fs::create_dir_all(tmp.join("no-manifest")).unwrap();
        // Directory with invalid JSON.
        let bad = tmp.join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("plugin.json"), "{ not json").unwrap();
        assert!(discover(&tmp).is_empty());
    }

    #[test]
    fn missing_dir_yields_no_plugins() {
        let tmp = tempdir();
        assert!(discover(&tmp.join("does-not-exist")).is_empty());
    }

    #[test]
    fn multiple_plugins_sorted_by_id() {
        let tmp = tempdir();
        write_plugin(&tmp, "zeta", r#"{"id":"zeta"}"#, Some(("z-skill", "Z.")));
        write_plugin(&tmp, "alpha", r#"{"id":"alpha"}"#, Some(("a-skill", "A.")));
        let plugins = discover(&tmp);
        let ids: Vec<_> = plugins.iter().map(|p| p.manifest.id.clone()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }

    /// A private, dependency-free temp dir (the crate doesn't pull in the
    /// `tempfile` crate for this module — mirror skills.rs's test helper).
    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "aish-plugins-test-{}-{}",
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
}
