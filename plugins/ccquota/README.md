# ccquota — Claude Code quota badge for aish

Surfaces your Claude Code subscription usage — **% used** and **burn-rate pace**
for the most-constrained window — as a segment on aish's SecondStatusLine:

```
… coordinator hint · ⚡cc 63%w ·142%
```

`63%w` = 63% of the weekly allowance used. `·142%` = you're burning it 42%
faster than the even-pace line (100% = perfectly on track to reset day).
The color goes dim → yellow → red as pressure rises.

## How it works

This plugin declares a **first-class statusline segment** (SPR-073 / TASK-318):
the manifest's `provides.statusline` block names a `command`, and aish **core**
owns everything else — the refresh cadence, the in-memory cache, the per-run
timeout, and staleness. The plugin never touches a cache file or picks a path.

```json
"provides": { "statusline": { "command": "statusline.sh", "every": "10m", "timeout_ms": 45000 } }
```

1. On startup, core arms one cheap detached loop for this statusline.
2. Every `every` (~10 min), off the agent turn loop, core runs `statusline.sh`
   with a 45s timeout.
3. `statusline.sh` runs `cclimits.sh --json` (vendored from
   [dandaka/ccquota](https://github.com/dandaka/ccquota), MIT — see `NOTICE`),
   which drives `claude` through a headless `tmux` session to read `/usage`,
   then pipes it through `badge.py` to render **one colored line**.
4. Core caches that line in memory and folds it onto the status line, hiding it
   automatically once it goes stale (a wedged capture self-heals).

> **Migrated from Phase 1.** Earlier versions used a throttled `TurnEnd` hook
> (`refresh.sh` + `hooks.json`) that owned the throttle stamp, a single-flight
> lock, a detached capture, and wrote `~/.aish/state/statusline/ccquota.txt` for
> the file-backed reader (TASK-316). All of that plumbing now lives in core;
> `refresh.sh` and `hooks.json` are gone and `statusline.sh` just prints a line.

## Requirements

| Tool      | Why                                   | If missing                        |
|-----------|---------------------------------------|-----------------------------------|
| `claude`  | source of `/usage`                    | plugin no-ops (no badge)          |
| `tmux`    | headless driver for Claude Code       | plugin no-ops (no badge)          |
| `python3` | JSON → badge formatting               | plugin no-ops (no badge)          |
| `bash`    | the scripts                           | required                          |

Every dependency is checked; a missing one degrades to **no badge**, never an
error that disrupts your turn.

## Install

Copy this directory to `~/.aish/plugins/ccquota/` and make the scripts
executable:

```
cp -r plugins/ccquota ~/.aish/plugins/
chmod +x ~/.aish/plugins/ccquota/cclimits.sh ~/.aish/plugins/ccquota/statusline.sh
```

Restart aish (or reload plugins). The badge appears after the first refresh
(~a couple seconds past the startup settle, then every `every`).

## Configuration

Cadence and timeout live in the manifest `provides.statusline` block:

- `every` (default `"10m"`) — how often core runs `statusline.sh`. Keep it
  generous; each refresh spins up a real Claude Code session through tmux.
- `timeout_ms` (default `45000`) — per-run wall-clock budget; an overrun is
  killed and the prior badge ages out.

## Portability (TASK-319)

`cclimits.sh`'s pace math works on both **GNU coreutils `date`** (Linux) and
**BSD `date`** (macOS). The reset-timestamp parser detects the flavor once and
uses the matching syntax; any unparseable timestamp simply omits the pace field
rather than aborting.

## Attribution

`cclimits.sh` is vendored from **dandaka/ccquota** (MIT). See `NOTICE`.
