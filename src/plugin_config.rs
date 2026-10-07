//! `:plugin config` — view and edit a plugin's configuration (TASK-271, SPR-104).
//!
//! A plugin's configuration lives at `~/.aish/plugins/<id>/config.json` and is
//! overlaid on the manifest's `config_schema` property defaults, with every
//! `${env:VAR}` reference expanded (Phase 1.4, [`crate::plugins::load_config`]).
//! This module is the user-facing management layer on top of that model:
//!
//! * [`view`] / [`format_view`] — the EFFECTIVE config with per-key provenance
//!   (`default` / `file`, `env` when it came through a `${env:…}` reference)
//!   and credential-like values redacted;
//! * [`set_key`] — set one top-level key; the WHOLE resulting config is
//!   validated (discovery's `validate_config` + the `config_schema` JSON-Schema
//!   validator) before an atomic write, so an invalid value never lands;
//! * [`reset`] — drop one key (back to its default) or the whole file.
//!
//! **PO decision (recorded on the card):** the location stays the plugin-dir
//! `config.json` — no `~/.aish/config/plugins/{id}.json` and no
//! `broker.json` (broker/client config stays env-only, PR #777).
//!
//! Discovery/load paths in `plugins.rs` are deliberately untouched (TASK-274
//! owns config-error handling at discovery); this module only reuses its
//! helpers.

use crate::plugins::{
    PluginManifest, is_credential_like, resolve_env_refs, schema_properties, validate_config,
    validate_json_schema,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Placeholder shown instead of a credential-like value.
pub const REDACTED: &str = "<redacted>";

/// The plugin's user config file: `<plugin_dir>/config.json`.
pub fn config_path(plugin_dir: &Path) -> PathBuf {
    plugin_dir.join("config.json")
}

/// Read `config.json` as an object map. Absent → empty map; present but not
/// valid JSON (or not an object) → `Err` with the reason.
pub fn read_file_map(plugin_dir: &Path) -> Result<Map<String, Value>, String> {
    let path = config_path(plugin_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("config.json is not a JSON object".into()),
        Err(e) => Err(format!("config.json is malformed ({e})")),
    }
}

/// Parse a `--set` value: JSON when it parses (`3600`, `true`, `["a"]`,
/// `{"k":1}`, `"quoted"`), otherwise the raw text as a string.
pub fn parse_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `config_schema.properties.<key>.default`.
    Default,
    /// The user's `config.json`.
    File,
}

/// One row of [`ConfigView`].
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRow {
    pub key: String,
    /// The pre-substitution value (`None` → declared in the schema but set
    /// nowhere, no default).
    pub raw: Option<Value>,
    /// The `${env:VAR}`-resolved value, or why resolution failed.
    pub resolved: Option<Result<Value, String>>,
    pub source: Option<Source>,
    /// The raw value contains a `${env:…}` reference.
    pub via_env: bool,
}

/// The effective configuration of one plugin, for display.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigView {
    pub path: PathBuf,
    pub file_exists: bool,
    pub rows: Vec<ConfigRow>,
    /// Whole-config failure (malformed file, unset env var, missing required
    /// key, type mismatch, schema violation) — `None` when it validates.
    pub error: Option<String>,
}

/// Outcome of [`reset`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetOutcome {
    /// Something was actually removed.
    pub changed: bool,
    /// The resulting config does not validate (reset is never refused — it is
    /// the escape hatch — but the operator is told).
    pub warning: Option<String>,
}

fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn contains_env_ref(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("${env:"),
        Value::Array(a) => a.iter().any(contains_env_ref),
        Value::Object(m) => m.values().any(contains_env_ref),
        _ => false,
    }
}

/// Validate the config that WOULD result from `file` (the full `config.json`
/// map): fill schema defaults → expand `${env:VAR}` → discovery's
/// `validate_config` (required + type) → the `config_schema` JSON-Schema
/// validator (enum, bounds, pattern, additionalProperties, …).
pub fn validate_candidate_with<F>(
    manifest: &PluginManifest,
    file: &Map<String, Value>,
    get_env: &F,
) -> Result<(), String>
where
    F: Fn(&str) -> Option<String>,
{
    let schema = manifest.config_schema.clone().unwrap_or(Value::Null);
    let mut config = file.clone();
    for (k, prop) in schema_properties(&schema) {
        if !config.contains_key(&k)
            && let Some(def) = prop.get("default")
        {
            config.insert(k, def.clone());
        }
    }
    let resolved =
        resolve_env_refs("", Value::Object(config), get_env).map_err(|e| e.to_string())?;
    let Value::Object(map) = &resolved else {
        unreachable!("resolve_env_refs preserves the object shape");
    };
    validate_config(&schema, map).map_err(|e| e.to_string())?;
    if schema.is_object() {
        let violations = validate_json_schema(&schema, &resolved);
        if !violations.is_empty() {
            return Err(violations
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("; "));
        }
    }
    Ok(())
}

/// [`view_with`] against the real process environment.
pub fn view(plugin_dir: &Path, manifest: &PluginManifest) -> ConfigView {
    view_with(plugin_dir, manifest, &real_env)
}

/// Build the effective-config view: `config.json` keys (source `file`) ∪
/// schema defaults (source `default`) ∪ declared-but-unset schema keys. Each
/// value is resolved on its own so one unset env var doesn't hide the rest.
pub fn view_with<F>(plugin_dir: &Path, manifest: &PluginManifest, get_env: &F) -> ConfigView
where
    F: Fn(&str) -> Option<String>,
{
    let path = config_path(plugin_dir);
    let file_exists = path.exists();
    let (file, mut error) = match read_file_map(plugin_dir) {
        Ok(m) => (m, None),
        Err(e) => (Map::new(), Some(e)),
    };
    let schema = manifest.config_schema.clone().unwrap_or(Value::Null);
    let props = schema_properties(&schema);

    let mut keys: Vec<String> = file.keys().cloned().collect();
    keys.extend(props.keys().cloned());
    keys.sort();
    keys.dedup();

    let rows = keys
        .into_iter()
        .map(|key| {
            let (raw, source) = match file.get(&key) {
                Some(v) => (Some(v.clone()), Some(Source::File)),
                None => match props.get(&key).and_then(|p| p.get("default")) {
                    Some(d) => (Some(d.clone()), Some(Source::Default)),
                    None => (None, None),
                },
            };
            let via_env = raw.as_ref().is_some_and(contains_env_ref);
            let resolved = raw
                .clone()
                .map(|v| resolve_env_refs(&key, v, get_env).map_err(|e| e.to_string()));
            ConfigRow {
                key,
                raw,
                resolved,
                source,
                via_env,
            }
        })
        .collect();

    if error.is_none() {
        error = validate_candidate_with(manifest, &file, get_env).err();
    }
    ConfigView {
        path,
        file_exists,
        rows,
        error,
    }
}

/// Redact credential-like values: every string under a credential-like key
/// (recursively, so nested objects/arrays are covered too) becomes
/// [`REDACTED`]. Non-string scalars (a numeric `token_refresh_interval`) are
/// not secrets and stay visible.
pub fn redact(key: &str, v: &Value) -> Value {
    redact_inner(v, is_credential_like(key))
}

fn redact_inner(v: &Value, secret: bool) -> Value {
    match v {
        Value::String(_) if secret => Value::String(REDACTED.into()),
        Value::Array(a) => Value::Array(a.iter().map(|x| redact_inner(x, secret)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), redact_inner(x, secret || is_credential_like(k))))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Compact one-line rendering of a value for display.
fn show(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Display form of `value` for key `key`, redacted when credential-like.
pub fn display_value(key: &str, v: &Value) -> String {
    show(&redact(key, v))
}

/// Render `:plugin config <id>`.
pub fn format_view(id: &str, v: &ConfigView) -> String {
    let mut out = format!("plugin `{id}` config ({})\n", v.path.display());
    if !v.file_exists {
        out.push_str("  (no config.json — schema defaults only)\n");
    }
    if v.rows.is_empty() {
        out.push_str("  (this plugin declares no configuration)\n");
    }
    let width = v.rows.iter().map(|r| r.key.len()).max().unwrap_or(0);
    for r in &v.rows {
        let value = match (&r.raw, &r.resolved) {
            (None, _) => "(unset)".to_string(),
            (Some(raw), Some(Ok(res))) if r.via_env => {
                format!("{} → {}", show(raw), display_value(&r.key, res))
            }
            (Some(_), Some(Ok(res))) => display_value(&r.key, res),
            (Some(raw), Some(Err(e))) => format!("{}  (unresolved: {e})", show(raw)),
            (Some(raw), None) => display_value(&r.key, raw),
        };
        let mut tags = Vec::new();
        match r.source {
            Some(Source::File) => tags.push("file"),
            Some(Source::Default) => tags.push("default"),
            None => {}
        }
        if r.via_env {
            tags.push("env");
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        out.push_str(&format!("  {:<width$} = {value}{tags}\n", r.key));
    }
    if let Some(e) = &v.error {
        out.push_str(&format!("  ✗ config does not validate: {e}\n"));
    }
    out.push_str(&format!(
        "\n:plugin config {id} --set <key> <value>   (value parsed as JSON, else string)\n\
         :plugin config {id} --reset [key]        (drop one key, or all of config.json)"
    ));
    out
}

/// Write `map` to `config.json` atomically (`.tmp` + rename). An empty map
/// removes the file instead (all defaults).
fn write_atomic(plugin_dir: &Path, map: &Map<String, Value>) -> Result<(), String> {
    let path = config_path(plugin_dir);
    if map.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("could not remove {}: {e}", path.display())),
        };
    }
    let body = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{body}\n"))
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("could not write {}: {e}", path.display())
        })
}

/// [`set_key_with`] against the real process environment.
pub fn set_key(
    plugin_dir: &Path,
    manifest: &PluginManifest,
    key: &str,
    value: Value,
) -> Result<(), String> {
    set_key_with(plugin_dir, manifest, key, value, &real_env)
}

/// Set top-level `key` to `value` in `config.json`. The whole resulting config
/// must validate first; on any error the file is left untouched.
pub fn set_key_with<F>(
    plugin_dir: &Path,
    manifest: &PluginManifest,
    key: &str,
    value: Value,
    get_env: &F,
) -> Result<(), String>
where
    F: Fn(&str) -> Option<String>,
{
    let id = &manifest.id;
    if key.is_empty() {
        return Err("config key must not be empty".into());
    }
    let mut map = read_file_map(plugin_dir)
        .map_err(|e| format!("{e} — fix it or run :plugin config {id} --reset"))?;
    map.insert(key.to_string(), value);
    validate_candidate_with(manifest, &map, get_env)
        .map_err(|e| format!("invalid config for `{id}`: {e} (config.json unchanged)"))?;
    write_atomic(plugin_dir, &map)
}

/// [`reset_with`] against the real process environment.
pub fn reset(
    plugin_dir: &Path,
    manifest: &PluginManifest,
    key: Option<&str>,
) -> Result<ResetOutcome, String> {
    reset_with(plugin_dir, manifest, key, &real_env)
}

/// Remove `key` from `config.json` (it falls back to its schema default), or
/// with `None` delete `config.json` entirely. Never refused on validation
/// grounds; an invalid result is reported via [`ResetOutcome::warning`].
pub fn reset_with<F>(
    plugin_dir: &Path,
    manifest: &PluginManifest,
    key: Option<&str>,
    get_env: &F,
) -> Result<ResetOutcome, String>
where
    F: Fn(&str) -> Option<String>,
{
    let path = config_path(plugin_dir);
    let (changed, remaining) = match key {
        None => {
            let existed = path.exists();
            write_atomic(plugin_dir, &Map::new())?;
            (existed, Map::new())
        }
        Some(k) => {
            let mut map = read_file_map(plugin_dir).map_err(|e| {
                format!(
                    "{e} — run :plugin config {} --reset to delete it",
                    manifest.id
                )
            })?;
            if map.remove(k).is_none() {
                (false, map)
            } else {
                write_atomic(plugin_dir, &map)?;
                (true, map)
            }
        }
    };
    let warning = validate_candidate_with(manifest, &remaining, get_env)
        .err()
        .map(|e| format!("resulting config does not validate: {e}"));
    Ok(ResetOutcome { changed, warning })
}

/// Completion candidates for `--set|--reset <key>`: keys declared in
/// `config_schema.properties` ∪ keys present in `config.json`, sorted.
pub fn config_keys(plugin_dir: &Path, manifest: &PluginManifest) -> Vec<String> {
    let schema = manifest.config_schema.clone().unwrap_or(Value::Null);
    let mut keys: Vec<String> = schema_properties(&schema).keys().cloned().collect();
    if let Ok(m) = read_file_map(plugin_dir) {
        keys.extend(m.keys().cloned());
    }
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tempdir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "aish-plugin-config-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn manifest() -> PluginManifest {
        serde_json::from_value(json!({
            "id": "gh",
            "config_schema": {
                "type": "object",
                "properties": {
                    "token_refresh_interval": {"type": "integer", "default": 7200, "minimum": 60},
                    "webhook_events": {"type": "array", "default": ["pull_request", "issues"]},
                    "mode": {"type": "string", "enum": ["fast", "safe"], "default": "safe"},
                    "api_token": {"type": "string", "default": "${env:GH_TOKEN}"},
                    "label": {"type": "string"}
                }
            }
        }))
        .unwrap()
    }

    fn env(name: &str) -> Option<String> {
        match name {
            "GH_TOKEN" => Some("ghp_supersecret".into()),
            "WHO" => Some("greg".into()),
            _ => None,
        }
    }

    #[test]
    fn parse_value_json_or_string() {
        assert_eq!(parse_value("3600"), json!(3600));
        assert_eq!(parse_value("true"), json!(true));
        assert_eq!(parse_value(r#"["a","b"]"#), json!(["a", "b"]));
        assert_eq!(parse_value(r#"{"k":1}"#), json!({"k": 1}));
        assert_eq!(parse_value(r#""42""#), json!("42"));
        assert_eq!(parse_value("hello world"), json!("hello world"));
        assert_eq!(parse_value("${env:X}"), json!("${env:X}"));
    }

    #[test]
    fn set_persists_and_load_config_sees_it() {
        let dir = tempdir();
        // No `${env:…}` default here: `load_config` reads the real (global)
        // environment, which tests must not mutate.
        let mut m = manifest();
        m.config_schema.as_mut().unwrap()["properties"]
            .as_object_mut()
            .unwrap()
            .remove("api_token");
        set_key_with(&dir, &m, "token_refresh_interval", json!(3600), &env).unwrap();
        set_key_with(&dir, &m, "label", json!("work"), &env).unwrap();
        let on_disk: Value =
            serde_json::from_str(&fs::read_to_string(config_path(&dir)).unwrap()).unwrap();
        assert_eq!(
            on_disk,
            json!({"token_refresh_interval": 3600, "label": "work"})
        );
        // Persistence: the startup loader (fresh read) resolves the new values.
        let cfg = crate::plugins::load_config(&dir, &m).unwrap();
        assert_eq!(cfg["token_refresh_interval"], 3600);
        assert_eq!(cfg["label"], "work");
        assert_eq!(cfg["mode"], "safe");
        assert!(!dir.join("config.json.tmp").exists());
    }

    #[test]
    fn set_type_mismatch_rejected_file_unchanged() {
        let dir = tempdir();
        let m = manifest();
        fs::write(config_path(&dir), "{\"label\": \"x\"}").unwrap();
        let before = fs::read(config_path(&dir)).unwrap();
        let err =
            set_key_with(&dir, &m, "token_refresh_interval", json!("soon"), &env).unwrap_err();
        assert!(err.contains("token_refresh_interval"), "{err}");
        assert!(err.contains("integer"), "{err}");
        assert!(err.contains("unchanged"), "{err}");
        assert_eq!(fs::read(config_path(&dir)).unwrap(), before);
    }

    #[test]
    fn set_schema_bounds_and_enum_rejected() {
        let dir = tempdir();
        let m = manifest();
        let err = set_key_with(&dir, &m, "token_refresh_interval", json!(5), &env).unwrap_err();
        assert!(err.contains("token_refresh_interval"), "{err}");
        let err = set_key_with(&dir, &m, "mode", json!("turbo"), &env).unwrap_err();
        assert!(err.contains("mode"), "{err}");
        assert!(!config_path(&dir).exists(), "nothing written on reject");
        set_key_with(&dir, &m, "mode", json!("fast"), &env).unwrap();
    }

    #[test]
    fn set_unset_env_reference_rejected() {
        let dir = tempdir();
        let m = manifest();
        let err = set_key_with(&dir, &m, "label", json!("${env:NOPE}"), &env).unwrap_err();
        assert!(err.contains("NOPE"), "{err}");
        set_key_with(&dir, &m, "label", json!("${env:WHO}"), &env).unwrap();
    }

    #[test]
    fn set_required_key_enforced() {
        let dir = tempdir();
        let m: PluginManifest = serde_json::from_value(json!({
            "id": "r",
            "config_schema": {"type": "object", "required": ["endpoint"],
                "properties": {"endpoint": {"type": "string"}, "n": {"type": "integer"}}}
        }))
        .unwrap();
        let err = set_key_with(&dir, &m, "n", json!(1), &env).unwrap_err();
        assert!(err.contains("endpoint"), "{err}");
        set_key_with(&dir, &m, "endpoint", json!("https://x"), &env).unwrap();
        set_key_with(&dir, &m, "n", json!(1), &env).unwrap();
        // Resetting the required key is allowed but warned about.
        let out = reset_with(&dir, &m, Some("endpoint"), &env).unwrap();
        assert!(out.changed);
        assert!(out.warning.unwrap().contains("endpoint"));
    }

    #[test]
    fn set_on_malformed_file_rejected_unchanged() {
        let dir = tempdir();
        fs::write(config_path(&dir), "{not json").unwrap();
        let err = set_key_with(&dir, &manifest(), "label", json!("x"), &env).unwrap_err();
        assert!(err.contains("malformed"), "{err}");
        assert!(err.contains("--reset"), "{err}");
        assert_eq!(fs::read_to_string(config_path(&dir)).unwrap(), "{not json");
    }

    #[test]
    fn reset_key_and_all() {
        let dir = tempdir();
        let m = manifest();
        set_key_with(&dir, &m, "token_refresh_interval", json!(3600), &env).unwrap();
        set_key_with(&dir, &m, "label", json!("x"), &env).unwrap();

        let out = reset_with(&dir, &m, Some("token_refresh_interval"), &env).unwrap();
        assert_eq!(
            out,
            ResetOutcome {
                changed: true,
                warning: None
            }
        );
        let v = view_with(&dir, &m, &env);
        let row = v
            .rows
            .iter()
            .find(|r| r.key == "token_refresh_interval")
            .unwrap();
        assert_eq!(row.source, Some(Source::Default));
        assert_eq!(row.raw, Some(json!(7200)));

        // Absent key → no-op.
        assert!(!reset_with(&dir, &m, Some("nope"), &env).unwrap().changed);

        // Removing the last key deletes the file.
        assert!(reset_with(&dir, &m, Some("label"), &env).unwrap().changed);
        assert!(!config_path(&dir).exists());

        set_key_with(&dir, &m, "label", json!("y"), &env).unwrap();
        assert!(reset_with(&dir, &m, None, &env).unwrap().changed);
        assert!(!config_path(&dir).exists());
        assert!(!reset_with(&dir, &m, None, &env).unwrap().changed);
    }

    #[test]
    fn reset_all_works_on_malformed_file() {
        let dir = tempdir();
        fs::write(config_path(&dir), "garbage").unwrap();
        assert!(reset_with(&dir, &manifest(), None, &env).unwrap().changed);
        assert!(!config_path(&dir).exists());
    }

    #[test]
    fn view_sources_env_and_unset() {
        let dir = tempdir();
        let m = manifest();
        fs::write(config_path(&dir), r#"{"label":"${env:WHO}","extra":1}"#).unwrap();
        let v = view_with(&dir, &m, &env);
        assert!(v.file_exists);
        assert_eq!(v.error, None);
        let get = |k: &str| v.rows.iter().find(|r| r.key == k).unwrap().clone();
        assert_eq!(get("label").source, Some(Source::File));
        assert!(get("label").via_env);
        assert_eq!(get("label").resolved, Some(Ok(json!("greg"))));
        assert_eq!(get("extra").source, Some(Source::File));
        assert_eq!(get("mode").source, Some(Source::Default));
        assert!(get("api_token").via_env);

        let text = format_view("gh", &v);
        assert!(text.contains("[file, env]"), "{text}");
        assert!(text.contains("[default]"), "{text}");
        assert!(text.contains("\"greg\""), "{text}");
    }

    #[test]
    fn view_surfaces_unresolved_env_and_unset_keys() {
        let dir = tempdir();
        let m = manifest();
        let v = view_with(&dir, &m, &|_| None);
        assert!(!v.file_exists);
        let err = v.error.clone().unwrap();
        assert!(err.contains("GH_TOKEN"), "{err}");
        let label = v.rows.iter().find(|r| r.key == "label").unwrap();
        assert_eq!(label.raw, None);
        let text = format_view("gh", &v);
        assert!(text.contains("(unset)"), "{text}");
        assert!(text.contains("unresolved"), "{text}");
        assert!(text.contains("does not validate"), "{text}");
        assert!(text.contains("schema defaults only"), "{text}");
    }

    #[test]
    fn redaction_of_credential_like_values() {
        let dir = tempdir();
        let m = manifest();
        fs::write(
            config_path(&dir),
            r#"{"password":"hunter2","nested":{"client_secret":"s3","name":"ok"}}"#,
        )
        .unwrap();
        let text = format_view("gh", &view_with(&dir, &m, &env));
        assert!(!text.contains("ghp_supersecret"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("s3\""), "{text}");
        assert!(text.contains(REDACTED), "{text}");
        assert!(
            text.contains("${env:GH_TOKEN}"),
            "reference itself is shown: {text}"
        );
        assert!(text.contains("\"ok\""), "{text}");
        // Non-string value under a credential-like key stays visible.
        assert!(text.contains("7200"), "{text}");
        assert_eq!(display_value("label", &json!("plain")), "\"plain\"");
        assert_eq!(
            display_value("auth", &json!(["a", 1])),
            format!("[\"{REDACTED}\",1]")
        );
    }

    #[test]
    fn config_keys_union_sorted() {
        let dir = tempdir();
        fs::write(config_path(&dir), r#"{"zeta":1,"label":"x"}"#).unwrap();
        assert_eq!(
            config_keys(&dir, &manifest()),
            vec![
                "api_token",
                "label",
                "mode",
                "token_refresh_interval",
                "webhook_events",
                "zeta"
            ]
        );
    }

    #[test]
    fn no_schema_plugin_accepts_any_key() {
        let dir = tempdir();
        let m: PluginManifest = serde_json::from_value(json!({"id": "bare"})).unwrap();
        set_key_with(&dir, &m, "anything", json!({"a": [1]}), &env).unwrap();
        let text = format_view("bare", &view_with(&dir, &m, &env));
        assert!(text.contains("anything"), "{text}");
    }
}
