# Telemetry & Startup Efficiency Optimization Recommendations

**Date**: 2026-06-01  
**Author**: Code Review Excellence Analysis  
**Status**: PROPOSED  
**Priority**: P1 (medium-term efficiency win)

## Executive Summary

aish's telemetry and startup instrumentation (update checks, tool-call logging, reasoning decision tracking) are currently **best-effort but unbounded**, creating unnecessary:
- **Network overhead**: every startup hits GitHub API for release checks (no TTL cache)
- **Disk I/O amplification**: one SQLite insert per tool call (no batching)
- **Compute waste**: full-file scan on every `:reasoning` command (no memoization)
- **Storage growth**: unbounded append-only JSONL with no rotation

These are **not token-expensive** (no LLM input), but they create latency, I/O friction, and prevent scaling to high-frequency use (batch loops, CI workflows, rapid iteration). The fixes are **low-risk, mechanical, and high-ROI**.

---

## Problem Statement

### 1. Update Check: Zero Caching (~99% of startups)
**File**: `src/update.rs` + `src/repl.rs` (lines 207-230)

- Every startup spawns `crate::update::check()` → `gh release view` → network call
- **No caching**: no check happens if "I checked 6 hours ago"
- **No throttling**: no protection against GitHub's 60/hour unauthenticated API limit
- **Startup latency**: adds ~200-500ms per launch in normal network conditions
- **Impact**: In batch/test loops (10+ starts/minute), you hit rate limits or waste 100+ API calls daily

**Current behavior**: Intentionally silent on failure, but the silence costs network overhead.

### 2. Tool Telemetry: Per-Call Synchronous Writes
**File**: `src/tool_telemetry.rs` + `src/db.rs`

- Every tool call → `db.record_tool_event()` → SQLite INSERT
- **No batching**: one fsync per call in worst case; 50-tool runs = 50 fsyncs
- **Write amplification**: high variance in disk I/O; can block interactive responsiveness
- **Scale issue**: a 1000-turn agent run writes 1000 separate transactions

**Current behavior**: Best-effort, failures swallowed. But the attempts themselves are expensive.

### 3. Reasoning Telemetry: Unbounded File + Full Scans
**File**: `src/reasoning_telemetry.rs`

- Append-only `~/.aish/reasoning-telemetry.jsonl`, no rotation
- **`summarize()` is O(file)**; reads entire file on every `:reasoning` command
- **No caching**: a user running `:reasoning` twice in 1 minute re-scans the whole log
- **Storage growth**: file grows indefinitely; after 10k decisions (normal for a power user), scans become noticeable

**Current behavior**: Best-effort writes, skip-corrupted-lines on read. But every `:reasoning` command is now a full-file I/O operation.

---

## Proposed Optimizations

### 1. **Cache the Update Check (TTL File)** — HIGH ROI / LOW EFFORT
**Benefit**: Eliminates ~99% of startup network calls  
**Effort**: ~50 lines  
**Status**: SAFE (non-breaking, purely additive)

**Implementation**:
```
Location: ~/.config/aish/update-check.json
Schema:   { "last_check_ts": "2026-06-01T12:00:00Z", "latest_version": "0.26.0", "latest_tag": "v0.26.0" }

Logic:
  On startup, read cache (if exists)
  If now - last_check_ts < 24h:
    Use cached version → no network call
  Else:
    Spawn async gh release view (as today)
    Write result to cache (best-effort)
```

**Protection**: 24h default (configurable via `AISH_UPDATE_CHECK_TTL`). Respects GitHub's 60/hr limit; in practice, ~120 startups/day = 0 network calls after first check.

**Interaction**: Still falls back to network if cache is missing/stale/corrupt. No behavior change for users.

---

### 2. **Batch Tool-Telemetry DB Writes** — MEDIUM ROI / LOW-MEDIUM EFFORT
**Benefit**: Cuts fsync/write amplification in tool-heavy sessions  
**Effort**: ~100 lines (ring buffer + flush logic)  
**Status**: SAFE (changes internal DB layer, public API unchanged)

**Implementation**:
```
Changes to src/tool_telemetry.rs + src/db.rs:

- Add a ring buffer to Session (e.g., Vec<ToolEvent>, capacity 20)
- record() appends to buffer instead of direct INSERT
- Flush on:
  - buffer is full (20 events) → single transaction
  - timer fires (every 5s) → single transaction
  - session drop → graceful flush (best-effort)
- Keep query aggregation unchanged; buffered writes are transparent to :telemetry
```

**Trade-off**: Events take up to 5s to be visible in DB (query delay). Acceptable for observational telemetry.

---

### 3. **Memoize + Incrementally Update Reasoning Summary** — MEDIUM ROI / LOW EFFORT
**Benefit**: O(file) → O(1) on `:reasoning` hot path  
**Effort**: ~80 lines (memo struct + invalidation logic)  
**Status**: SAFE (new code path, old file format unchanged)

**Implementation**:
```
New file: ~/.aish/reasoning-telemetry-memo.json
Schema:   { "total": 500, "overall": { "escalated": 150, "guessed": 350, ... }, "by_complexity": {...}, "by_risk": {...}, "computed_from_line": 5000 }

Logic in summarize():
  1. Check if memo exists and file mtime hasn't changed since memo was written
  2. If fresh: deserialize memo, compute deltas for new lines only
  3. If stale: full recompute (as today) + write new memo
  
:reasoning command:
  - Read memo (O(1)) instead of scanning file
  - Fallback to full scan if memo is missing
```

**Invalidation**: File mtime or explicit `--force-rescan` flag.

---

### 4. **Rotate Reasoning JSONL at Size Threshold** — MEDIUM ROI / LOW EFFORT
**Benefit**: Unbounded growth → bounded storage + scan time  
**Effort**: ~30 lines (rotation + cleanup logic)  
**Status**: SAFE (archive old entries, no loss of data)

**Implementation**:
```
When ~/.aish/reasoning-telemetry.jsonl reaches 5 MB:
  1. Rotate to reasoning-telemetry.jsonl.1
  2. Compress .jsonl.1 to .jsonl.1.gz (optional, saves ~70%)
  3. Keep last 3 archives; delete older ones
  4. Recompute memo on first post-rotation summarize()
```

**Privacy note**: Older compressed logs are auditable for compliance/analysis but don't slow down the hot `:reasoning` path.

---

### 5. **Pre-Aggregate `:telemetry` SELECTs** — LOW-MEDIUM ROI / LOW EFFORT
**Benefit**: Avoid repeated expensive GROUP BY scans  
**Effort**: ~40 lines (cache + 60s invalidation)  
**Status**: NICE-TO-HAVE (doesn't block users, purely optimization)

**Implementation**:
```
Cache the aggregation result in Session:
  { "cached_at": ts, "totals": [...], "class_failures": [...], "retries": [...] }

:telemetry command:
  If cache age < 60s: return cached result
  Else: run aggregation queries, cache result, return

Invalidate on:
  - Tool call recorded (exact cache invalidation)
  - 60s timeout (loose invalidation)
```

---

## Rollout Plan

### Phase 1: Cache (ASAP)
- Implement update-check TTL cache
- **No behavior change**, non-breaking
- Test: verify startup skips network when cache is fresh
- **Target**: Next release or hotfix (high polish/low complexity ratio)

### Phase 2: Batch (Next Release)
- Implement tool-telemetry buffering
- Verify `:telemetry` aggregation still works correctly
- Test: high-throughput session (100+ tool calls) for reduced I/O
- **Target**: v0.27.0 or next planned release

### Phase 3: Memoize + Rotate (Next Sprint)
- Implement memo caching + JSONL rotation
- Test: verify `:reasoning` scales to 10k+ decisions
- **Target**: v0.27.0 or next sprint (pair with Phase 2 for batch releases)

### Phase 4: Pre-aggregate (Polish Pass)
- Add :telemetry caching
- **Target**: v0.28.0 or future (lowest priority)

---

## Testing Strategy

| Optimization | Unit Tests | Integration Tests | Notes |
|---|---|---|---|
| Update cache | Cache hit/miss/stale/corrupt | Startup with/without network | Mock time + filesystem |
| Tool batching | Buffer full/timer/flush | 100+ tool calls in session | Verify transaction count |
| Reasoning memo | Memo fresh/stale/invalid | `:reasoning` before/after 10k events | Profile O(file) → O(1) |
| JSONL rotation | Rotate at size | Accumulate 10k+ events | Verify archive + cleanup |
| Telemetry cache | Cache age/invalidation | Repeated `:telemetry` calls | Measure query latency |

---

## Configuration & Override Flags

All optimizations respect environment variables for testing/debugging:

```bash
# Update cache
AISH_UPDATE_CHECK_TTL=0          # Disable cache (test: always fresh)
AISH_UPDATE_CHECK_TTL=86400      # 1 day (default)
AISH_UPDATE_CHECK_CACHE_PATH=... # Override location

# Tool telemetry
AISH_TELEMETRY_BATCH_SIZE=20     # Buffer size
AISH_TELEMETRY_FLUSH_SECS=5      # Timer interval
AISH_TELEMETRY_UNBUFFERED=1      # Disable batching (test: per-call inserts)

# Reasoning telemetry
AISH_REASONING_MEMO_FORCE_RESCAN=1  # Disable memo cache
AISH_REASONING_ROTATE_MB=5           # Size threshold

# Telemetry cache
AISH_TELEMETRY_CACHE_SECS=60     # Aggregation cache TTL
```

---

## Rationale: Why These Fixes Matter

### For Interactive Sessions
- **Startup latency**: 24h cache eliminates 200-500ms per launch
- **Responsiveness**: batched writes reduce jank from fsync pauses
- **`:reasoning` command**: memoization makes the feedback loop instant

### For Batch Workflows
- **Rate limiting**: 60/hr GitHub API protection
- **Throughput**: batch writes scale to 1000+ tool calls
- **Cost**: no extra LLM tokens; pure infrastructure win

### For Long-Running Sessions
- **Storage**: rotation prevents unbounded JSONL growth
- **Observability**: memoized summary scales to 10k+ decisions
- **Auditability**: compressed archives preserve the full log

---

## Known Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Cache staleness (user expects latest release) | Default 24h TTL; users can force-check with `:update --refresh` or delete cache file |
| Buffered writes lose tail on crash | Flush on Drop + signal handlers (SIGTERM graceful shutdown) |
| Memo invalidation miss (file changes externally) | mtime check; fallback to full scan if memo is old |
| JSONL rotation data loss | Archive with .gz compression; keep 3 generations |

All optimizations remain **best-effort** — failures do not block interactive flow or turn execution.

---

## Success Metrics

Post-implementation, we expect to see:

| Metric | Target | Measurement |
|---|---|---|
| Startup latency (with cache hit) | < 100ms network overhead | `time aish -c 'exit'` × 10 (average 2nd+ run) |
| Tool-call write latency | < 10ms buffered vs 50-200ms direct | Session with 50+ tool calls; profile fsync count |
| `:reasoning` latency (10k events) | < 50ms vs 500-1000ms full scan | Run `:reasoning` command; measure time |
| JSONL storage (after 10k events) | < 2 MB vs 5-10 MB unrotated | Filesystem check after rotation threshold |
| GitHub API calls (daily) | < 2 (initial check + manual :update) vs 100+ | API call logs / rate-limit headers |

---

## References

- `src/update.rs`: Update check flow (no caching)
- `src/repl.rs`: Startup check (lines 207-230)
- `src/tool_telemetry.rs`: Per-call recording
- `src/reasoning_telemetry.rs`: JSONL append + summarize
- `.repospec.json`: Entrypoints & test targets

---

## Decision

**Recommendation**: Implement Phase 1 (update cache) immediately for high ROI/low risk; queue Phase 2–3 for next release cycle.

**Approval Required From**:
- [ ] Maintainer (verify no conflicting design decisions)
- [ ] No breaking changes; purely internal optimization

---

*This document was generated by code-review-excellence analysis on 2026-06-01. Follow-up PRs should reference this doc.*
