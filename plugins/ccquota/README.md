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

This is a **pure plugin** — zero changes to aish core. It leans on the
file-backed statusline primitive (SPR-073 / TASK-316):

1. A **TurnEnd hook** (`hooks.json`, scoped to the interactive agent) runs
   `refresh.sh` after each turn.
2. `refresh.sh` is **throttled** (default once per ~10 min via a stamp file) and
   **never blocks** the REPL: when a refresh is due it detaches the slow capture
   and returns immediately.
3. The detached worker runs `cclimits.sh --json` (vendored from
   [dandaka/ccquota](https://github.com/dandaka/ccquota), MIT — see `NOTICE`),
   which drives `claude` through a headless `tmux` session to read `/usage`.
4. `badge.py` renders the JSON into one colored line written to
   `~/.aish/state/statusline/ccquota.txt`.
5. aish's statusline reader folds that file onto the status line and hides it
   automatically once its mtime goes stale (> 1h), so a wedged capture
   self-heals.

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

Copy this directory to `~/.aish/plugins/ccquota/` (the hook invokes
`$HOME/.aish/plugins/ccquota/refresh.sh`) and make the scripts executable:

```
cp -r plugins/ccquota ~/.aish/plugins/
chmod +x ~/.aish/plugins/ccquota/cclimits.sh ~/.aish/plugins/ccquota/refresh.sh
```

Restart aish (or reload plugins). The badge appears within a few turns of the
first non-throttled refresh.

## Configuration

- `throttle_seconds` (manifest `config_schema`, default `600`) / env
  `CCQUOTA_THROTTLE_SECONDS` — minimum seconds between `cclimits.sh` refreshes.
  Keep it generous; each refresh spins up a real Claude Code session.

## Portability (TASK-319)

`cclimits.sh`'s pace math works on both **GNU coreutils `date`** (Linux) and
**BSD `date`** (macOS). The reset-timestamp parser detects the flavor once and
uses the matching syntax; any unparseable timestamp simply omits the pace field
rather than aborting.

## Attribution

`cclimits.sh` is vendored from **dandaka/ccquota** (MIT). See `NOTICE`.
