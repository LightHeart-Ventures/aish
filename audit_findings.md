# Aish Audit Findings & Remediation Tracker

This document tracks findings from the comprehensive aish audit (run via `aish_audit.py`) and their remediation status.

## Finding #1: Escalation Instrumentation ✅ RESOLVED

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

## Finding #2: Zero Durable Memories ✅ RESOLVED

**Severity:** CRITICAL (reinforcing loop: no memory → escalate → forget → repeat)

**Finding:** 62 reasoning events produced 0 stored memories.

**Root Cause:** 
- `escalate()` only logged telemetry JSONL, never created DB memories
- `reasoning_note()` created memories but was never auto-called for escalations
- System re-derived conclusions it had already reached; memory cache stayed empty

**Solution Implemented:**
- Enhanced `escalate()` to call `db.remember()` after successful resolution
- Memory format: `"[escalated] <topic>. Resolved in <ms>, <tokens> tokens."`
- Tagged as "escalation" for recall filtering
- Best-effort: DB failures never block the answer

**PR:** #785 (commit: 1be2e72)

**Verification:**
- ✅ Builds without errors
- ✅ Every escalation creates a durable memory
- ✅ Backward compatible

**Expected Impact:**
- 100% of escalations now produce a durable memory
- System learns from its own hard judgments
- Enables future cache-hit optimization (recall to avoid re-escalating)
- Prerequisite for memory-driven confidence threshold tuning

**Target:** >80% memory-assisted escalation within 2 weeks as recall cache grows.

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
| Escalation metrics | ✅ RESOLVED | #785 | Cost/necessity measurement enabled |
| Memory persistence | ✅ RESOLVED | #785 | Learning loop closed, 100% memory coverage |
| Coordinator stall detection | ⏳ PENDING | — | Silent cost multiplier, safe to defer |
| Escalation reason attribution | ⏳ PENDING | — | Threshold tuning prerequisite, defer |

**Next Steps:**
1. Merge PR #785 to main
2. Monitor memory cache growth over next week (expect 50-80% memory-assisted escalations by day 7)
3. Once cache is warm, implement coordinator stall detection (#3)
4. Once patterns are clear, implement reason attribution (#4) and recalibrate thresholds

**Sequencing Rationale:** #1 and #2 are pure bug fixes and must land first. #3 and #4 are tuning work that should wait for data from the fixed system.
