# Aish Audit Findings & Remediation Tracker

This document tracks findings from the comprehensive aish audit (run via `aish_audit.py`) and their remediation status.

## Finding #1: Memory Persistence Visibility 🔍 IN PROGRESS

**Severity:** CRITICAL (blocking root-cause analysis)

**Finding:** 62 reasoning events produced 0 stored memories, but system had no visibility into WHY.

**Root Cause:** Both `escalate()` and `reasoning_note()` wrapped memory writes with `let _ =`, silently discarding any database errors.

**Solution Implemented (PR #786):**
- `escalate()`: Match on `db.remember()` result, log success/error to stderr
- `reasoning_note()`: Match on `db.remember()` result, report in function output
- Check for missing database session and log that separately
- Best-effort: failures still never block the answer

**Impact:**
- ✅ System now has visibility into memory persistence failures
- ✅ Can debug why memories aren't being stored
- ⏳ Prerequisite to fixing the actual root cause once we see the error

**Status:** Waiting for stderr logs from deployed fix to identify actual failure reason.

**Next Step:** Once this lands in production and we see error logs, implement the real fix.

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

## Finding #2: Silent Memory Persistence Failures ⏳ IN PROGRESS (PR #786)

**Severity:** CRITICAL (no visibility into memory storage failures)

**Finding:** Same as Finding #1 but from a different angle: The code tried to store memories via `db.remember()`, but both call sites silently swallowed errors with `let _ =`.

**Root Cause:** 
- `escalate()` calls `db.remember()` but wraps it with `let _ =`
- `reasoning_note()` calls `db.remember()` but wraps it with `let _ =`
- Any database error (connection, permission, table schema) was silently discarded
- System had no way to know if memories were failing to persist

**Solution Implemented (PR #786):**
- `escalate()`: Match on `db.remember()` result, log success/error to stderr
- `reasoning_note()`: Match on `db.remember()` result, include in function output
- Add check for missing database session and log that separately
- Best-effort: failures still never block the answer

**Status:** Ready for testing. Once deployed, stderr logs will reveal the actual failure reason.

**Next Steps:**
1. Merge PR #786 to get error visibility
2. Monitor stderr logs to identify the actual failure  
3. Implement the real fix based on what we learn
4. Verify 100% of escalations create durable memories

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

**Status:** Deferred pending #1 & #2 resolution.

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

| Finding | Status | PR | Impact |
|---------|--------|----|----|
| Memory persistence visibility | ⏳ IN PROGRESS | #786 | Prerequisite to identifying root cause |
| Silent memory failures | ⏳ IN PROGRESS | #786 | Once deployed, stderr logs will reveal actual error |
| Coordinator stall detection | ⏳ PENDING | — | Safe to defer until memory issue is fixed |
| Escalation reason attribution | ⏳ PENDING | — | Threshold tuning prerequisite, defer |

**Current Focus:** 
- PR #786 adds comprehensive error logging to memory persistence
- Once deployed, stderr logs will reveal WHY memories aren't being stored
- Could be: missing database session, schema mismatch, permission issue, or something else

**Next Steps:**
1. Merge PR #786 to main
2. Deploy and monitor stderr logs to identify the actual memory persistence failure
3. Implement the real fix based on what the logs show
4. Verify ≥40 stored memories from next 62 reasoning events

**Sequencing Rationale:** Finding #1 and #2 are the same issue seen two ways. The visibility fix (#786) is prerequisite to finding and fixing the root cause.
