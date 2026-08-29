# Development — Worktree Lifecycle & Hygiene

aish spawns background **coordinators** and interactive dispatches into isolated
git worktrees under `~/.aish/worktrees/<owner--repo>/<id>`. Without discipline
these accumulate: merged branches never get pruned, abandoned coordinator trees
pile up, and stale `release/*` checkouts collide on merge. SPR-064 formalizes the
retention policy and ships the tooling that keeps the pool clean.

## Retention policy

SPR-064 originally centralized these values in a repo-root `.repospec.json`
(`worktreeRetention` key), but that file was removed in
`chore: remove .repospec.json integration (deprecation)` shortly after SPR-064
landed. There is no live config file today — `ttlIdleDays` is a hardcoded
default (`ttl_days=30`) in `scripts/audit-worktrees.sh` and
`scripts/cleanup-worktrees.sh` (each optionally re-reads `.repospec.json` if
one happens to exist, but the repo does not ship one, so this always falls
through to the default). The other fields below are documentation-only
convention, not enforced by any script:

| Field | Default | Meaning | Enforced by |
|-------|---------|---------|-------------|
| `ttlIdleDays` | `30` | A worktree idle (no new commit) longer than this is reclaimable. | `scripts/audit-worktrees.sh`, `scripts/cleanup-worktrees.sh` (hardcoded default; override with `--ttl N`) |
| `maxConcurrentPerBranch` | `1` | At most one worktree per branch — duplicates are a collision smell. | convention only — not checked by any script |
| `allowedPrefixes` | `feat/ fix/ docs/ aish/w_ release/` | Branch prefixes worktrees may use. | convention only — not checked by any script |
| `autoCleanupSchedule` | `0 2 * * *` | Cron for the nightly TTL reaper. | `.github/workflows/worktree-hygiene.yml` schedule trigger |
| `worktreeRoot` | `~/.aish/worktrees` | Where worktrees are created. | convention only — not checked by any script |

## Lifecycle

1. **Create** — a coordinator/dispatch adds `~/.aish/worktrees/<repo>/w_<id>` on a
   fresh branch off clean trunk (or `--base head`).
2. **Work** — commits land on the branch; a PR is opened.
3. **Complete** — when the card reaches `col_completed` (PR merged), the worktree
   is force-removed and its merged branch pruned by
   `scripts/remove-worktree-on-complete.sh` (board-driven, TASK-327).
4. **Sweep** — anything missed is caught by the nightly TTL reaper (TASK-328) and
   surfaced by the monthly audit (TASK-329).

## Tooling

| Script | Task | What it does |
|--------|------|--------------|
| `scripts/check-branch-freshness.sh` | TASK-326 | CI gate: fails a PR/push on a stale `release/*` branch (behind the latest release tag). Fresh `feat/ fix/ docs/` pass. |
| `scripts/remove-worktree-on-complete.sh <id\|branch>` | TASK-327 | Board-driven auto-delete: force-removes the worktree for a finished task and prunes its merged branch. No-op safe. Logs to `.worktree-cleanup.log`. |
| `scripts/cleanup-worktrees.sh [--apply] [--ttl N]` | TASK-328 | Nightly reaper: removes worktrees merged-to-`origin/main` **or** idle `>ttlIdleDays`, prunes merged branches. **Dry-run by default** — pass `--apply` to act. |
| `scripts/audit-worktrees.sh [out.csv]` | TASK-329 | Read-only classifier: writes `.worktree-audit.csv` (`STATE,name,branch,commit,age_days,path`) + summary counts. |

All four are wired into [`.github/workflows/worktree-hygiene.yml`](./.github/workflows/worktree-hygiene.yml):
the fresh-branch gate runs on every PR/push; a scheduled job runs the audit +
dry-run cleanup nightly and uploads the CSV as an artifact.

### Running the reaper on the dev host

GitHub runners can't see your local worktree pool, so the actual disk
reclamation runs where the worktrees live. Cron:

```cron
# nightly worktree reaper (matches the schedule trigger in worktree-hygiene.yml)
0 2 * * * cd ~/projects/aish && bash scripts/cleanup-worktrees.sh --apply >> ~/.aish/worktree-cron.log 2>&1
```

or a systemd timer (`~/.config/systemd/user/aish-worktree-cleanup.{service,timer}`)
calling the same command. Always eyeball a plain `scripts/cleanup-worktrees.sh`
(dry-run) first.

### Wiring the board-driven auto-delete (TASK-327)

`remove-worktree-on-complete.sh` is the action; trigger it when a card enters
`col_completed`. Options:

- **Atum board automation / webhook** — on `card_moved` with `toColumn=col_completed`,
  run the script with the card's worktree id (`w_<id>`) or branch.
- **Local post-merge hook** — after merging a PR, call the script with the merged
  branch name.

The script resolves the matching worktree from `git worktree list --porcelain`,
force-removes it, and prunes the branch only if it is merged to trunk — exiting
`0` (no-op) if the tree is already gone.

## Building the optional voice feature (SPR-068 / FR-334)

Push-to-talk voice input is gated behind the `voice` Cargo feature and is **off
by default** — the standard build and the CI `--no-default-features --locked`
gate never pull in the audio stack. To build or work on it you must opt in and
install the native prerequisites the audio + Whisper crates compile against.

```bash
# Linux (Debian/Ubuntu): ALSA headers for cpal + a C/C++ toolchain for whisper-rs
sudo apt-get install -y libasound2-dev cmake clang libclang-dev build-essential

# Build / run with the feature enabled
cargo build   --features voice
cargo run     --features voice
cargo clippy  --features voice --all-targets
```

| Prereq | Needed by | Why |
|--------|-----------|-----|
| `libasound2-dev` | `cpal` | ALSA capture backend on Linux (CoreAudio on macOS needs no extra pkg). |
| `cmake`, `clang`, `libclang-dev` | `whisper-rs` | Builds the bundled `whisper.cpp` C/C++ sources + bindgen. |
| `build-essential` | native crates | C/C++ compiler + linker. |

macOS needs no extra system packages (CoreAudio + the Xcode CLT toolchain
cover it). The `voice-api` feature (`--features voice-api`) swaps local Whisper
inference for a hosted endpoint and pulls the same capture front-end.

The optional stack is exercised in CI by
[`.github/workflows/voice-feature-build.yml`](./.github/workflows/voice-feature-build.yml),
which builds `--features voice` on PRs that touch voice paths. It is
**non-gating** until the `src/voice.rs` module lands, so it never blocks
unrelated merges.
