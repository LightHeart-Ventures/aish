//! Plugin discovery — the minimal, shipping slice of the plugin system.
//!
//! Plugins live in `~/.aish/plugins/<plugin-id>/` and, for now, can contribute
//! **skills** that expand the shell's skill registry. A plugin is any
//! subdirectory holding a readable, parseable `plugin.json`; its skills live in
//! `<plugin>/skills/<skill-name>/SKILL.md` — the exact on-disk layout
//! [`crate::skills::load`] already understands, so a plugin's skills flow into
//! the same catalog the agent sees for `~/.aish/skills`.
//!
//! This is deliberately the smallest useful piece of a broader plugin system
//! (webhooks, hooks, MCP servers, tools, memory, schemas — see
//! `docs/reference/plugins/` and `docs/design/*-plugin-integration.md` for the
//! surfaces that have since landed). Everything not needed to expand the skill
//! registry is ignored — unknown `plugin.json` keys parse and are dropped — so
//! a richer manifest still loads today and future phases can grow into it
//! without breaking existing plugins.

use crate::skills::Skill;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Minimal `plugin.json` manifest. Only the fields the current phases need are
/// parsed; every other key in the design doc (tools, schemas, …) is ignored via
/// serde's default unknown-field-dropping so a fuller manifest still
/// deserializes.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // name/version/description are parsed manifest surface, consumed by later plugin phases
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
    /// Phase 1.6 webhook opt-in: a command run (fork/exec as argv, NO shell —
    /// SPR-069 TASK-379) on each lifecycle event, event JSON on stdin. Consumed
    /// by [`crate::plugin_dispatcher`].
    #[serde(default)]
    pub webhook_command: Option<String>,
    /// Optional JSON-Schema-shaped description of the plugin's configuration
    /// (`{ "type": "object", "properties": {...}, "required": [...] }`). Drives
    /// default-filling and validation in [`load_config`] (Phase 1.4). Absent →
    /// the plugin takes no configuration and `load_config` yields `{}`.
    #[serde(default)]
    pub config_schema: Option<Value>,
    /// The capabilities a plugin contributes to the shell — lifecycle hooks,
    /// event hooks, config/env injection, login command, … Only the fields
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
    /// Phase 1 (plugin-skill-sources, design §3): declares this plugin a
    /// **skill source** — a federated search/add provider that joins the
    /// `:skill search` fan-out and `:skill add` priority-resolution. Absent →
    /// the plugin contributes no skill source. Consumed by
    /// [`discover_skill_sources`] and (later phases) the `SkillSource` façade
    /// and `:skill` verb handlers.
    #[serde(default)]
    pub skill_source: Option<SkillSource>,
    /// Phase 4 (TASK-317, SPR-073): declarative background **timers**. Each entry
    /// runs a plain program on a fixed interval and (optionally) caches its
    /// stdout to a file — a cheap, always-on alternative to throttling work
    /// inside a `TurnEnd` hook, e.g. keeping a SecondStatusLine segment fresh.
    /// Absent/empty → the plugin arms no timers. Consumed by
    /// [`crate::plugin_timers::arm`].
    #[serde(default)]
    pub timers: Vec<PluginTimer>,
    /// Phase 2b (TASK-318, SPR-073): a first-class **statusline** segment. The
    /// plugin declares only a `command` (plus optional `args`/`every`/
    /// `timeout_ms`); **core** owns the refresh cadence, the cache, and the
    /// render contract — the plugin never touches the raw
    /// `~/.aish/state/statusline/*.txt` file convention. Core runs the command
    /// on a cadence and folds its stdout onto the SecondStatusLine. Absent →
    /// the plugin contributes no first-class statusline segment. Consumed by
    /// [`crate::plugin_statusline::arm`].
    #[serde(default)]
    pub statusline: Option<PluginStatusline>,
}

/// A `provides.statusline` block (TASK-318, SPR-073): a plugin's declarative,
/// first-class SecondStatusLine segment. Unlike the Phase 1 file convention
/// (where a plugin armed a timer, picked the magic cache path, and wrote a
/// `*.txt` file itself), here the plugin declares ONLY a `command` and core
/// owns everything else — cadence, an in-memory cache, staleness, and the
/// render. `command` runs via direct fork/exec (NO shell) with the plugin
/// directory as CWD; its first non-empty stdout line becomes the segment (the
/// plugin owns any ANSI color). Unknown keys are dropped by serde for the usual
/// forward-compat story.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct PluginStatusline {
    /// Program to exec (`argv[0]`). A value that resolves to a file inside the
    /// plugin directory runs that file; otherwise it's looked up on `PATH`.
    pub command: String,
    /// Extra arguments passed verbatim to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Refresh interval as a compact duration — `"30s"`, `"10m"`, `"1h"`,
    /// `"1d"` (a bare integer means seconds); same grammar as
    /// [`crate::plugin_timers::parse_every`]. Absent/unparseable → the loader's
    /// default cadence (see [`crate::plugin_statusline`]).
    #[serde(default)]
    pub every: Option<String>,
    /// Per-run wall-clock timeout in milliseconds. A run that overruns is killed
    /// and skipped; the prior segment ages out naturally. Absent → the loader
    /// default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// A single `provides.timers[]` entry (TASK-317, SPR-073): run `command` (with
/// `args`) every `every`, and — when `cache` is set — write its stdout to that
/// file. The turn-independent primitive that lets a plugin keep a status segment
/// fresh without hanging refresh work off the agent turn loop. Programs run via
/// direct fork/exec (NO shell) with the plugin directory as CWD; unknown keys
/// are dropped by serde for the usual forward-compat story.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct PluginTimer {
    /// Program to exec (`argv[0]`). A value that resolves to a file inside the
    /// plugin directory runs that file; otherwise it's looked up on `PATH`.
    pub command: String,
    /// Extra arguments passed verbatim to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Interval between runs as a compact duration — `"30s"`, `"10m"`, `"1h"`,
    /// `"1d"` (a bare integer means seconds). Parsed by
    /// [`crate::plugin_timers::parse_every`]; an unparseable/zero value disarms
    /// the timer (logged, never fatal).
    pub every: String,
    /// Optional file the command's stdout is written to after each run (relative
    /// paths resolve under `~/.aish/`). Absent → the command's own side effects
    /// are the payload and nothing is written by the loader.
    #[serde(default)]
    pub cache: Option<String>,
    /// Per-run wall-clock timeout in milliseconds (default 60000). A run that
    /// overruns is killed and skipped; the interval cadence is preserved.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// The `provides.skill_source` block (design
/// `docs/design/plugin-skill-sources.md` §3): declares a plugin as a federated
/// skill **search/add** provider. Every field is optional so a source may be
/// search-only, add-only, or a bare priority-labelled façade; unknown keys are
/// dropped by serde for the same forward/backward-compat story as the rest of
/// the manifest surface.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct SkillSource {
    /// Source label shown in the SOURCE column and `:skill sources`. Defaults
    /// to the owning plugin id when absent (resolved in
    /// [`discover_skill_sources`]).
    #[serde(default)]
    pub id: Option<String>,
    /// Merge/precedence rank — higher wins on ref/name-dedup ties and orders
    /// `add` attempts. The built-in embedded index sits at a low fixed priority
    /// so plugins can outrank it. Defaults to `0`.
    #[serde(default)]
    pub priority: i64,
    /// Handler script (relative to the plugin dir) answering `:skill search`.
    /// `None` → the source is add-only.
    #[serde(default)]
    pub search: Option<String>,
    /// Handler script resolving a `:skill add <ref>`. `None` → search-only.
    #[serde(default)]
    pub add: Option<String>,
    /// Glob/prefix patterns of `reference` namespaces this source claims for
    /// `add` routing (e.g. `"github:*"`, `"acme/*"`, `"*"`). Drives which
    /// source(s) a given `:skill add` is offered to, in priority order.
    #[serde(default)]
    pub handles: Vec<String>,
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

    /// This plugin's `provides.skill_source` block, if it declares one
    /// (plugin-skill-sources design §3). `None` when the plugin contributes no
    /// skill source. The paired discovery helper is [`discover_skill_sources`].
    #[allow(dead_code)] // consumed by the Phase 2+ SkillSource façade / `:skill` verb handlers
    pub fn skill_source(&self) -> Option<&SkillSource> {
        self.provides.as_ref().and_then(|p| p.skill_source.as_ref())
    }

    /// This plugin's declared background timers (`provides.timers`, TASK-317).
    /// Empty slice when the plugin declares no `provides` block or no timers.
    /// Consumed by [`crate::plugin_timers::arm`].
    pub fn timers(&self) -> &[PluginTimer] {
        match &self.provides {
            Some(p) => &p.timers,
            None => &[],
        }
    }

    /// This plugin's first-class statusline segment (`provides.statusline`,
    /// TASK-318). `None` when the plugin declares no `provides` block or no
    /// statusline. Consumed by [`crate::plugin_statusline::arm`].
    pub fn statusline(&self) -> Option<&PluginStatusline> {
        self.provides.as_ref().and_then(|p| p.statusline.as_ref())
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
    /// The JSON Schemas this plugin ships under `<plugin>/schemas/*.json`
    /// (Phase 3). Each file's stem is the schema name; the parsed body is a
    /// JSON-Schema document used to validate structured tool/skill output via
    /// [`validate_json_schema`]. Malformed schema files are skipped at
    /// discovery (forgiving, like everything else here). Empty when the plugin
    /// ships no `schemas/` directory.
    pub schemas: Vec<PluginSchema>,
}

impl Plugin {
    /// This plugin's schema with the given name (file stem), or `None`.
    // Phase 3.4: WIRED — reached via `validate` → the engine tool-return hook.
    pub fn schema(&self, name: &str) -> Option<&PluginSchema> {
        self.schemas.iter().find(|s| s.name == name)
    }

    /// Validate a structured value against one of this plugin's named schemas
    /// (Phase 3.4). `Err(SchemaValidationError::UnknownSchema)` when the plugin
    /// ships no schema by that name; `Err(Failed)` with the collected violations
    /// when the value doesn't conform.
    // Phase 3.4: WIRED — called by `validate_against_plugin_schema`, which the
    // engine tool-return hook invokes on every schema-declaring result.
    pub fn validate(&self, schema_name: &str, value: &Value) -> Result<(), SchemaValidationError> {
        match self.schema(schema_name) {
            None => Err(SchemaValidationError::UnknownSchema(schema_name.to_string())),
            Some(s) => {
                let violations = validate_json_schema(&s.schema, value);
                if violations.is_empty() {
                    Ok(())
                } else {
                    Err(SchemaValidationError::Failed(violations))
                }
            }
        }
    }
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
        // Phase 3.1: load every JSON Schema the plugin ships under `schemas/`.
        let schemas = load_schemas(&pdir);
        plugins.push(Plugin {
            manifest,
            dir: pdir,
            skills,
            config,
            schemas,
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

/// A discovered plugin that declares `provides.skill_source`, resolved and
/// paired with the plugin directory its handler scripts run in. Produced by
/// [`discover_skill_sources`], one per skill-source plugin.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // fields consumed by the Phase 2+ SkillSource façade / search fan-out
pub struct ResolvedSkillSource {
    /// Owning plugin id (`manifest.id`).
    pub plugin_id: String,
    /// Source label — `skill_source.id` when set, else the owning plugin id.
    pub id: String,
    /// Merge/precedence rank; higher wins. Mirrors `skill_source.priority`.
    pub priority: i64,
    /// `search` handler script (relative to `dir`), if the source answers search.
    pub search: Option<String>,
    /// `add` handler script (relative to `dir`), if the source resolves add.
    pub add: Option<String>,
    /// `handles` globs the source claims for `:skill add` routing.
    pub handles: Vec<String>,
    /// Plugin directory the handler scripts are exec'd in.
    pub dir: PathBuf,
}

/// Collect every discovered plugin that declares a `provides.skill_source`
/// block into [`ResolvedSkillSource`]s, ordered by `priority` **descending**
/// then source `id` **ascending** — the deterministic merge order the
/// `:skill search` fan-out and `:skill add` priority-resolution consume
/// (design §4). A source's `id` defaults to the owning plugin id when the
/// manifest leaves `skill_source.id` unset. Mirrors [`plugin_skills`]'s
/// `discover`-then-traverse shape and inherits its forgiving discovery — a
/// broken plugin is skipped, never fatal.
#[allow(dead_code)] // WIRED by TASK-342 (`:skill search` fan-out) / TASK-343 (`add` resolution)
pub fn discover_skill_sources(dir: &Path) -> Vec<ResolvedSkillSource> {
    let mut sources: Vec<ResolvedSkillSource> = discover(dir)
        .into_iter()
        .filter_map(|p| {
            let src = p
                .manifest
                .provides
                .as_ref()
                .and_then(|pr| pr.skill_source.clone())?;
            let plugin_id = p.manifest.id.clone();
            let id = src.id.clone().unwrap_or_else(|| plugin_id.clone());
            Some(ResolvedSkillSource {
                plugin_id,
                id,
                priority: src.priority,
                search: src.search,
                add: src.add,
                handles: src.handles,
                dir: p.dir,
            })
        })
        .collect();
    // priority desc, then id asc — stable, total, deterministic.
    sources.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));
    sources
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

// ---- Phase 3: schemas & structured-data validation ------------------------
//
// A plugin may ship JSON Schema documents under `<plugin>/schemas/*.json`. Each
// file's stem is the schema *name* (`schemas/issue.json` → `"issue"`); the body
// is a JSON-Schema document. These describe the shape of structured data a
// plugin's tools/skills return, so aish can validate that output at runtime
// (Phase 3.4) and surface schema provenance in `:plugin info --schema`
// (Phase 3.5). Discovery is forgiving — a malformed schema file is skipped, not
// fatal, mirroring the rest of the loader.
//
// NOTE: `validate_json_schema` and the `Plugin::validate` /
// `validate_against_plugin_schema` seam are the Phase-3.4 "validate tool output
// at runtime" entry points. RESOLVED (Phase 3.4 runtime-enforcement half): the
// tool-return dispatch wiring now exists — `engine::validate_output_schema`
// calls `validate_against_plugin_schema` after every tool call whose
// `ToolResult` carries an `output_schema` declaration, logging violations
// fail-open and annotating the result for the model. These are therefore LIVE
// runtime paths, not dead code.
//
// The validator ([`validate_json_schema`]) is a pragmatic subset of JSON Schema
// draft-07 covering the keywords structured tool output actually uses:
//   type (incl. type arrays), enum, const, required, properties,
//   additionalProperties (bool | schema), items, min/maxItems, min/maxLength,
//   minimum/maximum (+ exclusive*), and pattern (regex). Unknown keywords are
//   ignored (permissive), and every violation is collected (not fail-fast) so
//   error reports point at *all* the problems at once.

/// One JSON Schema a plugin contributes under `<plugin>/schemas/<name>.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginSchema {
    /// Schema name — the file stem (`schemas/issue.json` → `"issue"`).
    pub name: String,
    /// The parsed JSON-Schema document.
    pub schema: Value,
}

/// Load every `<plugin_dir>/schemas/*.json` into a name→schema list, sorted by
/// name. Absent `schemas/` dir → empty. Unreadable or non-object / invalid-JSON
/// files are skipped silently (forgiving discovery). Only files with a `.json`
/// extension are considered.
pub fn load_schemas(plugin_dir: &Path) -> Vec<PluginSchema> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plugin_dir.join("schemas")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(schema) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // A schema document must be a JSON object (or `true`/`false`); reject
        // arrays/strings/etc. so a stray non-schema file doesn't masquerade.
        if !schema.is_object() && !schema.is_boolean() {
            continue;
        }
        out.push(PluginSchema {
            name: stem.to_string(),
            schema,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A single schema-validation failure: a JSON-pointer-ish `path` into the
/// instance (`""` = root, `/items/0/name` = nested) plus a human `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SchemaViolation {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for SchemaViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let at = if self.path.is_empty() { "(root)" } else { &self.path };
        write!(f, "{at}: {}", self.message)
    }
}

/// The outcome of [`Plugin::validate`] / [`validate_against_plugin_schema`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SchemaValidationError {
    /// The plugin ships no schema by that name.
    UnknownSchema(String),
    /// The value did not conform; carries every collected violation.
    Failed(Vec<SchemaViolation>),
}

impl std::fmt::Display for SchemaValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaValidationError::UnknownSchema(n) => write!(f, "no such schema `{n}`"),
            SchemaValidationError::Failed(v) => {
                write!(f, "{} schema violation(s):", v.len())?;
                for x in v {
                    write!(f, "\n  - {x}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SchemaValidationError {}

/// Validate a JSON `instance` against a JSON-Schema `schema`, returning every
/// violation found (empty vec = valid). Pragmatic draft-07 subset — see the
/// module section comment for the covered keyword set. A boolean schema is
/// honored (`true` = accept anything, `false` = reject everything).
#[allow(dead_code)] // Phase 3.4 runtime seam — see section note; used by tests.
pub fn validate_json_schema(schema: &Value, instance: &Value) -> Vec<SchemaViolation> {
    let mut violations = Vec::new();
    validate_at("", schema, instance, &mut violations);
    violations
}

#[allow(dead_code)]
fn validate_at(path: &str, schema: &Value, instance: &Value, out: &mut Vec<SchemaViolation>) {
    // Boolean schemas: `true` accepts, `false` rejects.
    match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            push(out, path, "schema `false` rejects all values");
            return;
        }
        Value::Object(_) => {}
        // A non-object, non-bool "schema" can't constrain anything.
        _ => return,
    }

    // enum
    if let Some(Value::Array(allowed)) = schema.get("enum") {
        if !allowed.iter().any(|a| a == instance) {
            push(out, path, &format!("value not in enum {}", compact(&Value::Array(allowed.clone()))));
        }
    }
    // const
    if let Some(expected) = schema.get("const") {
        if expected != instance {
            push(out, path, &format!("value must equal const {}", compact(expected)));
        }
    }

    // type (string or array of strings)
    if let Some(t) = schema.get("type") {
        let ok = match t {
            Value::String(s) => json_type_matches(s, instance),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .any(|s| json_type_matches(s, instance)),
            _ => true,
        };
        if !ok {
            push(
                out,
                path,
                &format!("expected type {}, got {}", compact(t), json_type_name(instance)),
            );
        }
    }

    match instance {
        Value::Object(map) => validate_object(path, schema, map, out),
        Value::Array(arr) => validate_array(path, schema, arr, out),
        Value::String(s) => validate_string(path, schema, s, out),
        Value::Number(_) => validate_number(path, schema, instance, out),
        _ => {}
    }
}

#[allow(dead_code)]
fn validate_object(
    path: &str,
    schema: &Value,
    map: &serde_json::Map<String, Value>,
    out: &mut Vec<SchemaViolation>,
) {
    // required
    if let Some(Value::Array(req)) = schema.get("required") {
        for r in req {
            if let Some(name) = r.as_str() {
                if !map.contains_key(name) {
                    push(out, path, &format!("missing required property `{name}`"));
                }
            }
        }
    }
    let props = schema.get("properties").and_then(|p| p.as_object());
    // properties → recurse
    if let Some(props) = props {
        for (k, sub) in props {
            if let Some(v) = map.get(k) {
                validate_at(&child(path, k), sub, v, out);
            }
        }
    }
    // additionalProperties: false → reject unknowns; object → validate extras.
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            for k in map.keys() {
                let known = props.map(|p| p.contains_key(k)).unwrap_or(false);
                if !known {
                    push(out, path, &format!("additional property `{k}` is not allowed"));
                }
            }
        }
        Some(sub @ Value::Object(_)) => {
            for (k, v) in map {
                let known = props.map(|p| p.contains_key(k)).unwrap_or(false);
                if !known {
                    validate_at(&child(path, k), sub, v, out);
                }
            }
        }
        _ => {}
    }
}

#[allow(dead_code)]
fn validate_array(path: &str, schema: &Value, arr: &[Value], out: &mut Vec<SchemaViolation>) {
    if let Some(min) = schema.get("minItems").and_then(|v| v.as_u64()) {
        if (arr.len() as u64) < min {
            push(out, path, &format!("array has {} item(s), minItems is {min}", arr.len()));
        }
    }
    if let Some(max) = schema.get("maxItems").and_then(|v| v.as_u64()) {
        if (arr.len() as u64) > max {
            push(out, path, &format!("array has {} item(s), maxItems is {max}", arr.len()));
        }
    }
    // items: single schema applied to every element.
    if let Some(items) = schema.get("items") {
        if items.is_object() || items.is_boolean() {
            for (i, v) in arr.iter().enumerate() {
                validate_at(&child(path, &i.to_string()), items, v, out);
            }
        }
    }
}

#[allow(dead_code)]
fn validate_string(path: &str, schema: &Value, s: &str, out: &mut Vec<SchemaViolation>) {
    let len = s.chars().count() as u64;
    if let Some(min) = schema.get("minLength").and_then(|v| v.as_u64()) {
        if len < min {
            push(out, path, &format!("string length {len} is below minLength {min}"));
        }
    }
    if let Some(max) = schema.get("maxLength").and_then(|v| v.as_u64()) {
        if len > max {
            push(out, path, &format!("string length {len} exceeds maxLength {max}"));
        }
    }
    if let Some(pat) = schema.get("pattern").and_then(|v| v.as_str()) {
        match regex::Regex::new(pat) {
            Ok(re) => {
                if !re.is_match(s) {
                    push(out, path, &format!("string does not match pattern `{pat}`"));
                }
            }
            // An invalid pattern in the schema is the author's bug, not the
            // instance's — report it against the path so it's visible.
            Err(_) => push(out, path, &format!("schema has invalid regex pattern `{pat}`")),
        }
    }
}

#[allow(dead_code)]
fn validate_number(path: &str, schema: &Value, instance: &Value, out: &mut Vec<SchemaViolation>) {
    let Some(n) = instance.as_f64() else { return };
    if let Some(min) = schema.get("minimum").and_then(|v| v.as_f64()) {
        if n < min {
            push(out, path, &format!("{n} is below minimum {min}"));
        }
    }
    if let Some(max) = schema.get("maximum").and_then(|v| v.as_f64()) {
        if n > max {
            push(out, path, &format!("{n} exceeds maximum {max}"));
        }
    }
    if let Some(exmin) = schema.get("exclusiveMinimum").and_then(|v| v.as_f64()) {
        if n <= exmin {
            push(out, path, &format!("{n} must be > exclusiveMinimum {exmin}"));
        }
    }
    if let Some(exmax) = schema.get("exclusiveMaximum").and_then(|v| v.as_f64()) {
        if n >= exmax {
            push(out, path, &format!("{n} must be < exclusiveMaximum {exmax}"));
        }
    }
}

/// JSON-pointer child path: `child("/a", "b") == "/a/b"`, `child("", "b") == "/b"`.
#[allow(dead_code)]
fn child(parent: &str, key: &str) -> String {
    // Escape per RFC 6901 (~ → ~0, / → ~1) so keys with slashes stay unambiguous.
    let esc = key.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{esc}")
}

#[allow(dead_code)]
fn push(out: &mut Vec<SchemaViolation>, path: &str, message: &str) {
    out.push(SchemaViolation {
        path: path.to_string(),
        message: message.to_string(),
    });
}

/// Compact single-line JSON rendering for error messages.
fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "?".to_string())
}

/// Validate `value` against `<plugins_dir>/<plugin_id>/schemas/<schema_name>.json`
/// (Phase 3.4 runtime entry point — WIRED into `engine::validate_output_schema`).
/// Discovers the plugin fresh so callers need only the plugins dir + ids.
/// `UnknownSchema` when the plugin (or schema) is absent; `Failed` with all
/// violations otherwise.
pub fn validate_against_plugin_schema(
    plugins_dir: &Path,
    plugin_id: &str,
    schema_name: &str,
    value: &Value,
) -> Result<(), SchemaValidationError> {
    let plugins = discover(plugins_dir);
    match plugins.iter().find(|p| p.manifest.id == plugin_id) {
        Some(p) => p.validate(schema_name, value),
        None => Err(SchemaValidationError::UnknownSchema(schema_name.to_string())),
    }
}

/// Render the `:plugin info <id> --schema` detail block: every schema the plugin
/// ships, with its declared top-level `type`, `required` keys, and `properties`
/// names — enough to see the shape without dumping the whole document. `None`
/// when no such plugin exists.
pub fn format_plugin_schemas(dir: &Path, id: &str) -> Option<String> {
    let plugins = discover(dir);
    let plugin = plugins.iter().find(|p| p.manifest.id == id)?;
    let mut out = format!("plugin `{}` schemas\n", plugin.manifest.id);
    if plugin.schemas.is_empty() {
        out.push_str("  (none)\n");
        return Some(out.trim_end().to_string());
    }
    for s in &plugin.schemas {
        let ty = s
            .schema
            .get("type")
            .map(compact)
            .unwrap_or_else(|| "-".to_string());
        let required: Vec<String> = s
            .schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let props: Vec<String> = s
            .schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        out.push_str(&format!("  {} (type {ty})\n", s.name));
        if !props.is_empty() {
            out.push_str(&format!("    properties: {}\n", props.join(", ")));
        }
        if !required.is_empty() {
            out.push_str(&format!("    required:   {}\n", required.join(", ")));
        }
    }
    Some(out.trim_end().to_string())
}

/// Render the `:plugin info <id> --mcp` detail block: this plugin's own
/// `.mcp.json` server contributions, resolved against `existing` (server
/// names already claimed by project/user config or an earlier-discovered
/// plugin) via [`collect_plugin_mcp_servers`] — so the report shows exactly
/// which of the plugin's servers will actually connect and which lose to a
/// name collision. `None` when no such plugin exists. Never echoes a spec's
/// resolved secret refs, only server names.
pub fn format_plugin_mcp(dir: &Path, id: &str, existing: &[String]) -> Option<String> {
    let plugins = discover(dir);
    let plugin = plugins.iter().find(|p| p.manifest.id == id)?;
    let mut out = format!("plugin `{}` mcp servers\n", plugin.manifest.id);

    let raw = read_plugin_mcp(&plugin.dir).unwrap_or_default();
    if raw.is_empty() {
        out.push_str("  (none)\n");
        return Some(out.trim_end().to_string());
    }

    let (servers, collisions) = collect_plugin_mcp_servers(dir, existing);
    let mut names: Vec<&String> = raw.keys().collect();
    names.sort();
    for name in names {
        if servers.iter().any(|s| s.plugin_id == id && s.name == *name) {
            out.push_str(&format!("  {name} — will connect\n"));
        } else if let Some(c) = collisions
            .iter()
            .find(|c| c.loser_plugin_id == id && c.name == *name)
        {
            out.push_str(&format!(
                "  {name} — skipped, name already claimed by {}\n",
                c.winner
            ));
        } else {
            // Shouldn't happen (every raw name is either accepted or a
            // collision), but stay forgiving rather than panic on a report.
            out.push_str(&format!("  {name} — unresolved\n"));
        }
    }
    Some(out.trim_end().to_string())
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
    collect_lifecycle_env_at(
        dir,
        hook,
        session_env,
        &crate::plugin_auth::credentials_path(),
    )
}

/// [`collect_lifecycle_env`] against an explicit credentials path — the seam the
/// tests drive so credential→hook injection is exercised without touching the
/// real `~/.aish/credentials`.
///
/// **Phase 0.5.5 — credential→lifecycle-hook injection.** Before a plugin's hook
/// is fork/exec'd, that plugin's OWN logged-in credentials (persisted by
/// `aish login <id>` under `[profile:<id>]`) are exported into the hook's
/// environment as `AISH_PROFILE_<ID>_<FIELD>` via
/// [`crate::plugin_auth::profile_env_at`]. This lets `on_init.sh` use the token
/// for setup. Scope + safety:
///   * **hook-process-only** — the credential vars are handed to the child
///     process; they are NEVER merged into the shared session env (`merged`).
///   * **own-profile-only** — plugin `<id>` only ever sees `[profile:<id>]`; one
///     plugin cannot read another's credentials.
///   * a hook that echoes a credential back on stdout under a credential-like
///     key is still rejected by [`parse_hook_env`], so tokens can't leak into
///     the session env through the KEY=VALUE channel.
pub fn collect_lifecycle_env_at(
    dir: &Path,
    hook: &str,
    session_env: &[(String, String)],
    cred_path: &Path,
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
        // Phase 0.5.5: export this plugin's own logged-in credentials to its
        // hook as AISH_PROFILE_<ID>_<FIELD>. Passed to the hook process ONLY —
        // never merged into `merged` (the shared session env).
        let mut hook_env: Vec<(String, String)> = session_env.to_vec();
        hook_env.extend(crate::plugin_auth::profile_env_at(cred_path, &id));
        let Some(stdout) = run_lifecycle_hook(&plugin.dir, hook, &hook_env) else {
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

// ===================================================================
// Phase 0.5.6 — plugin/hook provenance introspection (`:plugin info`,
// and the plugin event-hook fragments consumed by `:hooks list`).
// ===================================================================

/// Build the [`crate::hooks::PluginHookFragment`] list for every discovered
/// plugin that ships a `hooks.json` — the seam that lets the session merge
/// plugin-contributed event hooks (tagged `HookSource::Plugin(id)`) into the
/// runtime `HookSet` so `:hooks list` can show their provenance. Plugins with
/// no `hooks.json` contribute nothing. Ordered by plugin id (discovery order).
pub fn plugin_hook_fragments(dir: &Path) -> Vec<crate::hooks::PluginHookFragment> {
    discover(dir)
        .into_iter()
        .filter_map(|p| {
            let path = p.dir.join("hooks.json");
            path.is_file().then(|| crate::hooks::PluginHookFragment {
                plugin_id: p.manifest.id.clone(),
                path,
            })
        })
        .collect()
}

/// Render the `:plugin info <id>` provenance report for the plugin with id
/// `id` under `dir`, or `None` when no such (enabled) plugin exists. Pure apart
/// from reading the plugin's own on-disk files (`.mcp.json`, `hooks.json`), so
/// the REPL command and the unit tests share one code path.
///
/// The report surfaces every capability the plugin contributes: manifest
/// metadata (name/version/description/enabled), the login command it handles,
/// its plugin-lifecycle hooks (`on_init`, …), the MCP servers it injects (with
/// refs redacted — names only), the event-catalog hooks from its `hooks.json`,
/// and the skills it expands into the registry.
pub fn format_plugin_info(dir: &Path, id: &str) -> Option<String> {
    let plugins = discover(dir);
    let plugin = plugins.iter().find(|p| p.manifest.id == id)?;
    let m = &plugin.manifest;
    let mut out = String::new();
    let field = |out: &mut String, k: &str, v: &str| {
        out.push_str(&format!("  {k:<14} {v}\n"));
    };

    out.push_str(&format!("plugin `{}`\n", m.id));
    field(&mut out, "name", if m.name.is_empty() { &m.id } else { &m.name });
    field(&mut out, "version", if m.version.is_empty() { "-" } else { &m.version });
    field(
        &mut out,
        "description",
        if m.description.is_empty() { "-" } else { &m.description },
    );
    field(&mut out, "enabled", if m.is_enabled() { "yes" } else { "no" });
    field(&mut out, "dir", &plugin.dir.display().to_string());
    field(&mut out, "login", m.login_command().unwrap_or("-"));

    // Plugin-lifecycle hooks (on_init, on_shell_ready, …).
    let lifecycle = m.lifecycle_hooks();
    field(
        &mut out,
        "lifecycle",
        &if lifecycle.is_empty() {
            "-".to_string()
        } else {
            lifecycle.join(", ")
        },
    );

    // MCP servers this plugin injects (names only — never echo resolved refs).
    let mut mcp_names: Vec<String> = read_plugin_mcp(&plugin.dir)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    mcp_names.sort();
    field(
        &mut out,
        "mcp servers",
        &if mcp_names.is_empty() {
            "-".to_string()
        } else {
            mcp_names.join(", ")
        },
    );

    // Event-catalog hooks from this plugin's hooks.json — reuse the real
    // HookSet loader (single-plugin fragment) so parsing stays identical.
    let hooks_path = plugin.dir.join("hooks.json");
    let event_hooks: Vec<String> = if hooks_path.is_file() {
        let frag = crate::hooks::PluginHookFragment {
            plugin_id: m.id.clone(),
            path: hooks_path,
        };
        crate::hooks::HookSet::load_layered(&[], std::slice::from_ref(&frag))
            .hooks()
            .iter()
            .map(|h| match &h.name {
                Some(n) => format!("{} ({n})", h.event.as_str()),
                None => h.event.as_str().to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    field(
        &mut out,
        "event hooks",
        &if event_hooks.is_empty() {
            "-".to_string()
        } else {
            event_hooks.join(", ")
        },
    );

    // Schemas the plugin ships (Phase 3) — names only; use
    // `:plugin info <id> --schema` for the shape breakdown.
    field(
        &mut out,
        "schemas",
        &if plugin.schemas.is_empty() {
            "-".to_string()
        } else {
            plugin
                .schemas
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        },
    );

    // Skills contributed to the registry.
    field(
        &mut out,
        "skills",
        &if plugin.skills.is_empty() {
            "-".to_string()
        } else {
            plugin
                .skills
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        },
    );

    Some(out.trim_end().to_string())
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
    fn plugin_hook_fragments_only_for_plugins_with_hooks_json() {
        let tmp = tempdir();
        write_plugin(&tmp, "with-hooks", r#"{"id":"with-hooks"}"#, None);
        write_plugin(&tmp, "no-hooks", r#"{"id":"no-hooks"}"#, None);
        fs::write(
            tmp.join("with-hooks").join("hooks.json"),
            r#"{"hooks":[{"event":"PreToolUse","action":{"type":"observe"}}]}"#,
        )
        .unwrap();
        let frags = plugin_hook_fragments(&tmp);
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].plugin_id, "with-hooks");
    }

    #[test]
    fn format_plugin_info_unknown_id_is_none() {
        let tmp = tempdir();
        write_plugin(&tmp, "hello", r#"{"id":"hello"}"#, None);
        assert!(format_plugin_info(&tmp, "nope").is_none());
    }

    #[test]
    fn format_plugin_info_reports_full_provenance() {
        let tmp = tempdir();
        write_plugin(
            &tmp,
            "enterprise",
            r#"{"id":"enterprise","name":"Enterprise","version":"1.2.0",
                "description":"corp plugin",
                "provides":{"lifecycle_hooks":["on_init"],"login":"mycompany"}}"#,
            Some(("audit", "Audit things.")),
        );
        fs::write(
            tmp.join("enterprise").join("hooks.json"),
            r#"{"hooks":[{"event":"PreToolUse","name":"observe","action":{"type":"command","program":"true"}}]}"#,
        )
        .unwrap();
        fs::write(
            tmp.join("enterprise").join(".mcp.json"),
            r#"{"mcpServers":{"corp":{"url":"https://mcp.example.com"}}}"#,
        )
        .unwrap();
        let report = format_plugin_info(&tmp, "enterprise").expect("plugin exists");
        assert!(report.contains("Enterprise"));
        assert!(report.contains("1.2.0"));
        assert!(report.contains("mycompany"));
        assert!(report.contains("on_init"));
        assert!(report.contains("corp"));
        assert!(report.contains("PreToolUse (observe)"));
        assert!(report.contains("audit"));
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

    // ---- Phase 1 (plugin-skill-sources, TASK-336 manifest / TASK-337 discovery) ----

    #[test]
    fn provides_skill_source_parses_all_fields() {
        let m = manifest(
            r#"{"id":"skillfish","provides":{"skill_source":{
                "id":"skillfish","priority":100,"search":"search.sh","add":"add.sh",
                "handles":["*","skillfish:*","*/*"]}}}"#,
        );
        let src = m.skill_source().expect("skill_source parsed");
        assert_eq!(src.id.as_deref(), Some("skillfish"));
        assert_eq!(src.priority, 100);
        assert_eq!(src.search.as_deref(), Some("search.sh"));
        assert_eq!(src.add.as_deref(), Some("add.sh"));
        assert_eq!(src.handles, vec!["*", "skillfish:*", "*/*"]);
    }

    #[test]
    fn provides_without_skill_source_is_none() {
        // A `provides` block that declares only other capabilities → no source.
        let m = manifest(r#"{"id":"p","provides":{"login":"acme"}}"#);
        assert!(m.skill_source().is_none());
    }

    #[test]
    fn skill_source_defaults_are_empty() {
        // A bare block: search/add/id absent, priority 0, no handles.
        let m = manifest(r#"{"id":"p","provides":{"skill_source":{}}}"#);
        let src = m.skill_source().expect("empty block still parses");
        assert!(src.id.is_none());
        assert_eq!(src.priority, 0);
        assert!(src.search.is_none() && src.add.is_none());
        assert!(src.handles.is_empty());
    }

    #[test]
    fn discover_skill_sources_collects_only_skill_source_plugins() {
        let tmp = tempdir();
        write_plugin(&tmp, "plain", r#"{"id":"plain"}"#, Some(("s", "d")));
        write_plugin(
            &tmp,
            "fish",
            r#"{"id":"fish","provides":{"skill_source":{"search":"search.sh"}}}"#,
            Some(("s", "d")),
        );
        let sources = discover_skill_sources(&tmp);
        assert_eq!(sources.len(), 1, "only the skill_source plugin is collected");
        assert_eq!(sources[0].plugin_id, "fish");
        // id defaults to the owning plugin id when the block omits it.
        assert_eq!(sources[0].id, "fish");
        assert_eq!(sources[0].search.as_deref(), Some("search.sh"));
        assert!(sources[0].add.is_none());
        assert_eq!(sources[0].dir, tmp.join("fish"));
    }

    #[test]
    fn discover_skill_sources_sorted_by_priority_desc_then_id_asc() {
        let tmp = tempdir();
        // Two sources share priority 50 → the tie breaks by id asc (aaa < bbb).
        write_plugin(
            &tmp,
            "p_hi",
            r#"{"id":"p_hi","provides":{"skill_source":{"id":"hi","priority":100}}}"#,
            None,
        );
        write_plugin(
            &tmp,
            "p_bbb",
            r#"{"id":"p_bbb","provides":{"skill_source":{"id":"bbb","priority":50}}}"#,
            None,
        );
        write_plugin(
            &tmp,
            "p_aaa",
            r#"{"id":"p_aaa","provides":{"skill_source":{"id":"aaa","priority":50}}}"#,
            None,
        );
        write_plugin(
            &tmp,
            "p_lo",
            r#"{"id":"p_lo","provides":{"skill_source":{"id":"lo","priority":1}}}"#,
            None,
        );
        let ids: Vec<_> = discover_skill_sources(&tmp)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["hi", "aaa", "bbb", "lo"]);
    }

    #[test]
    fn discover_skill_sources_empty_when_no_sources() {
        let tmp = tempdir();
        write_plugin(&tmp, "plain", r#"{"id":"plain"}"#, Some(("s", "d")));
        assert!(discover_skill_sources(&tmp).is_empty());
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

    // ---- Phase 0.5.5: credential→lifecycle-hook injection tests ----

    /// Write a `[profile:<login>]` INI section (mode 0600) to a temp credentials
    /// file and return its path.
    fn write_credentials(root: &Path, login: &str, fields: &[(&str, &str)]) -> PathBuf {
        let path = root.join("credentials");
        let mut body = format!("[profile:{login}]\n");
        for (k, v) in fields {
            body.push_str(&format!("{k} = {v}\n"));
        }
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    /// A plugin's own logged-in credentials are exported into its `on_init` hook
    /// as `AISH_PROFILE_<ID>_<FIELD>`. The hook re-exports one under a benign key
    /// to prove it saw the value; the raw token never lands in the session env.
    #[test]
    fn hook_sees_own_profile_credentials() {
        let tmp = tempdir();
        write_plugin(&tmp, "acme", r#"{"id":"acme"}"#, None);
        // Hook echoes the injected region back under a non-credential-like key.
        write_hook(
            &tmp,
            "acme",
            "on_init",
            "#!/bin/sh\necho HOOK_SAW_REGION=$AISH_PROFILE_ACME_REGION\n",
        );
        let cred = write_credentials(&tmp, "acme", &[("region", "us-east-1"), ("access_token", "s3cr3t")]);

        let env = collect_lifecycle_env_at(&tmp, "on_init", &[], &cred);
        // The hook received AISH_PROFILE_ACME_REGION and echoed it back.
        assert_eq!(
            env.vars,
            vec![("HOOK_SAW_REGION".to_string(), "us-east-1".to_string())]
        );
        // The credential token itself was NEVER merged into the session env.
        assert!(!env.vars.iter().any(|(_, v)| v.contains("s3cr3t")));
    }

    /// A hook that tries to re-export a credential under a credential-like key
    /// is rejected by `parse_hook_env`, so tokens can't leak into session env
    /// via the KEY=VALUE channel even though the hook process saw the value.
    #[test]
    fn hook_cannot_leak_credential_into_session_env() {
        let tmp = tempdir();
        write_plugin(&tmp, "acme", r#"{"id":"acme"}"#, None);
        write_hook(
            &tmp,
            "acme",
            "on_init",
            "#!/bin/sh\necho ACCESS_TOKEN=$AISH_PROFILE_ACME_ACCESS_TOKEN\n",
        );
        let cred = write_credentials(&tmp, "acme", &[("access_token", "s3cr3t")]);

        let env = collect_lifecycle_env_at(&tmp, "on_init", &[], &cred);
        // Nothing merged — the credential-like key was rejected + warned.
        assert!(env.vars.is_empty());
        assert!(env
            .warnings
            .iter()
            .any(|w| w.contains("credential-like")));
    }

    /// A plugin only ever sees ITS OWN profile — never another plugin's.
    #[test]
    fn hook_does_not_see_other_plugins_credentials() {
        let tmp = tempdir();
        write_plugin(&tmp, "acme", r#"{"id":"acme"}"#, None);
        // acme's hook probes for a DIFFERENT plugin's profile var — must be empty.
        write_hook(
            &tmp,
            "acme",
            "on_init",
            "#!/bin/sh\necho OTHER=[$AISH_PROFILE_OTHER_REGION]\n",
        );
        // Credentials exist only for `other`, not for `acme`.
        let cred = write_credentials(&tmp, "other", &[("region", "eu-west-1")]);

        let env = collect_lifecycle_env_at(&tmp, "on_init", &[], &cred);
        assert_eq!(env.vars, vec![("OTHER".to_string(), "[]".to_string())]);
    }

    // ---- Phase 3: schema loading & validation ---------------------------

    /// Write `<root>/<id>/schemas/<name>.json` with the given body.
    fn write_schema(root: &Path, id: &str, name: &str, body: &str) {
        let sdir = root.join(id).join("schemas");
        fs::create_dir_all(&sdir).unwrap();
        fs::write(sdir.join(format!("{name}.json")), body).unwrap();
    }

    #[test]
    fn load_schemas_reads_json_sorted_and_skips_junk() {
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p"}"#, None);
        write_schema(&tmp, "p", "zeta", r#"{"type":"object"}"#);
        write_schema(&tmp, "p", "alpha", r#"{"type":"string"}"#);
        // Malformed JSON — skipped.
        write_schema(&tmp, "p", "broken", "{not json");
        // Non-.json file in schemas/ — ignored.
        fs::write(tmp.join("p").join("schemas").join("readme.txt"), "hi").unwrap();
        // Array body (not a schema object) — skipped.
        write_schema(&tmp, "p", "arr", r#"[1,2,3]"#);

        let schemas = load_schemas(&tmp.join("p"));
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn load_schemas_absent_dir_is_empty() {
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p"}"#, None);
        assert!(load_schemas(&tmp.join("p")).is_empty());
    }

    #[test]
    fn validate_type_required_and_additional_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer" }
            },
            "additionalProperties": false
        });
        // Valid.
        assert!(validate_json_schema(&schema, &serde_json::json!({"name":"a","count":2})).is_empty());
        // Missing required + wrong type + extra prop → 3 violations.
        let v = validate_json_schema(
            &schema,
            &serde_json::json!({"count":"nope","extra":1}),
        );
        assert_eq!(v.len(), 3, "violations: {v:?}");
        assert!(v.iter().any(|x| x.message.contains("missing required property `name`")));
        assert!(v.iter().any(|x| x.path == "/count" && x.message.contains("expected type")));
        assert!(v.iter().any(|x| x.message.contains("additional property `extra`")));
    }

    #[test]
    fn validate_enum_const_and_number_bounds() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "status": { "enum": ["open", "closed"] },
                "kind":   { "const": "issue" },
                "score":  { "type": "number", "minimum": 0, "maximum": 100 }
            }
        });
        assert!(validate_json_schema(
            &schema,
            &serde_json::json!({"status":"open","kind":"issue","score":50})
        )
        .is_empty());
        let v = validate_json_schema(
            &schema,
            &serde_json::json!({"status":"weird","kind":"bug","score":150}),
        );
        assert_eq!(v.len(), 3, "violations: {v:?}");
        assert!(v.iter().any(|x| x.path == "/status" && x.message.contains("enum")));
        assert!(v.iter().any(|x| x.path == "/kind" && x.message.contains("const")));
        assert!(v.iter().any(|x| x.path == "/score" && x.message.contains("maximum")));
    }

    #[test]
    fn validate_array_items_and_string_pattern() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string", "pattern": "^[a-z]+$" }
                }
            }
        });
        assert!(validate_json_schema(&schema, &serde_json::json!({"tags":["ok","fine"]})).is_empty());
        let v = validate_json_schema(&schema, &serde_json::json!({"tags":["OK9"]}));
        assert_eq!(v.len(), 1, "violations: {v:?}");
        assert_eq!(v[0].path, "/tags/0");
        assert!(v[0].message.contains("pattern"));
        // Empty array trips minItems.
        let v2 = validate_json_schema(&schema, &serde_json::json!({"tags":[]}));
        assert!(v2.iter().any(|x| x.message.contains("minItems")));
    }

    #[test]
    fn validate_boolean_schema_false_rejects_everything() {
        assert!(validate_json_schema(&Value::Bool(true), &serde_json::json!(1)).is_empty());
        let v = validate_json_schema(&Value::Bool(false), &serde_json::json!(1));
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("rejects all"));
    }

    #[test]
    fn plugin_validate_and_unknown_schema() {
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p"}"#, None);
        write_schema(
            &tmp,
            "p",
            "issue",
            r#"{"type":"object","required":["title"],"properties":{"title":{"type":"string"}}}"#,
        );
        let plugins = discover(&tmp);
        let p = plugins.iter().find(|p| p.manifest.id == "p").unwrap();
        assert_eq!(p.schemas.len(), 1);
        assert!(p.validate("issue", &serde_json::json!({"title":"x"})).is_ok());
        assert!(matches!(
            p.validate("issue", &serde_json::json!({})),
            Err(SchemaValidationError::Failed(_))
        ));
        assert!(matches!(
            p.validate("nope", &serde_json::json!({})),
            Err(SchemaValidationError::UnknownSchema(_))
        ));
    }

    #[test]
    fn validate_against_plugin_schema_end_to_end() {
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p"}"#, None);
        write_schema(&tmp, "p", "ping", r#"{"type":"object","required":["ok"]}"#);
        assert!(validate_against_plugin_schema(&tmp, "p", "ping", &serde_json::json!({"ok":true})).is_ok());
        assert!(matches!(
            validate_against_plugin_schema(&tmp, "p", "ping", &serde_json::json!({})),
            Err(SchemaValidationError::Failed(_))
        ));
        // Unknown plugin id.
        assert!(matches!(
            validate_against_plugin_schema(&tmp, "ghost", "ping", &serde_json::json!({})),
            Err(SchemaValidationError::UnknownSchema(_))
        ));
    }

    #[test]
    fn format_plugin_info_and_schemas_render() {
        let tmp = tempdir();
        write_plugin(&tmp, "p", r#"{"id":"p","name":"P"}"#, None);
        write_schema(
            &tmp,
            "p",
            "issue",
            r#"{"type":"object","required":["title"],"properties":{"title":{"type":"string"},"body":{"type":"string"}}}"#,
        );
        let info = format_plugin_info(&tmp, "p").unwrap();
        assert!(info.contains("schemas"), "info missing schemas row: {info}");
        assert!(info.contains("issue"));

        let detail = format_plugin_schemas(&tmp, "p").unwrap();
        assert!(detail.contains("issue (type \"object\")"), "detail: {detail}");
        assert!(detail.contains("properties: body, title"));
        assert!(detail.contains("required:   title"));

        // No-such-plugin → None.
        assert!(format_plugin_schemas(&tmp, "ghost").is_none());
    }

    #[test]
    fn format_plugin_mcp_renders_connect_and_collision() {
        let tmp = tempdir();
        // "aaa" claims `shared` first (id-sorted discovery); "bbb" loses it to
        // "aaa" and also contributes an uncontested server of its own.
        write_plugin(&tmp, "aaa", r#"{"id":"aaa"}"#, None);
        write_plugin_mcp(&tmp, "aaa", r#"{"mcpServers":{"shared":{"command":"a"}}}"#);
        write_plugin(&tmp, "bbb", r#"{"id":"bbb"}"#, None);
        write_plugin_mcp(
            &tmp,
            "bbb",
            r#"{"mcpServers":{"shared":{"command":"b"},"solo":{"command":"c"}}}"#,
        );

        let winner = format_plugin_mcp(&tmp, "aaa", &[]).unwrap();
        assert!(winner.contains("shared — will connect"), "winner: {winner}");

        let loser = format_plugin_mcp(&tmp, "bbb", &[]).unwrap();
        assert!(
            loser.contains("shared — skipped, name already claimed by plugin:aaa"),
            "loser: {loser}"
        );
        assert!(loser.contains("solo — will connect"), "loser: {loser}");

        // A server name already reserved by project/user config also renders
        // as a collision, attributed to "config".
        write_plugin(&tmp, "ccc", r#"{"id":"ccc"}"#, None);
        write_plugin_mcp(&tmp, "ccc", r#"{"mcpServers":{"github":{"command":"c"}}}"#);
        let vs_config = format_plugin_mcp(&tmp, "ccc", &["github".to_string()]).unwrap();
        assert!(
            vs_config.contains("github — skipped, name already claimed by config"),
            "vs_config: {vs_config}"
        );

        // A plugin with no `.mcp.json` at all.
        write_plugin(&tmp, "none", r#"{"id":"none"}"#, None);
        let empty = format_plugin_mcp(&tmp, "none", &[]).unwrap();
        assert!(empty.contains("(none)"), "empty: {empty}");

        // No-such-plugin → None.
        assert!(format_plugin_mcp(&tmp, "ghost", &[]).is_none());
    }

    /// TASK-376 (SPR-069) — schema-reconciliation guard.
    ///
    /// The ADR (`docs/design/webhook-plugin-routing.md`, Decision 1) pins the
    /// canonical webhook-handler schema to the `webhooks[]` array parsed by
    /// `aish_webhook_client::PluginManifest`, and asserts that a single
    /// `plugin.json` "satisfies both" the aish-core loader
    /// (`super::PluginManifest`) and the webhook-client loader. This test turns
    /// that prose claim into a CI-enforced invariant: the shipped GitHub
    /// reference plugin (`plugins/github/plugin.json`) must parse cleanly under
    /// BOTH loaders, and the canonical loader must surface the declared
    /// handlers. If a future change forks the schema (adds a competing field,
    /// or tightens either struct to reject the other's keys), this test fails.
    #[test]
    fn github_plugin_manifest_satisfies_both_loaders() {
        use aish_webhook_client::PluginManifest as WebhookManifest;

        let text = fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("plugins")
                .join("github")
                .join("plugin.json"),
        )
        .expect("plugins/github/plugin.json present");

        // (1) aish-core loader accepts the manifest — the unknown
        //     `webhooks`/`author`/`license` keys are dropped by serde, never a
        //     hard error. One manifest, both loaders.
        let core: PluginManifest =
            serde_json::from_str(&text).expect("core PluginManifest parses github plugin");
        assert_eq!(core.id, "github");

        // (2) The canonical webhook-client loader parses the same bytes and
        //     surfaces the declared handlers — the single source of truth.
        let hooks: WebhookManifest =
            serde_json::from_str(&text).expect("canonical PluginManifest parses github plugin");
        assert_eq!(hooks.id, "github");
        assert_eq!(hooks.webhooks.len(), 5, "all five handlers parsed");
        let first = &hooks.webhooks[0];
        assert_eq!(first.event_type, "pull_request");
        assert_eq!(first.command, vec!["handlers/pr-review.sh".to_string()]);
        assert_eq!(
            first.filters.get("action").and_then(|v| v.as_str()),
            Some("opened"),
        );
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
