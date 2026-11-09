//! Tool-call failure & fallback telemetry (ISS: repair-heuristic instrumentation).
//!
//! aish's operating doctrine is "try ONE smart fix, then report" — but until
//! now there was no *data* on which tools actually fail, what class of error
//! they fail with, or whether a retry (the "one smart fix") actually recovered.
//! This module is that missing feedback loop.
//!
//! For every executed tool call the engine calls [`record`], which:
//!   1. classifies the error (when the call failed) into a coarse [`ErrorClass`]
//!      (timeout, rate-limit, auth, not-found, …) by substring-matching the
//!      result text — no regex, no allocation beyond a lowercase copy;
//!   2. detects a **retry**: a call to a tool that most recently *failed* in
//!      this session. If that retry succeeds it's marked **recovered** — the
//!      fix worked; if it fails again it stays unrecovered;
//!   3. appends one row to the `tool_telemetry` SQLite table (best-effort — a
//!      telemetry write NEVER sinks a real turn).
//!
//! `:telemetry` then aggregates the table into the two questions that actually
//! tune the repair heuristic:
//!   • which tools fail most, and with what error class?
//!   • per (tool, error-class), what fraction of retries recover? — i.e. is this
//!     error worth retrying, or should it escalate immediately?
//!
//! The retry state (last-unresolved-failure per tool) lives in
//! [`crate::session::Session::tool_failures`] — session-local, never persisted;
//! only the aggregatable event rows land in SQLite.

use crate::backend::ToolResult;
use crate::session::Session;

// -- Batching configuration (TASK-249 / FR-305) ---------------------------
//
// Tool-heavy turns previously issued one INSERT (one transaction, one fsync)
// per tool call. The Session now holds a small ring buffer; `record` appends to
// it and the buffer is flushed as ONE transaction when it fills, when the flush
// interval elapses, or on session Drop. The knobs below are resolved from the
// environment ONCE at `Session::new` (so a live turn does no env lookups) and
// cached on the Session. Parsing is factored into pure `parse_*` helpers so it
// can be unit-tested without mutating process env.

/// Default ring-buffer capacity: flush after this many buffered events.
pub const DEFAULT_BATCH_SIZE: usize = 20;
/// Default flush interval: buffered events at most this many seconds stale.
pub const DEFAULT_FLUSH_SECS: u64 = 5;

/// Parse `AISH_TELEMETRY_BATCH_SIZE`. A positive integer sets the buffer
/// capacity; anything unset/unparseable/zero falls back to the default (0 would
/// mean "never flush on size", which is a footgun — coerced to the default).
pub fn parse_batch_size(v: Option<&str>) -> usize {
    v.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_BATCH_SIZE)
}

/// Parse `AISH_TELEMETRY_FLUSH_SECS`. `0` is honoured verbatim (flush on the
/// very next record — effectively no time-buffering); unset/unparseable falls
/// back to the default.
pub fn parse_flush_secs(v: Option<&str>) -> u64 {
    v.and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_FLUSH_SECS)
}

/// Default `:telemetry` aggregation cache TTL (seconds). Repeated `:telemetry`
/// within this window is served from the Session snapshot instead of re-running
/// the GROUP BY scans (TASK-252 / FR-305).
pub const DEFAULT_CACHE_SECS: u64 = 60;

/// Parse `AISH_TELEMETRY_CACHE_SECS`. `0` is honoured verbatim (disables the
/// cache — every `:telemetry` re-queries); unset/unparseable falls back to the
/// default.
pub fn parse_cache_secs(v: Option<&str>) -> u64 {
    v.and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_SECS)
}

/// Parse `AISH_TELEMETRY_UNBUFFERED`. Truthy (`1`/`true`/`yes`/`on`, case-
/// insensitive) restores the legacy per-call insert path — every `record`
/// flushes immediately. Anything else keeps buffering on.
pub fn parse_unbuffered(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// A coarse bucket for a failed tool call. Deliberately small: the point is to
/// spot *patterns* ("atum_list_* timeouts recover 80% of the time"), not to
/// preserve every error string. Ordered most-specific-first in [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Operation exceeded a deadline / was killed by a timeout.
    Timeout,
    /// 429 / "rate limit" / "too many requests" — provider throttling.
    RateLimit,
    /// 401 / 403 / unauthorized / forbidden / bad credentials.
    Auth,
    /// 404 / "not found" / "no such file" / "does not exist".
    NotFound,
    /// 409 / "conflict" / "already exists".
    Conflict,
    /// Local OS-level permission denial (EACCES) distinct from remote Auth.
    Permission,
    /// Connection refused/reset, DNS failure, broken pipe — transport layer.
    Network,
    /// 400 / 422 / "invalid" / "missing required" / bad usage — caller error.
    InvalidArgs,
    /// User (or a hook) declined / cancelled the call.
    Declined,
    /// Anything we couldn't bucket.
    Other,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::Timeout => "timeout",
            ErrorClass::RateLimit => "rate-limit",
            ErrorClass::Auth => "auth",
            ErrorClass::NotFound => "not-found",
            ErrorClass::Conflict => "conflict",
            ErrorClass::Permission => "permission",
            ErrorClass::Network => "network",
            ErrorClass::InvalidArgs => "invalid-args",
            ErrorClass::Declined => "declined",
            ErrorClass::Other => "other",
        }
    }
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Bucket an error result's text into an [`ErrorClass`]. Case-insensitive
/// substring match, most-specific classes first so e.g. a 429 is `RateLimit`
/// rather than the more generic `Auth`/`InvalidArgs`.
pub fn classify(content: &str) -> ErrorClass {
    let c = content.to_ascii_lowercase();
    let has = |needle: &str| c.contains(needle);

    if has("timed out") || has("timeout") || has("deadline exceeded") || has("etimedout") {
        return ErrorClass::Timeout;
    }
    if has("rate limit")
        || has("rate-limit")
        || has("429")
        || has("too many requests")
        || has("throttl")
    {
        return ErrorClass::RateLimit;
    }
    if has(" 401")
        || has(" 403")
        || has("unauthorized")
        || has("forbidden")
        || has("authentication")
        || has("bad credentials")
        || has("invalid token")
        || has("not authorized")
        || has("token authentication required")
        || has("requires authentication")
        || has("authentication required")
    {
        return ErrorClass::Auth;
    }
    if has(" 404")
        || has("not found")
        || has("no such file")
        || has("does not exist")
        || has("doesn't exist")
        || has("enoent")
    {
        return ErrorClass::NotFound;
    }
    if has(" 409")
        || has("conflict")
        || has("already exists")
        || has("pull request already exists")
        || has("reference already exists")
        || has("branch already exists")
    {
        return ErrorClass::Conflict;
    }
    if has("permission denied")
        || has("eacces")
        || has("operation not permitted")
        || has("access denied")
    {
        return ErrorClass::Permission;
    }
    if has("is a directory") || has("isadirectory") || has("illegal operation on a directory") {
        return ErrorClass::InvalidArgs;
    }
    if has("connection refused")
        || has("connection reset")
        || has("connection closed")
        || has("broken pipe")
        || has("could not resolve")
        || has("dns")
        || has("econnrefused")
        || has("network is unreachable")
        || has("network error")
    {
        return ErrorClass::Network;
    }
    if has(" 400")
        || has(" 422")
        || has("invalid")
        || has("missing required")
        || has("unrecognized")
        || has("bad request")
        || has("unexpected argument")
        || has("usage:")
    {
        return ErrorClass::InvalidArgs;
    }
    if has("declined") || has("cancelled") || has("canceled") || has("denied by") {
        return ErrorClass::Declined;
    }
    ErrorClass::Other
}

/// One row to persist for a completed tool call.
#[derive(Debug, Clone)]
pub struct ToolEvent {
    pub tool: String,
    pub is_error: bool,
    /// The classified error bucket, or `None` on success.
    pub error_class: Option<String>,
    /// True when this call follows a still-unresolved failure of the same tool.
    pub is_retry: bool,
    /// True when this call was a retry AND it succeeded (the fix worked).
    pub recovered: bool,
    /// The error class this retry was recovering from (present iff `is_retry`).
    pub prev_class: Option<String>,
    pub session_id: String,
}

/// Per-tool call totals — the top-level "what fails most" table.
#[derive(Debug, Clone)]
pub struct ToolTotals {
    pub tool: String,
    pub calls: i64,
    pub failures: i64,
}

/// Per-(tool, error-class) failure counts — the "top error" breakdown.
#[derive(Debug, Clone)]
pub struct ClassFailure {
    pub tool: String,
    pub class: String,
    pub count: i64,
}

/// Per-(tool, error-class) retry-recovery stats — the heuristic-tuning table:
/// "of the times a `tool` retried after a `prev_class` failure, how many
/// recovered?".
#[derive(Debug, Clone)]
pub struct RetryStat {
    pub tool: String,
    pub prev_class: String,
    pub retries: i64,
    pub recovered: i64,
}

/// A pre-aggregated `:telemetry` snapshot cached on the [`Session`] (TASK-252 /
/// FR-305). Holds the four values the report is rendered from plus the instant
/// it was computed, so a repeated `:telemetry` inside the cache window skips the
/// GROUP BY scans entirely. Cloned out on a hit; cheap (a handful of small rows).
#[derive(Debug, Clone)]
pub struct TelemetryCache {
    /// When this snapshot was computed — drives the TTL freshness check.
    pub cached_at: std::time::Instant,
    /// Total tool-call rows logged (`db.tool_telemetry_count`).
    pub total: i64,
    /// Per-tool call/failure totals.
    pub totals: Vec<ToolTotals>,
    /// Per-(tool, error-class) failure counts.
    pub class_failures: Vec<ClassFailure>,
    /// Per-(tool, prev-class) retry-recovery stats.
    pub retries: Vec<RetryStat>,
}

/// Return the aggregated `:telemetry` snapshot, served from the Session cache
/// when it is fresh — age `< session.tool_telemetry_cache_secs` and present —
/// otherwise re-run the GROUP BY queries, cache the result, and return it.
/// Returns `None` when no persistent store is attached (nothing to aggregate).
///
/// A `cache_secs` of `0` makes every call a miss (the cache is effectively
/// disabled), and a freshly recorded tool call clears the cache in [`record`]
/// for exact invalidation, so a stale window can only ever elapse when NO new
/// tool calls happened — exactly when the numbers wouldn't have changed anyway.
pub fn aggregate_cached(session: &mut Session) -> Option<TelemetryCache> {
    // No store ⇒ nothing to aggregate.
    session.db.as_ref()?;

    // Fresh cache hit: serve the snapshot, no DB scan.
    if let Some(c) = &session.tool_telemetry_cache
        && c.cached_at.elapsed() < session.tool_telemetry_cache_secs
    {
        return Some(c.clone());
    }

    // Miss (empty, stale, or TTL=0): re-aggregate and repopulate the cache.
    let db = session.db.as_ref().unwrap();
    let snap = TelemetryCache {
        cached_at: std::time::Instant::now(),
        total: db.tool_telemetry_count().unwrap_or(0),
        totals: db.tool_telemetry_totals().unwrap_or_default(),
        class_failures: db.tool_telemetry_class_failures().unwrap_or_default(),
        retries: db.tool_telemetry_retry_stats().unwrap_or_default(),
    };
    session.tool_telemetry_cache = Some(snap.clone());
    Some(snap)
}

/// Record one completed tool call. Best-effort: updates the session's
/// Record one completed tool call. Best-effort: updates the session's
/// last-unresolved-failure map (for retry detection) and appends the event to
/// the session ring buffer, flushing to SQLite as one transaction when the
/// buffer fills or the flush interval elapses. A DB write failure is swallowed —
/// telemetry must never break a real turn. No-op when no persistent store is
/// attached. Set `AISH_TELEMETRY_UNBUFFERED=1` to restore per-call inserts.
pub fn record(session: &mut Session, tool: &str, result: &ToolResult) {
    let prev = session.tool_failures.get(tool).cloned();
    let is_retry = prev.is_some();

    let error_class = if result.is_error {
        Some(classify(&result.content).as_str().to_string())
    } else {
        None
    };
    let recovered = is_retry && !result.is_error;

    // Update the pending-failure map: a failure (re)arms it, a success clears it.
    // This is session-local retry state and must update whether or not a store
    // is attached, so it happens BEFORE the early return.
    if let Some(cls) = &error_class {
        session.tool_failures.insert(tool.to_string(), cls.clone());
    } else {
        session.tool_failures.remove(tool);
    }

    // TASK-252: a freshly recorded tool call makes any cached `:telemetry`
    // aggregate stale — invalidate it exactly here (the loose TTL in
    // `aggregate_cached` is only the backstop). Cheap and store-independent.
    session.tool_telemetry_cache = None;

    // No store ⇒ nothing to persist or buffer.
    if session.db.is_none() {
        return;
    }

    let ev = ToolEvent {
        tool: tool.to_string(),
        is_error: result.is_error,
        error_class,
        is_retry,
        recovered,
        prev_class: prev,
        session_id: session.session_id.clone(),
    };

    persist(session, ev);
}

/// Buffer (or immediately insert) one already-built row — the shared tail of
/// every telemetry writer. Factored out of [`record`] so [`record_gate`] goes
/// through the SAME batching, unbuffered-override and flush-timer path rather
/// than opening a second way for rows to reach the table.
fn persist(session: &mut Session, ev: ToolEvent) {
    // Legacy per-call insert path: persist immediately, bypassing the buffer, so
    // behaviour is byte-for-byte the pre-batching one.
    if session.tool_telemetry_unbuffered {
        if let Some(db) = session.db.as_ref() {
            let _ = db.record_tool_event(&ev);
        }
        return;
    }

    session.tool_telemetry_buf.push(ev);

    // Flush when the buffer reaches capacity or the flush interval has elapsed
    // since the last flush. The timer is checked here (piggy-backing on tool
    // activity) rather than on a background task — the tail is caught by the
    // Drop flush at shutdown.
    let full = session.tool_telemetry_buf.len() >= session.tool_telemetry_batch_size;
    let timed = session.tool_telemetry_last_flush.elapsed() >= session.tool_telemetry_flush;
    if full || timed {
        flush(session);
    }
}

/// `error_class` for a gate that stopped the call outright.
pub const CLASS_GATE_REFUSE: &str = "gate-refuse";
/// `error_class` for a gate that stopped to ask first.
pub const CLASS_GATE_CONFIRM: &str = "gate-confirm";

/// Record one GATE decision (TASK-943 / SEC-2.1). The table stores no argv —
/// deliberately, it never should — which is also why there was previously no
/// way to answer "which rule refuses what, and how often?". That question has
/// to be answerable before a seed allowlist can be tuned against real traffic
/// (ISS-410409), so the two things that are safe to keep are recorded instead:
/// the name of the RULE that fired and the normalised `bin[:subcommand]` — both
/// bounded, low-cardinality tokens, never a path or a flag value (see
/// `bin_subcommand` in `tools.rs`).
///
/// `tool` is written as `gate:<rule>:<bin[:subcommand]>` so the existing
/// per-tool aggregates bucket it without a schema change. A refusal sets
/// `is_error` (the call was stopped); a confirm does not (the call goes on to
/// ask), and both are findable by `error_class`.
pub fn record_gate(session: &mut Session, class: &str, rule: &str, bin_sub: &str) {
    // Keep the `:telemetry` aggregate honest, exactly as `record` does.
    session.tool_telemetry_cache = None;

    // No store ⇒ nothing to persist or buffer.
    if session.db.is_none() {
        return;
    }

    persist(
        session,
        ToolEvent {
            tool: format!("gate:{rule}:{bin_sub}"),
            is_error: class == CLASS_GATE_REFUSE,
            error_class: Some(class.to_string()),
            is_retry: false,
            recovered: false,
            prev_class: None,
            session_id: session.session_id.clone(),
        },
    );
}

// -- Not-found retry guard (stop re-running calls that cannot succeed) ----
//
// Telemetry from a real 4-hour window: ~100 tool errors, ALL classified
// `not-found`, 35 of them a `run_program` retry of the SAME missing
// path/binary, and ZERO recoveries. Retrying a miss on an unchanged target is
// structurally unrecoverable — the turn is spent re-proving the target does not
// exist. This guard remembers, per SESSION and per TARGET (not per tool — too
// coarse), which exact calls already failed `not-found` and short-circuits an
// IDENTICAL repeat with a result that steers the model to discovery
// (`list_dir` / `glob_expand` / `grep_files`) instead.
//
// Properties (deliberate, and each one unit-tested below):
//   • only `not-found` arms the guard — no other class is ever blocked;
//   • only the identical (tool, target) pair is blocked; a different path runs;
//   • a later SUCCESS on that target disarms it (the file appeared / was built);
//   • bounded: at most `NOTFOUND_TARGETS_CAP` keys, each ≤ `NOTFOUND_KEY_MAX`
//     bytes, FIFO-evicted — a long session cannot grow it without limit;
//   • fail-open: an unknown tool shape, or args that yield no key, just runs;
//   • defeatable with `AISH_NOTFOUND_GUARD=0`.
//
// A blocked retry is recorded through the EXISTING gate-refusal telemetry shape
// (`record_gate` + [`CLASS_GATE_REFUSE`]) so it shows up in `:telemetry` as
// `gate:notfound-retry:<tool>` rather than being invisible.

/// Guard default: on. `AISH_NOTFOUND_GUARD=0` turns it off.
pub const DEFAULT_NOTFOUND_GUARD: bool = true;
/// Max distinct not-found targets remembered per session (FIFO beyond this).
pub const NOTFOUND_TARGETS_CAP: usize = 64;
/// Max bytes of a single target key (truncated on a char boundary).
pub const NOTFOUND_KEY_MAX: usize = 512;
/// `rule` token for the gate row a blocked retry writes.
pub const NOTFOUND_GUARD_RULE: &str = "notfound-retry";

/// Parse `AISH_NOTFOUND_GUARD`. Falsy (`0`/`false`/`no`/`off`, case-insensitive)
/// disables the guard; unset/empty/anything else keeps the default (on), so the
/// knob can only ever be used to opt OUT — never to silently opt in to a
/// different behaviour.
pub fn parse_notfound_guard(v: Option<&str>) -> bool {
    match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("0" | "false" | "no" | "off") => false,
        _ => DEFAULT_NOTFOUND_GUARD,
    }
}

/// Derive a stable per-TARGET key for the not-found guard, or `None` when this
/// tool shape has no meaningful target (⇒ the call is never guarded — fail-open).
///
/// Shapes handled:
///   • `run_program` → program + argv (the whole command identity: a different
///     flag is a different call and must be allowed to run);
///   • `read_file` / `list_dir` / `stat_file` → `path`;
///   • `glob_expand` → `pattern` (plus `path` when the caller scoped it).
///
/// `grep_files` is intentionally NOT guarded: it is one of the discovery tools
/// the steer points AT, so blocking it could dead-end the recovery path.
pub fn notfound_target_key(tool: &str, args: &serde_json::Value) -> Option<String> {
    let str_arg = |k: &str| -> Option<String> {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let target = match tool {
        "run_program" => {
            let mut t = str_arg("program")?;
            if let Some(arr) = args.get("args").and_then(|v| v.as_array()) {
                for a in arr {
                    t.push('\u{1f}');
                    match a.as_str() {
                        Some(s) => t.push_str(s.trim()),
                        // Non-string argv entry: stringify rather than bail, so
                        // a sloppy call still gets a stable key.
                        None => t.push_str(&a.to_string()),
                    }
                }
            }
            t
        }
        "read_file" | "list_dir" | "stat_file" => str_arg("path")?,
        "glob_expand" => {
            let pat = str_arg("pattern")?;
            match str_arg("path") {
                Some(base) => format!("{base}\u{1f}{pat}"),
                None => pat,
            }
        }
        // Unknown/unguarded tool shape ⇒ no key ⇒ always executes.
        _ => return None,
    };

    let mut key = format!("{tool}\u{1e}{target}");
    if key.len() > NOTFOUND_KEY_MAX {
        let mut cut = NOTFOUND_KEY_MAX;
        while cut > 0 && !key.is_char_boundary(cut) {
            cut -= 1;
        }
        key.truncate(cut);
    }
    Some(key)
}

/// Session-local set of targets that already failed `not-found`. A bounded FIFO
/// (insertion-ordered, oldest evicted at [`NOTFOUND_TARGETS_CAP`]) — small
/// enough that a linear `contains` is cheaper than hashing, and incapable of
/// unbounded growth. Never persisted.
#[derive(Debug, Clone)]
pub struct NotFoundGuard {
    enabled: bool,
    seen: std::collections::VecDeque<String>,
}

impl Default for NotFoundGuard {
    fn default() -> Self {
        Self::new(DEFAULT_NOTFOUND_GUARD)
    }
}

impl NotFoundGuard {
    /// Build a guard with the enablement resolved once (at `Session::new`), so
    /// a live turn does no env lookups.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            seen: std::collections::VecDeque::new(),
        }
    }

    /// Whether the guard is active at all (`AISH_NOTFOUND_GUARD`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Number of remembered targets (≤ [`NOTFOUND_TARGETS_CAP`]). Test-only:
    /// the bounded-eviction test is the sole reader, and a `pub fn` with no
    /// production caller trips CI's `-D dead_code`.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// True when nothing is remembered. Test-only — see [`Self::len`].
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Has this exact target already failed `not-found` this session?
    pub fn is_armed(&self, key: &str) -> bool {
        self.seen.iter().any(|k| k == key)
    }

    /// Remember a target that failed `not-found`. Idempotent; evicts the oldest
    /// entry once the cap is reached.
    pub fn arm(&mut self, key: &str) {
        if self.is_armed(key) {
            return;
        }
        while self.seen.len() >= NOTFOUND_TARGETS_CAP {
            self.seen.pop_front();
        }
        self.seen.push_back(key.to_string());
    }

    /// Forget a target (it resolved — the path appeared, the binary installed).
    pub fn clear(&mut self, key: &str) {
        self.seen.retain(|k| k != key);
    }
}

/// The steering result the model gets INSTEAD of a re-run of a known miss.
/// Names the three discovery tools explicitly — the whole point is to convert a
/// wasted retry into a locate step. Pure so it can be asserted on in tests.
pub fn notfound_block_text(desc: &str) -> String {
    format!(
        "aish: this exact call already failed not-found this session \
         ({desc}) — it was NOT re-run, because an identical call on an unchanged \
         target cannot succeed. Locate the real path/binary first with list_dir, \
         glob_expand, or grep_files, then call again with the corrected target \
         (or say plainly that it does not exist)."
    )
}

/// Pre-execution check: should this call be blocked as a known not-found
/// repeat? Returns `Some(steering_text)` to block (and records the refusal as a
/// gate row so it is visible in `:telemetry`), `None` to execute normally.
/// Fail-open on every unknown shape, and a no-op when the guard is disabled.
pub fn notfound_guard_check(
    session: &mut Session,
    tool: &str,
    args: &serde_json::Value,
) -> Option<String> {
    if !session.notfound_guard.enabled() {
        return None;
    }
    let key = notfound_target_key(tool, args)?;
    if !session.notfound_guard.is_armed(&key) {
        return None;
    }
    // Observable, bounded, argv-free: `gate:notfound-retry:<tool>`.
    record_gate(session, CLASS_GATE_REFUSE, NOTFOUND_GUARD_RULE, tool);
    Some(notfound_block_text(&describe_target(tool, args)))
}

/// Human-readable target for the steer text — the tool plus its concrete
/// path/program, with no key separators leaking through.
fn describe_target(tool: &str, args: &serde_json::Value) -> String {
    notfound_target_key(tool, args)
        .map(|k| k.replace('\u{1e}', ": ").replace('\u{1f}', " "))
        .unwrap_or_else(|| tool.to_string())
}

/// Post-execution bookkeeping for the guard: a `not-found` failure ARMS this
/// target, a success CLEARS it, and every other error class is left untouched
/// (so a timeout or a permission error is still freely retryable). Called from
/// the engine right after [`record`], which has the args this needs.
pub fn note_notfound_outcome(
    session: &mut Session,
    tool: &str,
    args: &serde_json::Value,
    result: &ToolResult,
) {
    if !session.notfound_guard.enabled() {
        return;
    }
    let Some(key) = notfound_target_key(tool, args) else {
        return;
    };
    if !result.is_error {
        session.notfound_guard.clear(&key);
    } else if classify(&result.content) == ErrorClass::NotFound {
        session.notfound_guard.arm(&key);
    }
}

/// Drain the session's tool-telemetry ring buffer to SQLite as ONE transaction.
/// Best-effort: a write failure is swallowed (the buffer is still cleared so a
/// persistent DB error can't wedge an ever-growing buffer). No-op when the
/// buffer is empty or no store is attached. Called on buffer-full, on the flush
/// timer, at graceful shutdown, and from `Session`'s `Drop`.
pub fn flush(session: &mut Session) {
    if session.tool_telemetry_buf.is_empty() {
        return;
    }
    if let Some(db) = session.db.as_ref() {
        let _ = db.record_tool_events_batch(&session.tool_telemetry_buf);
    }
    session.tool_telemetry_buf.clear();
    session.tool_telemetry_last_flush = std::time::Instant::now();
}

/// Render the `:telemetry` report from the aggregated tables. Pure so it can be
/// unit-tested without a DB.
pub fn render_report(
    total: i64,
    totals: &[ToolTotals],
    class_failures: &[ClassFailure],
    retries: &[RetryStat],
) -> String {
    if total == 0 {
        return "no tool-call telemetry recorded yet — it accrues as tools run".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!("tool-call telemetry · {total} calls logged\n\n"));

    // "top error" per tool: pick the highest-count class for each tool.
    let top_error = |tool: &str| -> String {
        class_failures
            .iter()
            .filter(|c| c.tool == tool)
            .max_by_key(|c| c.count)
            .map(|c| format!("{}({})", c.class, c.count))
            .unwrap_or_else(|| "-".to_string())
    };

    // Totals table, worst failure-rate first.
    let mut totals: Vec<&ToolTotals> = totals.iter().collect();
    totals.sort_by(|a, b| {
        let ra = fail_rate(a.failures, a.calls);
        let rb = fail_rate(b.failures, b.calls);
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.failures.cmp(&a.failures))
    });
    out.push_str(&format!(
        "{:<28} {:>6} {:>5} {:>6}  {}\n",
        "TOOL", "CALLS", "FAIL", "FAIL%", "TOP ERROR"
    ));
    for t in &totals {
        out.push_str(&format!(
            "{:<28} {:>6} {:>5} {:>5.0}%  {}\n",
            truncate(&t.tool, 28),
            t.calls,
            t.failures,
            fail_rate(t.failures, t.calls) * 100.0,
            if t.failures > 0 {
                top_error(&t.tool)
            } else {
                "-".to_string()
            },
        ));
    }

    // Retry / fallback recovery table.
    if !retries.is_empty() {
        out.push_str("\nretry / fallback recovery (did the one smart fix work?)\n");
        out.push_str(&format!(
            "{:<24} {:<14} {:>7} {:>9} {:>6}\n",
            "TOOL", "ERROR CLASS", "RETRIES", "RECOVERED", "RATE"
        ));
        let mut retries: Vec<&RetryStat> = retries.iter().collect();
        retries.sort_by_key(|r| std::cmp::Reverse(r.retries));
        for r in retries {
            out.push_str(&format!(
                "{:<24} {:<14} {:>7} {:>9} {:>5.0}%\n",
                truncate(&r.tool, 24),
                truncate(&r.prev_class, 14),
                r.retries,
                r.recovered,
                fail_rate(r.recovered, r.retries) * 100.0,
            ));
        }
        out.push_str(
            "\ntip: high recovery% ⇒ worth an auto-retry; low ⇒ escalate/refresh creds instead\n",
        );
    }
    out
}

fn fail_rate(num: i64, denom: i64) -> f64 {
    if denom == 0 {
        0.0
    } else {
        num as f64 / denom as f64
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        format!("{}…", s.chars().take(keep).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_buckets_common_errors() {
        assert_eq!(
            classify("Error: request timed out after 120s"),
            ErrorClass::Timeout
        );
        assert_eq!(
            classify("HTTP 429 Too Many Requests"),
            ErrorClass::RateLimit
        );
        assert_eq!(
            classify("GitHub API returned 403 Forbidden"),
            ErrorClass::Auth
        );
        assert_eq!(
            classify("fatal: repository not found (404)"),
            ErrorClass::NotFound
        );
        assert_eq!(
            classify("refusing: destination already exists"),
            ErrorClass::Conflict
        );
        assert_eq!(
            classify("open /etc/shadow: permission denied"),
            ErrorClass::Permission
        );
        assert_eq!(
            classify("dial tcp: connection refused"),
            ErrorClass::Network
        );
        assert_eq!(
            classify("400 Bad Request: missing required field"),
            ErrorClass::InvalidArgs
        );
        assert_eq!(classify("declined by user"), ErrorClass::Declined);
        assert_eq!(classify("something weird happened"), ErrorClass::Other);
    }

    #[test]
    fn rate_limit_wins_over_auth_on_429() {
        // A 429 body sometimes also mentions auth; rate-limit is more specific.
        assert_eq!(
            classify("429 rate limit exceeded; unauthorized retry"),
            ErrorClass::RateLimit
        );
    }

    #[test]
    fn render_report_empty_is_friendly() {
        let s = render_report(0, &[], &[], &[]);
        assert!(s.contains("no tool-call telemetry"));
    }

    #[test]
    fn render_report_orders_by_fail_rate_and_shows_recovery() {
        let totals = vec![
            ToolTotals {
                tool: "run_program".into(),
                calls: 100,
                failures: 2,
            },
            ToolTotals {
                tool: "atum_list_tasks".into(),
                calls: 10,
                failures: 8,
            },
        ];
        let class_failures = vec![
            ClassFailure {
                tool: "atum_list_tasks".into(),
                class: "timeout".into(),
                count: 7,
            },
            ClassFailure {
                tool: "atum_list_tasks".into(),
                class: "other".into(),
                count: 1,
            },
            ClassFailure {
                tool: "run_program".into(),
                class: "not-found".into(),
                count: 2,
            },
        ];
        let retries = vec![RetryStat {
            tool: "atum_list_tasks".into(),
            prev_class: "timeout".into(),
            retries: 10,
            recovered: 8,
        }];
        let s = render_report(112, &totals, &class_failures, &retries);
        // worst fail-rate (atum_list_tasks 80%) appears before run_program.
        let a = s.find("atum_list_tasks").unwrap();
        let b = s.find("run_program").unwrap();
        assert!(a < b, "high failure-rate tool should sort first");
        assert!(s.contains("timeout(7)"));
        assert!(s.contains("RECOVERED"));
        // 8/10 recovered ⇒ 80%.
        assert!(s.contains("80%"));
    }

    // -- batching (TASK-249 / FR-305) -------------------------------------

    #[test]
    fn parse_helpers_honor_env_strings() {
        assert_eq!(parse_batch_size(None), DEFAULT_BATCH_SIZE);
        assert_eq!(parse_batch_size(Some("50")), 50);
        assert_eq!(parse_batch_size(Some(" 8 ")), 8);
        // 0 is a footgun ("never flush on size") ⇒ coerced to default.
        assert_eq!(parse_batch_size(Some("0")), DEFAULT_BATCH_SIZE);
        assert_eq!(parse_batch_size(Some("nope")), DEFAULT_BATCH_SIZE);

        assert_eq!(parse_flush_secs(None), DEFAULT_FLUSH_SECS);
        assert_eq!(parse_flush_secs(Some("0")), 0); // honoured verbatim
        assert_eq!(parse_flush_secs(Some("30")), 30);
        assert_eq!(parse_flush_secs(Some("x")), DEFAULT_FLUSH_SECS);

        assert!(parse_unbuffered(Some("1")));
        assert!(parse_unbuffered(Some("TRUE")));
        assert!(parse_unbuffered(Some("yes")));
        assert!(parse_unbuffered(Some("On")));
        assert!(!parse_unbuffered(Some("0")));
        assert!(!parse_unbuffered(Some("")));
        assert!(!parse_unbuffered(None));
    }

    fn tele_tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "aish_tele_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tele_session(dir: &std::path::Path) -> Session {
        let mut s = Session::new().unwrap();
        s.db = Some(crate::db::Db::open(&dir.join("t.db")).unwrap());
        s.tool_telemetry_buf.clear();
        s.tool_telemetry_unbuffered = false;
        s.tool_telemetry_flush = std::time::Duration::from_secs(3600); // disable timer
        s.tool_telemetry_last_flush = std::time::Instant::now();
        s
    }

    #[test]
    fn record_buffers_then_flushes_as_one_batch() {
        let dir = tele_tmp("buf");
        let mut s = tele_session(&dir);
        s.tool_telemetry_batch_size = 3;

        let ok = ToolResult::text("t", "done", false);
        record(&mut s, "run_program", &ok);
        record(&mut s, "run_program", &ok);
        // Two buffered, nothing persisted yet (batch not full, timer disabled).
        assert_eq!(s.tool_telemetry_buf.len(), 2);
        assert_eq!(s.db.as_ref().unwrap().tool_telemetry_count().unwrap(), 0);

        // Third hits capacity → a single batched flush of all three.
        record(&mut s, "run_program", &ok);
        assert!(s.tool_telemetry_buf.is_empty());
        assert_eq!(s.db.as_ref().unwrap().tool_telemetry_count().unwrap(), 3);
    }

    #[test]
    fn high_throughput_collapses_transactions() {
        // 100 tool calls with batch 20 ⇒ 5 flushes, all 100 rows persisted.
        let dir = tele_tmp("hot");
        let mut s = tele_session(&dir);
        s.tool_telemetry_batch_size = 20;
        let ok = ToolResult::text("t", "done", false);
        for _ in 0..100 {
            record(&mut s, "run_program", &ok);
        }
        // 100 is a multiple of 20 ⇒ buffer drained exactly, nothing stranded.
        assert!(s.tool_telemetry_buf.is_empty());
        assert_eq!(s.db.as_ref().unwrap().tool_telemetry_count().unwrap(), 100);
    }

    #[test]
    fn batching_collapses_transactions_at_scale() {
        // AC#2: reduced transaction count at scale. With batch=20, a run of 1000
        // tool calls persists in batch-sized steps — proving each 20-event window
        // is ONE store transaction (≈50 total), not 1000 per-event writes.
        let dir = tele_tmp("scale_tx");
        let mut s = tele_session(&dir);
        s.tool_telemetry_batch_size = 20;
        let ok = ToolResult::text("t", "done", false);
        for _ in 0..19 {
            record(&mut s, "run_program", &ok);
        }
        assert_eq!(
            s.db.as_ref().unwrap().tool_telemetry_count().unwrap(),
            0,
            "19 events collapsed into 0 transactions (still buffered)"
        );
        record(&mut s, "run_program", &ok);
        assert_eq!(s.db.as_ref().unwrap().tool_telemetry_count().unwrap(), 20);
        for n in 21..=1000 {
            record(&mut s, "run_program", &ok);
            let persisted = s.db.as_ref().unwrap().tool_telemetry_count().unwrap();
            assert_eq!(persisted, (n / 20) * 20, "persist tracks batch boundaries");
        }
        assert!(s.tool_telemetry_buf.is_empty());
        assert_eq!(s.db.as_ref().unwrap().tool_telemetry_count().unwrap(), 1000);
    }

    #[test]
    fn drop_flushes_buffered_tail() {
        let dir = tele_tmp("drop");
        let dbpath = dir.join("t.db");
        {
            let mut s = tele_session(&dir);
            s.tool_telemetry_batch_size = 100; // never reached
            let ok = ToolResult::text("t", "done", false);
            record(&mut s, "glob_expand", &ok);
            record(&mut s, "glob_expand", &ok);
            assert_eq!(s.tool_telemetry_buf.len(), 2);
            // `s` drops here → Drop for Session flushes the tail.
        }
        let db2 = crate::db::Db::open(&dbpath).unwrap();
        assert_eq!(db2.tool_telemetry_count().unwrap(), 2);
    }

    #[test]
    fn unbuffered_mode_persists_immediately() {
        let dir = tele_tmp("unbuf");
        let mut s = tele_session(&dir);
        s.tool_telemetry_unbuffered = true;
        let ok = ToolResult::text("t", "done", false);
        record(&mut s, "read_file", &ok);
        assert!(s.tool_telemetry_buf.is_empty());
        assert_eq!(s.db.as_ref().unwrap().tool_telemetry_count().unwrap(), 1);
    }

    // -- pre-aggregated :telemetry cache (TASK-252 / FR-305) ---------------

    #[test]
    fn parse_cache_secs_defaults_and_overrides() {
        assert_eq!(parse_cache_secs(None), DEFAULT_CACHE_SECS);
        assert_eq!(parse_cache_secs(Some("0")), 0); // honoured — disables cache
        assert_eq!(parse_cache_secs(Some(" 120 ")), 120);
        assert_eq!(parse_cache_secs(Some("nope")), DEFAULT_CACHE_SECS);
    }

    /// A tool event minted directly (bypassing `record`, so it does NOT
    /// invalidate the cache) — used to prove a fresh cache masks new rows.
    fn direct_event(session_id: &str) -> ToolEvent {
        ToolEvent {
            tool: "read_file".into(),
            is_error: false,
            error_class: None,
            is_retry: false,
            recovered: false,
            prev_class: None,
            session_id: session_id.to_string(),
        }
    }

    #[test]
    fn aggregate_cached_serves_fresh_snapshot_within_window() {
        let dir = tele_tmp("cache_hit");
        let mut s = tele_session(&dir);
        s.tool_telemetry_cache_secs = std::time::Duration::from_secs(3600);
        let ok = ToolResult::text("t", "done", false);
        record(&mut s, "read_file", &ok);
        flush(&mut s); // 1 row lands in the DB
        // First read is a miss → queries + caches total=1.
        let a = aggregate_cached(&mut s).unwrap();
        assert_eq!(a.total, 1);
        assert!(s.tool_telemetry_cache.is_some());
        // Insert out-of-band (no `record`, no invalidation). The fresh cache
        // must still serve the old total — that's the whole point of caching.
        let ev = direct_event(&s.session_id);
        s.db.as_ref().unwrap().record_tool_event(&ev).unwrap();
        let b = aggregate_cached(&mut s).unwrap();
        assert_eq!(b.total, 1, "fresh cache should mask the out-of-band insert");
    }

    #[test]
    fn record_invalidates_telemetry_cache_exactly() {
        let dir = tele_tmp("cache_inval");
        let mut s = tele_session(&dir);
        s.tool_telemetry_cache_secs = std::time::Duration::from_secs(3600);
        let ok = ToolResult::text("t", "done", false);
        record(&mut s, "read_file", &ok);
        flush(&mut s);
        assert_eq!(aggregate_cached(&mut s).unwrap().total, 1);
        assert!(s.tool_telemetry_cache.is_some());
        // A newly recorded tool call clears the cache immediately.
        record(&mut s, "read_file", &ok);
        assert!(
            s.tool_telemetry_cache.is_none(),
            "record must invalidate the cache"
        );
        flush(&mut s);
        assert_eq!(
            aggregate_cached(&mut s).unwrap().total,
            2,
            "re-aggregation after invalidation reflects the new row"
        );
    }

    #[test]
    fn cache_secs_zero_disables_the_cache() {
        let dir = tele_tmp("cache_zero");
        let mut s = tele_session(&dir);
        s.tool_telemetry_cache_secs = std::time::Duration::ZERO;
        let ok = ToolResult::text("t", "done", false);
        record(&mut s, "read_file", &ok);
        flush(&mut s);
        assert_eq!(aggregate_cached(&mut s).unwrap().total, 1);
        // TTL=0 ⇒ every call re-queries, so an out-of-band insert is picked up.
        let ev = direct_event(&s.session_id);
        s.db.as_ref().unwrap().record_tool_event(&ev).unwrap();
        assert_eq!(
            aggregate_cached(&mut s).unwrap().total,
            2,
            "TTL=0 must re-query on every :telemetry"
        );
    }

    #[test]
    fn aggregate_cached_none_without_store() {
        let mut s = Session::new().unwrap();
        s.db = None;
        assert!(aggregate_cached(&mut s).is_none());
    }

    #[test]
    fn classify_directory_error() {
        assert_eq!(
            classify("is a directory"),
            ErrorClass::InvalidArgs,
            "should categorize 'is a directory' as InvalidArgs"
        );
        assert_eq!(
            classify("IsADirectory"),
            ErrorClass::InvalidArgs,
            "case-insensitive"
        );
        assert_eq!(
            classify("illegal operation on a directory"),
            ErrorClass::InvalidArgs,
            "should match verbose form"
        );
    }

    #[test]
    fn classify_permission_errors() {
        assert_eq!(classify("permission denied"), ErrorClass::Permission);
        assert_eq!(classify("EACCES"), ErrorClass::Permission);
        assert_eq!(classify("access denied"), ErrorClass::Permission);
        assert_eq!(classify("operation not permitted"), ErrorClass::Permission);
    }

    #[test]
    fn classify_not_found_errors() {
        assert_eq!(classify("no such file"), ErrorClass::NotFound);
        assert_eq!(classify("ENOENT"), ErrorClass::NotFound);
        assert_eq!(classify("does not exist"), ErrorClass::NotFound);
        assert_eq!(classify("not found"), ErrorClass::NotFound);
    }

    // ───────────────────────── not-found retry guard ─────────────────────────

    fn nf_args(pairs: serde_json::Value) -> serde_json::Value {
        pairs
    }

    #[test]
    fn notfound_guard_env_knob_parse() {
        // Unset / empty / junk ⇒ default (on). The knob is opt-OUT only.
        assert!(parse_notfound_guard(None));
        assert!(parse_notfound_guard(Some("")));
        assert!(parse_notfound_guard(Some("1")));
        assert!(parse_notfound_guard(Some("yes")));
        assert!(parse_notfound_guard(Some("banana")));
        // Falsy spellings disable it, case/whitespace insensitive.
        assert!(!parse_notfound_guard(Some("0")));
        assert!(!parse_notfound_guard(Some("false")));
        assert!(!parse_notfound_guard(Some("No")));
        assert!(!parse_notfound_guard(Some(" OFF ")));
    }

    #[test]
    fn notfound_key_derivation_per_tool_shape() {
        // run_program: program + argv is the command identity.
        let a = notfound_target_key(
            "run_program",
            &nf_args(serde_json::json!({"program": "cargo", "args": ["nextest", "run"]})),
        )
        .unwrap();
        let b = notfound_target_key(
            "run_program",
            &nf_args(serde_json::json!({"program": "cargo", "args": ["nextest", "run"]})),
        )
        .unwrap();
        assert_eq!(a, b, "same command ⇒ same key (stable)");
        let c = notfound_target_key(
            "run_program",
            &nf_args(serde_json::json!({"program": "cargo", "args": ["test"]})),
        )
        .unwrap();
        assert_ne!(a, c, "different argv ⇒ different key");
        assert!(a.starts_with("run_program"), "key is tool-scoped: {a}");

        // Path-shaped tools key off `path`, and the tool name is part of the key
        // so read_file and list_dir on the same path never collide.
        let r = notfound_target_key("read_file", &nf_args(serde_json::json!({"path": "/x/y"})))
            .unwrap();
        let l =
            notfound_target_key("list_dir", &nf_args(serde_json::json!({"path": "/x/y"}))).unwrap();
        let s = notfound_target_key("stat_file", &nf_args(serde_json::json!({"path": "/x/y"})))
            .unwrap();
        assert_ne!(r, l);
        assert_ne!(r, s);
        // Whitespace is normalized.
        assert_eq!(
            r,
            notfound_target_key(
                "read_file",
                &nf_args(serde_json::json!({"path": "  /x/y  "}))
            )
            .unwrap()
        );

        // glob_expand keys off pattern (+ base path when scoped).
        let g1 = notfound_target_key(
            "glob_expand",
            &nf_args(serde_json::json!({"pattern": "**/*.rs"})),
        )
        .unwrap();
        let g2 = notfound_target_key(
            "glob_expand",
            &nf_args(serde_json::json!({"pattern": "**/*.rs", "path": "src"})),
        )
        .unwrap();
        assert_ne!(g1, g2, "scoping by path is a different target");
    }

    #[test]
    fn notfound_key_fails_open_on_unknown_or_empty_shapes() {
        // Unguarded tool ⇒ no key ⇒ always executes.
        assert!(
            notfound_target_key("write_file", &nf_args(serde_json::json!({"path": "/a"})))
                .is_none()
        );
        // grep_files is a discovery tool the steer points AT — never guarded.
        assert!(
            notfound_target_key("grep_files", &nf_args(serde_json::json!({"pattern": "x"})))
                .is_none()
        );
        // Missing / empty / wrong-typed required args ⇒ no key (no panic).
        assert!(notfound_target_key("read_file", &nf_args(serde_json::json!({}))).is_none());
        assert!(
            notfound_target_key("read_file", &nf_args(serde_json::json!({"path": "   "})))
                .is_none()
        );
        assert!(
            notfound_target_key("read_file", &nf_args(serde_json::json!({"path": 7}))).is_none()
        );
        assert!(notfound_target_key("run_program", &nf_args(serde_json::json!({}))).is_none());
        assert!(notfound_target_key("run_program", &nf_args(serde_json::Value::Null)).is_none());
        // Non-string argv entries are stringified, not fatal.
        assert!(
            notfound_target_key(
                "run_program",
                &nf_args(serde_json::json!({"program": "ls", "args": [1, true]}))
            )
            .is_some()
        );
    }

    #[test]
    fn notfound_key_is_length_capped_on_char_boundary() {
        let huge = "é".repeat(4096);
        let k =
            notfound_target_key("read_file", &nf_args(serde_json::json!({"path": huge}))).unwrap();
        assert!(k.len() <= NOTFOUND_KEY_MAX, "key capped: {} bytes", k.len());
        // Truncating mid-codepoint would have panicked above; this asserts the
        // result is still valid UTF-8 we can round-trip.
        assert_eq!(k, k.clone());
    }

    #[test]
    fn second_identical_notfound_call_is_blocked() {
        let dir = tele_tmp("nf_block");
        let mut s = tele_session(&dir);
        let args = nf_args(serde_json::json!({"program": "kubectl", "args": ["get", "pods"]}));
        let miss = ToolResult::text("t", "no such file or directory: kubectl", true);

        // First attempt is never blocked (nothing armed yet).
        assert!(notfound_guard_check(&mut s, "run_program", &args).is_none());
        note_notfound_outcome(&mut s, "run_program", &args, &miss);
        assert_eq!(s.notfound_guard.len(), 1, "the miss armed the target");

        // The identical retry is blocked with a steer naming the discovery tools.
        let steer = notfound_guard_check(&mut s, "run_program", &args)
            .expect("identical not-found retry must be blocked");
        assert!(steer.contains("already failed not-found"), "steer: {steer}");
        assert!(steer.contains("list_dir"), "steer names list_dir: {steer}");
        assert!(
            steer.contains("glob_expand"),
            "steer names glob_expand: {steer}"
        );
        assert!(
            steer.contains("grep_files"),
            "steer names grep_files: {steer}"
        );
        assert!(steer.contains("kubectl"), "steer names the target: {steer}");
        assert!(
            !steer.contains('\u{1e}') && !steer.contains('\u{1f}'),
            "no key separators leak into the steer: {steer}"
        );

        // The block is observable in telemetry as a gate refusal.
        assert!(
            s.tool_telemetry_buf
                .iter()
                .any(|e| e.error_class.as_deref() == Some(CLASS_GATE_REFUSE)
                    && e.tool == format!("gate:{NOTFOUND_GUARD_RULE}:run_program")),
            "blocked retry must land a gate row: {:?}",
            s.tool_telemetry_buf
                .iter()
                .map(|e| (e.tool.clone(), e.error_class.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn different_target_is_not_blocked() {
        let dir = tele_tmp("nf_other");
        let mut s = tele_session(&dir);
        let missing = nf_args(serde_json::json!({"path": "/nope/a.txt"}));
        let other = nf_args(serde_json::json!({"path": "/nope/b.txt"}));
        let miss = ToolResult::text("t", "No such file or directory", true);

        note_notfound_outcome(&mut s, "read_file", &missing, &miss);
        assert!(notfound_guard_check(&mut s, "read_file", &missing).is_some());
        assert!(
            notfound_guard_check(&mut s, "read_file", &other).is_none(),
            "a different path must execute normally"
        );
        assert!(
            notfound_guard_check(&mut s, "list_dir", &missing).is_none(),
            "a different tool on the same path must execute normally"
        );
    }

    #[test]
    fn only_notfound_class_arms_the_guard() {
        let dir = tele_tmp("nf_class");
        let mut s = tele_session(&dir);
        let args = nf_args(serde_json::json!({"path": "/locked/secret"}));

        for msg in [
            "permission denied",
            "timed out after 120s",
            "is a directory",
        ] {
            let err = ToolResult::text("t", msg, true);
            note_notfound_outcome(&mut s, "read_file", &args, &err);
            assert!(
                s.notfound_guard.is_empty(),
                "{msg:?} must NOT arm the not-found guard"
            );
            assert!(notfound_guard_check(&mut s, "read_file", &args).is_none());
        }
    }

    #[test]
    fn guard_entry_is_cleared_after_a_success() {
        let dir = tele_tmp("nf_clear");
        let mut s = tele_session(&dir);
        let args = nf_args(serde_json::json!({"path": "/tmp/appears-later"}));
        let miss = ToolResult::text("t", "ENOENT: no such file", true);
        let ok = ToolResult::text("t", "contents", false);

        note_notfound_outcome(&mut s, "read_file", &args, &miss);
        assert!(notfound_guard_check(&mut s, "read_file", &args).is_some());
        // The file appeared → the target is disarmed and retryable again.
        note_notfound_outcome(&mut s, "read_file", &args, &ok);
        assert!(s.notfound_guard.is_empty());
        assert!(notfound_guard_check(&mut s, "read_file", &args).is_none());
    }

    #[test]
    fn guard_map_is_bounded_and_fifo_evicts() {
        let mut g = NotFoundGuard::new(true);
        for i in 0..(NOTFOUND_TARGETS_CAP + 10) {
            g.arm(&format!("read_file\u{1e}/p/{i}"));
        }
        assert_eq!(g.len(), NOTFOUND_TARGETS_CAP, "map is capped");
        // Oldest evicted, newest retained.
        assert!(!g.is_armed("read_file\u{1e}/p/0"));
        assert!(g.is_armed(&format!("read_file\u{1e}/p/{}", NOTFOUND_TARGETS_CAP + 9)));
        // arm() is idempotent — a repeat does not grow or reorder the map.
        let before = g.len();
        g.arm("read_file\u{1e}/p/70");
        g.arm("read_file\u{1e}/p/70");
        assert_eq!(g.len(), before);
    }

    #[test]
    fn disabled_guard_never_blocks_or_arms() {
        let dir = tele_tmp("nf_off");
        let mut s = tele_session(&dir);
        s.notfound_guard = NotFoundGuard::new(false);
        let args = nf_args(serde_json::json!({"path": "/nope"}));
        let miss = ToolResult::text("t", "no such file", true);

        note_notfound_outcome(&mut s, "read_file", &args, &miss);
        assert!(
            s.notfound_guard.is_empty(),
            "disabled guard records nothing"
        );
        assert!(
            notfound_guard_check(&mut s, "read_file", &args).is_none(),
            "disabled guard never blocks"
        );
    }
}
