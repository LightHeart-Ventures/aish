# Telemetry & startup efficiency

aish records two kinds of telemetry to local SQLite/JSONL under `~/.aish/`:

- **Tool telemetry** — one row per tool call (`run_program`, `read_file`, …),
  used by `:telemetry` for latency/usage aggregates.
- **Reasoning telemetry** — a JSONL event log (escalate-vs-guess decisions and
  their outcomes) plus a rolled-up aggregate *memo*, surfaced by `:reasoning`.

The optimizations described here (write-batching, aggregate caching, JSONL
rotation, incremental memo folding, and update-check caching) exist purely to
keep the hot path cheap. They are **best-effort and have no user-facing
behavior change**: telemetry is never allowed to block or fail a real command,
and every knob below has a sensible default so the defaults "just work" with
nothing set. See
[`TELEMETRY_OPTIMIZATION_RECOMMENDATIONS.md`](../TELEMETRY_OPTIMIZATION_RECOMMENDATIONS.md)
for the design rationale and measured wins.

All overrides are environment variables read once, at session/component
construction. Set them before launching `aish`.

## Tool-telemetry write batching

Instead of one INSERT per tool call, rows are buffered and flushed in a single
transaction when the buffer fills **or** the flush timer elapses (whichever
comes first), then again on clean shutdown. This collapses ~N transactions into
~N/`BATCH_SIZE`.

| Env var | Default | Effect |
|---|---|---|
| `AISH_TELEMETRY_BATCH_SIZE` | `20` | Rows buffered before a size-triggered flush. Larger ⇒ fewer transactions, more rows at risk on a hard kill. |
| `AISH_TELEMETRY_FLUSH_SECS` | `5` | Max seconds a buffered row waits before a time-triggered flush. `0` flushes on every call (time-based batching off). |
| `AISH_TELEMETRY_UNBUFFERED` | *(unset)* | Truthy (`1`/`true`/`yes`/`on`) restores the legacy per-call INSERT path — no buffering at all. Handy for debugging. |

## `:telemetry` aggregate cache

The `:telemetry` summary is memoized so repeated invocations in a session don't
re-scan the table.

| Env var | Default | Effect |
|---|---|---|
| `AISH_TELEMETRY_CACHE_SECS` | `60` | Seconds a computed aggregate is reused before recompute. `0` disables the cache (always recompute). |

## Reasoning-log JSONL rotation

The reasoning event log grows append-only. When it crosses the threshold the
active file is rotated aside so the working file stays small.

| Env var | Default | Effect |
|---|---|---|
| `AISH_REASONING_ROTATE_MB` | `5.0` | Rotation threshold in MB (fractional allowed). `0` disables rotation (unbounded log). |
| `AISH_REASONING_LOG` | *(platform path)* | Override the event-log file path. Primarily for tests/isolation. |
| `AISH_REASONING_MEMO` | *(sibling of log)* | Override the aggregate-memo file path. Primarily for tests/isolation. |

## Incremental reasoning memo (`:reasoning`)

`:reasoning` reads a rolled-up **memo** rather than re-scanning the whole event
log every time. Normally only newly-appended events are folded in (O(new)
work), so `:reasoning` stays constant-time regardless of backlog size. Force a
full re-scan + memo rewrite when you suspect the memo has drifted:

| Env var | Default | Effect |
|---|---|---|
| `AISH_REASONING_MEMO_FORCE_RESCAN` | *(unset)* | Truthy (`1`/`true`/`yes`/`on`) rescans the entire event log and rewrites the memo from scratch on the next read. |

## Update-check caching

The "is a newer aish available?" check is cached so aish doesn't hit the GitHub
releases API on every launch.

| Env var | Default | Effect |
|---|---|---|
| `AISH_UPDATE_CHECK_TTL` | `86400` (24h) | Seconds the cached check result is trusted. `0` forces a fresh check on the next launch. |
| `AISH_UPDATE_CHECK_CACHE_PATH` | *(platform cache dir)* | Override where the update-check result is cached. |

### Forcing a fresh update check or reasoning rescan

```bash
# Re-query GitHub for the latest release right now (bypass the 24h cache):
AISH_UPDATE_CHECK_TTL=0 aish

# Rebuild the :reasoning memo from the full event log:
AISH_REASONING_MEMO_FORCE_RESCAN=1 aish
```

## Testing usage

The parse helpers behind every knob above are unit-tested, and the batching and
memo optimizations have at-scale integration tests. Run the telemetry suite
with the project's canonical test invocation:

```bash
cargo test --no-default-features --locked telemetry
```

Tests set `AISH_REASONING_LOG` / `AISH_REASONING_MEMO` (and the batching/cache
knobs) to point at temp paths so they never touch real telemetry.
