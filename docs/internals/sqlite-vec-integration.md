# sqlite-vec Integration Strategy for aish Workers

## Context
aish uses SQLite with sqlite-vec already loaded in `db.rs` via `sqlite3_auto_extension`. Current adoption is partial:
- ✅ `vec_memories` (vec0) table exists for semantic search on agent memories  
- ⚠️ `history` table is plain (no vector index)  
- ⚠️ `offloads` (context offload transcripts) are plain  
- ❌ Worker transcripts (`worker_store.rs`) are JSON-line files, not SQL-queryable

## Three Strategic Opportunities

### 1. History Table: Add Vector Index for Session Replay
**Current schema:**
```sql
CREATE TABLE history (
    id INTEGER PRIMARY KEY,
    ts TEXT DEFAULT current_timestamp,
    cwd TEXT,
    kind TEXT CHECK (kind IN ('input', 'output')),
    content TEXT
);
```

**Enhancement:** Mirror `vec_memories` approach with a vec0 companion table:
```sql
CREATE VIRTUAL TABLE IF NOT EXISTS vec_history USING vec0(
    history_id INTEGER PRIMARY KEY,
    embedding float[384]  -- Same as vec_memories for consistency
);
```

**Benefit:**
- Session replay (`:attach`) can rank historical context by semantic relevance
- Search assistant interactions by intent, not just keyword
- Detect/cluster similar errors across sessions

**Implementation in `db.rs`:**
- Add `vec_history` creation in schema init
- Wire embedding generation into `write_history()` at append time
- Expose `search_history(query_embedding, k)` reader for semantic recall

---

### 2. Offloads Table: Index Context Transcripts
**Current schema:**
```sql
CREATE TABLE offloads (
    id INTEGER PRIMARY KEY,
    ts TEXT DEFAULT current_timestamp,
    content TEXT  -- 1+ MB transcripts moved out of memories
);
```

**Enhancement:** Make offloads vector-searchable without loading full content:
```sql
-- Attach vector index
CREATE VIRTUAL TABLE IF NOT EXISTS vec_offloads USING vec0(
    offload_id INTEGER PRIMARY KEY,
    embedding float[384],
    truncated_preview text  -- First 200 chars (metadata column)
);

-- Create FTS5 companion for keyword search
CREATE VIRTUAL TABLE IF NOT EXISTS offloads_fts USING fts5(
    content=offloads,
    content_rowid=id,
    content  -- Full-text indexed
);
```

**Benefit:**
- Offload rehydration (context-offload recall) is O(log N) instead of O(N)
- Hybrid search: "find context about payment failures from last sprint" works
- Bounded memory footprint for recall (only load matched offloads)

**Implementation in `db.rs`:**
- Extend schema init with `vec_offloads` + FTS5
- Backfill embeddings on first migration run
- Expose `search_offloads(query, k)` that returns offload ids + preview + distance

---

### 3. Worker Transcripts: SQLite-Backed Journal (Major)
**Current implementation:** Per-worker JSON-line files (`transcript.jsonl`)
- ✅ Crash-safe appends, bounded cap with rotation
- ❌ No SQL queryability, can't correlate across workers
- ❌ Resume from checkpoint requires full file re-parse

**Strategic refactor:**
```sql
-- Per-worker transcript table (one master table, indexed by worker_id)
CREATE TABLE worker_transcripts (
    id INTEGER PRIMARY KEY,
    worker_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    ts TEXT DEFAULT current_timestamp,
    role TEXT CHECK (role IN ('user', 'assistant', 'system', 'tool')),
    kind TEXT CHECK (kind IN ('text', 'tool_call', 'tool_result', 'narration', 'synthesis', 'truncation')),
    tool_name TEXT,  -- For tool_call / tool_result
    content TEXT NOT NULL,  -- Redacted (preserves AC8 secrets policy)
    is_error BOOLEAN DEFAULT 0,
    PRIMARY KEY (worker_id, seq)
);

-- Companion vector index (for "what did this worker do?" retrieval)
CREATE VIRTUAL TABLE IF NOT EXISTS vec_worker_transcripts USING vec0(
    transcript_id INTEGER PRIMARY KEY,
    embedding float[384],
    worker_id TEXT PARTITION KEY,
    kind TEXT PARTITION KEY
);

CREATE INDEX idx_transcripts_worker_ts ON worker_transcripts(worker_id, ts);
```

**Benefit:**
- `:attach <worker-id>` can load transcript 100x faster (SQL seek vs. file parse)
- Resume from checkpoint: `SELECT * FROM worker_transcripts WHERE worker_id = ? AND seq > ?` direct
- Semantic queries: "show me all inference loops across all workers"
- Retention sweep is a SQL `DELETE` with age/status predicates (no `should_sweep_worker` predicate)
- Durable worker lifecycle: `status` field moves to table, `meta.json` becomes optional

**Migration path** (non-breaking):
1. Create the new tables alongside existing JSON files
2. On coordinator start: backfill transcript rows from `transcript.jsonl` if not already there
3. `TranscriptWriter` appends to BOTH files and table simultaneously (until cutover)
4. `:attach` and retention logic reads from SQL first, falls back to files
5. Deprecate `worker_store.rs` file paths once all old workers are reaped

---

## Phased Rollout

| Phase | Focus | Effort | ROI |
|-------|-------|--------|-----|
| **P1** | History + Offloads vec0 tables | 2-3h | High (improves recall quality immediately) |
| **P2** | Worker transcripts SQL backing (non-breaking dual-write) | 4-6h | Very High (unlocks queries, perf) |
| **P3** | File-to-SQL migration script | 1-2h | Medium (cleanup) |
| **P4** | Full cutover (retire JSON files) | 0.5h | Low (tech debt payoff) |

---

## Implementation Notes

### Embedding Strategy (All Three)
- Use the **same 384-dim model** across history, offloads, transcripts for consistency
- **Lazy embedding:** Generate on first semantic query, cache in vec0
- **Batch backfill:** `python scripts/backfill_embeddings.py` for historical data
- API: Client code calls e.g. `db.search_history(query_text, k=5)` → embeds query, runs KNN

### Crash Safety & Atomicity
- **vec0 inserts:** Wrap with transaction (`BEGIN; INSERT INTO t VALUES (...); INSERT INTO vec_t VALUES (...); COMMIT;`)
- **Dual-write (P2):** File append success doesn't wait for SQL; SQL insert is best-effort (keep existing AC edge semantics)

### AC (Assurance Criteria) Compliance
- AC8 (secrets): Redaction happens pre-storage (existing `turn_audit::redact_input` logic)
- AC6 (cap): Transcript table grows unbounded by design; use partition key sharding or a separate rotation trigger
- AC7 (retention): Move from `should_sweep_worker` predicate to SQL WHERE clause: `DELETE WHERE status IN ('done','failed') AND ts < now() - ? days AND worker_id NOT IN (SELECT id FROM branches)`

### Testing Strategy
1. Unit tests: `db::tests` — vec0 insert/query parity with file transcripts
2. Integration: S9.3 resume tests — `:attach` against both file and SQL backends, compare history
3. Fuzzing: Rotation + concurrent reads (SQLite WAL handles this)

---

## Success Metrics
- **History recall:** 3-5 top results when agent self-asks "what did I do last week?"
- **Worker queries:** `SELECT COUNT(*) FROM worker_transcripts WHERE kind='tool_call' AND tool_name='run_program'` executes in <100ms
- **Perf:** Resume from checkpoint 10x faster (measure `:attach` latency)
- **Retention:** Sweep logic expressed in 5 lines of SQL vs. current 40-line predicate

---

## References
- sqlite-vec skill: `/home/grhohertz/.aish/skills/sqlite-vec/SKILL.md`
- Current db.rs: `src/db.rs` (lines 1–100 show schema; migrate_memory_store is the pattern)
- Worker store: `src/worker_store.rs` (TranscriptWriter, append_record, read_records)
