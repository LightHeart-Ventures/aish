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
use serde_json::Value;
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
    /// Optional JSON-Schema-shaped description of the plugin's configuration
    /// (`{ "type": "object", "properties": {...}, "required": [...] }`). Drives
    /// default-filling and validation in [`load_config`] (Phase 1.4). Absent →
    /// the plugin takes no configuration and `load_config` yields `{}`.
    #[serde(default)]
    pub config_schema: Option<Value>,
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
    /// The plugin's resolved configuration (Phase 1.4): `config.json` overlaid
    /// on `config_schema` defaults, with every `${env:VAR}` reference expanded
    /// and required/type rules validated. `None` when config loading failed
    /// (malformed `config.json`, unset `${env:VAR}`, missing required key, or a
    /// type mismatch) — discovery stays forgiving and still contributes the
    /// plugin's skills, mirroring the loader's "a broken plugin never blocks
    /// startup" contract. The concrete failure is available on demand via
    /// [`load_config`].
    pub config: Option<Value>,
}

/// Why a plugin's configuration could not be resolved (Phase 1.4). Kept as a
/// typed error so `:plugin config` / `:plugin info` can report the exact cause
/// rather than a generic "config failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `config.json` existed but wasn't valid JSON, or wasn't a JSON object.
    Malformed(String),
    /// A `${env:VAR}` reference pointed at an environment variable that is unset.
    MissingEnv { key: String, var: String },
    /// A key listed in `config_schema.required` was absent or `null`.
    MissingRequired(String),
    /// A value's JSON type didn't match its `config_schema` `type`.
    TypeMismatch {
        key: String,
        expected: String,
        got: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Malformed(e) => write!(f, "malformed config.json: {e}"),
            ConfigError::MissingEnv { key, var } => {
                write!(f, "config key `{key}` references unset environment variable `{var}`")
            }
            ConfigError::MissingRequired(k) => write!(f, "required config key `{k}` is missing"),
            ConfigError::TypeMismatch { key, expected, got } => {
                write!(f, "config key `{key}` should be {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Load and resolve a plugin's configuration (Phase 1.4).
///
/// Pipeline: read `<plugin_dir>/config.json` (absent → start empty) → fill any
/// missing keys from `config_schema.properties.<k>.default` → expand every
/// `${env:VAR}` reference against the process environment → validate `required`
/// keys and declared value `type`s. Returns the resolved config object, or the
/// first [`ConfigError`] encountered. Secret values live only in the
/// environment: `config.json` and `plugin.json` carry `${env:VAR}` references,
/// never the resolved secret.
pub fn load_config(plugin_dir: &Path, manifest: &PluginManifest) -> Result<Value, ConfigError> {
    load_config_with(plugin_dir, manifest, &|name| std::env::var(name).ok())
}

/// [`load_config`] with an injectable environment lookup — the seam the tests
/// use so they never mutate the (process-global, racy) real environment.
fn load_config_with<F>(
    plugin_dir: &Path,
    manifest: &PluginManifest,
    get_env: &F,
) -> Result<Value, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    // 1. Base layer: config.json, if present. Missing file → empty object;
    //    present-but-broken → hard error (a plugin author's typo shouldn't
    //    silently run with defaults).
    let mut config = match std::fs::read_to_string(plugin_dir.join("config.json")) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|e| ConfigError::Malformed(e.to_string()))?
            .as_object()
            .cloned()
            .ok_or_else(|| ConfigError::Malformed("config.json is not a JSON object".into()))?,
        Err(_) => serde_json::Map::new(),
    };

    let schema = manifest.config_schema.clone().unwrap_or(Value::Null);

    // 2. Fill missing keys from schema defaults.
    for (k, prop) in schema_properties(&schema) {
        if !config.contains_key(&k) {
            if let Some(def) = prop.get("default") {
                config.insert(k, def.clone());
            }
        }
    }

    // 3. Expand ${env:VAR} everywhere (strings, recursively through arrays/objects).
    let resolved = resolve_env_refs("", Value::Object(config), get_env)?;
    let config = match resolved {
        Value::Object(m) => m,
        _ => unreachable!("resolve_env_refs preserves the object shape"),
    };

    // 4. Validate required keys + declared types.
    validate_config(&schema, &config)?;

    Ok(Value::Object(config))
}

/// `config_schema.properties` as a map, or empty when the schema is absent or
/// shaped unexpectedly.
fn schema_properties(schema: &Value) -> serde_json::Map<String, Value> {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Recursively expand `${env:VAR}` references inside a JSON value. `key` is the
/// dotted path used only for error messages.
fn resolve_env_refs<F>(key: &str, val: Value, get_env: &F) -> Result<Value, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    match val {
        Value::String(s) => Ok(Value::String(interpolate_env(key, &s, get_env)?)),
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, item) in arr.into_iter().enumerate() {
                out.push(resolve_env_refs(&format!("{key}[{i}]"), item, get_env)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, item) in map {
                let child = if key.is_empty() {
                    k.clone()
                } else {
                    format!("{key}.{k}")
                };
                out.insert(k, resolve_env_refs(&child, item, get_env)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other),
    }
}

/// Substitute every `${env:NAME}` occurrence in `s`. An unset variable is a hard
/// [`ConfigError::MissingEnv`] (fail-closed — a half-resolved secret is worse
/// than a clear error). A `${env:` with no closing `}` is left verbatim.
fn interpolate_env<F>(key: &str, s: &str, get_env: &F) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    const OPEN: &str = "${env:";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        match after.find('}') {
            Some(end) => {
                let var = &after[..end];
                match get_env(var) {
                    Some(v) => out.push_str(&v),
                    None => {
                        return Err(ConfigError::MissingEnv {
                            key: key.to_string(),
                            var: var.to_string(),
                        });
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated reference — emit the rest untouched and stop.
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Validate `required` presence and declared value `type`s against the schema.
fn validate_config(
    schema: &Value,
    config: &serde_json::Map<String, Value>,
) -> Result<(), ConfigError> {
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for r in required {
            if let Some(name) = r.as_str() {
                match config.get(name) {
                    None | Some(Value::Null) => {
                        return Err(ConfigError::MissingRequired(name.to_string()));
                    }
                    _ => {}
                }
            }
        }
    }
    for (k, prop) in schema_properties(schema) {
        let Some(v) = config.get(&k) else { continue };
        if v.is_null() {
            continue;
        }
        if let Some(expected) = prop.get("type").and_then(|t| t.as_str()) {
            if !json_type_matches(expected, v) {
                return Err(ConfigError::TypeMismatch {
                    key: k,
                    expected: expected.to_string(),
                    got: json_type_name(v).to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Does a JSON value satisfy a JSON-Schema `type` keyword? `integer` additionally
/// requires the number to have no fractional part.
fn json_type_matches(expected: &str, v: &Value) -> bool {
    match expected {
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        // Unknown/unsupported type keyword → don't reject.
        _ => true,
    }
}

/// A short human name for a JSON value's type, for error messages.
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::Null => "null",
    }
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
        // Phase 1.4: resolve config best-effort. A config error (unset
        // `${env:VAR}`, missing required key, …) yields `None` but never drops
        // the plugin — its skills still load, preserving the "a broken plugin
        // never blocks startup" contract.
        let config = load_config(&pdir, &manifest).ok();
        plugins.push(Plugin {
            manifest,
            dir: pdir,
            skills,
            config,
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

    // ---- Phase 1.4: config loading + ${env:VAR} substitution ----

    /// Parse a manifest string into a `PluginManifest` for direct config tests.
    fn manifest(json: &str) -> PluginManifest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn no_schema_and_no_config_yields_empty_object() {
        let tmp = tempdir();
        let m = manifest(r#"{"id":"p"}"#);
        let cfg = load_config_with(&tmp, &m, &|_| None).unwrap();
        assert_eq!(cfg, serde_json::json!({}));
    }

    #[test]
    fn schema_defaults_are_applied() {
        let tmp = tempdir();
        let m = manifest(
            r#"{"id":"p","config_schema":{"type":"object","properties":{
                "greeting":{"type":"string","default":"Hello, World!"},
                "retries":{"type":"integer","default":3}}}}"#,
        );
        let cfg = load_config_with(&tmp, &m, &|_| None).unwrap();
        assert_eq!(cfg["greeting"], "Hello, World!");
        assert_eq!(cfg["retries"], 3);
    }

    #[test]
    fn config_json_overrides_schema_default() {
        let tmp = tempdir();
        fs::write(tmp.join("config.json"), r#"{"greeting":"Hola"}"#).unwrap();
        let m = manifest(
            r#"{"id":"p","config_schema":{"type":"object","properties":{
                "greeting":{"type":"string","default":"Hello, World!"}}}}"#,
        );
        let cfg = load_config_with(&tmp, &m, &|_| None).unwrap();
        assert_eq!(cfg["greeting"], "Hola");
    }

    #[test]
    fn env_reference_is_substituted() {
        let tmp = tempdir();
        fs::write(
            tmp.join("config.json"),
            r#"{"token":"${env:HELLO_TOKEN}","url":"https://x/${env:HELLO_ID}/hook"}"#,
        )
        .unwrap();
        let m = manifest(r#"{"id":"p"}"#);
        let env = |name: &str| match name {
            "HELLO_TOKEN" => Some("sekret".to_string()),
            "HELLO_ID" => Some("42".to_string()),
            _ => None,
        };
        let cfg = load_config_with(&tmp, &m, &env).unwrap();
        assert_eq!(cfg["token"], "sekret");
        assert_eq!(cfg["url"], "https://x/42/hook");
    }

    #[test]
    fn env_reference_resolves_inside_nested_structures() {
        let tmp = tempdir();
        fs::write(
            tmp.join("config.json"),
            r#"{"headers":{"Authorization":"Bearer ${env:TOK}"},"scopes":["${env:SCOPE}","read"]}"#,
        )
        .unwrap();
        let m = manifest(r#"{"id":"p"}"#);
        let env = |name: &str| match name {
            "TOK" => Some("abc".to_string()),
            "SCOPE" => Some("write".to_string()),
            _ => None,
        };
        let cfg = load_config_with(&tmp, &m, &env).unwrap();
        assert_eq!(cfg["headers"]["Authorization"], "Bearer abc");
        assert_eq!(cfg["scopes"][0], "write");
        assert_eq!(cfg["scopes"][1], "read");
    }

    #[test]
    fn unset_env_reference_errors() {
        let tmp = tempdir();
        fs::write(tmp.join("config.json"), r#"{"token":"${env:NOPE}"}"#).unwrap();
        let m = manifest(r#"{"id":"p"}"#);
        let err = load_config_with(&tmp, &m, &|_| None).unwrap_err();
        assert_eq!(
            err,
            ConfigError::MissingEnv {
                key: "token".into(),
                var: "NOPE".into()
            }
        );
    }

    #[test]
    fn env_default_reference_is_resolved() {
        // A default value may itself be an env reference.
        let tmp = tempdir();
        let m = manifest(
            r#"{"id":"p","config_schema":{"type":"object","properties":{
                "user":{"type":"string","default":"${env:WHO}"}}}}"#,
        );
        let cfg = load_config_with(&tmp, &m, &|n| (n == "WHO").then(|| "ada".to_string())).unwrap();
        assert_eq!(cfg["user"], "ada");
    }

    #[test]
    fn required_key_missing_errors() {
        let tmp = tempdir();
        let m = manifest(
            r#"{"id":"p","config_schema":{"type":"object",
                "properties":{"token":{"type":"string"}},"required":["token"]}}"#,
        );
        let err = load_config_with(&tmp, &m, &|_| None).unwrap_err();
        assert_eq!(err, ConfigError::MissingRequired("token".into()));
    }

    #[test]
    fn required_key_present_passes() {
        let tmp = tempdir();
        fs::write(tmp.join("config.json"), r#"{"token":"${env:T}"}"#).unwrap();
        let m = manifest(
            r#"{"id":"p","config_schema":{"type":"object",
                "properties":{"token":{"type":"string"}},"required":["token"]}}"#,
        );
        let cfg = load_config_with(&tmp, &m, &|_| Some("v".into())).unwrap();
        assert_eq!(cfg["token"], "v");
    }

    #[test]
    fn type_mismatch_errors() {
        let tmp = tempdir();
        fs::write(tmp.join("config.json"), r#"{"retries":"lots"}"#).unwrap();
        let m = manifest(
            r#"{"id":"p","config_schema":{"type":"object","properties":{
                "retries":{"type":"integer"}}}}"#,
        );
        let err = load_config_with(&tmp, &m, &|_| None).unwrap_err();
        assert_eq!(
            err,
            ConfigError::TypeMismatch {
                key: "retries".into(),
                expected: "integer".into(),
                got: "string".into()
            }
        );
    }

    #[test]
    fn malformed_config_json_errors() {
        let tmp = tempdir();
        fs::write(tmp.join("config.json"), "{ not json").unwrap();
        let m = manifest(r#"{"id":"p"}"#);
        assert!(matches!(
            load_config_with(&tmp, &m, &|_| None),
            Err(ConfigError::Malformed(_))
        ));
    }

    #[test]
    fn discover_attaches_resolved_config() {
        let tmp = tempdir();
        write_plugin(
            &tmp,
            "cfg",
            r#"{"id":"cfg","config_schema":{"type":"object","properties":{
                "greeting":{"type":"string","default":"Hi"}}}}"#,
            Some(("cfg-skill", "does a thing")),
        );
        let plugins = discover(&tmp);
        assert_eq!(plugins.len(), 1);
        let cfg = plugins[0].config.as_ref().expect("config resolved");
        assert_eq!(cfg["greeting"], "Hi");
    }

    #[test]
    fn discover_is_forgiving_of_config_errors() {
        // A plugin whose required config can't resolve still loads its skills;
        // only `config` is `None`.
        let tmp = tempdir();
        write_plugin(
            &tmp,
            "needs-env",
            r#"{"id":"needs-env","config_schema":{"type":"object",
                "properties":{"token":{"type":"string"}},"required":["token"]}}"#,
            Some(("s", "d")),
        );
        let plugins = discover(&tmp);
        assert_eq!(plugins.len(), 1);
        assert!(plugins[0].config.is_none());
        assert_eq!(plugins[0].skills.len(), 1, "skills still load");
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

    /// End-to-end against the shipped `examples/plugins/hello-world` fixture:
    /// real `plugin.json` (`config_schema` with a default + `${env:USER}`
    /// reference) overlaid by the real `config.json` (overrides `greeting`,
    /// sets `shout`). Proves Phase 1.4 resolves the on-disk example plugin, not
    /// just synthetic temp fixtures.
    #[test]
    fn example_hello_world_plugin_config_resolves() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("plugins")
            .join("hello-world");
        let text = fs::read_to_string(dir.join("plugin.json")).unwrap();
        let m: PluginManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(m.id, "hello-world");

        let env = |n: &str| (n == "USER").then(|| "ada".to_string());
        let cfg = load_config_with(&dir, &m, &env).unwrap();
        // config.json overrides the schema default…
        assert_eq!(cfg["greeting"], "¡Hola, mundo!");
        assert_eq!(cfg["shout"], true);
        // …and the schema default's ${env:USER} reference is expanded.
        assert_eq!(cfg["greeter"], "ada");
    }

    /// A private, dependency-free temp dir (the crate doesn't pull in the
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
