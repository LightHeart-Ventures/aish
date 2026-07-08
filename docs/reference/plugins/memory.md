# Plugin Memory Schema (Phase 2)

**Status:** implemented — `src/plugin_memory.rs`
**Related:** `docs/PLUGIN_SYSTEM_DESIGN.md` §"Plugin Memory Schema" and §Phase 2; sibling store `src/plugin_state.rs` (Phase 1.5, SQLite).

Plugin memory is a persistent, **namespaced, per-plugin** store backed by plain
JSON files. It is deliberately *not* the SQLite `plugin_state` store: those are
two complementary layers.

| | `plugin_state` (Phase 1.5) | `plugin_memory` (Phase 2) |
|---|---|---|
| Backing | one global SQLite DB (`~/.aish/database/plugins.db`) | one JSON file per namespace, per plugin |
| Shape | flat `(plugin_id, key) → value` | namespaced tree, dot-notation keys |
| Perms | DB-file perms (coarse) | **per-namespace** (`auth` = `0600`) |
| Human-inspectable | no (binary DB) | yes (`cat cache.json`) |
| Best for | cheap scalar config | secrets, cache, webhook state, prefs |

---

## 1. On-disk layout

```text
~/.aish/plugins/<plugin-id>/memory/
├── auth.json      # credentials  — mode 0600 (secret namespace)
├── cache.json     # rate limits, timestamps — mode 0644, TTL-eligible
├── webhooks.json  # webhook subscription + delivery state — mode 0644
└── prefs.json     # user preferences — mode 0644, user-editable
```

> **Path reconciliation.** `PLUGIN_SYSTEM_DESIGN.md` shows the directory tree
> under `~/.aish/plugins/<id>/memory/` **and** (in the struct comment) an
> alternate `~/.aish/memory/plugins/<id>/<ns>.json`. Phase 2 uses the former —
> co-locating a plugin's memory with the plugin keeps everything a plugin owns
> under one directory (skills, config, credentials, memory), matches the
> canonical directory diagram, and makes uninstall a single `rm -rf`.

**Format: JSON.** Chosen over TOML for consistency with the rest of the plugin
system (`plugin.json`, `config.json`, `plugin_state`'s `serde_json::Value`) and
because the design-doc examples are already JSON. Files are written
pretty-printed so `cat`/diff stays readable.

**One flat object per file.** Each namespace file is a JSON object mapping keys
to arbitrary JSON values. Nested structure is addressed with **dot-notation**
keys (`webhooks.github.last_delivery_id`).

### Version field / migrations

v1 has **no on-disk envelope or `version` field** — files are bare objects for
readability and a 1:1 mapping to the `(namespace, key)` API. The `__` (double
underscore) top-level key prefix is **reserved** for future metadata (e.g.
`__schema_version`, per-entry TTL envelopes). A forward migration that needs a
version can introduce `{"__schema_version": 2, …}` and branch on its presence;
absence means v1. This keeps the common path clean today without painting us
into a corner.

### Concrete example — `webhooks.json`

```json
{
  "github": {
    "last_delivery_id": "12345",
    "subscribed_events": ["push", "pull_request"]
  },
  "configured_webhooks": [
    { "repo": "LightHeart-Ventures/atum_ai_app", "hook_id": 12345678 }
  ]
}
```

`prefs.json`:

```json
{ "auto_sync": true, "notification_level": "info" }
```

`auth.json` (mode `0600`, redacted on any display):

```json
{
  "access_token": "gho_xxx",
  "refresh_token": "ghr_xxx",
  "expires_at": "2026-07-02T10:00:00Z"
}
```

---

## 2. API

All operations live on `PluginMemory` (rooted at a plugins dir; use
`plugin_memory::global()` for the default `~/.aish/plugins` root). `namespace` is
a string parsed into the `MemoryNamespace` enum — unknown namespaces are
rejected, never invented.

```rust
pub fn get   (&self, plugin_id: &str, namespace: &str, key: &str)               -> Result<Value, MemoryError>;
pub fn set   (&self, plugin_id: &str, namespace: &str, key: &str, value: Value) -> Result<(),    MemoryError>;
pub fn append(&self, plugin_id: &str, namespace: &str, key: &str, value: Value) -> Result<(),    MemoryError>;
pub fn delete(&self, plugin_id: &str, namespace: &str, key: &str)               -> Result<(),    MemoryError>;
pub fn clear (&self, plugin_id: &str, namespace: &str)                          -> Result<(),    MemoryError>;
```

Semantics:

- **get** — descends dot-notation `key`; `NotFound` if the path is absent.
- **set** — creates/overwrites the leaf, creating intermediate objects; rejects
  descending *through* a non-object (won't silently clobber a scalar). In the
  `auth` namespace, refuses to write the redaction sentinel `"***"` (guards
  against a redacted display value round-tripping back in).
- **append** — pushes to the array at `key`; a missing key becomes a
  one-element array; appending to a non-array is a `Validation` error.
- **delete** — removes the leaf; `NotFound` if it wasn't there.
- **clear** — overwrites the namespace file with `{}` (file/inode/perms kept).

### Helpers

- `load_namespace(plugin_id, ns) -> Value` — whole namespace object (missing
  file → `{}`; corrupt file → `Malformed`).
- `display_namespace(plugin_id, ns) -> Value` — like `load_namespace` but
  **redacted** for secret namespaces. Always use this for user-facing output.
- `key_count(plugin_id, ns) -> usize` — top-level key count (for `:plugin
  memory list`).
- `redact(&Value) -> Value` — blanks every leaf to `"***"`, keeps structure.

### Error types (`MemoryError`)

| Variant | Meaning |
|---|---|
| `NotFound { namespace, key }` | key path absent (get/delete) |
| `PermissionDenied(String)` | OS refused read/write |
| `Io(String)` | other I/O failure (disk full, …) |
| `Malformed { namespace, detail }` | file present but not a JSON object |
| `Validation(String)` | append-to-non-array, descend-through-scalar, `***` into auth, bad key |
| `InvalidNamespace(String)` | namespace ∉ {auth,cache,webhooks,prefs} |
| `PathTraversal(String)` | `plugin_id` tried to escape its root |
| `AccessDenied { plugin_id, namespace }` | reserved for future per-namespace rules |

### Reading / writing memory — plugin's-eye view

```rust
let mem = plugin_memory::global();

// Persist webhook state (creates ~/.aish/plugins/github/memory/webhooks.json).
mem.set("github", "webhooks", "github.last_delivery_id", json!("12345"))?;
mem.append("github", "cache", "webhook_delivery_timestamps", json!(1719926000))?;

// Read it back.
let last = mem.get("github", "webhooks", "github.last_delivery_id")?; // "12345"

// Secrets — written to auth.json at 0600, never shown verbatim.
mem.set("github", "auth", "access_token", json!("gho_xxx"))?;
let shape = mem.display_namespace("github", MemoryNamespace::Auth)?; // {"access_token":"***"}
```

---

## 3. Namespace rules

| Namespace  | File            | Mode  | Redact | TTL | Notes |
|------------|-----------------|-------|--------|-----|-------|
| `auth`     | `auth.json`     | `0600`| yes    | no  | credentials; perms enforced on create/write/read |
| `cache`    | `cache.json`    | `0644`| no     | *eligible* | rate limits, timestamps |
| `webhooks` | `webhooks.json` | `0644`| no     | no  | persistent webhook state |
| `prefs`    | `prefs.json`    | `0644`| no     | no  | persistent, user-editable |

The namespace set is closed and modeled as an enum, so it is impossible to
construct a memory path for a namespace outside this table — a namespace-safety
guarantee in the type system.

### TTL semantics

TTL is **designed but not auto-enforced in v1** (the design doc marks it
optional/low-priority). The intended v2 shape: cache entries wrapped as
`{"__ttl": <unix_expiry>, "v": <value>}`, pruned on read/write. v1 stores cache
values as plain JSON and never expires them; the reserved `__` prefix leaves
room to add this without a breaking change. See open questions.

---

## 4. Security model

### Threat model — what this protects against

1. **Cross-plugin secret theft.** Plugin A must not read plugin B's
   `auth.json`. Every path is `<root>/<plugin-id>/memory/<ns>.json` and
   `plugin_id` is validated as a single safe path component: empty, `.`, `..`,
   anything containing `/`, `\`, `..`, or NUL is hard-rejected
   (`PathTraversal`). So a crafted id like `../victim` cannot escape the
   caller's own memory root.
2. **World-readable secrets on disk.** `auth.json` is *created* with mode
   `0600` (temp file opened with `O_CREAT` + mode `0600`, so it is never even
   briefly group/world-readable), re-chmod'd `0600` after every atomic write,
   and **auto-corrected on read** if its perms drifted (with a logged warning).
   The memory directory itself is tightened to `0700`.
3. **Secret leakage via display/logs.** `display_namespace` / `redact` blank
   every leaf value in `auth` to `"***"` before it can reach a terminal or log.
   The REPL `:plugin memory` commands only ever use the redacting path for
   `auth`.
4. **Redaction round-trip.** Writing `"***"` into `auth` is refused, so a user
   who copies a redacted display back into a `set` can't overwrite a real token
   with the sentinel.
5. **Corruption on crash / concurrent write.** Every write is a temp-file +
   `rename` (atomic within a filesystem), so a crash mid-write leaves either the
   old file or the new file — never a half-written, unparseable one.

### What it does **not** protect against

- An attacker with the user's UID (they can read `0600` files anyway) — this is
  process-isolation of secrets between plugins, not against the OS user.
- Encryption at rest (`auth.json` is plaintext JSON; the design doc's
  "encrypted/opaque" note is aspirational — perms are the v2 boundary).
- A malicious plugin reading its *own* secrets (by design it owns them).

### File-perms guarantee (auth namespace)

- Created with `0600` atomically via `OpenOptions::mode(0o600)` on the temp file.
- `0600` re-applied after every atomic write.
- On read, wrong perms are **silently fixed** to `0600` and the correction is
  logged: `aish: corrected perms on …/auth.json (644 -> 600)`.
- **Do not `chmod` auth files by hand** — aish restores `0600` on next access.
- Non-`auth` namespaces are `0644` and are *not* forced to `0600`.

---

## 5. Implementation notes (2.2–2.4)

- **Module:** `src/plugin_memory.rs`, self-contained (std + serde_json only) so
  the integration test compiles it directly with `#[path = …]` (aish is a binary
  crate, no lib target — same pattern as `plugin_state_tests.rs`).
- **Atomic write:** `atomic_write(path, bytes, mode)` writes `.{name}.tmp.{pid}.{nanos}`
  in the same directory (same-filesystem rename), `sync_all`s, then `rename`s
  over the target and re-applies `mode`. Temp is removed on rename failure.
- **Missing file = empty**, corrupt file = `Malformed` (never silent data loss).
- **Nested access:** `split_key` splits on `.` (rejects empty segments);
  `get_path`/`set_path`/`delete_path` walk/create the object tree.
- **Perms are `#[cfg(unix)]`;** on non-unix, perms enforcement is a documented
  no-op (aish's target is Linux).

---

## 6. Open questions for follow-up

- **TTL:** implement the `{"__ttl", "v"}` cache envelope + read-time pruning, or
  leave cache expiry to plugins? (v1: deferred.)
- **`clear` unlink vs empty:** v1 keeps the file as `{}`. Unlinking would drop
  the `0600` inode; keeping it preserves perms. Confirm this is the desired
  behavior for `auth`.
- **Encryption at rest** for `auth` (age/OS keychain) — perms-only in v1.
- **Cross-plugin shared namespaces** (e.g. a read-only shared `cache`) — the
  `can_access` seam exists but is unused.
