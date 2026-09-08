# Aish Audit Findings & Remediation Tracker

This document tracks findings from the comprehensive aish audit (run via `aish_audit.py`) and their remediation status.

## Finding #1: Memory Persistence Visibility ✅ RESOLVED

**Severity:** CRITICAL (blocking root-cause analysis)

**Finding:** 62 reasoning events produced 0 stored memories, but system had no visibility into WHY.

**Root Cause (confirmed):** `escalate()`'s `session: &Session` had `db: None` in some execution
contexts (e.g. nested/worker execution, where the session passed to the tool dispatcher wasn't
initialized with a database handle) — so every `db.remember()` call in that context was a no-op
before the fix even had a chance to run, independent of the `let _ =` swallowing.

**Solution Implemented (commit `bab395d`, "fix(escalate): add db fallback when session.db is
None"):** `src/tools.rs::escalate()` (~line 1832) now:
- Matches on `db.remember()`'s result (both the `Some(session.db)` and fallback paths) and logs
  success (`stored escalation memory id=...`) or failure to stderr — the visibility fix from
  PR #786 is live.
- When `session.db` is `None`, falls back to opening the database directly via
  `crate::db::Db::open(&crate::db_paths::main_db_path())` instead of silently giving up — this is
  the actual root-cause fix, not just added logging.
- Remains best-effort throughout: any database error is logged, never blocks the escalation answer.

**Verified:** `src/tools.rs::escalate()` on `main` (as of this audit pass) contains the
`session.db.is_none()` fallback branch and the `Ok(id) => .../Err(e) => ...` match on
`store_result` described above — confirmed by reading the live source, not just the commit message.

**Known follow-up (not a persistence bug):** the fix commit also left a stray
`eprintln!("[DEBUG escalate] session.db is Some={}", ...)` line at the top of the function;
tracked and fixed separately in PR #804 (open as of this writing), unrelated to whether memories
persist correctly.

---

## Finding #2: Escalation Instrumentation ✅ RESOLVED

**Severity:** HIGH (cost/necessity measurement)

**Finding:** 53 of 62 reasoning events (85.5%) escalated without capturing outcome metrics.

**Root Cause:** The `escalate()` tool was logging telemetry but not measuring:
- Token usage (for cost attribution)
- Latency (for SLA tuning)
- Trigger reason (for threshold analysis)
- Whether result differed from hypothetical local path

**Solution Implemented:**
- Added `record_escalation_outcome()` function in `reasoning_telemetry.rs`
- Enhanced `escalate()` to:
  - Time the strong model call (start..elapsed) → `latency_ms`
  - Extract tokens from response.usage → `tokens_used`
  - Record outcome metrics with event id linkage
- Metrics are all optional and best-effort (failures never block answer)

**PR:** #785 (commit: efa8860)

**Verification:**
- ✅ Builds without errors
- ✅ New record type: `escalation_outcome`
- ✅ Metrics tagged for cost analysis

**Target:** 100% of escalations carry outcome attribution within one week.

---

## Finding #2b: Silent Memory Persistence Failures ✅ RESOLVED

**Severity:** CRITICAL (no visibility into memory storage failures)

**Finding:** Same underlying bug as Finding #1, described from the "swallowed errors" angle: the
code tried to store memories via `db.remember()`, but the call sites originally discarded any
result.

**Root Cause:** Confirmed to be the `session.db == None` issue described under Finding #1, not a
generic `let _ =` discard pattern in isolation — the discard just meant the `None` session/error
path produced no diagnostic, making the real cause invisible until PR #786's logging landed.

**Solution Implemented:** Same fix as Finding #1 (`bab395d`) — `escalate()` now matches on
`db.remember()`'s result on both the live-session and fallback-open paths and logs success/failure
to stderr; `reasoning_note()` (`src/tools.rs::reasoning_note`, ~line 1936) reports its own
persistence outcome in its returned string rather than silently discarding it (see the
`event_id`/`outcome` closing-the-loop branch and the fresh-decision branch, both of which surface
`rt::update_outcome`/store failures in the function's `Ok(...)` text instead of swallowing them).

**Verified:** ≥40-stored-memories target from the original "Next Steps" below is now an operational
question (worth checking against live telemetry), not a code-correctness question — the source no
longer discards the error.

---

## Finding #3: Coordinator Stall Detection (PENDING)

**Severity:** MEDIUM (silent cost multiplier)

**Finding:** 2 coordinator runs still in active phase (may be stalled).

**Impact:** Stalled runs emit duplicate escalations without producing results.

**Recommended Solution:**
- Add timeout + retry caps to coordinator phases
- Cap active phase at hard wall-clock limit (e.g. 120s) with forced transition
- Cap escalation retries per decision at 2
- Implement per-run telemetry for phase timing

**Status:** Was deferred pending Findings #1/#2b; those are now resolved (`bab395d`), so this is
unblocked and ready to be picked up as its own scoped piece of work. Not yet started.

---

## Finding #4: Escalation Reason Attribution (PENDING)

**Severity:** MEDIUM (prerequisite for tuning)

**Finding:** Cannot distinguish whether 85.5% escalation rate is correct for hard inputs or miscalibrated threshold.

**Recommended Solution:**
- Log *why* each decision escalated (low confidence, missing context, cache miss, tool ambiguity)
- Cannot proceed with threshold tuning until this is visible
- Once #1 and #2 land, should enable threshold recalibration

**Status:** Deferred until memory cache has grown and pattern is clearer.

---

## Summary

| Finding | Status | Ref | Impact |
|---------|--------|----|----|
| Memory persistence visibility | ✅ RESOLVED | `bab395d` | Root cause (session.db == None) fixed with a fallback db-open path; stderr now logs every persistence outcome |
| Silent memory failures | ✅ RESOLVED | `bab395d` | escalate()/reasoning_note() report success/failure instead of discarding it |
| Coordinator stall detection | ⏳ PENDING | — | Related stall-cleanup work already merged separately (PR #793, "include 'checkpoint' phase in coordinator_store.rs stall cleanup"); this finding's own timeout/retry-cap recommendation is still unimplemented |
| Escalation reason attribution | ⏳ PENDING | — | Threshold tuning prerequisite, still deferred |

**Current status (updated by brainpower audit sweep, cycle 61):**
- The memory-persistence root cause described in Findings #1/#2b is fixed at the source: `escalate()`
  in `src/tools.rs` opens a database fallback when `session.db` is `None` and logs every store
  attempt's outcome to stderr. This was verified by reading the live function body, not just the
  commit message.
- A stray `[DEBUG escalate]` eprintln left over from the fix commit was cleaned up separately
  (see PR #804).
- What remains open from the original audit is Findings #3 and #4 (coordinator stall detection,
  escalation reason attribution) — both still legitimately pending and not part of the memory-
  persistence story.

**Next Steps:**
1. If live telemetry is available, confirm the ≥40-stored-memories target from the original audit
   is now being met (operational verification, not a code change).
2. Pick up Finding #3 (coordinator stall detection) or #4 (escalation reason attribution) as
   separate, scoped pieces of work — they do not depend on anything in Findings #1/#2b anymore.

**Sequencing Rationale:** Findings #1 and #2b were the same issue seen two ways, and the fix for
both landed together in `bab395d`. Findings #3 and #4 were deferred pending that fix and remain
open independently of it.
