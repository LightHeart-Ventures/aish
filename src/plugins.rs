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

// ---- Phase 0.5.3: `.mcp.json` merge from plugins into the client MCP set ----
//
// A plugin may ship a `<plugin>/.mcp.json` using the SAME schema as the user's
// `~/.aish/.mcp.json` (`{ "mcpServers": { "<name>": { … } } }`). At startup the
// existing `<plugin>/.mcp.json` paths are appended — in plugin-id order — AFTER
// the project- and user-scope paths and handed to [`crate::mcp::McpHost::start`].
// Because `McpHost` connects paths in order and skips any server name already
// connected (earlier path wins), the collision policy is **first-one-wins**:
//   project config  >  user config  >  plugin (id-sorted, alphabetically-first).
// Malformed / absent `.mcp.json` files are handled by `McpHost` (a bad JSON file
// earns a warning and is skipped, never aborting the scan). Secret references
// (`${env:VAR}` / `${profile:KEY}` / `${NAME}`) are resolved by `McpHost` at
// connect time — they never touch disk.

/// Parse a plugin's `<dir>/.mcp.json` and return its `mcpServers` object, or
/// `None` when the file is absent, unreadable, malformed, or has no
/// `mcpServers` object. Forgiving by design — a broken plugin never blocks
/// startup (mirrors [`discover`]).
pub fn read_plugin_mcp(plugin_dir: &Path) -> Option<serde_json::Map<String, Value>> {
    let text = std::fs::read_to_string(plugin_dir.join(".mcp.json")).ok()?;
    let root: Value = serde_json::from_str(&text).ok()?;
    root.get("mcpServers")?.as_object().cloned()
}

/// Existing `<plugin>/.mcp.json` paths across all discovered plugins, in
/// plugin-id order. Handed to [`crate::mcp::McpHost::start`] AFTER the project-
/// and user-scope config paths so those win on a name clash (first-path-wins).
pub fn plugin_mcp_paths(dir: &Path) -> Vec<PathBuf> {
    discover(dir)
        .into_iter()
        .map(|p| p.dir.join(".mcp.json"))
        .filter(|p| p.is_file())
        .collect()
}

/// One MCP server a plugin contributes to the client set.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginMcpServer {
    pub plugin_id: String,
    pub name: String,
    pub spec: Value,
}

/// A rejected server contribution: a plugin tried to register a `name` already
/// claimed by an earlier (higher-precedence) source. `winner` is the source
/// that keeps the name (`"config"` for project/user scope, or `"plugin:<id>"`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpCollision {
    pub name: String,
    pub winner: String,
    pub loser_plugin_id: String,
}

/// Resolve the effective set of plugin-contributed MCP servers to merge into
/// the client set, applying the first-one-wins collision policy against
/// `existing` (server names already defined by project/user config) and between
/// plugins (plugin-id order). Returns `(servers_to_add, collisions)`. This
/// mirrors — and is kept consistent with — the runtime path
/// ([`plugin_mcp_paths`] + `McpHost`'s earlier-path-wins connect), so
/// `:plugin info` and diagnostics can report exactly what will (and won't)
/// merge without connecting anything. Malformed/absent `.mcp.json` skipped.
pub fn collect_plugin_mcp_servers(
    dir: &Path,
    existing: &[String],
) -> (Vec<PluginMcpServer>, Vec<McpCollision>) {
    use std::collections::HashSet;
    let mut taken: HashSet<String> = existing.iter().cloned().collect();
    let mut servers: Vec<PluginMcpServer> = Vec::new();
    let mut collisions: Vec<McpCollision> = Vec::new();
    for plugin in discover(dir) {
        let id = plugin.manifest.id.clone();
        let Some(map) = read_plugin_mcp(&plugin.dir) else {
            continue;
        };
        for (name, spec) in map {
            if taken.contains(&name) {
                let winner = servers
                    .iter()
                    .find(|s| s.name == name)
                    .map(|s| format!("plugin:{}", s.plugin_id))
                    .unwrap_or_else(|| "config".to_string());
                collisions.push(McpCollision {
                    name,
                    winner,
                    loser_plugin_id: id.clone(),
                });
                continue;
            }
            taken.insert(name.clone());
            servers.push(PluginMcpServer {
                plugin_id: id.clone(),
                name,
                spec,
            });
        }
    }
    (servers, collisions)
}

// ---- Phase 0.5.4: session-env injection from lifecycle-hook stdout ----
//
// A plugin lifecycle hook (`<plugin>/hooks/<name>.sh`) may print `KEY=VALUE`
// lines on stdout; the loader parses them and injects the pairs into the session
// environment BEFORE the REPL starts, so every subsequently-spawned child sees
// them. Used to point the OSS skill provider at an org registry
// (`AISH_SKILL_REGISTRY`), pass a gateway URL into an injected `.mcp.json`, etc.
//
// Trust model (mirrors `src/hooks.rs`): fork/exec — NO shell; the `AISH_IN_HOOK`
// recursion guard is set so a hook that shells back into aish can't re-trigger
// itself; a per-hook timeout bounds a wedged script; **no credential values** —
// credential-like KEYs are rejected with a warning (secrets flow through the
// credential-profile path, never the session env). Operators can disable the
// whole mechanism with `AISH_ENV_INJECTION_DISABLED=1`.

/// Recursion guard env var — set on every spawned lifecycle hook so a hook that
/// invokes `aish` cannot re-enter the hook machinery. Matches `src/hooks.rs`.
const HOOK_RECURSION_GUARD: &str = "AISH_IN_HOOK";

/// Operator escape hatch: when set (to any non-empty, non-`0`/`false` value),
/// plugin lifecycle-hook env injection is skipped entirely.
pub const ENV_INJECTION_DISABLED: &str = "AISH_ENV_INJECTION_DISABLED";

/// Per-hook wall-clock budget. A hook that outruns it is killed and its output
/// discarded — a wedged `on_init.sh` must never hang shell startup.
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Case-insensitive substrings in an env-var NAME that mark it credential-like.
/// A lifecycle hook emitting such a var is REJECTED (never injected) — plugins
/// must route secrets through the credential-profile path, not the session env.
const CREDENTIAL_MARKERS: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "token",
    "apikey",
    "api_key",
    "credential",
    "private_key",
    "access_key",
    "auth",
    "key",
];

/// True when `AISH_ENV_INJECTION_DISABLED` is set to a truthy value.
pub fn env_injection_disabled() -> bool {
    matches!(
        std::env::var(ENV_INJECTION_DISABLED).ok().as_deref(),
        Some(v) if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
    )
}

/// A credential-like env-var NAME must never be injected from a plugin hook.
fn is_credential_like(key: &str) -> bool {
    let lk = key.to_ascii_lowercase();
    CREDENTIAL_MARKERS.iter().any(|m| lk.contains(m))
}

/// Valid POSIX-ish env-var NAME: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The outcome of parsing a lifecycle hook's stdout for `KEY=VALUE` exports.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HookEnv {
    /// Accepted `KEY=VALUE` pairs in first-seen order. A later duplicate KEY
    /// overwrites the earlier value (last-wins within a single hook's output).
    pub vars: Vec<(String, String)>,
    /// Human-readable warnings — rejected credential-like keys, etc. Surfaced to
    /// the operator so a silently-dropped export is never a mystery.
    pub warnings: Vec<String>,
}

/// Parse `KEY=VALUE` env exports from one lifecycle hook's stdout.
///
/// Rules:
/// - Blank lines and `#`-comment lines are ignored.
/// - A line must be `NAME=VALUE` where NAME is a valid env name; anything else
///   (status text, log lines) is **silently ignored** — hooks legitimately print
///   human output alongside exports.
/// - A leading `export ` prefix is tolerated (`export FOO=bar`).
/// - The value is taken verbatim after the first `=` (may contain `=`), with one
///   layer of surrounding single/double quotes stripped and trailing `\r` (CRLF)
///   removed. No shell expansion is performed.
/// - A credential-like NAME is **rejected** with a warning (never returned).
pub fn parse_hook_env(stdout: &str) -> HookEnv {
    let mut out = HookEnv::default();
    for raw in stdout.lines() {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue; // not a KEY=VALUE line → ignore
        };
        let key = key.trim();
        if !is_valid_env_name(key) {
            continue; // malformed name → ignore (not our export syntax)
        }
        if is_credential_like(key) {
            out.warnings.push(format!(
                "plugin env injection: rejected credential-like key `{key}` (secrets must use a credential profile, not session env)"
            ));
            continue;
        }
        let value = strip_one_quote_layer(value.trim());
        // last-wins within a single hook's output
        out.vars.retain(|(k, _)| k != key);
        out.vars.push((key.to_string(), value));
    }
    out
}

/// Strip exactly one matching layer of surrounding single or double quotes.
fn strip_one_quote_layer(v: &str) -> String {
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Run one plugin lifecycle-hook script and capture its stdout.
///
/// The script is `<plugin_dir>/hooks/<hook>.sh`, fork/exec'd directly (NO shell)
/// with `AISH_IN_HOOK=1` set and the current session env layered on. Returns the
/// captured stdout on a clean exit; `None` when the script is absent, cannot be
/// spawned, times out, or exits non-zero (a broken hook never blocks startup).
fn run_lifecycle_hook(plugin_dir: &Path, hook: &str, session_env: &[(String, String)]) -> Option<String> {
    let script = plugin_dir.join("hooks").join(format!("{hook}.sh"));
    if !script.is_file() {
        return None;
    }
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&script);
    cmd.env(HOOK_RECURSION_GUARD, "1")
        .current_dir(plugin_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in session_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    // Bounded wait: poll for completion up to HOOK_TIMEOUT, then kill.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut buf = String::new();
                if let Some(mut so) = child.stdout.take() {
                    use std::io::Read;
                    let _ = so.read_to_string(&mut buf);
                }
                return status.success().then_some(buf);
            }
            Ok(None) => {
                if start.elapsed() >= HOOK_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Run the named lifecycle hook (`on_init`, …) across every enabled plugin and
/// merge the `KEY=VALUE` exports into a single [`HookEnv`].
///
/// **Merge order across plugins:** plugin-id order (discovery is id-sorted), and
/// **first-plugin-wins** on a key clash — the alphabetically-first plugin that
/// exports a key keeps it; a later plugin exporting the same key is dropped with
/// a warning. This mirrors the `.mcp.json` first-one-wins policy (Phase 0.5.3).
///
/// The `session_env` (ambient/user env, already-set session vars) is passed to
/// each hook AND used as a precedence floor: a hook may not override a key that
/// already exists there — **user/existing env wins over plugin-injected env** —
/// so operator-set values are authoritative. Such a clash is dropped + warned.
pub fn collect_lifecycle_env(
    dir: &Path,
    hook: &str,
    session_env: &[(String, String)],
) -> HookEnv {
    use std::collections::HashSet;
    let mut merged = HookEnv::default();
    if env_injection_disabled() {
        return merged;
    }
    let existing: HashSet<String> = session_env.iter().map(|(k, _)| k.clone()).collect();
    let mut taken: HashSet<String> = HashSet::new();
    for plugin in discover(dir) {
        if !plugin.manifest.is_enabled() {
            continue;
        }
        let id = plugin.manifest.id.clone();
        let Some(stdout) = run_lifecycle_hook(&plugin.dir, hook, session_env) else {
            continue;
        };
        let parsed = parse_hook_env(&stdout);
        merged.warnings.extend(parsed.warnings);
        for (k, v) in parsed.vars {
            if existing.contains(&k) {
                merged.warnings.push(format!(
                    "plugin `{id}` env `{k}` ignored — already set in the environment (user env wins)"
                ));
                continue;
            }
            if taken.contains(&k) {
                merged.warnings.push(format!(
                    "plugin `{id}` env `{k}` ignored — already contributed by an earlier plugin (first-plugin-wins)"
                ));
                continue;
            }
            taken.insert(k.clone());
            merged.vars.push((k, v));
        }
    }
    merged
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

    // ---- Phase 0.5.3: `.mcp.json` merge from plugins into the client set ----

    /// Write `<root>/<id>/.mcp.json` with the given raw JSON body.
    fn write_plugin_mcp(root: &Path, id: &str, mcp_json: &str) {
        let pdir = root.join(id);
        fs::create_dir_all(&pdir).unwrap();
        fs::write(pdir.join(".mcp.json"), mcp_json).unwrap();
    }

    #[test]
    fn basic_mcp_merge_one_plugin_one_server() {
        let tmp = tempdir();
        write_plugin(&tmp, "p1", r#"{"id":"p1"}"#, None);
        write_plugin_mcp(
            &tmp,
            "p1",
            r#"{"mcpServers":{"weather":{"command":"weather-mcp","args":["--stdio"]}}}"#,
        );
        let (servers, collisions) = collect_plugin_mcp_servers(&tmp, &[]);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].plugin_id, "p1");
        assert_eq!(servers[0].name, "weather");
        assert_eq!(servers[0].spec["command"], "weather-mcp");
        assert!(collisions.is_empty());
        // The path collector sees the same file.
        assert_eq!(plugin_mcp_paths(&tmp).len(), 1);
    }

    #[test]
    fn env_and_profile_refs_are_preserved_verbatim_for_mcphost() {
        // Phase 0.5.3 does NOT resolve secret refs itself — McpHost does that at
        // connect time. The merge must carry `${env:…}` / `${profile:…}` through
        // untouched so they never land resolved on disk or in memory early.
        let tmp = tempdir();
        write_plugin(&tmp, "sec", r#"{"id":"sec"}"#, None);
        write_plugin_mcp(
            &tmp,
            "sec",
            r#"{"mcpServers":{"api":{"command":"x","env":{"TOKEN":"${env:MY_TOKEN}","KEY":"${profile:sec}"}}}}"#,
        );
        let (servers, _) = collect_plugin_mcp_servers(&tmp, &[]);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].spec["env"]["TOKEN"], "${env:MY_TOKEN}");
        assert_eq!(servers[0].spec["env"]["KEY"], "${profile:sec}");
    }

    #[test]
    fn collision_between_two_plugins_first_id_wins() {
        let tmp = tempdir();
        // "alpha" sorts before "beta"; both declare a server named "dup".
        write_plugin(&tmp, "alpha", r#"{"id":"alpha"}"#, None);
        write_plugin(&tmp, "beta", r#"{"id":"beta"}"#, None);
        write_plugin_mcp(
            &tmp,
            "alpha",
            r#"{"mcpServers":{"dup":{"command":"from-alpha"}}}"#,
        );
        write_plugin_mcp(
            &tmp,
            "beta",
            r#"{"mcpServers":{"dup":{"command":"from-beta"}}}"#,
        );
        let (servers, collisions) = collect_plugin_mcp_servers(&tmp, &[]);
        assert_eq!(servers.len(), 1, "only the first-id plugin's server merges");
        assert_eq!(servers[0].plugin_id, "alpha");
        assert_eq!(servers[0].spec["command"], "from-alpha");
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].name, "dup");
        assert_eq!(collisions[0].winner, "plugin:alpha");
        assert_eq!(collisions[0].loser_plugin_id, "beta");
    }

    #[test]
    fn existing_config_server_wins_over_plugin() {
        // A server name already defined by project/user config shadows any
        // plugin's contribution of the same name (config > plugin precedence).
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p"}"#, None);
        write_plugin_mcp(
            &tmp,
            "p",
            r#"{"mcpServers":{"github":{"command":"plugin-gh"}}}"#,
        );
        let (servers, collisions) =
            collect_plugin_mcp_servers(&tmp, &["github".to_string()]);
        assert!(servers.is_empty());
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].winner, "config");
        assert_eq!(collisions[0].loser_plugin_id, "p");
    }

    #[test]
    fn malformed_mcp_json_is_skipped_gracefully() {
        let tmp = tempdir();
        write_plugin(&tmp, "bad", r#"{"id":"bad"}"#, None);
        write_plugin_mcp(&tmp, "bad", "{ not json");
        write_plugin(&tmp, "good", r#"{"id":"good"}"#, None);
        write_plugin_mcp(
            &tmp,
            "good",
            r#"{"mcpServers":{"ok":{"command":"ok"}}}"#,
        );
        let (servers, collisions) = collect_plugin_mcp_servers(&tmp, &[]);
        // The malformed file is silently skipped; the good plugin still merges.
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].plugin_id, "good");
        assert!(collisions.is_empty());
        // read_plugin_mcp returns None for the malformed file.
        assert!(read_plugin_mcp(&tmp.join("bad")).is_none());
        // …and plugin_mcp_paths still lists it (existence, not validity).
        assert_eq!(plugin_mcp_paths(&tmp).len(), 2);
    }

    #[test]
    fn missing_mcp_json_yields_no_servers() {
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p"}"#, Some(("s", "d")));
        let (servers, collisions) = collect_plugin_mcp_servers(&tmp, &[]);
        assert!(servers.is_empty());
        assert!(collisions.is_empty());
        assert!(plugin_mcp_paths(&tmp).is_empty());
        assert!(read_plugin_mcp(&tmp.join("p")).is_none());
    }

    #[test]
    fn example_hello_world_plugin_mcp_parses() {
        // The shipped example fixture must carry a valid `.mcp.json`.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("plugins")
            .join("hello-world");
        let map = read_plugin_mcp(&dir).expect(".mcp.json present and valid");
        assert!(
            !map.is_empty(),
            "example .mcp.json declares at least one server"
        );
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
    // ---- Phase 0.5.4: lifecycle-hook env-injection tests ----

    /// Write an executable `<root>/<id>/hooks/<hook>.sh` with the given body.
    fn write_hook(root: &Path, id: &str, hook: &str, body: &str) {
        let hdir = root.join(id).join("hooks");
        fs::create_dir_all(&hdir).unwrap();
        let path = hdir.join(format!("{hook}.sh"));
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn parse_basic_key_value() {
        let env = parse_hook_env("FOO=bar\nBAZ=qux\n");
        assert_eq!(
            env.vars,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string())
            ]
        );
        assert!(env.warnings.is_empty());
    }

    #[test]
    fn parse_ignores_non_kv_and_comments() {
        // Status text, comments, blank lines, and bad names are silently ignored.
        let env = parse_hook_env(
            "starting hook...\n# a comment\n\nFOO=bar\n1BAD=x\nnot a pair\n  SPACED = ok \n",
        );
        assert_eq!(
            env.vars,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("SPACED".to_string(), "ok".to_string()),
            ]
        );
    }

    #[test]
    fn parse_quotes_export_prefix_and_crlf() {
        let env = parse_hook_env("export FOO=\"hello world\"\r\nBAR='v=1=2'\nQ=plain\n");
        assert_eq!(
            env.vars,
            vec![
                ("FOO".to_string(), "hello world".to_string()),
                ("BAR".to_string(), "v=1=2".to_string()),
                ("Q".to_string(), "plain".to_string()),
            ]
        );
    }

    #[test]
    fn parse_last_wins_within_hook() {
        let env = parse_hook_env("FOO=first\nFOO=second\n");
        assert_eq!(env.vars, vec![("FOO".to_string(), "second".to_string())]);
    }

    #[test]
    fn parse_rejects_credential_like_keys() {
        let env = parse_hook_env(
            "API_TOKEN=abc\nMY_PASSWORD=hunter2\nAWS_SECRET_ACCESS_KEY=z\nSAFE=ok\nPUBLIC_KEY=pk\n",
        );
        // Only the non-credential-like key survives.
        assert_eq!(env.vars, vec![("SAFE".to_string(), "ok".to_string())]);
        // Four rejects (TOKEN, PASSWORD, SECRET/KEY, KEY) each warned.
        assert_eq!(env.warnings.len(), 4);
        assert!(env.warnings.iter().all(|w| w.contains("credential-like")));
    }

    #[test]
    fn run_hook_captures_multiple_vars() {
        let tmp = tempdir();
        write_hook(
            &tmp,
            "p",
            "on_init",
            "#!/bin/sh\necho 'plugin loaded'\necho FOO=bar\necho BAZ=qux\n",
        );
        let out = run_lifecycle_hook(&tmp.join("p"), "on_init", &[]).expect("hook ran");
        let env = parse_hook_env(&out);
        assert_eq!(
            env.vars,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string())
            ]
        );
    }

    #[test]
    fn run_hook_absent_returns_none() {
        let tmp = tempdir();
        fs::create_dir_all(tmp.join("p")).unwrap();
        assert!(run_lifecycle_hook(&tmp.join("p"), "on_init", &[]).is_none());
    }

    #[test]
    fn run_hook_nonzero_exit_returns_none() {
        let tmp = tempdir();
        write_hook(&tmp, "p", "on_init", "#!/bin/sh\necho FOO=bar\nexit 1\n");
        assert!(run_lifecycle_hook(&tmp.join("p"), "on_init", &[]).is_none());
    }

    #[test]
    fn collect_merges_and_injects_from_enabled_plugins() {
        let tmp = tempdir();
        write_plugin(&tmp, "aaa", r#"{"id":"aaa"}"#, None);
        write_hook(&tmp, "aaa", "on_init", "#!/bin/sh\necho FROM_A=1\n");
        write_plugin(&tmp, "bbb", r#"{"id":"bbb"}"#, None);
        write_hook(&tmp, "bbb", "on_init", "#!/bin/sh\necho FROM_B=2\n");
        let env = collect_lifecycle_env(&tmp, "on_init", &[]);
        assert_eq!(
            env.vars,
            vec![
                ("FROM_A".to_string(), "1".to_string()),
                ("FROM_B".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn collect_first_plugin_wins_on_collision() {
        let tmp = tempdir();
        // id-sorted discovery ⇒ "aaa" contributes SHARED first; "zzz" is dropped.
        write_plugin(&tmp, "aaa", r#"{"id":"aaa"}"#, None);
        write_hook(&tmp, "aaa", "on_init", "#!/bin/sh\necho SHARED=from_aaa\n");
        write_plugin(&tmp, "zzz", r#"{"id":"zzz"}"#, None);
        write_hook(&tmp, "zzz", "on_init", "#!/bin/sh\necho SHARED=from_zzz\n");
        let env = collect_lifecycle_env(&tmp, "on_init", &[]);
        assert_eq!(env.vars, vec![("SHARED".to_string(), "from_aaa".to_string())]);
        assert!(env
            .warnings
            .iter()
            .any(|w| w.contains("zzz") && w.contains("first-plugin-wins")));
    }

    #[test]
    fn collect_user_env_wins_over_plugin() {
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p"}"#, None);
        write_hook(&tmp, "p", "on_init", "#!/bin/sh\necho PATH=hacked\necho NEW=ok\n");
        let ambient = vec![("PATH".to_string(), "/usr/bin".to_string())];
        let env = collect_lifecycle_env(&tmp, "on_init", &ambient);
        // PATH already set by user ⇒ dropped; NEW is fresh ⇒ kept.
        assert_eq!(env.vars, vec![("NEW".to_string(), "ok".to_string())]);
        assert!(env
            .warnings
            .iter()
            .any(|w| w.contains("PATH") && w.contains("user env wins")));
    }

    #[test]
    fn collect_disabled_plugin_hook_is_skipped() {
        let tmp = tempdir();
        write_plugin(&tmp, "off", r#"{"id":"off","enabled":false}"#, None);
        write_hook(&tmp, "off", "on_init", "#!/bin/sh\necho FOO=bar\n");
        assert!(collect_lifecycle_env(&tmp, "on_init", &[]).vars.is_empty());
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
