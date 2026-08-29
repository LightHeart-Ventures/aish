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
    if has(" 409") || has("conflict") || has("already exists") {
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
    if session.db.is_none() {
        return None;
    }

    // Fresh cache hit: serve the snapshot, no DB scan.
    if let Some(c) = &session.tool_telemetry_cache {
        if c.cached_at.elapsed() < session.tool_telemetry_cache_secs {
            return Some(c.clone());
        }
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
        retries.sort_by(|a, b| b.retries.cmp(&a.retries));
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
        assert_eq!(classify("Error: request timed out after 120s"), ErrorClass::Timeout);
        assert_eq!(classify("HTTP 429 Too Many Requests"), ErrorClass::RateLimit);
        assert_eq!(classify("GitHub API returned 403 Forbidden"), ErrorClass::Auth);
        assert_eq!(classify("fatal: repository not found (404)"), ErrorClass::NotFound);
        assert_eq!(classify("refusing: destination already exists"), ErrorClass::Conflict);
        assert_eq!(classify("open /etc/shadow: permission denied"), ErrorClass::Permission);
        assert_eq!(classify("dial tcp: connection refused"), ErrorClass::Network);
        assert_eq!(classify("400 Bad Request: missing required field"), ErrorClass::InvalidArgs);
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
            ToolTotals { tool: "run_program".into(), calls: 100, failures: 2 },
            ToolTotals { tool: "atum_list_tasks".into(), calls: 10, failures: 8 },
        ];
        let class_failures = vec![
            ClassFailure { tool: "atum_list_tasks".into(), class: "timeout".into(), count: 7 },
            ClassFailure { tool: "atum_list_tasks".into(), class: "other".into(), count: 1 },
            ClassFailure { tool: "run_program".into(), class: "not-found".into(), count: 2 },
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
        assert!(s.tool_telemetry_cache.is_none(), "record must invalidate the cache");
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
        assert_eq!(
            classify("operation not permitted"),
            ErrorClass::Permission
        );
    }

    #[test]
    fn classify_not_found_errors() {
        assert_eq!(classify("no such file"), ErrorClass::NotFound);
        assert_eq!(classify("ENOENT"), ErrorClass::NotFound);
        assert_eq!(classify("does not exist"), ErrorClass::NotFound);
        assert_eq!(classify("not found"), ErrorClass::NotFound);
    }
}
