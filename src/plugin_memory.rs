//! File-based plugin memory (Phase 2).
//!
//! A persistent, namespaced, per-plugin memory store backed by plain JSON files
//! on disk. This is the sibling of [`crate::plugin_state`] (a single global
//! SQLite key/value store): where `plugin_state` is one DB for cheap scalar
//! config, `plugin_memory` gives each plugin a small tree of **human-readable,
//! individually-permissioned** JSON documents — one file per *namespace* — so
//! that secrets (`auth`) can be locked to `0600` independently of cache or
//! preferences, and so an operator can `cat`/inspect the non-secret files.
//!
//! ## Layout
//!
//! ```text
//! ~/.aish/plugins/<plugin-id>/memory/
//! ├── auth.json      # credentials — always chmod 0600 (secret namespace)
//! ├── cache.json     # rate limits, timestamps (TTL-eligible, see docs)
//! ├── webhooks.json  # webhook delivery/subscription state
//! └── prefs.json     # user preferences
//! ```
//!
//! Each file is a **flat JSON object** mapping keys to arbitrary JSON values.
//! Nested access uses dot-notation (`webhooks.github.last_delivery_id`), so a
//! key path descends into nested objects. There is no on-disk envelope/version
//! field in v1 — see `docs/reference/plugins/memory.md` § Version field /
//! migrations for the reserved `__*` key namespace and the forward-migration plan.
//!
//! ## Namespaces & rules
//!
//! | Namespace  | File           | Perms  | Rules                              |
//! |------------|----------------|--------|-----------------------------------|
//! | `auth`     | `auth.json`    | `0600` | secret: redact on display, no TTL |
//! | `cache`    | `cache.json`   | `0644` | TTL-eligible (documented, v1 no-op)|
//! | `webhooks` | `webhooks.json`| `0644` | persistent                        |
//! | `prefs`    | `prefs.json`   | `0644` | persistent, user-editable         |
//!
//! ## Security
//!
//! * **Namespace isolation** — every path is rooted at
//!   `<plugins_dir>/<plugin-id>/memory/`. `plugin-id` must be a single path
//!   component; `..`, `/`, `\`, and empty ids are rejected
//!   ([`MemoryError::PathTraversal`]), so plugin A can never read plugin B's
//!   `auth.json` via a crafted id.
//! * **0600 for `auth`** — the secret namespace file is created with mode `0600`
//!   and re-chmod'd `0600` after every atomic write; a read that finds wrong
//!   perms auto-corrects and logs.
//! * **Redaction** — [`redact`] blanks every leaf value (keeps key names) so the
//!   REPL can show the *shape* of `auth` without leaking token values.
//! * **Atomic writes** — every save writes a sibling temp file and `rename`s it
//!   over the target, so a crash mid-write never leaves a half-written /
//!   corrupt JSON document.
//!
//! Every fallible call returns `Result<_, MemoryError>`. The module is
//! self-contained (std + serde_json only) so `tests/plugin_memory_tests.rs` can
//! compile it directly via `#[path = "../src/plugin_memory.rs"]` — `aish` is a
//! binary crate with no library target.

// The full public API (get/set/append/delete/clear + free-function wrappers)
// exists for REPL commands and later plugin phases; not every function is wired
// at every call site yet, so quiet the not-yet-consumed-surface warnings.
#![allow(dead_code)]

use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

/// The four plugin memory namespaces. Each maps to one JSON file and carries its
/// own access/permission rules. Modeled as an enum (not a free `&str`) so the
/// type system guarantees only valid namespaces ever reach the filesystem —
/// there is no way to construct an out-of-set namespace, which is itself a
/// namespace-isolation guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryNamespace {
    /// Credentials (access/refresh tokens, expiry). Secret: `0600`, redacted on
    /// display, never TTL-expired.
    Auth,
    /// Ephemeral cache (rate-limit resets, delivery timestamps). TTL-eligible.
    Cache,
    /// Webhook subscription + delivery state. Persistent.
    Webhooks,
    /// User preferences. Persistent, user-editable.
    Prefs,
}

impl MemoryNamespace {
    /// Every namespace, in declaration order — for `:plugin memory list` and
    /// clear-all style operations.
    pub const ALL: [MemoryNamespace; 4] = [
        MemoryNamespace::Auth,
        MemoryNamespace::Cache,
        MemoryNamespace::Webhooks,
        MemoryNamespace::Prefs,
    ];

    /// The canonical lowercase namespace name (`"auth"`, `"cache"`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryNamespace::Auth => "auth",
            MemoryNamespace::Cache => "cache",
            MemoryNamespace::Webhooks => "webhooks",
            MemoryNamespace::Prefs => "prefs",
        }
    }

    /// The on-disk file name for this namespace (`"auth.json"`, …).
    pub fn file_name(&self) -> String {
        format!("{}.json", self.as_str())
    }

    /// Parse a namespace from a user/plugin-supplied string (case-insensitive).
    /// Unknown names are rejected with [`MemoryError::InvalidNamespace`] rather
    /// than silently defaulting — an unknown namespace is a programming/user
    /// error, not a new namespace.
    pub fn parse(s: &str) -> Result<Self, MemoryError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auth" => Ok(MemoryNamespace::Auth),
            "cache" => Ok(MemoryNamespace::Cache),
            "webhooks" => Ok(MemoryNamespace::Webhooks),
            "prefs" => Ok(MemoryNamespace::Prefs),
            other => Err(MemoryError::InvalidNamespace(other.to_string())),
        }
    }

    /// True for the `auth` namespace — the only secret namespace. Drives `0600`
    /// enforcement and display redaction.
    pub fn is_secret(&self) -> bool {
        matches!(self, MemoryNamespace::Auth)
    }

    /// The unix file mode this namespace's file must have. `0600` (owner
    /// read/write only) for the secret `auth` namespace; `0644` otherwise.
    pub fn file_mode(&self) -> u32 {
        if self.is_secret() {
            0o600
        } else {
            0o644
        }
    }
}

impl fmt::Display for MemoryNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything that can go wrong talking to plugin memory. Flat, `Display`-able
/// variants so REPL/hook callers can surface an actionable message without
/// importing this module's internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    /// The requested key (or key path) does not exist in the namespace.
    NotFound { namespace: String, key: String },
    /// A filesystem permission problem (e.g. the OS refused to read/write).
    PermissionDenied(String),
    /// Any other underlying I/O failure (disk full, etc.), with context.
    Io(String),
    /// A file existed but did not contain a valid JSON object.
    Malformed { namespace: String, detail: String },
    /// A semantic violation: appending to a non-array, descending through a
    /// non-object, writing a redaction sentinel into `auth`, etc.
    Validation(String),
    /// The namespace string was not one of auth/cache/webhooks/prefs.
    InvalidNamespace(String),
    /// The `plugin_id` (or a component of it) tried to escape the plugin's
    /// memory root — `..`, an absolute path, or a separator. Hard-rejected.
    PathTraversal(String),
    /// A cross-plugin / cross-namespace access was denied by [`can_access`].
    AccessDenied { plugin_id: String, namespace: String },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::NotFound { namespace, key } => {
                write!(f, "key `{key}` not found in namespace `{namespace}`")
            }
            MemoryError::PermissionDenied(e) => write!(f, "permission denied: {e}"),
            MemoryError::Io(e) => write!(f, "i/o error: {e}"),
            MemoryError::Malformed { namespace, detail } => {
                write!(f, "malformed memory file for namespace `{namespace}`: {detail}")
            }
            MemoryError::Validation(e) => write!(f, "invalid memory operation: {e}"),
            MemoryError::InvalidNamespace(n) => write!(
                f,
                "unknown namespace `{n}` (expected one of: auth, cache, webhooks, prefs)"
            ),
            MemoryError::PathTraversal(id) => {
                write!(f, "illegal plugin id `{id}` (path traversal rejected)")
            }
            MemoryError::AccessDenied { plugin_id, namespace } => {
                write!(f, "plugin `{plugin_id}` may not access namespace `{namespace}`")
            }
        }
    }
}

impl std::error::Error for MemoryError {}

/// Map a `std::io::Error` to the right [`MemoryError`] variant, preserving the
/// permission-vs-other distinction the callers care about.
fn io_err(context: &str, e: std::io::Error) -> MemoryError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        MemoryError::PermissionDenied(format!("{context}: {e}"))
    } else {
        MemoryError::Io(format!("{context}: {e}"))
    }
}

/// The redaction sentinel shown in place of secret values on display.
pub const REDACTED: &str = "***";

/// A file-based plugin memory store rooted at a plugins directory.
///
/// Cheap to clone (just a `PathBuf`). Construct with [`PluginMemory::new`] for a
/// custom root (tests) or use the [`global`]/free-function wrappers rooted at
/// `~/.aish/plugins` for production callers.
#[derive(Debug, Clone)]
pub struct PluginMemory {
    /// The plugins root, e.g. `~/.aish/plugins`. A plugin's memory lives at
    /// `<root>/<plugin-id>/memory/<namespace>.json`.
    root: PathBuf,
}

impl PluginMemory {
    /// Root a store at `plugins_dir` (`<plugins_dir>/<id>/memory/…`).
    pub fn new(plugins_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: plugins_dir.into(),
        }
    }

    // ---- path construction + access control ---------------------------------

    /// Validate a `plugin_id` is a single, safe path component. Rejects empty
    /// ids, `.`/`..`, anything containing a path separator, and absolute-looking
    /// ids. This is the load-bearing guard for namespace isolation: without it a
    /// `plugin_id` of `../other-plugin` would let one plugin read another's
    /// `auth.json`.
    fn validate_plugin_id(plugin_id: &str) -> Result<(), MemoryError> {
        let id = plugin_id.trim();
        if id.is_empty()
            || id == "."
            || id == ".."
            || id.contains('/')
            || id.contains('\\')
            || id.contains("..")
            || id.contains('\0')
        {
            return Err(MemoryError::PathTraversal(plugin_id.to_string()));
        }
        Ok(())
    }

    /// Per-namespace access control. Today every namespace is "owning plugin
    /// only", so this reduces to validating the `plugin_id` is a safe component
    /// — but the signature is the seam for future rules (e.g. a shared read-only
    /// `cache`), and every read/write funnels through it.
    pub fn can_access(&self, plugin_id: &str, _ns: MemoryNamespace) -> Result<(), MemoryError> {
        Self::validate_plugin_id(plugin_id)
    }

    /// `<root>/<plugin-id>/memory/` — the plugin's memory directory.
    fn memory_dir(&self, plugin_id: &str) -> Result<PathBuf, MemoryError> {
        Self::validate_plugin_id(plugin_id)?;
        Ok(self.root.join(plugin_id).join("memory"))
    }

    /// `<root>/<plugin-id>/memory/<namespace>.json`.
    fn namespace_path(&self, plugin_id: &str, ns: MemoryNamespace) -> Result<PathBuf, MemoryError> {
        Ok(self.memory_dir(plugin_id)?.join(ns.file_name()))
    }

    // ---- low-level file I/O -------------------------------------------------

    /// Read + parse a namespace file into its top-level JSON object. A missing
    /// file is **not** an error — it yields an empty object `{}` (a plugin that
    /// has never written has empty memory). A present-but-corrupt file is a hard
    /// [`MemoryError::Malformed`] so silent data loss never masquerades as
    /// "empty".
    ///
    /// For the secret `auth` namespace this also enforces the `0600` invariant
    /// on read: if the file's perms drifted (e.g. a hand `chmod 644`), they are
    /// corrected back to `0600` and the correction is logged.
    pub fn load_namespace(
        &self,
        plugin_id: &str,
        ns: MemoryNamespace,
    ) -> Result<Value, MemoryError> {
        self.can_access(plugin_id, ns)?;
        let path = self.namespace_path(plugin_id, ns)?;

        if ns.is_secret() {
            self.enforce_read_perms(&path, ns);
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Value::Object(Map::new()));
            }
            Err(e) => return Err(io_err(&format!("read {}", path.display()), e)),
        };
        if text.trim().is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        let parsed: Value = serde_json::from_str(&text).map_err(|e| MemoryError::Malformed {
            namespace: ns.as_str().to_string(),
            detail: e.to_string(),
        })?;
        match parsed {
            Value::Object(_) => Ok(parsed),
            _ => Err(MemoryError::Malformed {
                namespace: ns.as_str().to_string(),
                detail: "root is not a JSON object".to_string(),
            }),
        }
    }

    /// Serialize `value` and write it atomically to the namespace file (temp
    /// file + `rename`), creating the memory directory if needed. Secret
    /// namespaces are created and re-chmod'd `0600`.
    pub fn save_namespace(
        &self,
        plugin_id: &str,
        ns: MemoryNamespace,
        value: &Value,
    ) -> Result<(), MemoryError> {
        self.can_access(plugin_id, ns)?;
        let dir = self.memory_dir(plugin_id)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| io_err(&format!("create {}", dir.display()), e))?;
        // Best-effort tighten the memory dir to owner-only; a plugin's memory
        // dir has no reason to be group/world traversable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }

        let path = self.namespace_path(plugin_id, ns)?;
        let bytes = serde_json::to_vec_pretty(value)
            .map_err(|e| MemoryError::Io(format!("serialize {ns}: {e}")))?;
        atomic_write(&path, &bytes, ns.file_mode())?;
        Ok(())
    }

    /// On read of a secret namespace, verify perms are exactly `0600`; if not,
    /// silently fix (chmod 0600) and log the correction. A missing file is
    /// nothing to check. No-op on non-unix.
    #[cfg(unix)]
    fn enforce_read_perms(&self, path: &Path, ns: MemoryNamespace) {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return; // missing file: nothing to enforce
        };
        let mode = meta.permissions().mode() & 0o777;
        let want = ns.file_mode();
        if mode != want {
            if std::fs::set_permissions(path, std::fs::Permissions::from_mode(want)).is_ok() {
                eprintln!(
                    "\x1b[33maish:\x1b[0m corrected perms on {} ({:o} -> {:o}) — never chmod plugin auth files by hand",
                    path.display(),
                    mode,
                    want
                );
            }
        }
    }

    #[cfg(not(unix))]
    fn enforce_read_perms(&self, _path: &Path, _ns: MemoryNamespace) {}

    // ---- public API: get / set / append / delete / clear --------------------

    /// Fetch the value at dot-notation `key` in `namespace`. Descends nested
    /// objects (`webhooks.github.last_delivery_id`). Errors with
    /// [`MemoryError::NotFound`] when the key path is absent.
    pub fn get(&self, plugin_id: &str, namespace: &str, key: &str) -> Result<Value, MemoryError> {
        let ns = MemoryNamespace::parse(namespace)?;
        let root = self.load_namespace(plugin_id, ns)?;
        let path = split_key(key)?;
        get_path(&root, &path)
            .cloned()
            .ok_or_else(|| MemoryError::NotFound {
                namespace: ns.as_str().to_string(),
                key: key.to_string(),
            })
    }

    /// Set (create or overwrite) the value at dot-notation `key`, creating
    /// intermediate objects as needed, then persist. Writing a redaction
    /// sentinel (`"***"`) into the secret `auth` namespace is rejected — it is
    /// almost always an accidental round-trip of a redacted display value.
    pub fn set(
        &self,
        plugin_id: &str,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        let ns = MemoryNamespace::parse(namespace)?;
        if ns.is_secret() && contains_redaction_sentinel(&value) {
            return Err(MemoryError::Validation(format!(
                "refusing to write redaction sentinel `{REDACTED}` into secret namespace `auth` \
                 (did a redacted display value round-trip?)"
            )));
        }
        let mut root = self.load_namespace(plugin_id, ns)?;
        let path = split_key(key)?;
        set_path(&mut root, &path, value)?;
        self.save_namespace(plugin_id, ns, &root)
    }

    /// Append `value` to the array at dot-notation `key`. A missing key is
    /// created as a one-element array; an existing array is pushed to; anything
    /// else is a [`MemoryError::Validation`] (can't append to a scalar/object).
    pub fn append(
        &self,
        plugin_id: &str,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), MemoryError> {
        let ns = MemoryNamespace::parse(namespace)?;
        let mut root = self.load_namespace(plugin_id, ns)?;
        let path = split_key(key)?;
        match get_path(&root, &path) {
            None | Some(Value::Null) => {
                set_path(&mut root, &path, Value::Array(vec![value]))?;
            }
            Some(Value::Array(_)) => {
                // Re-fetch mutably and push.
                let slot = get_path_mut(&mut root, &path)
                    .expect("path existed on immutable read above");
                if let Value::Array(arr) = slot {
                    arr.push(value);
                }
            }
            Some(other) => {
                return Err(MemoryError::Validation(format!(
                    "cannot append to `{key}`: value is {}, not an array",
                    json_type_name(other)
                )));
            }
        }
        self.save_namespace(plugin_id, ns, &root)
    }

    /// Delete the value at dot-notation `key`. Deleting a missing key is a
    /// [`MemoryError::NotFound`] (callers who want idempotent delete can ignore
    /// it) — this makes "did it exist?" observable.
    pub fn delete(&self, plugin_id: &str, namespace: &str, key: &str) -> Result<(), MemoryError> {
        let ns = MemoryNamespace::parse(namespace)?;
        let mut root = self.load_namespace(plugin_id, ns)?;
        let path = split_key(key)?;
        if delete_path(&mut root, &path) {
            self.save_namespace(plugin_id, ns, &root)
        } else {
            Err(MemoryError::NotFound {
                namespace: ns.as_str().to_string(),
                key: key.to_string(),
            })
        }
    }

    /// Clear an entire namespace: overwrite the file with `{}`. The file is kept
    /// (not unlinked) so its perms/inode are preserved and a subsequent read is
    /// a clean empty object.
    pub fn clear(&self, plugin_id: &str, namespace: &str) -> Result<(), MemoryError> {
        let ns = MemoryNamespace::parse(namespace)?;
        self.save_namespace(plugin_id, ns, &Value::Object(Map::new()))
    }

    // ---- introspection ------------------------------------------------------

    /// The top-level key count for one namespace (0 when the file is absent or
    /// empty). Powers `:plugin memory list`.
    pub fn key_count(&self, plugin_id: &str, ns: MemoryNamespace) -> Result<usize, MemoryError> {
        let root = self.load_namespace(plugin_id, ns)?;
        Ok(root.as_object().map(|m| m.len()).unwrap_or(0))
    }

    /// The whole namespace object, with secret namespaces redacted for display.
    /// Non-secret namespaces are returned as-is. Use this — never
    /// [`load_namespace`] — when rendering to a user.
    pub fn display_namespace(
        &self,
        plugin_id: &str,
        ns: MemoryNamespace,
    ) -> Result<Value, MemoryError> {
        let root = self.load_namespace(plugin_id, ns)?;
        if ns.is_secret() {
            Ok(redact(&root))
        } else {
            Ok(root)
        }
    }
}

// ---- nested-path helpers ---------------------------------------------------

/// Split a dot-notation key into its components, rejecting an empty key or an
/// empty component (`a..b`, leading/trailing dot).
fn split_key(key: &str) -> Result<Vec<&str>, MemoryError> {
    if key.is_empty() {
        return Err(MemoryError::Validation("empty key".to_string()));
    }
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(MemoryError::Validation(format!(
            "malformed key `{key}` (empty path segment)"
        )));
    }
    Ok(parts)
}

/// Immutable descent through nested objects following `path`.
fn get_path<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path {
        cur = cur.as_object()?.get(*seg)?;
    }
    Some(cur)
}

/// Mutable descent (no creation) through nested objects following `path`.
fn get_path_mut<'a>(root: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path {
        cur = cur.as_object_mut()?.get_mut(*seg)?;
    }
    Some(cur)
}

/// Set `val` at `path`, creating intermediate objects as needed. An existing
/// intermediate that is present but not an object is a hard error (we won't
/// clobber a scalar with a subtree implicitly).
fn set_path(root: &mut Value, path: &[&str], val: Value) -> Result<(), MemoryError> {
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let mut cur = root;
    for (i, seg) in path.iter().enumerate() {
        let last = i == path.len() - 1;
        let obj = cur.as_object_mut().expect("ensured object above/below");
        if last {
            obj.insert((*seg).to_string(), val);
            return Ok(());
        }
        let entry = obj
            .entry((*seg).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            return Err(MemoryError::Validation(format!(
                "cannot descend into `{seg}`: value is {}, not an object",
                json_type_name(entry)
            )));
        }
        cur = entry;
    }
    Ok(())
}

/// Remove the value at `path`. Returns `true` if something was removed.
fn delete_path(root: &mut Value, path: &[&str]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let Some(parent) = get_path_mut(root, parents) else {
        return false;
    };
    parent
        .as_object_mut()
        .map(|obj| obj.remove(*last).is_some())
        .unwrap_or(false)
}

/// Recursively replace every leaf (string/number/bool) with the redaction
/// sentinel, preserving object keys and array/object structure. Used to display
/// the *shape* of a secret namespace without leaking values.
pub fn redact(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            Value::Object(m.iter().map(|(k, val)| (k.clone(), redact(val))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        Value::Null => Value::Null,
        _ => Value::String(REDACTED.to_string()),
    }
}

/// Does a value contain the redaction sentinel anywhere in its leaves? Guards
/// against a redacted display value being written back into `auth`.
fn contains_redaction_sentinel(v: &Value) -> bool {
    match v {
        Value::String(s) => s == REDACTED,
        Value::Array(a) => a.iter().any(contains_redaction_sentinel),
        Value::Object(m) => m.values().any(contains_redaction_sentinel),
        _ => false,
    }
}

/// Short human name for a JSON value's type (error messages).
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "a string",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::Object(_) => "an object",
        Value::Array(_) => "an array",
        Value::Null => "null",
    }
}

// ---- atomic write ----------------------------------------------------------

/// Write `bytes` to `path` atomically: write a uniquely-named sibling temp file,
/// fsync it, then `rename` it over the target (rename is atomic within a
/// filesystem). On unix the temp file is created with `mode` so a secret file is
/// never briefly world-readable, and `mode` is re-applied to the final path
/// after the rename for good measure.
fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), MemoryError> {
    let dir = path.parent().ok_or_else(|| {
        MemoryError::Io(format!("path {} has no parent directory", path.display()))
    })?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("memory.json");
    let tmp = dir.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    // Scoped so the file handle is closed (flushed) before we rename.
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| io_err(&format!("create temp {}", tmp.display()), e))?;
        f.write_all(bytes)
            .map_err(|e| io_err(&format!("write temp {}", tmp.display()), e))?;
        let _ = f.sync_all();
    }

    // Atomic replace.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp); // don't leak the temp on failure
        return Err(io_err(&format!("rename into {}", path.display()), e));
    }

    // Re-assert perms on the final path (rename preserves the temp's mode, but
    // an existing target's inode is replaced, so this is belt-and-suspenders).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    let _ = mode; // used only under cfg(unix)
    Ok(())
}

// ---- process-global convenience wrappers -----------------------------------

/// The default plugins root, `~/.aish/plugins` — resolved identically to
/// [`crate::plugins::default_plugins_dir`] but computed inline so this module
/// stays free of intra-crate deps and can be compiled directly into the
/// `#[path]`-included integration test (aish has no lib target).
pub fn default_plugins_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("plugins")
}

/// A [`PluginMemory`] rooted at the default `~/.aish/plugins` directory, for
/// REPL commands and plugin hooks that don't thread a root through.
pub fn global() -> PluginMemory {
    PluginMemory::new(default_plugins_dir())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn namespace_parse_and_props() {
        assert_eq!(MemoryNamespace::parse("AUTH").unwrap(), MemoryNamespace::Auth);
        assert_eq!(MemoryNamespace::parse(" cache ").unwrap(), MemoryNamespace::Cache);
        assert!(MemoryNamespace::parse("bogus").is_err());
        assert!(MemoryNamespace::Auth.is_secret());
        assert!(!MemoryNamespace::Prefs.is_secret());
        assert_eq!(MemoryNamespace::Auth.file_mode(), 0o600);
        assert_eq!(MemoryNamespace::Cache.file_mode(), 0o644);
        assert_eq!(MemoryNamespace::Webhooks.file_name(), "webhooks.json");
    }

    #[test]
    fn plugin_id_validation_rejects_traversal() {
        for bad in ["", ".", "..", "../evil", "a/b", "a\\b", "x..y"] {
            assert!(
                PluginMemory::validate_plugin_id(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
        for ok in ["github", "hello-world", "my_plugin", "a.b"] {
            // note: `a.b` has no `..` and no separator → allowed as an id.
            if ok.contains("..") {
                continue;
            }
            assert!(
                PluginMemory::validate_plugin_id(ok).is_ok(),
                "expected `{ok}` to be allowed"
            );
        }
    }

    #[test]
    fn redact_blanks_leaves_keeps_shape() {
        let v = serde_json::json!({
            "access_token": "secret-abc",
            "expires_at": 123,
            "nested": {"refresh": "r", "flag": true},
            "list": ["a", 1]
        });
        let r = redact(&v);
        assert_eq!(
            r,
            serde_json::json!({
                "access_token": "***",
                "expires_at": "***",
                "nested": {"refresh": "***", "flag": "***"},
                "list": ["***", "***"]
            })
        );
    }

    #[test]
    fn nested_set_get_delete() {
        let mut root = Value::Object(Map::new());
        set_path(&mut root, &["webhooks", "github", "id"], serde_json::json!("12345")).unwrap();
        assert_eq!(
            get_path(&root, &["webhooks", "github", "id"]),
            Some(&serde_json::json!("12345"))
        );
        assert!(delete_path(&mut root, &["webhooks", "github", "id"]));
        assert!(get_path(&root, &["webhooks", "github", "id"]).is_none());
        // parent object remains
        assert!(get_path(&root, &["webhooks", "github"]).is_some());
    }

    #[test]
    fn set_through_scalar_is_rejected() {
        let mut root = Value::Object(Map::new());
        set_path(&mut root, &["a"], serde_json::json!(5)).unwrap();
        let err = set_path(&mut root, &["a", "b"], serde_json::json!(1)).unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)));
    }
}
