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

/// Minimal `plugin.json` manifest. Only the fields the current phases need are
/// parsed; every other key in the design doc (tools, schemas, …) is ignored via
/// serde's default unknown-field-dropping so a fuller manifest still
/// deserializes.
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
    /// The capabilities a plugin contributes to the shell — lifecycle hooks,
    /// event hooks, config/env injection, login command, … (see
    /// `docs/PLUGIN_SYSTEM_DESIGN.md` § Enterprise Addendum). Only the fields
    /// modeled on [`Provides`] are understood today; unknown keys are dropped.
    #[serde(default)]
    pub provides: Option<Provides>,
}

/// The `provides` block of a `plugin.json` manifest: the capabilities a plugin
/// contributes to the shell. Only the fields the current phase understands are
/// modeled; unknown keys are dropped by serde so a richer manifest still loads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Provides {
    /// Plugin **lifecycle** hook names — `on_init`, `on_shell_ready`,
    /// `on_shutdown`, `on_webhook_url_changed`, … — the plugin registers. These
    /// are the *plugin-loader* lifecycle points, distinct from the shell's
    /// 33-event agent-lifecycle hook catalog (`src/hooks.rs`). Canonical key as
    /// of Phase 0.5.1.
    #[serde(default)]
    pub lifecycle_hooks: Vec<String>,
    /// **Deprecated** alias for [`Self::lifecycle_hooks`], retained for one
    /// release so manifests written against the old schema keep loading. Prefer
    /// `lifecycle_hooks`; a manifest that still sets `hooks` triggers a one-time
    /// deprecation warning at [`discover`] time. Renamed (Phase 0.5.1) to free
    /// the word "hooks" for the event-catalog contribution surface.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// The **login command** this plugin handles (Phase 0.5.5): declaring
    /// `"login": "mycompany"` makes `aish login mycompany` route to this
    /// plugin's `login.sh` auth handler, whose JSON output is persisted to
    /// `~/.aish/credentials` under `[profile:mycompany]`. Absent → the plugin
    /// contributes no login command. See [`crate::plugin_auth`].
    #[serde(default)]
    pub login: Option<String>,
}

impl PluginManifest {
    /// A plugin ships enabled unless it opts out with `"enabled": false`.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// The effective plugin-lifecycle hook list. Prefers the canonical
    /// `provides.lifecycle_hooks`; falls back to the deprecated `provides.hooks`
    /// alias only when the canonical key is absent/empty (Phase 0.5.1
    /// migration). Empty slice when the plugin declares no `provides` block.
    #[allow(dead_code)] // canonical accessor consumed by the Phase 0.5.2 lifecycle-hook dispatch
    pub fn lifecycle_hooks(&self) -> &[String] {
        match &self.provides {
            Some(p) if !p.lifecycle_hooks.is_empty() => &p.lifecycle_hooks,
            Some(p) => &p.hooks,
            None => &[],
        }
    }

    /// The login command this plugin handles, if any (`provides.login`). When
    /// `Some("mycompany")`, `aish login mycompany` routes here (Phase 0.5.5).
    pub fn login_command(&self) -> Option<&str> {
        self.provides.as_ref().and_then(|p| p.login.as_deref())
    }


    /// True when the manifest relies on the deprecated `provides.hooks` alias —
    /// i.e. `hooks` is populated and the canonical `lifecycle_hooks` is not.
    /// Drives the one-release deprecation warning emitted at [`discover`] time.
    pub fn uses_deprecated_hooks_key(&self) -> bool {
        matches!(&self.provides, Some(p) if p.lifecycle_hooks.is_empty() && !p.hooks.is_empty())
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
        // Phase 0.5.1: `provides.hooks` was renamed to `provides.lifecycle_hooks`.
        // The old key still resolves (see `lifecycle_hooks`) but earns a one-time
        // deprecation warning so authors migrate within the one-release window.
        if manifest.uses_deprecated_hooks_key() {
            eprintln!(
                "aish: plugin `{}`: `provides.hooks` is deprecated and will be removed in a \
                 future release — rename it to `provides.lifecycle_hooks`.",
                manifest.id
            );
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

// ===================================================================
// Phase 0.5.3 — plugin `.mcp.json` merge into the client MCP set
// ===================================================================

/// One MCP server a plugin injects into the client's runtime set, resolved and
/// ready for [`crate::mcp::McpHost::start_with_plugins`].
#[derive(Debug, Clone, PartialEq)]
pub struct PluginMcpServer {
    /// Server name (the `mcpServers` object key).
    pub name: String,
    /// The server spec (url/command/headers/env/args) with `${env:…}` and
    /// `${profile:…}` refs already expanded.
    pub spec: Value,
    /// The plugin id that contributed the server — provenance for `:plugin info`.
    pub plugin_id: String,
}

/// Discover plugins under `dir` and return their resolved MCP servers as
/// `(name, spec)` pairs for the MCP host, plus any warnings (unresolved refs,
/// malformed files, name collisions). The caller prints the warnings.
///
/// **Collision policy:** first-plugin-by-id wins (plugins are id-sorted); a
/// later plugin registering an already-claimed name is skipped with a warning.
/// Project/user `.mcp.json` servers still shadow ALL plugin servers — the MCP
/// host connects plugin servers last (see `mcp::McpHost::connect_missing`).
pub fn discover_mcp_servers(dir: &Path) -> (Vec<(String, Value)>, Vec<String>) {
    let plugins = discover(dir);
    let (servers, warnings) = collect_mcp_servers(&plugins);
    (
        servers.into_iter().map(|s| (s.name, s.spec)).collect(),
        warnings,
    )
}

/// Collect every plugin's `.mcp.json` MCP servers, resolving `${env:VAR}` and
/// `${profile:<login>[:FIELD]}` references against the process environment and
/// the `~/.aish/credentials` store. First-plugin-by-id wins on a name clash.
pub fn collect_mcp_servers(plugins: &[Plugin]) -> (Vec<PluginMcpServer>, Vec<String>) {
    collect_mcp_servers_with(
        plugins,
        &|var| std::env::var(var).ok(),
        &profile_field,
    )
}

/// [`collect_mcp_servers`] with injectable env + profile lookups — the seam the
/// tests drive so they never touch the real environment or credentials file.
fn collect_mcp_servers_with<E, P>(
    plugins: &[Plugin],
    get_env: &E,
    get_profile: &P,
) -> (Vec<PluginMcpServer>, Vec<String>)
where
    E: Fn(&str) -> Option<String>,
    P: Fn(&str, Option<&str>) -> Option<String>,
{
    let mut out: Vec<PluginMcpServer> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    // name -> winning plugin id (for the collision warning).
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for plugin in plugins {
        let path = plugin.dir.join(".mcp.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue, // no `.mcp.json` — most plugins don't ship one.
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!(
                    "plugin `{}`: .mcp.json is not valid JSON: {e} — skipped",
                    plugin.manifest.id
                ));
                continue;
            }
        };
        let Some(servers) = parsed.get("mcpServers").and_then(|m| m.as_object()) else {
            warnings.push(format!(
                "plugin `{}`: .mcp.json has no `mcpServers` object — skipped",
                plugin.manifest.id
            ));
            continue;
        };
        // Deterministic order within a plugin.
        let mut names: Vec<&String> = servers.keys().collect();
        names.sort();
        for name in names {
            if let Some(winner) = seen.get(name) {
                warnings.push(format!(
                    "plugin `{}`: MCP server `{name}` already contributed by plugin `{winner}` \
                     — keeping first, skipping duplicate",
                    plugin.manifest.id
                ));
                continue;
            }
            let mut spec = servers[name].clone();
            resolve_mcp_refs(&mut spec, plugin, get_env, get_profile, &mut warnings);
            seen.insert(name.clone(), plugin.manifest.id.clone());
            out.push(PluginMcpServer {
                name: name.clone(),
                spec,
                plugin_id: plugin.manifest.id.clone(),
            });
        }
    }
    (out, warnings)
}

/// Real credential-profile lookup: read `[profile:<login>]` from
/// `~/.aish/credentials`. `field=Some(k)` → that field; `field=None` → the
/// primary credential (see [`primary_credential`]).
fn profile_field(login: &str, field: Option<&str>) -> Option<String> {
    let path = crate::plugin_auth::credentials_path();
    let vars = crate::mcp::load_profile(
        &path.to_string_lossy(),
        &crate::plugin_auth::profile_section(login),
    );
    match field {
        Some(k) => vars.get(k).cloned(),
        None => primary_credential(&vars),
    }
}

/// Pick the "primary" credential from a profile for a bare `${profile:<login>}`
/// ref: prefer the well-known token field names, else the alphabetically-first
/// key (deterministic). `None` when the profile is empty/absent.
fn primary_credential(vars: &std::collections::HashMap<String, String>) -> Option<String> {
    for k in ["token", "access_token", "api_key", "apikey", "key"] {
        if let Some(v) = vars.get(k) {
            return Some(v.clone());
        }
    }
    vars.iter()
        .min_by(|a, b| a.0.cmp(b.0))
        .map(|(_, v)| v.clone())
}

/// Recursively resolve `${…}` refs in every string within an MCP server spec.
fn resolve_mcp_refs<E, P>(
    v: &mut Value,
    plugin: &Plugin,
    get_env: &E,
    get_profile: &P,
    warnings: &mut Vec<String>,
) where
    E: Fn(&str) -> Option<String>,
    P: Fn(&str, Option<&str>) -> Option<String>,
{
    match v {
        Value::String(s) => {
            *s = resolve_mcp_str(s, plugin, get_env, get_profile, warnings);
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                resolve_mcp_refs(item, plugin, get_env, get_profile, warnings);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                resolve_mcp_refs(item, plugin, get_env, get_profile, warnings);
            }
        }
        _ => {}
    }
}

/// Resolve every `${…}` occurrence in one string. Recognized prefixes:
///   * `${env:VAR}`                    → process env var `VAR`
///   * `${profile:<login>}`            → primary credential of `[profile:<login>]`
///   * `${profile:<login>:<FIELD>}`    → that field of `[profile:<login>]`
///
/// A bare `${NAME}` (no recognized prefix) is left **verbatim** — it belongs to
/// the existing `mcp` credentials-block interpolation that runs at connect time.
/// An unset `env:`/`profile:` ref is also left verbatim and a warning recorded
/// (graceful — one bad ref never blocks startup).
fn resolve_mcp_str<E, P>(
    s: &str,
    plugin: &Plugin,
    get_env: &E,
    get_profile: &P,
    warnings: &mut Vec<String>,
) -> String
where
    E: Fn(&str) -> Option<String>,
    P: Fn(&str, Option<&str>) -> Option<String>,
{
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let inner = &after[..end];
                match resolve_ref_token(inner, plugin, get_env, get_profile, warnings) {
                    Some(v) => out.push_str(&v),
                    None => {
                        // Leave the ref untouched.
                        out.push_str("${");
                        out.push_str(inner);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated `${` — emit the remainder verbatim and stop.
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolve one ref token (the text between `${` and `}`). `None` means "leave
/// verbatim": either an unrecognized prefix (bare `${NAME}`) or an unresolved
/// `env:`/`profile:` ref (a warning is pushed for the latter).
fn resolve_ref_token<E, P>(
    inner: &str,
    plugin: &Plugin,
    get_env: &E,
    get_profile: &P,
    warnings: &mut Vec<String>,
) -> Option<String>
where
    E: Fn(&str) -> Option<String>,
    P: Fn(&str, Option<&str>) -> Option<String>,
{
    if let Some(var) = inner.strip_prefix("env:") {
        match get_env(var) {
            Some(v) => Some(v),
            None => {
                warnings.push(format!(
                    "plugin `{}`: ${{env:{var}}} is unset — left unresolved",
                    plugin.manifest.id
                ));
                None
            }
        }
    } else if let Some(rest) = inner.strip_prefix("profile:") {
        let (login, field) = match rest.split_once(':') {
            Some((l, f)) => (l, Some(f)),
            None => (rest, None),
        };
        match get_profile(login, field) {
            Some(v) => Some(v),
            None => {
                warnings.push(format!(
                    "plugin `{}`: ${{profile:{rest}}} did not resolve \
                     (run `login {login}`?) — left unresolved",
                    plugin.manifest.id
                ));
                None
            }
        }
    } else {
        // Bare ${NAME}: not ours — the mcp credentials-block interpolation owns it.
        None
    }
}

// ===================================================================
// Phase 0.5.4 — session-env injection from lifecycle-hook stdout
// ===================================================================
//
// A plugin lifecycle hook is a script at `<plugin_dir>/<hook>.sh` (e.g.
// `on_init.sh`). At plugin-load time — BEFORE the REPL starts — the shell
// fork/execs the script, captures its **stdout**, and parses any `KEY=VALUE`
// lines into the session environment. Lines that aren't `KEY=VALUE`, blank
// lines, and `#` comments are ignored. Credential-looking payloads are rejected
// by a redaction guard (see [`looks_like_secret`]).
//
// **Merge order (documented decision):** existing session/user env ALWAYS wins.
// Plugin-emitted vars only *fill gaps* — the caller skips any key already present
// in `session.env` (which is seeded from the real process env). Among plugins,
// first-plugin-by-id wins on a key clash (plugins are id-sorted), matching the
// 0.5.3 MCP collision policy; the loser is skipped with a warning.
//
// **Escape hatch:** set `AISH_ENV_INJECTION_DISABLED` (to anything other than
// empty/`0`/`false`) to disable injection entirely — hooks are not even run.

/// The environment variable that, when truthy, disables lifecycle-hook env
/// injection wholesale (operators' kill switch).
const ENV_INJECTION_DISABLED_VAR: &str = "AISH_ENV_INJECTION_DISABLED";

/// Substrings that mark a key or value as credential-like. If a `KEY=VALUE`
/// pair's key OR value contains any of these (case-insensitive), the pair is
/// rejected with a warning and never injected. Kept intentionally broad —
/// lifecycle-hook stdout is the wrong channel for secrets (use `${profile:…}`
/// credential refs instead).
const SECRET_MARKERS: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "token",
    "credential",
    "api_key",
    "apikey",
    "private_key",
    "access_key",
    "secret_key",
];

/// Hard cap on a lifecycle hook's runtime. A hook that neither exits nor closes
/// stdout within this window is killed and its output discarded (startup must
/// not wedge on a hung plugin). Documented caveat: a hook emitting more than a
/// pipe buffer (~64 KB) of output without exiting can also hit this cap.
const LIFECYCLE_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Is a `KEY=VALUE` pair credential-like (and therefore refused)? Matches the
/// [`SECRET_MARKERS`] denylist as a case-insensitive substring on either the key
/// or the value, plus any key that *is* `key` or ends in `_key` (catches
/// `API_KEY`, `AWS_SECRET_KEY`, … without tripping on innocent words like
/// `MONKEY` that merely contain "key").
fn looks_like_secret(key: &str, value: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let v = value.to_ascii_lowercase();
    if SECRET_MARKERS.iter().any(|m| k.contains(m) || v.contains(m)) {
        return true;
    }
    k == "key" || k.ends_with("_key")
}

/// A syntactically valid environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a lifecycle hook's stdout into `(KEY, VALUE)` pairs plus warnings.
///
/// Rules:
///   * Blank lines and `#` comments are skipped.
///   * A line without `=` (or with an invalid key) is **silently ignored** —
///     hooks routinely print human-readable status lines.
///   * A pair whose key/value looks like a credential is **rejected with a
///     warning** ([`looks_like_secret`]).
/// Surrounding whitespace on both key and value is trimmed.
pub fn parse_env_lines(stdout: &str, plugin_id: &str) -> (Vec<(String, String)>, Vec<String>) {
    let mut pairs = Vec::new();
    let mut warnings = Vec::new();
    for raw in stdout.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue; // not KEY=VALUE — ignore
        };
        let key = k.trim();
        let val = v.trim();
        if !is_valid_env_key(key) {
            continue; // junk key — ignore
        }
        if looks_like_secret(key, val) {
            warnings.push(format!(
                "plugin `{plugin_id}`: refusing to inject env var `{key}` — name/value looks \
                 like a credential (blocked by the env-injection redaction guard)"
            ));
            continue;
        }
        pairs.push((key.to_string(), val.to_string()));
    }
    (pairs, warnings)
}

/// True when the operator kill switch [`ENV_INJECTION_DISABLED_VAR`] is set to a
/// truthy value.
fn env_injection_disabled() -> bool {
    std::env::var(ENV_INJECTION_DISABLED_VAR)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false"
        })
        .unwrap_or(false)
}

/// Fork/exec a plugin's `<hook>.sh` and capture its stdout. Returns `None` when
/// the script does not exist (most plugins ship no lifecycle hook) or fails to
/// spawn. The child runs with `cwd = plugin.dir`, stdin closed, stderr
/// discarded, and `AISH_PLUGIN_ID` / `AISH_PLUGIN_DIR` exported. A hook that
/// outruns [`LIFECYCLE_HOOK_TIMEOUT`] is killed and its output discarded.
fn run_lifecycle_hook_script(plugin: &Plugin, hook: &str) -> Option<String> {
    let script = plugin.dir.join(format!("{hook}.sh"));
    if !script.is_file() {
        return None;
    }
    let mut child = std::process::Command::new(&script)
        .current_dir(&plugin.dir)
        .env("AISH_PLUGIN_ID", &plugin.manifest.id)
        .env("AISH_PLUGIN_DIR", plugin.dir.to_string_lossy().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + LIFECYCLE_HOOK_TIMEOUT;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    if timed_out {
        eprintln!(
            "\x1b[33maish:\x1b[0m plugin `{}`: lifecycle hook `{hook}` timed out after {}s — \
             output discarded",
            plugin.manifest.id,
            LIFECYCLE_HOOK_TIMEOUT.as_secs()
        );
        return None;
    }
    let mut buf = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut buf);
    }
    Some(buf)
}

/// Run `hook` for every plugin and collect the env vars they emit, resolving the
/// collision + redaction policy. Real entry point; uses [`run_lifecycle_hook_script`]
/// to fork/exec and the process env to check the kill switch.
pub fn collect_lifecycle_env(plugins: &[Plugin], hook: &str) -> (Vec<(String, String)>, Vec<String>) {
    collect_lifecycle_env_with(
        plugins,
        hook,
        env_injection_disabled(),
        &run_lifecycle_hook_script,
    )
}

/// [`collect_lifecycle_env`] with an injectable hook runner + disabled flag — the
/// seam the tests drive so they never fork a real process. `run` returns the
/// hook's stdout, or `None` when the plugin has no such hook.
fn collect_lifecycle_env_with<R>(
    plugins: &[Plugin],
    hook: &str,
    disabled: bool,
    run: &R,
) -> (Vec<(String, String)>, Vec<String>)
where
    R: Fn(&Plugin, &str) -> Option<String>,
{
    let mut warnings = Vec::new();
    if disabled {
        return (Vec::new(), warnings);
    }
    let mut out: Vec<(String, String)> = Vec::new();
    // key -> winning plugin id (for the collision warning).
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for plugin in plugins {
        let Some(stdout) = run(plugin, hook) else {
            continue; // plugin ships no such lifecycle hook
        };
        let (pairs, mut w) = parse_env_lines(&stdout, &plugin.manifest.id);
        warnings.append(&mut w);
        for (k, v) in pairs {
            if let Some(winner) = seen.get(&k) {
                warnings.push(format!(
                    "plugin `{}`: env var `{k}` already set by plugin `{winner}` \
                     — keeping first, skipping duplicate",
                    plugin.manifest.id
                ));
                continue;
            }
            seen.insert(k.clone(), plugin.manifest.id.clone());
            out.push((k, v));
        }
    }
    (out, warnings)
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
    fn provides_lifecycle_hooks_no_provides_is_empty() {
        let m = manifest(r#"{"id":"p"}"#);
        assert!(m.provides.is_none());
        assert!(m.lifecycle_hooks().is_empty());
        assert!(!m.uses_deprecated_hooks_key());
    }

    #[test]
    fn provides_canonical_lifecycle_hooks_parses() {
        let m = manifest(r#"{"id":"p","provides":{"lifecycle_hooks":["on_init","on_shutdown"]}}"#);
        assert_eq!(m.lifecycle_hooks(), &["on_init", "on_shutdown"]);
        assert!(!m.uses_deprecated_hooks_key());
    }

    #[test]
    fn provides_deprecated_hooks_alias_parses_and_is_flagged() {
        let m = manifest(r#"{"id":"p","provides":{"hooks":["on_init","on_shell_ready"]}}"#);
        // The old key still resolves to the effective lifecycle-hook list…
        assert_eq!(m.lifecycle_hooks(), &["on_init", "on_shell_ready"]);
        // …but is flagged so discovery emits a one-time deprecation warning.
        assert!(m.uses_deprecated_hooks_key());
    }

    #[test]
    fn provides_canonical_key_takes_precedence_over_deprecated_alias() {
        // When both are present, the canonical `lifecycle_hooks` wins and the
        // manifest is NOT treated as using the deprecated alias.
        let m =
            manifest(r#"{"id":"p","provides":{"lifecycle_hooks":["on_shutdown"],"hooks":["on_init"]}}"#);
        assert_eq!(m.lifecycle_hooks(), &["on_shutdown"]);
        assert!(!m.uses_deprecated_hooks_key());
    }

    #[test]
    fn provides_empty_block_yields_no_lifecycle_hooks() {
        let m = manifest(r#"{"id":"p","provides":{}}"#);
        assert!(m.provides.is_some());
        assert!(m.lifecycle_hooks().is_empty());
        assert!(!m.uses_deprecated_hooks_key());
    }

    #[test]
    fn discover_resolves_provides_lifecycle_hooks() {
        let tmp = tempdir();
        write_plugin(
            &tmp,
            "hooked",
            r#"{"id":"hooked","provides":{"lifecycle_hooks":["on_init"]}}"#,
            Some(("s", "d")),
        );
        let plugins = discover(&tmp);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.lifecycle_hooks(), &["on_init"]);
        assert!(!plugins[0].manifest.uses_deprecated_hooks_key());
    }

    #[test]
    fn discover_still_loads_plugin_using_deprecated_hooks_key() {
        // A manifest on the old schema must keep loading (its skills too) —
        // deprecation is a warning, never a hard failure.
        let tmp = tempdir();
        write_plugin(
            &tmp,
            "legacy",
            r#"{"id":"legacy","provides":{"hooks":["on_init"]}}"#,
            Some(("s", "d")),
        );
        let plugins = discover(&tmp);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.lifecycle_hooks(), &["on_init"]);
        assert!(plugins[0].manifest.uses_deprecated_hooks_key());
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

    // ---- Phase 0.5.3: plugin `.mcp.json` merge ----

    /// Write a plugin at `<root>/<id>/` with a minimal manifest and a `.mcp.json`.
    fn write_mcp(root: &Path, id: &str, mcp_json: &str) {
        let pdir = root.join(id);
        fs::create_dir_all(&pdir).unwrap();
        fs::write(pdir.join("plugin.json"), format!(r#"{{"id":"{id}"}}"#)).unwrap();
        fs::write(pdir.join(".mcp.json"), mcp_json).unwrap();
    }

    /// env lookup that resolves nothing.
    fn no_env(_: &str) -> Option<String> {
        None
    }
    /// profile lookup that resolves nothing.
    fn no_profile(_: &str, _: Option<&str>) -> Option<String> {
        None
    }

    #[test]
    fn mcp_basic_merge_one_plugin_one_server() {
        let tmp = tempdir();
        write_mcp(
            &tmp,
            "acme",
            r#"{"mcpServers":{"docs":{"url":"https://example.com/mcp"}}}"#,
        );
        let (servers, warnings) =
            collect_mcp_servers_with(&discover(&tmp), &no_env, &no_profile);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].plugin_id, "acme");
        assert_eq!(servers[0].spec["url"], "https://example.com/mcp");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn mcp_env_ref_is_resolved() {
        let tmp = tempdir();
        write_mcp(
            &tmp,
            "acme",
            r#"{"mcpServers":{"api":{"url":"https://${env:HOST}/mcp","env":{"TOKEN":"${env:TK}"}}}}"#,
        );
        let env = |v: &str| match v {
            "HOST" => Some("api.acme.test".to_string()),
            "TK" => Some("sekret".to_string()),
            _ => None,
        };
        let (servers, warnings) = collect_mcp_servers_with(&discover(&tmp), &env, &no_profile);
        assert_eq!(servers[0].spec["url"], "https://api.acme.test/mcp");
        assert_eq!(servers[0].spec["env"]["TOKEN"], "sekret");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn mcp_profile_refs_bare_and_field() {
        let tmp = tempdir();
        write_mcp(
            &tmp,
            "acme",
            r#"{"mcpServers":{"api":{"headers":{"Authorization":"Bearer ${profile:acme}","X-Refresh":"${profile:acme:refresh_token}"}}}}"#,
        );
        // bare ${profile:acme} → primary; explicit field → that field.
        let profile = |login: &str, field: Option<&str>| {
            if login != "acme" {
                return None;
            }
            match field {
                None => Some("primary-tok".to_string()),
                Some("refresh_token") => Some("refresh-tok".to_string()),
                Some(_) => None,
            }
        };
        let (servers, warnings) = collect_mcp_servers_with(&discover(&tmp), &no_env, &profile);
        assert_eq!(
            servers[0].spec["headers"]["Authorization"],
            "Bearer primary-tok"
        );
        assert_eq!(servers[0].spec["headers"]["X-Refresh"], "refresh-tok");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn mcp_collision_first_plugin_by_id_wins() {
        let tmp = tempdir();
        // "aaa" sorts before "bbb" → aaa wins.
        write_mcp(
            &tmp,
            "aaa",
            r#"{"mcpServers":{"shared":{"url":"https://aaa.test/mcp"}}}"#,
        );
        write_mcp(
            &tmp,
            "bbb",
            r#"{"mcpServers":{"shared":{"url":"https://bbb.test/mcp"}}}"#,
        );
        let (servers, warnings) =
            collect_mcp_servers_with(&discover(&tmp), &no_env, &no_profile);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].plugin_id, "aaa");
        assert_eq!(servers[0].spec["url"], "https://aaa.test/mcp");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("already contributed by plugin `aaa`"));
    }

    #[test]
    fn mcp_malformed_json_is_graceful() {
        let tmp = tempdir();
        write_mcp(&tmp, "broken", "{ not valid json");
        write_mcp(
            &tmp,
            "good",
            r#"{"mcpServers":{"ok":{"url":"https://ok.test/mcp"}}}"#,
        );
        let (servers, warnings) =
            collect_mcp_servers_with(&discover(&tmp), &no_env, &no_profile);
        // The good plugin still loads; the broken one warns and is skipped.
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].plugin_id, "good");
        assert!(warnings.iter().any(|w| w.contains("broken")
            && w.contains("not valid JSON")));
    }

    #[test]
    fn mcp_unset_env_ref_left_verbatim_with_warning() {
        let tmp = tempdir();
        write_mcp(
            &tmp,
            "acme",
            r#"{"mcpServers":{"api":{"url":"https://${env:MISSING}/mcp"}}}"#,
        );
        let (servers, warnings) =
            collect_mcp_servers_with(&discover(&tmp), &no_env, &no_profile);
        assert_eq!(servers[0].spec["url"], "https://${env:MISSING}/mcp");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("${env:MISSING}") && warnings[0].contains("unset"));
    }

    #[test]
    fn mcp_bare_ref_untouched_for_credentials_block() {
        let tmp = tempdir();
        // A bare ${access_token} + a credentials block: our resolver must leave
        // the bare ref alone so mcp::start_http interpolates it at connect time.
        write_mcp(
            &tmp,
            "acme",
            r#"{"mcpServers":{"api":{"url":"https://acme.test/mcp","credentials":{"file":"~/.aish/credentials","profile":"profile:acme"},"headers":{"Authorization":"Bearer ${access_token}"}}}}"#,
        );
        let (servers, warnings) =
            collect_mcp_servers_with(&discover(&tmp), &no_env, &no_profile);
        assert_eq!(
            servers[0].spec["headers"]["Authorization"],
            "Bearer ${access_token}"
        );
        assert!(warnings.is_empty(), "bare refs must not warn: {warnings:?}");
    }

    // ---- Phase 0.5.4: session-env injection from lifecycle-hook stdout ----

    /// Build a bare `Plugin` value (no on-disk files needed) for the injectable
    /// `collect_lifecycle_env_with` seam.
    fn bare_plugin(id: &str) -> Plugin {
        Plugin {
            manifest: manifest(&format!(r#"{{"id":"{id}"}}"#)),
            dir: PathBuf::from("/nonexistent").join(id),
            skills: Vec::new(),
            config: None,
        }
    }

    #[test]
    fn parse_env_lines_basic() {
        let (pairs, warns) = parse_env_lines("FOO=bar\nBAZ=qux\n", "p");
        assert_eq!(pairs, vec![("FOO".into(), "bar".into()), ("BAZ".into(), "qux".into())]);
        assert!(warns.is_empty());
    }

    #[test]
    fn parse_env_lines_ignores_non_kv_and_comments() {
        // Human status lines, blanks, and comments are silently dropped.
        let (pairs, warns) =
            parse_env_lines("Hello from the plugin\n\n# a comment\nREADY=1\nnot a pair\n", "p");
        assert_eq!(pairs, vec![("READY".into(), "1".into())]);
        assert!(warns.is_empty());
    }

    #[test]
    fn parse_env_lines_trims_and_validates_keys() {
        // Whitespace trimmed; a value may itself contain '='; invalid keys dropped.
        let (pairs, _) = parse_env_lines("  FOO = a=b=c \n1BAD=x\nGOOD_1=y\n", "p");
        assert_eq!(
            pairs,
            vec![("FOO".into(), "a=b=c".into()), ("GOOD_1".into(), "y".into())]
        );
    }

    #[test]
    fn parse_env_lines_rejects_credentials() {
        // Key or value looking secret-y is refused with a warning.
        let (pairs, warns) = parse_env_lines(
            "API_KEY=abc123\nDB_PASSWORD=hunter2\nMY_SECRET=x\nSAFE=ok\nSESSION_TOKEN=zzz\n",
            "p",
        );
        assert_eq!(pairs, vec![("SAFE".into(), "ok".into())]);
        assert_eq!(warns.len(), 4, "four credential-like pairs rejected: {warns:?}");
        assert!(warns.iter().all(|w| w.contains("redaction")));
    }

    #[test]
    fn parse_env_lines_secret_value_rejected_even_with_safe_key() {
        let (pairs, warns) = parse_env_lines("CONFIG=my-password-is-hunter2\n", "p");
        assert!(pairs.is_empty());
        assert_eq!(warns.len(), 1);
    }

    #[test]
    fn collect_lifecycle_env_single_plugin_multiple_vars() {
        let plugins = vec![bare_plugin("solo")];
        let run = |_p: &Plugin, _h: &str| Some("A=1\nB=2\n".to_string());
        let (pairs, warns) = collect_lifecycle_env_with(&plugins, "on_init", false, &run);
        assert_eq!(pairs, vec![("A".into(), "1".into()), ("B".into(), "2".into())]);
        assert!(warns.is_empty());
    }

    #[test]
    fn collect_lifecycle_env_multi_plugin_collision_first_wins() {
        // Plugins are id-sorted (a < b); first contributor of SHARED wins.
        let plugins = vec![bare_plugin("a"), bare_plugin("b")];
        let run = |p: &Plugin, _h: &str| {
            Some(match p.manifest.id.as_str() {
                "a" => "SHARED=from_a\nONLY_A=1\n".to_string(),
                "b" => "SHARED=from_b\nONLY_B=2\n".to_string(),
                _ => String::new(),
            })
        };
        let (pairs, warns) = collect_lifecycle_env_with(&plugins, "on_init", false, &run);
        assert_eq!(
            pairs,
            vec![
                ("SHARED".into(), "from_a".into()),
                ("ONLY_A".into(), "1".into()),
                ("ONLY_B".into(), "2".into()),
            ]
        );
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("SHARED") && warns[0].contains("keeping first"));
    }

    #[test]
    fn collect_lifecycle_env_disabled_escape_hatch() {
        let plugins = vec![bare_plugin("solo")];
        let run = |_p: &Plugin, _h: &str| Some("A=1\n".to_string());
        let (pairs, warns) = collect_lifecycle_env_with(&plugins, "on_init", true, &run);
        assert!(pairs.is_empty(), "kill switch suppresses all injection");
        assert!(warns.is_empty());
    }

    #[test]
    fn collect_lifecycle_env_skips_plugins_without_hook() {
        // `run` returns None for a plugin that ships no such hook script.
        let plugins = vec![bare_plugin("has"), bare_plugin("hasnt")];
        let run = |p: &Plugin, _h: &str| {
            (p.manifest.id == "has").then(|| "X=1\n".to_string())
        };
        let (pairs, _) = collect_lifecycle_env_with(&plugins, "on_init", false, &run);
        assert_eq!(pairs, vec![("X".into(), "1".into())]);
    }

    #[test]
    fn run_lifecycle_hook_script_captures_stdout() {
        // End-to-end fork/exec of a real on_init.sh, then parse.
        let tmp = tempdir();
        let pdir = tmp.join("greeter");
        fs::create_dir_all(&pdir).unwrap();
        fs::write(pdir.join("plugin.json"), r#"{"id":"greeter"}"#).unwrap();
        let script = pdir.join("on_init.sh");
        fs::write(
            &script,
            "#!/usr/bin/env bash\necho GREETING=hi\necho status line not a pair\necho READY=1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plugins = discover(&tmp);
        let (pairs, warns) = collect_lifecycle_env(&plugins, "on_init");
        assert_eq!(
            pairs,
            vec![("GREETING".into(), "hi".into()), ("READY".into(), "1".into())]
        );
        assert!(warns.is_empty());
    }
}
