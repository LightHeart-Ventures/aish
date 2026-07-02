//! Plugin auth — `aish login <name>` command routing + credential persistence
//! (Phase 0.5.5).
//!
//! A plugin opts into a login command by declaring it in `plugin.json`:
//!
//! ```json
//! { "provides": { "login": "mycompany" } }
//! ```
//!
//! Then `aish login mycompany` routes to that plugin's auth handler
//! (`<plugin-dir>/login.sh`, fork/exec — no shell). The handler runs whatever
//! flow it likes (device-code, browser OAuth, …), showing prompts on **stderr**
//! and printing a single JSON credential object on **stdout**:
//!
//! ```json
//! { "access_token": "…", "refresh_token": "…", "expires_at": "…" }
//! ```
//!
//! aish captures that stdout and persists it to `~/.aish/credentials` under an
//! INI section named `[profile:mycompany]` (mode 0600). From there the
//! credential is reachable two ways, both reusing machinery that already exists:
//!
//!   * **`.mcp.json`** — a server spec references the profile and interpolates
//!     fields into its url/headers (see [`crate::mcp`]):
//!     ```json
//!     { "url": "https://api.mycompany.com/mcp",
//!       "credentials": { "file": "~/.aish/credentials", "profile": "profile:mycompany" },
//!       "headers": { "Authorization": "Bearer ${access_token}" } }
//!     ```
//!   * **lifecycle hooks** — [`profile_env`] exports each field as
//!     `AISH_PROFILE_MYCOMPANY_ACCESS_TOKEN=…` so `on_init.sh` & friends can read
//!     the token from their environment.
//!
//! Secret values live only in the 0600 credentials file and in the spawned
//! process environment — never in `plugin.json`, `.mcp.json`, or the conversation.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the plugin credentials store: `~/.aish/credentials`.
pub fn credentials_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("credentials")
}

/// The INI section name a login name maps to: `profile:<name>`. This is the
/// exact string a plugin's `.mcp.json` places in `credentials.profile`, and the
/// section [`crate::mcp::load_profile`] reads back.
pub fn profile_section(login_name: &str) -> String {
    format!("profile:{login_name}")
}

/// What a successful [`login`] produced — everything the REPL needs to report,
/// carrying **no** secret values (only field *names*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutcome {
    /// The plugin id that handled the login.
    pub plugin_id: String,
    /// The login name (`aish login <name>`), also the credential profile suffix.
    pub login_name: String,
    /// The persisted INI section name (`profile:<name>`).
    pub profile: String,
    /// Where the credentials were written (`~/.aish/credentials`).
    pub path: PathBuf,
    /// The credential field names persisted (values are never surfaced).
    pub field_names: Vec<String>,
}

/// Execute `aish login <login_name>` end to end: find the plugin whose
/// `provides.login` matches, run its `login.sh` handler, parse + persist the
/// returned credentials, and report the profile (no secret values).
///
/// Errors (never spawns anything it can't clean up):
///   * no plugin declares that login command → actionable "not found";
///   * the plugin declares it but ships no `login.sh` → clear message;
///   * the handler exits non-zero → its stderr was already shown to the user;
///   * the handler's stdout isn't a flat JSON credential object → parse error.
pub fn login(
    login_name: &str,
    plugins_dir: &Path,
    tenant_id: Option<&str>,
) -> Result<LoginOutcome> {
    login_at(login_name, plugins_dir, tenant_id, &credentials_path())
}

/// [`login`] against an explicit credentials path — the seam the tests drive so
/// the router can be exercised end to end without clobbering the real
/// `~/.aish/credentials`.
pub fn login_at(
    login_name: &str,
    plugins_dir: &Path,
    tenant_id: Option<&str>,
    cred_path: &Path,
) -> Result<LoginOutcome> {
    let plugin = crate::plugins::discover(plugins_dir)
        .into_iter()
        .find(|p| p.manifest.login_command() == Some(login_name))
        .ok_or_else(|| {
            anyhow!(
                "no plugin provides `login {login_name}` — declare \
                 \"provides\": {{ \"login\": \"{login_name}\" }} in a plugin's plugin.json \
                 (`:plugin list` shows installed plugins)"
            )
        })?;

    let stdout = run_login_handler(&plugin.manifest.id, login_name, &plugin.dir, tenant_id)?;
    let fields = parse_handler_output(&stdout)?;
    persist_credentials_at(cred_path, login_name, &fields)?;

    Ok(LoginOutcome {
        plugin_id: plugin.manifest.id,
        login_name: login_name.to_string(),
        profile: profile_section(login_name),
        path: cred_path.to_path_buf(),
        field_names: fields.keys().cloned().collect(),
    })
}

/// Run a plugin's login handler (`<plugin_dir>/login.sh`) and return its raw
/// stdout. stdin + stderr are **inherited** so an interactive/device-code flow
/// can prompt the user and print status; only stdout (the JSON credential
/// object) is captured. A non-zero exit is an error — the handler's stderr has
/// already reached the terminal, so we surface the exit status.
pub fn run_login_handler(
    plugin_id: &str,
    login_name: &str,
    plugin_dir: &Path,
    tenant_id: Option<&str>,
) -> Result<String> {
    let handler = plugin_dir.join("login.sh");
    if !handler.exists() {
        bail!(
            "plugin `{plugin_id}` declares `provides.login` but ships no login handler at {}",
            handler.display()
        );
    }

    let mut cmd = Command::new(&handler);
    cmd.current_dir(plugin_dir)
        .env("AISH_PLUGIN_ID", plugin_id)
        .env("AISH_LOGIN_NAME", login_name)
        .env("AISH_TENANT_ID", tenant_id.unwrap_or_default())
        .env("AISH_CREDENTIALS_FILE", credentials_path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .stdin(std::process::Stdio::inherit());

    let output = cmd
        .output()
        .with_context(|| format!("failed to run login handler {}", handler.display()))?;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        bail!("login handler for `{login_name}` exited with status {code}");
    }

    String::from_utf8(output.stdout)
        .context("login handler stdout was not valid UTF-8")
}

/// Parse a login handler's stdout into a flat map of credential fields.
///
/// Accepts a single JSON object whose values are scalars (string / number /
/// bool); `null` values are dropped. Nested objects or arrays are rejected — a
/// credentials profile is flat `KEY = value` INI lines. Empty output, non-JSON,
/// non-object JSON, an empty object, or a value that would corrupt the INI
/// (embedded newline, `=`/`[`/`]` in a key) all error, so a malformed handler
/// fails loudly instead of persisting junk.
pub fn parse_handler_output(stdout: &str) -> Result<BTreeMap<String, String>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        bail!("login handler produced no output (expected a JSON credential object)");
    }
    let value: Value =
        serde_json::from_str(trimmed).context("login handler output is not valid JSON")?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("login handler output must be a JSON object, got {}", kind(&value)))?;
    if obj.is_empty() {
        bail!("login handler returned an empty JSON object (no credential fields)");
    }

    let mut fields = BTreeMap::new();
    for (k, v) in obj {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => continue, // drop explicit nulls
            other => bail!(
                "credential field `{k}` must be a scalar (string/number/bool), got {}",
                kind(other)
            ),
        };
        if k.is_empty() || k.contains('\n') || k.contains('=') || k.contains('[') || k.contains(']')
        {
            bail!("invalid credential field name `{k}`");
        }
        if s.contains('\n') {
            bail!("credential field `{k}` value must not contain a newline");
        }
        fields.insert(k.clone(), s);
    }
    if fields.is_empty() {
        bail!("login handler returned no usable credential fields (all null?)");
    }
    Ok(fields)
}

/// Persist a credential profile to a credentials file, replacing any existing
/// `[profile:<login_name>]` section and preserving every other section. The file
/// is (re)written with mode 0600. `~/.aish/credentials` is the production path
/// ([`credentials_path`]); tests pass a temp path.
pub fn persist_credentials_at(
    path: &Path,
    login_name: &str,
    fields: &BTreeMap<String, String>,
) -> Result<()> {
    let header = format!("[{}]", profile_section(login_name));
    let existing = std::fs::read_to_string(path).unwrap_or_default();

    // Rebuild the file, dropping only the target section's header + body, and
    // keeping every other section verbatim.
    let mut out = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        let t = line.trim();
        let is_header = t.starts_with('[') && t.ends_with(']');
        if is_header {
            skipping = t == header;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    // Normalize trailing whitespace to exactly one blank-line separator.
    while out.ends_with('\n') {
        out.pop();
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }

    // Append the fresh target section.
    out.push_str(&header);
    out.push('\n');
    for (k, v) in fields {
        out.push_str(k);
        out.push_str(" = ");
        out.push_str(v);
        out.push('\n');
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, out.as_bytes())
        .with_context(|| format!("writing credentials to {}", path.display()))?;
    set_owner_only(path)?;
    Ok(())
}

/// Environment variables exported for a logged-in profile, for consumption by a
/// plugin's lifecycle hooks. `${profile:<name>}` resolves to the set
/// `AISH_PROFILE_<NAME>_<FIELD>=value`, with name + field upper-cased and every
/// non-alphanumeric character folded to `_`. Sorted for deterministic output.
/// Empty when the profile has no persisted credentials.
///
/// The live consumer is the lifecycle-hook runner (Phase 0.5.5): before a
/// plugin's `on_init.sh` is fork/exec'd, `plugins::collect_lifecycle_env_at`
/// merges this plugin's profile fields into the hook's environment.
#[allow(dead_code)] // convenience wrapper; the live path calls `profile_env_at`
pub fn profile_env(login_name: &str) -> Vec<(String, String)> {
    profile_env_at(&credentials_path(), login_name)
}

/// [`profile_env`] against an explicit credentials path — the seam both the
/// live hook runner (`plugins::collect_lifecycle_env_at`) and the tests drive
/// without hardcoding the real `~/.aish/credentials`.
pub fn profile_env_at(path: &Path, login_name: &str) -> Vec<(String, String)> {
    let vars = crate::mcp::load_profile(&path.to_string_lossy(), &profile_section(login_name));
    let mut out: Vec<(String, String)> = vars
        .into_iter()
        .map(|(k, v)| (env_key(login_name, &k), v))
        .collect();
    out.sort();
    out
}

/// `AISH_PROFILE_<NAME>_<FIELD>` — the env-var name a credential field is
/// exported under for lifecycle hooks.
fn env_key(login_name: &str, field: &str) -> String {
    format!("AISH_PROFILE_{}_{}", sanitize(login_name), sanitize(field))
}

/// Fold a token into an env-var-safe upper-case identifier: `[A-Z0-9]` kept,
/// everything else → `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// JSON value kind, for error messages.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting mode 0600 on {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ---- parse_handler_output -------------------------------------------

    #[test]
    fn parses_flat_credential_object() {
        let out = parse_handler_output(
            r#"{"access_token":"tok","refresh_token":"ref","expires_at":"2030-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(out.get("access_token").unwrap(), "tok");
        assert_eq!(out.get("refresh_token").unwrap(), "ref");
        assert_eq!(out.get("expires_at").unwrap(), "2030-01-01T00:00:00Z");
    }

    #[test]
    fn coerces_number_and_bool_and_drops_null() {
        let out =
            parse_handler_output(r#"{"access_token":"t","expires_in":3600,"mfa":true,"scope":null}"#)
                .unwrap();
        assert_eq!(out.get("expires_in").unwrap(), "3600");
        assert_eq!(out.get("mfa").unwrap(), "true");
        assert!(!out.contains_key("scope"), "null fields are dropped");
    }

    #[test]
    fn malformed_output_fails_gracefully() {
        // Empty stdout.
        assert!(parse_handler_output("   \n").is_err());
        // Not JSON.
        assert!(parse_handler_output("access_token=tok").is_err());
        // JSON but not an object.
        assert!(parse_handler_output(r#"["tok"]"#).is_err());
        assert!(parse_handler_output(r#""tok""#).is_err());
        // Empty object / all-null → no usable fields.
        assert!(parse_handler_output("{}").is_err());
        assert!(parse_handler_output(r#"{"a":null}"#).is_err());
        // Nested value rejected (profile must stay flat).
        assert!(parse_handler_output(r#"{"tok":{"nested":1}}"#).is_err());
        // INI-corrupting value.
        assert!(parse_handler_output("{\"tok\":\"line1\\nline2\"}").is_err());
    }

    // ---- persistence -----------------------------------------------------

    #[test]
    fn persists_section_with_0600_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("aish-cred-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials");

        persist_credentials_at(&path, "mycompany", &map(&[("access_token", "tok"), ("x", "1")]))
            .unwrap();

        // Section + fields present, readable by the shared INI loader.
        let vars = crate::mcp::load_profile(&path.to_string_lossy(), "profile:mycompany");
        assert_eq!(vars.get("access_token").unwrap(), "tok");
        assert_eq!(vars.get("x").unwrap(), "1");

        // Mode is exactly 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials must be user-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacing_a_profile_preserves_other_sections() {
        let dir = std::env::temp_dir().join(format!("aish-cred2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials");

        persist_credentials_at(&path, "alpha", &map(&[("token", "a1")])).unwrap();
        persist_credentials_at(&path, "beta", &map(&[("token", "b1")])).unwrap();
        // Overwrite alpha; beta must survive untouched.
        persist_credentials_at(&path, "alpha", &map(&[("token", "a2")])).unwrap();

        let a = crate::mcp::load_profile(&path.to_string_lossy(), "profile:alpha");
        let b = crate::mcp::load_profile(&path.to_string_lossy(), "profile:beta");
        assert_eq!(a.get("token").unwrap(), "a2", "alpha overwritten");
        assert_eq!(b.get("token").unwrap(), "b1", "beta preserved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- env-var mapping -------------------------------------------------

    #[test]
    fn env_key_sanitizes_and_uppercases() {
        assert_eq!(env_key("mycompany", "access_token"), "AISH_PROFILE_MYCOMPANY_ACCESS_TOKEN");
        assert_eq!(env_key("my-co.io", "refresh-token"), "AISH_PROFILE_MY_CO_IO_REFRESH_TOKEN");
    }

    // A persisted profile is readable by a plugin's lifecycle hooks: `${profile:
    // <name>}` resolves to sorted `AISH_PROFILE_<NAME>_<FIELD>=value` env pairs.
    #[test]
    fn profile_env_roundtrips_for_lifecycle_hooks() {
        let dir = std::env::temp_dir().join(format!("aish-cred-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials");

        persist_credentials_at(
            &path,
            "mycompany",
            &map(&[("access_token", "tok"), ("expires_in", "3600")]),
        )
        .unwrap();

        let env = profile_env_at(&path, "mycompany");
        assert_eq!(
            env,
            vec![
                ("AISH_PROFILE_MYCOMPANY_ACCESS_TOKEN".to_string(), "tok".to_string()),
                ("AISH_PROFILE_MYCOMPANY_EXPIRES_IN".to_string(), "3600".to_string()),
            ]
        );
        // A profile with no persisted credentials yields no env.
        assert!(profile_env_at(&path, "absent").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- login handler round-trip (fork/exec) ----------------------------

    #[cfg(unix)]
    #[test]
    fn run_handler_captures_stdout_and_persists() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("aish-login-{}", std::process::id()));
        let pdir = base.join("plugins").join("demo");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&pdir).unwrap();

        // A minimal handler: prints a JSON credential object, ignores stdin.
        let handler = pdir.join("login.sh");
        std::fs::write(
            &handler,
            "#!/usr/bin/env bash\necho '{\"access_token\":\"tok-123\",\"expires_at\":\"soon\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755)).unwrap();

        let stdout = run_login_handler("demo", "demo", &pdir, Some("t_1")).unwrap();
        let fields = parse_handler_output(&stdout).unwrap();
        assert_eq!(fields.get("access_token").unwrap(), "tok-123");

        let credpath = base.join("credentials");
        persist_credentials_at(&credpath, "demo", &fields).unwrap();
        let vars = crate::mcp::load_profile(&credpath.to_string_lossy(), "profile:demo");
        assert_eq!(vars.get("access_token").unwrap(), "tok-123");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn run_handler_nonzero_exit_is_error() {
        use std::os::unix::fs::PermissionsExt;
        let pdir = std::env::temp_dir().join(format!("aish-login-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pdir);
        std::fs::create_dir_all(&pdir).unwrap();
        let handler = pdir.join("login.sh");
        std::fs::write(&handler, "#!/usr/bin/env bash\necho oops >&2\nexit 7\n").unwrap();
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = run_login_handler("demo", "demo", &pdir, None).unwrap_err();
        assert!(err.to_string().contains("status 7"), "{err}");
        let _ = std::fs::remove_dir_all(&pdir);
    }

    #[test]
    fn missing_handler_is_error() {
        let pdir = std::env::temp_dir().join(format!("aish-login-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pdir);
        std::fs::create_dir_all(&pdir).unwrap();
        let err = run_login_handler("demo", "demo", &pdir, None).unwrap_err();
        assert!(err.to_string().contains("no login handler"), "{err}");
        let _ = std::fs::remove_dir_all(&pdir);
    }

    // ---- top-level `login()` command routing -----------------------------

    #[test]
    fn login_routing_rejects_unknown_plugin() {
        let plugins = std::env::temp_dir().join(format!("aish-route-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&plugins);
        std::fs::create_dir_all(&plugins).unwrap();
        let cred = plugins.join("credentials");

        let err = login_at("nobody", &plugins, None, &cred).unwrap_err();
        assert!(err.to_string().contains("no plugin provides `login nobody`"), "{err}");
        // Nothing persisted on the rejection path.
        assert!(!cred.exists(), "unknown-plugin routing must not write credentials");
        let _ = std::fs::remove_dir_all(&plugins);
    }

    #[cfg(unix)]
    #[test]
    fn login_routing_happy_path_selects_plugin_and_persists() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("aish-route-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Plugin declaring `provides.login == "acme"`.
        let pdir = base.join("plugins").join("acme");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("plugin.json"),
            r#"{"id":"acme","provides":{"login":"acme"}}"#,
        )
        .unwrap();
        let handler = pdir.join("login.sh");
        std::fs::write(
            &handler,
            "#!/usr/bin/env bash\necho '{\"access_token\":\"acme-tok\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cred = base.join("credentials");
        let out = login_at("acme", &base.join("plugins"), Some("t_9"), &cred).unwrap();

        assert_eq!(out.plugin_id, "acme");
        assert_eq!(out.profile, "profile:acme");
        assert_eq!(out.field_names, vec!["access_token".to_string()]);
        // The credential landed in the right section and is env-resolvable.
        let vars = crate::mcp::load_profile(&cred.to_string_lossy(), "profile:acme");
        assert_eq!(vars.get("access_token").unwrap(), "acme-tok");
        assert_eq!(
            profile_env_at(&cred, "acme"),
            vec![("AISH_PROFILE_ACME_ACCESS_TOKEN".to_string(), "acme-tok".to_string())]
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
