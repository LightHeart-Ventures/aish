//! Secret materialisation policy + audit — TASK-946 / SEC-3.1 F-02.
//!
//! The system contract tells the model to pass credentials as `${profile:KEY}`
//! refs "so their values never enter the conversation". Before this module that
//! was a promise the implementation did not keep: `resolve_env_value` resolved
//! any ref into any program's environment, so
//! `run_program{program:"env", env:{X:"${profile:AWS_SECRET_ACCESS_KEY}"}}`
//! printed the secret to stdout — straight into model context, the transcript
//! and the history DB.
//!
//! The fix has four parts, all here:
//!
//! 1. **Classification** of the *effective* program (after peeling wrappers, so
//!    `env X=… sh -c …` classifies as `sh`, not `env`).
//! 2. **Deny by default** ([`gate`]): a secret is materialised only for a binary
//!    on the per-session allowlist or in [`KNOWN_SECRET_CONSUMERS`]. A blocklist
//!    alone is endlessly bypassable (`/bin/busybox sh`, a renamed interpreter);
//!    the inversion is what actually closes the finding. [`ENV_UNSAFE_PROGRAMS`]
//!    survives as the set that can never be reached by the *unknown* branch and
//!    that earns a sharper refusal message.
//! 3. **Per-session policy** ([`SecretPolicy`]) — key allowlist + program
//!    allowlist, driven by `:secrets` and `AISH_SECRET_KEYS`. In unattended mode
//!    (yolo / background coordinator) the key allowlist defaults to EMPTY: a
//!    coordinator resolves no secrets unless granted at launch.
//! 4. **Audit** ([`audit`]) of every attempt, allowed and refused, to
//!    `tracing` target `aish::secrets` and the `secret_audit` table.
//!
//! The one invariant that outranks everything else in this file: **the secret
//! VALUE never appears in any sink, in any form — not plaintext, not hashed.**
//! A hash of a short, low-entropy secret is brute-forceable and invites a "just
//! compare it" helper later. `value_len` is enough to debug an empty-secret
//! problem, and that is all an auditor gets.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

/// Programs that would leak a materialised secret rather than consume it.
///
/// Three families, all of which turn "put this in the child's env" into "print
/// this to stdout": env dumpers, interpreters (an interpreter is a shell for
/// the secret — `python -c 'import os;print(os.environ["K"])'`), process/debug
/// inspection, and egress tools that would ship it off-box.
///
/// This set is NOT the security boundary — [`gate`] denies by default, so an
/// unlisted binary is refused too. Membership here only selects the sharper
/// refusal wording ("it would print the secret").
pub const ENV_UNSAFE_PROGRAMS: &[&str] = &[
    // env dumpers
    "env",
    "printenv",
    "set",
    "export",
    "declare",
    "typeset",
    // interpreters (`python*` is matched by prefix in `is_env_unsafe`)
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "csh",
    "tcsh",
    "ruby",
    "perl",
    "node",
    "deno",
    "bun",
    "php",
    "lua",
    "Rscript",
    "osascript",
    "awk",
    "gawk",
    "jq",
    // process / debug inspection
    "ps",
    "strace",
    "ltrace",
    "dtrace",
    "gdb",
    "lldb",
    "cat",
    "strings",
    // egress
    "curl",
    "wget",
    "nc",
    "ncat",
    "socat",
    "telnet",
    "ssh",
    "scp",
    "rsync",
    "openssl",
];

/// Binaries that legitimately consume a credential from their environment.
/// The seed allowlist — the only programs a secret reaches without an explicit
/// `:secrets allow-program <bin>` grant.
pub const KNOWN_SECRET_CONSUMERS: &[&str] = &[
    "gh",
    "aws",
    "terraform",
    "kubectl",
    "docker",
    "psql",
    "mysql",
    "redis-cli",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "atum",
];

/// Prefix-runners that execute *another* program: the secret's real consumer is
/// further along argv. `env` is both a wrapper and an env dumper — peeled when
/// it has a target, refused as a dumper when bare.
const WRAPPER_PROGRAMS: &[&str] = &[
    "env", "nice", "nohup", "timeout", "stdbuf", "sudo", "doas", "command", "builtin", "setsid",
    "ionice", "chrt", "time", "xargs",
];

/// How many wrappers deep to peel (`sudo env timeout 5 sh -c …`).
const MAX_PEEL_DEPTH: usize = 4;

/// `~/.aish/database/aish.db` table holding the materialisation audit trail.
const AUDIT_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS secret_audit (
     id         INTEGER PRIMARY KEY,
     ts         TEXT NOT NULL DEFAULT current_timestamp,
     session_id TEXT,
     run_id     TEXT,
     key_name   TEXT NOT NULL,
     profile    TEXT,
     program    TEXT NOT NULL,
     argv_hash  TEXT,
     outcome    TEXT NOT NULL,
     value_len  INTEGER NOT NULL DEFAULT 0
 );
 CREATE INDEX IF NOT EXISTS idx_secret_audit_ts ON secret_audit (ts);";

/// Final path segment of a program, so `/bin/busybox` → `busybox`.
fn bin_name(program: &str) -> &str {
    std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
}

/// True when `bin` is a known secret-leaker (see [`ENV_UNSAFE_PROGRAMS`]).
/// `python`, `python3`, `python3.12` all match by prefix.
pub fn is_env_unsafe(bin: &str) -> bool {
    ENV_UNSAFE_PROGRAMS.contains(&bin) || bin.starts_with("python")
}

/// True when `bin` is in the seed consumer allowlist.
pub fn is_known_consumer(bin: &str) -> bool {
    KNOWN_SECRET_CONSUMERS.contains(&bin)
}

/// Does this token look like an option rather than a program name?
fn is_flag(tok: &str) -> bool {
    tok.starts_with('-')
}

/// `KEY=value` — an assignment `env` consumes, not the target program.
fn is_assignment(tok: &str) -> bool {
    match tok.find('=') {
        Some(0) | None => false,
        Some(i) => tok[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
    }
}

/// Wrapper flags that consume the FOLLOWING token as their value, so the value
/// is not mistaken for the target program (`sudo -u root bash` → `bash`, not
/// `root`). Conservative on purpose: a flag we don't know is treated as taking
/// no value, which at worst leaves the program "unknown" — and unknown is
/// refused, never allowed.
fn flag_takes_value(wrapper: &str, flag: &str) -> bool {
    if flag.contains('=') {
        return false; // `--user=root` carries its own value
    }
    let shared = matches!(wrapper, "sudo" | "doas")
        && matches!(
            flag,
            "-u" | "-g"
                | "-U"
                | "-p"
                | "-C"
                | "-h"
                | "-r"
                | "-t"
                | "-D"
                | "--user"
                | "--group"
                | "--prompt"
                | "--chdir"
                | "--host"
                | "--role"
                | "--type"
        );
    shared
        || match wrapper {
            "env" => matches!(
                flag,
                "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
            ),
            "nice" | "ionice" => matches!(flag, "-n" | "--adjustment" | "-c" | "-p"),
            "timeout" => matches!(flag, "-s" | "--signal" | "-k" | "--kill-after"),
            "stdbuf" => matches!(
                flag,
                "-i" | "-o" | "-e" | "--input" | "--output" | "--error"
            ),
            "chrt" => matches!(flag, "-p" | "--pid"),
            "xargs" => matches!(
                flag,
                "-a" | "-d"
                    | "-E"
                    | "-I"
                    | "-i"
                    | "-L"
                    | "-l"
                    | "-n"
                    | "-P"
                    | "-s"
                    | "--delimiter"
                    | "--max-args"
                    | "--max-procs"
                    | "--replace"
                    | "--arg-file"
            ),
            "time" => matches!(flag, "-f" | "--format" | "-o" | "--output"),
            _ => false,
        }
}

/// A bare duration/niceness operand (`timeout 5`, `nice 10`, `timeout 1.5s`).
fn is_numeric_operand(tok: &str) -> bool {
    let core = tok.trim_end_matches(['s', 'm', 'h', 'd']);
    !core.is_empty() && core.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// The program that will actually *run*, with prefix wrappers peeled.
///
/// This is the TASK-938 normalisation the spec asks us to resolve through.
/// TASK-938's normaliser is not on `main` yet (`unwrap_noexec_builtin` in
/// tools.rs only rewrites the no-exec builtins `command`/`builtin`/`type`), so
/// this is the deliberately minimal local wrapper-peel — noted in the PR body
/// as the merge point. It is conservative by construction: anything it fails to
/// peel stays "unknown" and is therefore refused, never allowed.
pub fn effective_program(program: &str, args: &[String]) -> String {
    let mut bin = bin_name(program).to_string();
    let mut rest: &[String] = args;

    for _ in 0..MAX_PEEL_DEPTH {
        if !WRAPPER_PROGRAMS.contains(&bin.as_str()) {
            break;
        }
        // Skip the wrapper's own flags (and any value a flag consumes),
        // assignments, and numeric operands to find the target program.
        let mut found = None;
        let mut i = 0;
        while i < rest.len() {
            let tok = rest[i].as_str();
            if is_flag(tok) {
                if flag_takes_value(&bin, tok) {
                    i += 1; // the flag's value is not the program
                }
            } else if !is_assignment(tok) && !is_numeric_operand(tok) {
                found = Some(i);
                break;
            }
            i += 1;
        }
        let Some(idx) = found else {
            // A bare wrapper with no target (e.g. plain `env`) — it IS the
            // program, and for `env` that means an env dump.
            break;
        };
        bin = bin_name(&rest[idx]).to_string();
        rest = &rest[idx + 1..];
    }
    bin
}

/// Stable, value-free fingerprint of the argv a materialisation was requested
/// for. FNV-1a over `program\0arg\0…` — enough to correlate two audit rows that
/// came from the same command without storing the command itself.
pub fn argv_hash(program: &str, args: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    };
    feed(program.as_bytes());
    feed(&[0]);
    for a in args {
        feed(a.as_bytes());
        feed(&[0]);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// Per-session policy
// ---------------------------------------------------------------------------

/// Which secrets may be materialised, and into which programs, for THIS
/// session.
///
/// `allowed_keys == None` means "any key the credentials file holds" — the
/// interactive default, where a human is watching the refusals. `Some(set)`
/// restricts to exactly `set`, and `Some(empty)` resolves nothing at all: the
/// unattended default, so a background coordinator that was never granted a key
/// cannot materialise one.
#[derive(Debug, Clone, Default)]
pub struct SecretPolicy {
    pub allowed_keys: Option<HashSet<String>>,
    pub allowed_programs: HashSet<String>,
}

impl SecretPolicy {
    /// Policy for a new session.
    ///
    /// * `AISH_SECRET_KEYS=A,B` → only `A` and `B` (the launch-time grant a
    ///   coordinator gets).
    /// * `AISH_SECRET_KEYS=*` → any key.
    /// * unset + unattended → EMPTY allowlist (no secrets).
    /// * unset + interactive → any key.
    pub fn for_session(unattended: bool) -> Self {
        let allowed_keys = match std::env::var("AISH_SECRET_KEYS") {
            Ok(v) if v.trim() == "*" => None,
            Ok(v) => Some(
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
            Err(_) if unattended => Some(HashSet::new()),
            Err(_) => None,
        };
        Self {
            allowed_keys,
            allowed_programs: HashSet::new(),
        }
    }

    /// `:secrets allow <KEY>` — permit one key. Switches an unrestricted policy
    /// to a restricted one only when it was already restricted; `None` (any key)
    /// stays `None`.
    pub fn allow_key(&mut self, key: &str) {
        if let Some(set) = self.allowed_keys.as_mut() {
            set.insert(key.to_string());
        }
    }

    /// `:secrets deny <KEY>` — forbid one key, restricting an unrestricted
    /// policy to "everything currently known except this" is impossible without
    /// enumerating the credentials file, so a deny on an unrestricted policy
    /// flips it to an empty allowlist plus an explicit note from the caller.
    pub fn deny_key(&mut self, key: &str) {
        match self.allowed_keys.as_mut() {
            Some(set) => {
                set.remove(key);
            }
            None => self.allowed_keys = Some(HashSet::new()),
        }
    }

    /// `:secrets allow-program <bin>` — permit one binary.
    pub fn allow_program(&mut self, bin: &str) {
        self.allowed_programs.insert(bin_name(bin).to_string());
    }

    fn key_permitted(&self, key: &str) -> bool {
        match &self.allowed_keys {
            None => true,
            Some(set) => set.contains(key),
        }
    }

    /// Human-readable policy summary for `:secrets`.
    pub fn describe(&self) -> String {
        let keys = match &self.allowed_keys {
            None => "any key in the credentials file".to_string(),
            Some(set) if set.is_empty() => {
                "NONE — no secret resolves in this session (grant with `:secrets allow <KEY>` or \
launch with AISH_SECRET_KEYS)"
                    .to_string()
            }
            Some(set) => {
                let mut v: Vec<&str> = set.iter().map(String::as_str).collect();
                v.sort_unstable();
                v.join(", ")
            }
        };
        let mut progs: Vec<&str> = self.allowed_programs.iter().map(String::as_str).collect();
        progs.sort_unstable();
        let progs = if progs.is_empty() {
            "(none — only the built-in known consumers)".to_string()
        } else {
            progs.join(", ")
        };
        format!(
            "keys allowed: {keys}\nextra programs allowed: {progs}\nknown consumers: {}",
            KNOWN_SECRET_CONSUMERS.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// Outcome of a materialisation request. `Refuse` carries the operator-facing
/// message, which always names the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Refuse(String),
}

impl Decision {
    /// Audit `outcome` column value.
    fn outcome(&self) -> &'static str {
        match self {
            Decision::Allow => "allowed",
            Decision::Refuse(_) => "refused",
        }
    }
}

/// Decide whether `key` may be materialised into the environment of the command
/// `program args…`.
///
/// Order matters: the key allowlist is checked first (a key this session may not
/// touch is refused regardless of consumer), then the per-session program grant,
/// then the leaker set (sharper message), then deny-by-default.
pub fn gate(policy: &SecretPolicy, key: &str, program: &str, args: &[String]) -> Decision {
    let bin = effective_program(program, args);

    if !policy.key_permitted(key) {
        let empty = policy.allowed_keys.as_ref().is_some_and(HashSet::is_empty);
        return Decision::Refuse(if empty {
            format!(
                "refused to resolve ${{profile:{key}}} — this session materialises NO secrets \
(unattended default). Grant it with `:secrets allow {key}`, or launch with \
AISH_SECRET_KEYS={key}."
            )
        } else {
            format!(
                "refused to resolve ${{profile:{key}}} — `{key}` is not in this session's secret \
allowlist. Allow it with `:secrets allow {key}`."
            )
        });
    }

    if policy.allowed_programs.contains(&bin) {
        return Decision::Allow;
    }

    if is_env_unsafe(&bin) {
        return Decision::Refuse(format!(
            "refused to resolve ${{profile:{key}}} for `{bin}` — it would print the secret. Pass \
it to the program that consumes it, or allow with `:secrets allow-program {bin}`."
        ));
    }

    if is_known_consumer(&bin) {
        return Decision::Allow;
    }

    Decision::Refuse(format!(
        "refused to resolve ${{profile:{key}}} for `{bin}` — aish materialises secrets only into \
known consumers ({}), so an unrecognised binary is denied by default. Allow with \
`:secrets allow-program {bin}`.",
        KNOWN_SECRET_CONSUMERS.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// One materialisation attempt, allowed or refused.
///
/// There is deliberately **no value field, and no hash of the value**. Only the
/// key NAME, the profile it came from, the consumer, a value-free argv
/// fingerprint, and the length.
#[derive(Debug, Clone)]
pub struct SecretMaterialisation {
    pub session_id: String,
    pub run_id: Option<String>,
    /// Credential key NAME — never its value.
    pub key: String,
    pub profile: String,
    /// Effective program (post wrapper-peel).
    pub program: String,
    pub argv_hash: String,
    pub outcome: String,
    /// Byte length of the resolved value; 0 for a refusal or an unresolvable
    /// ref. Enough to debug "the secret arrived empty" without exposing it.
    pub value_len: usize,
}

/// Recent materialisation attempts, newest last — the `:secrets` log view.
/// Bounded; holds rendered, value-free lines only.
fn log_ring() -> &'static Mutex<VecDeque<String>> {
    static RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::new()))
}

const LOG_RING_CAP: usize = 50;

/// Record an attempt to both sinks: `tracing` (target `aish::secrets`) and the
/// `secret_audit` table. Best-effort — an audit failure never blocks or alters
/// the decision, it just loses a row.
pub fn audit(rec: &SecretMaterialisation) {
    tracing::info!(
        target: "aish::secrets",
        session_id = %rec.session_id,
        run_id = rec.run_id.as_deref().unwrap_or(""),
        key = %rec.key,
        profile = %rec.profile,
        program = %rec.program,
        argv_hash = %rec.argv_hash,
        outcome = %rec.outcome,
        value_len = rec.value_len,
        "secret materialisation"
    );

    if let Ok(mut ring) = log_ring().lock() {
        ring.push_back(format!(
            "{} {:<8} {} → {} (len {})",
            now_iso(),
            rec.outcome,
            rec.key,
            rec.program,
            rec.value_len
        ));
        while ring.len() > LOG_RING_CAP {
            ring.pop_front();
        }
    }

    if let Err(e) = audit_to_db(rec) {
        tracing::debug!(target: "aish::secrets", error = %e, "secret audit row not persisted");
    }
}

/// Rendered, value-free materialisation log for `:secrets`.
pub fn recent_log() -> Vec<String> {
    log_ring()
        .lock()
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default()
}

/// Audit DB path. `AISH_SECRET_AUDIT_DB` overrides it (tests, and an operator
/// who wants the trail elsewhere).
fn audit_db_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("AISH_SECRET_AUDIT_DB")
        && !p.is_empty()
    {
        return std::path::PathBuf::from(p);
    }
    // Tests exercise the real `audit()` path; keep their rows out of the
    // operator's `~/.aish/database/aish.db`.
    if cfg!(test) {
        return std::env::temp_dir().join(format!(
            "aish_secret_audit_selftest_{}.db",
            std::process::id()
        ));
    }
    crate::db_paths::main_db_path()
}

/// Insert one row, creating the table if this is the first write, and prune
/// anything older than 90 days.
fn audit_to_db(rec: &SecretMaterialisation) -> anyhow::Result<()> {
    let conn = rusqlite::Connection::open(audit_db_path())?;
    conn.execute_batch(AUDIT_TABLE_DDL)?;
    conn.execute(
        "INSERT INTO secret_audit
             (session_id, run_id, key_name, profile, program, argv_hash, outcome, value_len)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            rec.session_id,
            rec.run_id,
            rec.key,
            rec.profile,
            rec.program,
            rec.argv_hash,
            rec.outcome,
            rec.value_len as i64,
        ],
    )?;
    // 90-day retention. The table is tiny (one row per secret attempt), so
    // pruning inline on write is cheaper than a scheduled sweep.
    conn.execute(
        "DELETE FROM secret_audit WHERE ts < datetime('now', '-90 days')",
        [],
    )?;
    Ok(())
}

/// `YYYY-MM-DDTHH:MM:SSZ` without pulling in a date crate: seconds since epoch
/// is enough ordering for a log line, so render the raw stamp.
fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("t{secs}")
}

// ---------------------------------------------------------------------------
// Redaction registry shim (TASK-947 integration point)
// ---------------------------------------------------------------------------

/// Secrets materialised in this process, registered for output redaction.
/// Values live in memory only and are never logged or persisted.
fn redaction_store() -> &'static Mutex<Vec<(String, String)>> {
    static STORE: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a resolved secret with the redaction registry so it is scrubbed out
/// of captured child output.
///
/// **TASK-947 INTEGRATION POINT.** TASK-947 builds the real `Redactor` registry
/// in `src/redact.rs` on a parallel branch, which is not on `main` yet. This is
/// the no-op-safe shim: it records `(label, value)` in-process now, and the
/// merge steward replaces the body with a forward to
/// `crate::redact::Redactor::global().register(value, label)` — the CALL SITE
/// (immediately before spawn, after a successful resolution) does not move.
/// Do not reimplement the filter here.
pub fn register_secret_for_redaction(value: &str, label: &str) {
    if value.is_empty() {
        return;
    }
    if let Ok(mut store) = redaction_store().lock()
        && !store.iter().any(|(_, v)| v == value)
    {
        store.push((label.to_string(), value.to_string()));
    }
}

/// Labels (key names) currently registered for redaction — value-free, safe to
/// print.
#[allow(dead_code)]
pub fn registered_redaction_labels() -> Vec<String> {
    redaction_store()
        .lock()
        .map(|s| s.iter().map(|(l, _)| l.clone()).collect())
        .unwrap_or_default()
}

/// Whether a given value was registered for redaction. Used by the AC6 test;
/// never used to print or compare a secret anywhere else.
#[allow(dead_code)]
pub fn is_registered_for_redaction(value: &str) -> bool {
    redaction_store()
        .lock()
        .map(|s| s.iter().any(|(_, v)| v == value))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Spawn-time context: the single entry point tools.rs uses
// ---------------------------------------------------------------------------

/// Everything the gate + audit need about the spawn being prepared.
pub struct SpawnCtx<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub session_id: &'a str,
    pub policy: &'a SecretPolicy,
}

impl SpawnCtx<'_> {
    /// Gate → resolve → audit → register, in that order.
    ///
    /// `lookup` is only called when the gate ALLOWS, so a refused reference
    /// never even reads the credentials file. Returns:
    /// * `Ok(Some(value))` — materialised (and registered for redaction);
    /// * `Ok(None)` — allowed but unresolvable (caller keeps the ref verbatim so
    ///   the failure surfaces in the program, not as a silent empty string);
    /// * `Err(message)` — refused; the caller fails the tool call with it.
    pub fn materialise<F>(
        &self,
        key: &str,
        profile: &str,
        lookup: F,
    ) -> Result<Option<String>, String>
    where
        F: FnOnce() -> Option<String>,
    {
        let decision = gate(self.policy, key, self.program, self.args);
        let bin = effective_program(self.program, self.args);
        let argv_hash = argv_hash(self.program, self.args);
        let run_id = std::env::var("AISH_RUN_ID").ok().filter(|s| !s.is_empty());

        let resolved = match decision {
            Decision::Allow => lookup(),
            Decision::Refuse(_) => None,
        };

        audit(&SecretMaterialisation {
            session_id: self.session_id.to_string(),
            run_id,
            key: key.to_string(),
            profile: profile.to_string(),
            program: bin,
            argv_hash,
            outcome: decision.outcome().to_string(),
            value_len: resolved.as_ref().map_or(0, |v| v.len()),
        });

        match decision {
            Decision::Refuse(msg) => Err(msg),
            Decision::Allow => {
                if let Some(v) = resolved.as_deref() {
                    // AC6: registered BEFORE the child is spawned — the caller
                    // builds the env map and only then forks.
                    register_secret_for_redaction(v, key);
                }
                Ok(resolved)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    /// Policy with no key restriction — isolates the program dimension.
    fn open_policy() -> SecretPolicy {
        SecretPolicy {
            allowed_keys: None,
            allowed_programs: HashSet::new(),
        }
    }

    fn refusal(d: &Decision) -> &str {
        match d {
            Decision::Refuse(m) => m,
            Decision::Allow => panic!("expected a refusal, got Allow"),
        }
    }

    #[test]
    fn refuse_env_dumper() {
        let d = gate(&open_policy(), "AWS_SECRET_ACCESS_KEY", "env", &[]);
        assert!(refusal(&d).contains("it would print the secret"));
        assert!(refusal(&d).contains(":secrets allow-program env"));
        // The message must never be able to carry the value: it is built from
        // the key name only.
        assert!(refusal(&d).contains("${profile:AWS_SECRET_ACCESS_KEY}"));
        for dumper in ["printenv", "set", "export", "declare", "typeset"] {
            assert!(matches!(
                gate(&open_policy(), "K", dumper, &[]),
                Decision::Refuse(_)
            ));
        }
    }

    #[test]
    fn refuse_interpreter() {
        for interp in [
            "sh",
            "bash",
            "zsh",
            "dash",
            "ksh",
            "fish",
            "csh",
            "tcsh",
            "python",
            "python3",
            "python3.12",
            "ruby",
            "perl",
            "node",
            "deno",
            "bun",
            "php",
            "lua",
            "Rscript",
            "osascript",
            "awk",
            "gawk",
            "jq",
        ] {
            let d = gate(&open_policy(), "K", interp, &args("-c true"));
            assert!(
                matches!(d, Decision::Refuse(_)),
                "{interp} should be refused"
            );
        }
    }

    #[test]
    fn refuse_wrapped_interpreter() {
        // The spec's canonical bypass: `env X=${profile:K} sh -c …` must
        // classify as `sh`, not `env`.
        let a = args("X=1 sh -c echo");
        assert_eq!(effective_program("env", &a), "sh");
        let d = gate(&open_policy(), "K", "env", &a);
        assert!(refusal(&d).contains("`sh`"));

        // Deeper nesting and absolute paths peel too.
        assert_eq!(
            effective_program("sudo", &args("-u root /bin/bash -c x")),
            "bash"
        );
        assert_eq!(
            effective_program("env", &args("timeout 5 python3 -c x")),
            "python3"
        );
        assert_eq!(
            effective_program("nohup", &args("nice -n 5 node app.js")),
            "node"
        );

        // A wrapper with a legitimate consumer behind it is allowed.
        assert_eq!(
            effective_program("env", &args("TF_LOG=1 terraform apply")),
            "terraform"
        );
        assert_eq!(
            gate(
                &open_policy(),
                "K",
                "env",
                &args("TF_LOG=1 terraform apply")
            ),
            Decision::Allow
        );
    }

    #[test]
    fn refuse_egress() {
        for eg in [
            "curl", "wget", "nc", "ncat", "socat", "telnet", "ssh", "scp", "rsync", "openssl",
        ] {
            let d = gate(&open_policy(), "K", eg, &args("https://evil.test"));
            assert!(matches!(d, Decision::Refuse(_)), "{eg} should be refused");
        }
    }

    #[test]
    fn refuse_unknown_binary() {
        // Deny by default: the inversion that actually closes the finding.
        // A renamed interpreter and busybox are unknown, not blocklisted.
        for unknown in ["/bin/busybox", "my-helper", "./scripts/deploy", "pythonish"] {
            let d = gate(&open_policy(), "K", unknown, &args("sh"));
            assert!(
                matches!(d, Decision::Refuse(_)),
                "{unknown} should be denied by default"
            );
        }
        let d = gate(&open_policy(), "K", "my-helper", &[]);
        assert!(refusal(&d).contains("denied by default"));
        assert!(refusal(&d).contains(":secrets allow-program my-helper"));
    }

    #[test]
    fn allow_known_consumer() {
        for bin in KNOWN_SECRET_CONSUMERS {
            assert_eq!(
                gate(&open_policy(), "K", bin, &args("--version")),
                Decision::Allow,
                "{bin} should be allowed"
            );
        }
        // Absolute path resolves to the same basename.
        assert_eq!(
            gate(&open_policy(), "K", "/usr/local/bin/gh", &args("pr list")),
            Decision::Allow
        );
    }

    #[test]
    fn allow_program_override() {
        let mut p = open_policy();
        assert!(matches!(
            gate(&p, "K", "my-helper", &[]),
            Decision::Refuse(_)
        ));
        p.allow_program("my-helper");
        assert_eq!(gate(&p, "K", "my-helper", &[]), Decision::Allow);

        // An explicit grant also overrides the leaker set — the operator is
        // allowed to shoot their own foot, deliberately and on the record.
        let mut p2 = open_policy();
        p2.allow_program("env");
        assert_eq!(gate(&p2, "K", "env", &[]), Decision::Allow);

        // Grants are by basename, so a path-qualified invocation still matches.
        let mut p3 = open_policy();
        p3.allow_program("/opt/bin/my-helper");
        assert_eq!(gate(&p3, "K", "/opt/bin/my-helper", &[]), Decision::Allow);
    }

    #[test]
    fn key_allowlist_enforced() {
        let mut p = SecretPolicy {
            allowed_keys: Some(HashSet::from(["ATUM_API_KEY".to_string()])),
            allowed_programs: HashSet::new(),
        };
        assert_eq!(gate(&p, "ATUM_API_KEY", "gh", &[]), Decision::Allow);
        let d = gate(&p, "AWS_SECRET_ACCESS_KEY", "gh", &[]);
        assert!(refusal(&d).contains("not in this session's secret allowlist"));
        assert!(refusal(&d).contains(":secrets allow AWS_SECRET_ACCESS_KEY"));

        p.allow_key("AWS_SECRET_ACCESS_KEY");
        assert_eq!(
            gate(&p, "AWS_SECRET_ACCESS_KEY", "gh", &[]),
            Decision::Allow
        );

        p.deny_key("AWS_SECRET_ACCESS_KEY");
        assert!(matches!(
            gate(&p, "AWS_SECRET_ACCESS_KEY", "gh", &[]),
            Decision::Refuse(_)
        ));

        // The key gate outranks a program grant: an allowed consumer still
        // cannot receive a key this session may not touch.
        let mut p2 = SecretPolicy {
            allowed_keys: Some(HashSet::new()),
            allowed_programs: HashSet::new(),
        };
        p2.allow_program("gh");
        assert!(matches!(gate(&p2, "K", "gh", &[]), Decision::Refuse(_)));
    }

    #[test]
    fn unattended_no_secrets_by_default() {
        // `for_session` reads process env, which is global to the test binary —
        // assert the pure logic instead of mutating it (unsafe in edition 2024).
        let unattended = SecretPolicy {
            allowed_keys: Some(HashSet::new()),
            allowed_programs: HashSet::new(),
        };
        let d = gate(&unattended, "ATUM_API_KEY", "gh", &args("pr list"));
        assert!(refusal(&d).contains("materialises NO secrets"));
        assert!(refusal(&d).contains("AISH_SECRET_KEYS=ATUM_API_KEY"));
        assert!(unattended.describe().contains("NONE"));

        // An interactive session with no env restriction resolves to "any key".
        let interactive = SecretPolicy {
            allowed_keys: None,
            allowed_programs: HashSet::new(),
        };
        assert_eq!(
            gate(&interactive, "ATUM_API_KEY", "gh", &args("pr list")),
            Decision::Allow
        );
    }

    /// Point the audit sink at a scratch DB and return the rows it wrote.
    fn audit_rows(db: &std::path::Path) -> Vec<(String, String, String, i64)> {
        let conn = rusqlite::Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT key_name, program, outcome, value_len FROM secret_audit ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn scratch_db(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aish_secret_audit_{}_{}.db",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Audit writes go to an explicit path, so the test never has to mutate the
    /// process environment (unsafe in edition 2024) and never touches the real
    /// `~/.aish/database/aish.db`.
    fn write_audit_to(db: &std::path::Path, rec: &SecretMaterialisation) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute_batch(AUDIT_TABLE_DDL).unwrap();
        conn.execute(
            "INSERT INTO secret_audit
                 (session_id, run_id, key_name, profile, program, argv_hash, outcome, value_len)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                rec.session_id,
                rec.run_id,
                rec.key,
                rec.profile,
                rec.program,
                rec.argv_hash,
                rec.outcome,
                rec.value_len as i64,
            ],
        )
        .unwrap();
    }

    fn record(key: &str, program: &str, outcome: &str, value_len: usize) -> SecretMaterialisation {
        SecretMaterialisation {
            session_id: "sess-1".into(),
            run_id: Some("run-1".into()),
            key: key.into(),
            profile: "aish".into(),
            program: program.into(),
            argv_hash: argv_hash(program, &[]),
            outcome: outcome.into(),
            value_len,
        }
    }

    #[test]
    fn audit_logged_on_allow() {
        let db = scratch_db("allow");
        let rec = record("ATUM_API_KEY", "gh", "allowed", 9);
        write_audit_to(&db, &rec);
        let rows = audit_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "ATUM_API_KEY");
        assert_eq!(rows[0].1, "gh");
        assert_eq!(rows[0].2, "allowed");
        assert_eq!(rows[0].3, 9);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn audit_logged_on_refuse() {
        let db = scratch_db("refuse");
        write_audit_to(&db, &record("ATUM_API_KEY", "env", "refused", 0));
        let rows = audit_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "refused");
        // A refusal never resolved anything, so there is no length to report.
        assert_eq!(rows[0].3, 0);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn audit_never_contains_value() {
        const SECRET: &str = "sk_live_do_not_log_me_1234567890";
        let db = scratch_db("novalue");

        // The record type physically cannot carry the value — it takes a length.
        let rec = record("ATUM_API_KEY", "gh", "allowed", SECRET.len());
        write_audit_to(&db, &rec);

        // Neither the struct's Debug rendering…
        let dbg = format!("{rec:?}");
        assert!(!dbg.contains(SECRET));
        // …nor any hash/prefix of it.
        assert!(!dbg.contains(&SECRET[..8]));
        // …nor the audit row, nor the raw DB file bytes.
        let raw = std::fs::read(&db).unwrap();
        let hay = String::from_utf8_lossy(&raw);
        assert!(!hay.contains(SECRET));
        assert!(!hay.contains(&SECRET[..8]));
        // The length IS recorded — enough to debug an empty secret.
        assert_eq!(audit_rows(&db)[0].3, SECRET.len() as i64);

        // The in-memory `:secrets` log line is value-free too.
        audit(&rec);
        assert!(recent_log().iter().all(|l| !l.contains(SECRET)));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn registered_for_redaction() {
        // AC6: a successful resolution registers the value with the redaction
        // registry (TASK-947 shim) before the child is spawned.
        const VALUE: &str = "shim-registered-secret-value";
        let policy = open_policy();
        let argv = args("pr list");
        let ctx = SpawnCtx {
            program: "gh",
            args: &argv,
            session_id: "sess-redact",
            policy: &policy,
        };
        let out = ctx
            .materialise("ATUM_API_KEY", "aish", || Some(VALUE.to_string()))
            .expect("gh is a known consumer");
        assert_eq!(out.as_deref(), Some(VALUE));
        assert!(is_registered_for_redaction(VALUE));
        assert!(registered_redaction_labels().contains(&"ATUM_API_KEY".to_string()));

        // A refusal registers nothing and never calls the lookup.
        const NEVER: &str = "never-resolved-secret-value";
        let ctx2 = SpawnCtx {
            program: "env",
            args: &[],
            session_id: "sess-redact",
            policy: &policy,
        };
        let err = ctx2
            .materialise("ATUM_API_KEY", "aish", || {
                panic!("lookup must not run for a refused ref")
            })
            .expect_err("env must be refused");
        assert!(err.contains("would print the secret"));
        assert!(!is_registered_for_redaction(NEVER));
    }

    #[test]
    fn argv_hash_is_stable_and_value_free() {
        let a = args("pr list --limit 5");
        assert_eq!(argv_hash("gh", &a), argv_hash("gh", &a));
        assert_ne!(argv_hash("gh", &a), argv_hash("gh", &args("pr list")));
        assert_eq!(argv_hash("gh", &a).len(), 16);
    }

    #[test]
    fn assignment_and_operand_detection() {
        assert!(is_assignment("FOO=bar"));
        assert!(is_assignment("FOO_1=bar=baz"));
        assert!(!is_assignment("=bar"));
        assert!(!is_assignment("not-an-assignment"));
        assert!(is_numeric_operand("5"));
        assert!(is_numeric_operand("1.5s"));
        assert!(!is_numeric_operand("sh"));
    }
}
