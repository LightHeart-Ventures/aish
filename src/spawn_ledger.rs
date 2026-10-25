//! Per-run spawn ledger: **width** and **total** caps on worker dispatch.
//!
//! # Why this exists
//!
//! [`crate::worker::spawn_budget_gate`] bounds spawn **depth** — how many
//! levels of nested coordinator a chain may fork (`AISH_SPAWN_BUDGET`, default
//! 3). That guard is necessary but not sufficient: depth 1 is unbounded in
//! *width*. A single coordinator that never nests can still dispatch fifty
//! siblings in one wave, or trickle out hundreds over a long run, and the depth
//! budget will wave every one of them through because the chain never got
//! deeper than one level.
//!
//! This module closes that hole with two independent per-run ceilings:
//!
//! | Cap | Env var | Default | Bounds |
//! |-----|---------|---------|--------|
//! | Width | `AISH_SPAWN_WIDTH_MAX` | 8 | Workers **concurrently in flight** from this run |
//! | Total | `AISH_SPAWN_TOTAL_MAX` | 32 | Workers dispatched over this run's **whole lifetime** |
//!
//! Together with the depth budget they bound the runaway case at *every* depth,
//! not just nesting: a fork-bomb needs depth, width, or volume, and all three
//! now have a hard backstop.
//!
//! # Semantics
//!
//! * **Width** is concurrency, so it is a *gauge*: incremented when a worker
//!   starts, decremented when it finishes. A [`SpawnTicket`] is the RAII handle
//!   that guarantees the decrement happens on every exit path (success, failure,
//!   panic, early return) — there is no manual release to forget.
//! * **Total** is a lifetime *counter*: it only ever rises, so a run cannot
//!   evade it by waiting for workers to drain.
//! * The ledger is **process-global** and therefore scoped to exactly one aish
//!   run, which is the unit the task cares about. A child coordinator is a
//!   separate process with a fresh ledger of its own — its spawns are bounded
//!   by its own caps *and* by the decremented depth budget it inherits.
//! * `0` **disables** a cap (reads as "no ceiling"), matching the ordinary
//!   reading of a `*_MAX` knob and giving operators an escape hatch. An
//!   unparseable or absent value falls back to the default, mirroring
//!   [`crate::worker::spawn_budget`]'s precedent.
//!
//! # Enforcement shape
//!
//! Reservation is a single atomic compare-and-swap ([`SpawnTicket::acquire`]),
//! so two threads racing at the ceiling cannot both win: the check and the
//! increment are one operation. [`check`] is the cheap non-mutating pre-flight
//! used by the dispatch gate to reject obvious over-budget calls early with a
//! clear message; `acquire` is the authoritative guard at the launch site. The
//! pre-check can go stale between call and launch — that is why it is only a
//! fast path and never the sole gate.

use std::sync::atomic::{AtomicU32, Ordering};

/// Env var capping how many workers this run may have **in flight at once**.
pub const WIDTH_MAX_ENV: &str = "AISH_SPAWN_WIDTH_MAX";

/// Env var capping how many workers this run may dispatch **in total**.
pub const TOTAL_MAX_ENV: &str = "AISH_SPAWN_TOTAL_MAX";

/// Default width ceiling. Eight concurrent coordinators is well above any
/// legitimate fan-out wave (the fan-out guidance tops out at a handful of
/// genuinely independent nodes) while staying inside what one host can run
/// without thrashing — each worker is a full aish process with its own git
/// worktree.
pub const DEFAULT_WIDTH_MAX: u32 = 8;

/// Default lifetime ceiling. Thirty-two workers is a generous long-run budget
/// (several full waves plus retries) and still a hard stop long before a
/// runaway dispatch loop can exhaust the host.
pub const DEFAULT_TOTAL_MAX: u32 = 32;

/// Workers currently in flight from this run (width gauge).
static LIVE: AtomicU32 = AtomicU32::new(0);

/// Workers dispatched by this run since start (lifetime counter).
static TOTAL: AtomicU32 = AtomicU32::new(0);

/// Which ceiling a refusal hit. Carried separately from the message so callers
/// (and tests) can branch on the cause without string-matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerDenial {
    /// Too many workers in flight right now.
    Width { live: u32, max: u32 },
    /// This run has dispatched its lifetime allowance.
    Total { total: u32, max: u32 },
}

impl LedgerDenial {
    /// Operator-facing refusal text. Names the offending env var and the actual
    /// numbers so the message is actionable rather than just a "no".
    pub fn message(&self) -> String {
        match *self {
            LedgerDenial::Width { live, max } => format!(
                "per-run spawn WIDTH budget reached ({live}/{max} workers already in flight; \
{WIDTH_MAX_ENV}={max}) — refusing to dispatch another concurrent worker. This is the width guard \
that bounds a runaway fan-out at any depth. Wait for an in-flight worker to finish, narrow the \
wave, or raise {WIDTH_MAX_ENV} if you understand the host-load risk."
            ),
            LedgerDenial::Total { total, max } => format!(
                "per-run spawn TOTAL budget exhausted ({total}/{max} workers dispatched by this \
run; {TOTAL_MAX_ENV}={max}) — refusing to dispatch another worker. This is the lifetime guard that \
stops a dispatch loop from trickling out unbounded workers. Finish or re-scope the current work, or \
raise {TOTAL_MAX_ENV} if you understand the runaway risk."
            ),
        }
    }
}

impl std::fmt::Display for LedgerDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Parse a `*_MAX` env value: absent / empty / unparseable ⇒ `default`.
/// A legitimate `0` is preserved and means "cap disabled". Pure → unit-tested
/// without mutating process-wide env.
fn parse_max(raw: Option<&str>, default: u32) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

/// This run's configured width ceiling.
pub fn width_max() -> u32 {
    parse_max(
        std::env::var(WIDTH_MAX_ENV).ok().as_deref(),
        DEFAULT_WIDTH_MAX,
    )
}

/// This run's configured lifetime ceiling.
pub fn total_max() -> u32 {
    parse_max(
        std::env::var(TOTAL_MAX_ENV).ok().as_deref(),
        DEFAULT_TOTAL_MAX,
    )
}

/// Whether one more concurrent worker fits under `max`. `max == 0` ⇒ no cap.
/// Pure → unit-tested.
pub fn width_ok(live: u32, max: u32) -> bool {
    max == 0 || live < max
}

/// Whether one more lifetime dispatch fits under `max`. `max == 0` ⇒ no cap.
/// Pure → unit-tested.
pub fn total_ok(total: u32, max: u32) -> bool {
    max == 0 || total < max
}

/// Decide a reservation against both ceilings, given the observed counters.
/// Pure (no atomics, no env) so the whole decision table is unit-testable; the
/// live wrappers supply the real numbers. Total is checked FIRST: it is the
/// terminal condition (waiting cannot clear it), so reporting it is strictly
/// more useful than reporting a width stall the operator would then retry.
pub fn decide(live: u32, total: u32, width_cap: u32, total_cap: u32) -> Result<(), LedgerDenial> {
    if !total_ok(total, total_cap) {
        return Err(LedgerDenial::Total {
            total,
            max: total_cap,
        });
    }
    if !width_ok(live, width_cap) {
        return Err(LedgerDenial::Width {
            live,
            max: width_cap,
        });
    }
    Ok(())
}

/// Current ledger reading: `(live, total)`. For status surfaces and tests.
pub fn snapshot() -> (u32, u32) {
    (LIVE.load(Ordering::SeqCst), TOTAL.load(Ordering::SeqCst))
}

/// Non-mutating pre-flight check against both ceilings.
///
/// Used by the dispatch gate so an over-budget call is refused early with a
/// clear message instead of being launched and then failed. This is a fast path
/// only — it can go stale between the check and the launch, so the launch site
/// must still [`SpawnTicket::acquire`], which is atomic and authoritative.
pub fn check() -> Result<(), LedgerDenial> {
    let (live, total) = snapshot();
    decide(live, total, width_max(), total_max())
}

/// RAII reservation for one in-flight worker.
///
/// Holding a ticket *is* the worker's slot in the width gauge: the gauge is
/// incremented by [`SpawnTicket::acquire`] and decremented by `Drop`, so every
/// exit path — normal return, early return, error, panic, task cancellation —
/// releases the slot without the caller remembering to. The lifetime counter is
/// deliberately **not** released on drop; it must only ever rise.
#[derive(Debug)]
pub struct SpawnTicket {
    /// Not constructible outside this module: a slot may only come from
    /// [`SpawnTicket::acquire`], so the gauge can never be incremented without
    /// a matching `Drop` to release it.
    _private: (),
}

impl SpawnTicket {
    /// Atomically reserve a slot, or refuse with the ceiling that was hit.
    ///
    /// The width reservation is a compare-and-swap loop, so concurrent
    /// acquisitions at the ceiling cannot both succeed — exactly one wins and
    /// the loser gets a [`LedgerDenial::Width`]. The lifetime counter is bumped
    /// only after the width slot is secured, so a refused dispatch never
    /// consumes lifetime budget.
    pub fn acquire() -> Result<Self, LedgerDenial> {
        let width_cap = width_max();
        let total_cap = total_max();

        // Reserve the concurrency slot via CAS so the check and the increment
        // are one atomic step.
        let mut live = LIVE.load(Ordering::SeqCst);
        loop {
            let total = TOTAL.load(Ordering::SeqCst);
            decide(live, total, width_cap, total_cap)?;
            match LIVE.compare_exchange_weak(live, live + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                // Another thread moved the gauge; re-read and re-decide against
                // the new value rather than forcing our stale one through.
                Err(observed) => live = observed,
            }
        }

        TOTAL.fetch_add(1, Ordering::SeqCst);
        Ok(SpawnTicket { _private: () })
    }
}

impl Drop for SpawnTicket {
    fn drop(&mut self) {
        // Saturating CAS: a stray double-release must never wrap the gauge to
        // u32::MAX and wedge the width guard shut for the rest of the run.
        let mut live = LIVE.load(Ordering::SeqCst);
        while live > 0 {
            match LIVE.compare_exchange_weak(live, live - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return,
                Err(observed) => live = observed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_max: env parsing is pure and covers the footgun values ────────

    #[test]
    fn parse_max_absent_or_junk_falls_back_to_default() {
        assert_eq!(parse_max(None, 8), 8);
        assert_eq!(parse_max(Some(""), 8), 8);
        assert_eq!(parse_max(Some("   "), 8), 8);
        assert_eq!(parse_max(Some("lots"), 8), 8);
        assert_eq!(parse_max(Some("-1"), 8), 8);
        assert_eq!(parse_max(Some("3.5"), 8), 8);
    }

    #[test]
    fn parse_max_accepts_values_and_preserves_explicit_zero() {
        assert_eq!(parse_max(Some("1"), 8), 1);
        assert_eq!(parse_max(Some("64"), 8), 64);
        assert_eq!(parse_max(Some(" 5 "), 8), 5);
        // 0 is meaningful ("cap disabled"), NOT a parse failure.
        assert_eq!(parse_max(Some("0"), 8), 0);
    }

    // ── width_ok / total_ok ─────────────────────────────────────────────────

    #[test]
    fn width_allows_up_to_but_not_at_the_cap() {
        assert!(width_ok(0, 2));
        assert!(width_ok(1, 2));
        assert!(!width_ok(2, 2), "at the cap the next spawn must be refused");
        assert!(!width_ok(9, 2), "over the cap stays refused");
    }

    #[test]
    fn total_allows_up_to_but_not_at_the_cap() {
        assert!(total_ok(0, 3));
        assert!(total_ok(2, 3));
        assert!(!total_ok(3, 3));
        assert!(!total_ok(100, 3));
    }

    #[test]
    fn zero_cap_disables_the_guard() {
        assert!(width_ok(9_999, 0));
        assert!(total_ok(9_999, 0));
    }

    // ── decide: the whole decision table ────────────────────────────────────

    #[test]
    fn decide_passes_when_both_caps_have_room() {
        assert_eq!(decide(0, 0, 8, 32), Ok(()));
        assert_eq!(decide(7, 31, 8, 32), Ok(()));
    }

    #[test]
    fn decide_reports_width_when_only_width_is_full() {
        assert_eq!(
            decide(8, 10, 8, 32),
            Err(LedgerDenial::Width { live: 8, max: 8 })
        );
    }

    #[test]
    fn decide_reports_total_when_only_total_is_full() {
        assert_eq!(
            decide(1, 32, 8, 32),
            Err(LedgerDenial::Total { total: 32, max: 32 })
        );
    }

    #[test]
    fn decide_prefers_total_when_both_are_full() {
        // Total is terminal — waiting cannot clear it — so it is the more
        // useful refusal to surface.
        assert_eq!(
            decide(8, 32, 8, 32),
            Err(LedgerDenial::Total { total: 32, max: 32 })
        );
    }

    #[test]
    fn decide_is_unbounded_when_both_caps_are_zero() {
        assert_eq!(decide(1_000, 1_000, 0, 0), Ok(()));
    }

    // ── refusal messages are actionable ─────────────────────────────────────

    #[test]
    fn denial_messages_name_their_env_var_and_counts() {
        let w = LedgerDenial::Width { live: 8, max: 8 }.message();
        assert!(w.contains(WIDTH_MAX_ENV), "width denial must name its knob");
        assert!(w.contains("8/8"), "width denial must show the counts: {w}");

        let t = LedgerDenial::Total { total: 32, max: 32 }.message();
        assert!(t.contains(TOTAL_MAX_ENV), "total denial must name its knob");
        assert!(
            t.contains("32/32"),
            "total denial must show the counts: {t}"
        );
    }

    #[test]
    fn denial_messages_are_distinguishable() {
        let w = LedgerDenial::Width { live: 1, max: 1 }.message();
        let t = LedgerDenial::Total { total: 1, max: 1 }.message();
        assert_ne!(w, t);
        assert!(w.contains("WIDTH"));
        assert!(t.contains("TOTAL"));
    }

    #[test]
    fn display_matches_message() {
        let d = LedgerDenial::Width { live: 2, max: 2 };
        assert_eq!(d.to_string(), d.message());
    }

    // ── defaults are the documented ones ────────────────────────────────────

    #[test]
    fn documented_defaults() {
        assert_eq!(DEFAULT_WIDTH_MAX, 8);
        assert_eq!(DEFAULT_TOTAL_MAX, 32);
    }

    // ── RAII: the gauge is released on every exit path ──────────────────────
    //
    // These exercise the process-global statics, so they must not run
    // concurrently with each other. They are serialised onto one test fn
    // (cargo runs test fns in parallel threads, but a single fn is sequential)
    // and restore the counters they touch.

    #[test]
    fn ticket_lifecycle_tracks_width_and_total() {
        let (live0, total0) = snapshot();

        {
            let _t1 = SpawnTicket::acquire().expect("first ticket fits under the default caps");
            let (live, total) = snapshot();
            assert_eq!(live, live0 + 1, "acquire must raise the width gauge");
            assert_eq!(total, total0 + 1, "acquire must raise the lifetime counter");

            {
                let _t2 = SpawnTicket::acquire().expect("second concurrent ticket fits");
                assert_eq!(snapshot().0, live0 + 2);
            }
            // Inner ticket dropped — width released, total retained.
            assert_eq!(snapshot().0, live0 + 1, "drop must release the width slot");
            assert_eq!(
                snapshot().1,
                total0 + 2,
                "drop must NOT release lifetime budget"
            );
        }

        // Outer ticket dropped: width fully unwound, total still monotonic.
        assert_eq!(snapshot().0, live0, "every width slot must be released");
        assert_eq!(snapshot().1, total0 + 2, "lifetime counter only rises");

        // Force the pathological double-release: a ticket dropped when the
        // gauge is already at its floor must saturate, not wrap to u32::MAX
        // (which would wedge the width guard shut for the whole run). Kept in
        // THIS test fn rather than its own so it cannot race the assertions
        // above — both touch the same process-global counters.
        LIVE.store(0, Ordering::SeqCst);
        drop(SpawnTicket { _private: () });
        assert_eq!(snapshot().0, 0, "saturating release, never a wrap");

        // Restore both counters so this test leaves no residue for anything
        // else that reads the global snapshot.
        LIVE.store(live0, Ordering::SeqCst);
        TOTAL.store(total0, Ordering::SeqCst);
    }
}
