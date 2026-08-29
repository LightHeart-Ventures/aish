# Issue: Reasoning Events Not Persisted to Memory

## Problem
Audit finding #1 severity: **Zero memories created despite 63 reasoning events logged**.

The `reasoning_note()` tool is being called during reasoning steps, and telemetry is recorded in `reasoning-telemetry.jsonl`, but:
1. The tool calls to `reasoning_note()` are NOT actually writing to the durable memory store
2. Only 3/63 events have been closed with outcomes — the rest remain in "pending" state
3. Without closure, escalation decisions cannot accumulate experience — every decision pays full cost again

## Root Cause
The `reasoning_note()` MCP tool implementation in the aish framework has a broken write path. Telemetry logging (JSON line writes) works fine, but the memory persistence (the tool's primary purpose) is failing silently.

## Solution
Two scripts added to scripts/ directory:

### 1. `export_reasoning_memories.py`
Extracts all reasoning events from telemetry that have outcomes and exports them in a format suitable for bulk import to durable memory. This allows recovery of lost insights from completed reasoning cycles.

Usage:
```bash
python3 scripts/export_reasoning_memories.py
```

Output: JSON lines format, one memory per line, ready for import.

### 2. `persist_reasoning_events.py`
Scans telemetry, identifies closed reasoning events, and prepares them for persistence to the memory system. This is a staging tool for the actual fix.

## Follow-Up Action
These scripts are diagnostic/recovery tools. The actual fix requires:
1. **Framework-side**: Fix the `reasoning_note()` tool implementation to actually write to durable memory
2. **Database-side**: Verify the memory table write permissions and connection pool
3. **Config-side**: Check if memory persistence is disabled in aish configuration

Once the framework fix lands, run:
```bash
python3 scripts/export_reasoning_memories.py | python3 scripts/import_memories.py
```

To backfill the lost reasoning memories and restore the feedback loop.
