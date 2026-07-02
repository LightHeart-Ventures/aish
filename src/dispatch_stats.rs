//! Background-job dispatch-efficiency metrics (feature 3).
//!
//! Closes the observability loop on `run_in_background` / `:dispatch`: the
//! coordinator offloads work but has had no visibility into whether that was the
//! right call. This module reads the durable `coordinator_runs` store and
//! derives:
//!
//!   • **dispatch volume** — how many jobs were launched, and how many actually
//!     completed vs failed vs are still running (complete-vs-timeout);
//!   • **latency distribution** — min / mean / median / max plus coarse buckets,
//!     so you can see whether a job was worth deferring or should have been
//!     answered inline;
//!   • **missed-inline opportunities** — a heuristic count of jobs that finished
//!     at or under [`QUICK_JOB_SECS`], i.e. so fast the answer was likely
//!     already reachable without offloading.
//!
//! Everything here is pure over [`crate::db::CoordinatorRow`] slices so it is
//! unit-tested without a live DB. The `:dispatch-stats` REPL command and the
//! end-of-session summary render [`summarize`] / [`render`].

use crate::db::CoordinatorRow;

/// A completed job whose wall-clock latency (created → terminal) is at or under
/// this many seconds is flagged as a *likely* missed inline opportunity: it
/// finished so quickly the coordinator probably could have answered without
/// offloading at all. Deliberately conservative — a job that genuinely needed a
/// build/test cycle rarely finishes this fast.
pub const QUICK_JOB_SECS: i64 = 45;

/// Aggregate dispatch-efficiency figures derived from a set of coordinator runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DispatchStats {
    /// Total jobs considered (every row in scope).
    pub dispatched: usize,
    /// Terminal-success rows (`phase = done`).
    pub done: usize,
    /// Terminal-failure rows (`phase = failed`).
    pub failed: usize,
    /// Still-in-flight rows (`coordinating` | `awaiting_batch`).
    pub running: usize,
    /// Of the still-running, how many are parked awaiting a batch result — a
    /// proxy for "routed to the slow/batch tier".
    pub awaiting_batch: usize,
    /// Completed jobs with a parseable created→terminal latency.
    pub timed: usize,
    /// Sorted completed-job latencies (seconds). Drives min/mean/median/max.
    pub latencies: Vec<i64>,
    /// Completed jobs at/under [`QUICK_JOB_SECS`] (missed-inline heuristic).
    pub quick: usize,
    /// Latency buckets over completed+timed jobs.
    pub bucket_lt_1m: usize,
    pub bucket_1_5m: usize,
    pub bucket_5_15m: usize,
    pub bucket_gt_15m: usize,
}

impl DispatchStats {
    /// Fraction of dispatched jobs that reached a terminal state (0.0–1.0).
    pub fn completion_rate(&self) -> f64 {
        let terminal = self.done + self.failed;
        if self.dispatched == 0 {
            0.0
        } else {
            terminal as f64 / self.dispatched as f64
        }
    }

    /// Fraction of *completed* jobs that finished quick enough to likely have
    /// been inline (0.0–1.0). Keyed on `timed` (the denominator we actually
    /// measured), not `dispatched`.
    pub fn quick_share(&self) -> f64 {
        if self.timed == 0 {
            0.0
        } else {
            self.quick as f64 / self.timed as f64
        }
    }

    pub fn min_secs(&self) -> Option<i64> {
        self.latencies.first().copied()
    }

    pub fn max_secs(&self) -> Option<i64> {
        self.latencies.last().copied()
    }

    pub fn mean_secs(&self) -> Option<i64> {
        if self.latencies.is_empty() {
            return None;
        }
        let sum: i64 = self.latencies.iter().sum();
        Some(sum / self.latencies.len() as i64)
    }

    /// Median (p50) — the middle of the sorted latencies (mean of the two middle
    /// values on an even count).
    pub fn median_secs(&self) -> Option<i64> {
        let n = self.latencies.len();
        if n == 0 {
            return None;
        }
        if n % 2 == 1 {
            Some(self.latencies[n / 2])
        } else {
            Some((self.latencies[n / 2 - 1] + self.latencies[n / 2]) / 2)
        }
    }
}

/// Parse a SQLite `current_timestamp` string (`YYYY-MM-DD HH:MM:SS`, UTC, with an
/// optional `T` separator and optional fractional seconds / trailing `Z`) into
/// epoch seconds. Returns `None` on any shape it doesn't recognize — callers
/// treat an unparseable timestamp as "untimed", never as zero.
pub fn parse_sqlite_ts(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once(['T', ' '])?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    // Drop any fractional-seconds tail before splitting the clock.
    let time = time.split('.').next()?;
    let mut tp = time.split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let ss: i64 = tp.next().unwrap_or("0").parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + hh * 3_600 + mi * 60 + ss)
}

/// Howard Hinnant's civil-date → days-since-1970 algorithm. Pure integer math,
/// valid across the whole proleptic Gregorian range — no chrono dependency.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Fold a set of coordinator rows into [`DispatchStats`]. `rows` is pre-filtered
/// by the caller (e.g. to this session). Latency is `heartbeat_at − created_at`
/// for terminal rows — `set_done` / `set_failed` bump the heartbeat, so it
/// freezes at the completion instant.
pub fn summarize(rows: &[&CoordinatorRow]) -> DispatchStats {
    let mut st = DispatchStats {
        dispatched: rows.len(),
        ..Default::default()
    };
    for r in rows {
        let terminal = match r.phase.as_str() {
            "done" => {
                st.done += 1;
                true
            }
            "failed" => {
                st.failed += 1;
                true
            }
            "awaiting_batch" => {
                st.running += 1;
                st.awaiting_batch += 1;
                false
            }
            _ => {
                // "coordinating" and any unknown phase count as in-flight.
                st.running += 1;
                false
            }
        };
        if !terminal {
            continue;
        }
        let (Some(start), Some(end)) = (
            r.created_at.as_deref().and_then(parse_sqlite_ts),
            r.heartbeat_at.as_deref().and_then(parse_sqlite_ts),
        ) else {
            continue;
        };
        let secs = (end - start).max(0);
        st.timed += 1;
        st.latencies.push(secs);
        if secs <= QUICK_JOB_SECS {
            st.quick += 1;
        }
        match secs {
            s if s < 60 => st.bucket_lt_1m += 1,
            s if s < 300 => st.bucket_1_5m += 1,
            s if s < 900 => st.bucket_5_15m += 1,
            _ => st.bucket_gt_15m += 1,
        }
    }
    st.latencies.sort_unstable();
    st
}

/// Human-friendly `Ns` / `Nm Ss` / `Nh Mm` duration.
fn fmt_dur(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3_600, (secs % 3_600) / 60)
    }
}

/// Render the stats as terse shell output lines (no trailing newlines). `scope`
/// labels what the numbers cover, e.g. `"this session"` or `"all sessions"`.
pub fn render(st: &DispatchStats, scope: &str) -> Vec<String> {
    let mut out = Vec::new();
    if st.dispatched == 0 {
        out.push(format!("dispatch stats ({scope}): no background jobs on record"));
        return out;
    }
    out.push(format!(
        "dispatch stats ({scope}) — {} job{} dispatched",
        st.dispatched,
        if st.dispatched == 1 { "" } else { "s" }
    ));
    out.push(format!(
        "  outcomes:  ✓ {} done · ✗ {} failed · ⏳ {} running{} · completion {:.0}%",
        st.done,
        st.failed,
        st.running,
        if st.awaiting_batch > 0 {
            format!(" ({} awaiting batch)", st.awaiting_batch)
        } else {
            String::new()
        },
        st.completion_rate() * 100.0,
    ));
    if st.timed > 0 {
        out.push(format!(
            "  latency:   min {} · median {} · mean {} · max {}   (n={})",
            fmt_dur(st.min_secs().unwrap_or(0)),
            fmt_dur(st.median_secs().unwrap_or(0)),
            fmt_dur(st.mean_secs().unwrap_or(0)),
            fmt_dur(st.max_secs().unwrap_or(0)),
            st.timed,
        ));
        out.push(format!(
            "  buckets:   <1m {} · 1–5m {} · 5–15m {} · 15m+ {}",
            st.bucket_lt_1m, st.bucket_1_5m, st.bucket_5_15m, st.bucket_gt_15m,
        ));
        out.push(format!(
            "  inline?:   {} of {} completed in ≤{}s ({:.0}% likely answerable inline)",
            st.quick,
            st.timed,
            QUICK_JOB_SECS,
            st.quick_share() * 100.0,
        ));
    }
    for line in insights(st) {
        out.push(format!("  → {line}"));
    }
    out
}

/// Actionable one-liners derived from the stats — the "payoff" the feature is
/// about (stop over-offloading, fix tier routing). Empty when nothing stands
/// out. Pure, so the thresholds are unit-tested.
pub fn insights(st: &DispatchStats) -> Vec<String> {
    let mut tips = Vec::new();
    if st.timed >= 3 && st.quick_share() >= 0.5 {
        tips.push(format!(
            "over half of completed jobs finished in ≤{}s — you're likely over-offloading quick questions; answer those inline",
            QUICK_JOB_SECS
        ));
    }
    if st.dispatched >= 3 && st.failed * 2 > st.done && st.failed > 0 {
        tips.push(
            "failure rate is high — inspect failing tasks before re-dispatching (the circuit breaker may be refusing repeats)".to_string(),
        );
    }
    if st.bucket_gt_15m > 0 && st.awaiting_batch == 0 && st.bucket_gt_15m >= st.timed.max(1) / 2 {
        tips.push(
            "many jobs ran 15m+ on the interactive tier — consider routing long, non-urgent work to the batch tier".to_string(),
        );
    }
    tips
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CoordinatorRow;

    fn row(phase: &str, created: Option<&str>, beat: Option<&str>) -> CoordinatorRow {
        CoordinatorRow {
            run_id: "r".into(),
            task: "t".into(),
            phase: phase.into(),
            result: None,
            error: None,
            session_id: None,
            session_name: None,
            created_at: created.map(String::from),
            heartbeat_at: beat.map(String::from),
        }
    }

    #[test]
    fn parses_sqlite_timestamp_both_separators() {
        // 2024-01-01 00:00:00 UTC = 1704067200.
        assert_eq!(parse_sqlite_ts("2024-01-01 00:00:00"), Some(1_704_067_200));
        assert_eq!(parse_sqlite_ts("2024-01-01T00:00:00Z"), Some(1_704_067_200));
        assert_eq!(
            parse_sqlite_ts("2024-01-01 00:00:30.5"),
            Some(1_704_067_230)
        );
        // Epoch itself.
        assert_eq!(parse_sqlite_ts("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_sqlite_ts("garbage"), None);
        assert_eq!(parse_sqlite_ts("2024-13-01 00:00:00"), None);
    }

    #[test]
    fn counts_outcomes_and_running() {
        let rows = vec![
            row("done", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:00:30")),
            row("failed", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:10:00")),
            row("coordinating", None, None),
            row("awaiting_batch", None, None),
        ];
        let refs: Vec<&CoordinatorRow> = rows.iter().collect();
        let st = summarize(&refs);
        assert_eq!(st.dispatched, 4);
        assert_eq!(st.done, 1);
        assert_eq!(st.failed, 1);
        assert_eq!(st.running, 2);
        assert_eq!(st.awaiting_batch, 1);
        assert_eq!(st.timed, 2);
        // completion = terminal/dispatched = 2/4.
        assert!((st.completion_rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn latency_stats_and_quick_flag() {
        // Three done jobs: 30s (quick), 120s, 600s.
        let rows = vec![
            row("done", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:00:30")),
            row("done", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:02:00")),
            row("done", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:10:00")),
        ];
        let refs: Vec<&CoordinatorRow> = rows.iter().collect();
        let st = summarize(&refs);
        assert_eq!(st.timed, 3);
        assert_eq!(st.min_secs(), Some(30));
        assert_eq!(st.max_secs(), Some(600));
        assert_eq!(st.median_secs(), Some(120));
        assert_eq!(st.mean_secs(), Some(250));
        assert_eq!(st.quick, 1);
        assert_eq!(st.bucket_lt_1m, 1);
        assert_eq!(st.bucket_1_5m, 1);
        assert_eq!(st.bucket_5_15m, 1);
        assert_eq!(st.bucket_gt_15m, 0);
    }

    #[test]
    fn negative_latency_clamped_to_zero() {
        // heartbeat before created (clock skew) must not go negative.
        let rows = vec![row(
            "done",
            Some("2024-01-01 00:01:00"),
            Some("2024-01-01 00:00:00"),
        )];
        let refs: Vec<&CoordinatorRow> = rows.iter().collect();
        let st = summarize(&refs);
        assert_eq!(st.latencies, vec![0]);
        assert_eq!(st.quick, 1);
    }

    #[test]
    fn over_offload_insight_fires_on_mostly_quick() {
        let rows = vec![
            row("done", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:00:10")),
            row("done", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:00:20")),
            row("done", Some("2024-01-01 00:00:00"), Some("2024-01-01 00:00:30")),
        ];
        let refs: Vec<&CoordinatorRow> = rows.iter().collect();
        let st = summarize(&refs);
        let tips = insights(&st);
        assert!(tips.iter().any(|t| t.contains("over-offloading")));
    }

    #[test]
    fn empty_scope_renders_placeholder() {
        let st = summarize(&[]);
        let lines = render(&st, "this session");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no background jobs"));
    }

    #[test]
    fn render_includes_core_sections_when_populated() {
        let rows = vec![row(
            "done",
            Some("2024-01-01 00:00:00"),
            Some("2024-01-01 00:05:00"),
        )];
        let refs: Vec<&CoordinatorRow> = rows.iter().collect();
        let st = summarize(&refs);
        let blob = render(&st, "all sessions").join("\n");
        assert!(blob.contains("outcomes:"));
        assert!(blob.contains("latency:"));
        assert!(blob.contains("buckets:"));
        assert!(blob.contains("inline?:"));
    }
}
