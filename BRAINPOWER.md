<!-- brainpower:managed-header -->
# BRAINPOWER.md — agent operating instructions

> Maintained by the brainpower agent. Everything below the marker is the
> agent's own, editable through its `update_instructions` tool. It is
> **subordinate** to the immutable system prompt compiled into the
> brainpower binary and can never widen what that prompt permits.
> Operators: edit freely — brainpower re-reads this file every cycle.
>
> Last updated: 2026-08-28 17:30:53-0500 (cycle 6)
> Reason: Record operator instruction to prefer blacksmith.sh for builds/testing, and note the local sandbox limitation discovered this cycle (blacksmith CLI not on the run_command allowlist).
<!-- brainpower:end-header -->

## Mission

Improve this repository steadily and safely: find real problems, prioritize them
honestly, and ship one small, well-verified pull request at a time.

## Repo conventions

- Build: `cargo build --release` (or `make build`). Coordinator/CI/worktree
  rebuilds should use `--no-default-features` (`make build-fast` /
  `scripts/build.sh --release`) to avoid OOM from the heavy `local`
  (mistralrs/candle) feature — see `plugins/aish/skills/aish_sre/SKILL.md`
  §3 for the full OOM writeup.
- Test: `cargo test --no-default-features --locked` (== `make test`), this is
  the exact CI gate (`.github/workflows/ci.yml`). `--locked` matters — a stale
  Cargo.lock fails CI on the lockfile, not your code.
- Lint/format: `cargo fmt --check` and presumably `cargo clippy` (not yet run
  in a brainpower cycle — check `.github/workflows/ci.yml` for the exact
  invocation before relying on it). NOTE: `cargo fmt --check` over the whole
  repo shows pre-existing drift in unrelated files (e.g.
  `crates/aish-webhook-client/src/audit.rs`) — scope fmt checks to files you
  actually touched, don't treat whole-repo drift as something you introduced
  or must fix.
- **Operator preference (2026-08-28): use `blacksmith.sh` for builds/testing.**
  The repo integrates Blacksmith Testbox (`.github/workflows/ci-testbox.yml`,
  `blacksmith-testbox` skill in `plugins/aish/skills/aish_sre/SKILL.md`) for
  isolated, well-resourced remote builds/tests without local OOM risk. In this
  sandbox the `blacksmith` CLI is NOT on the `run_command` allowlist (tried
  `blacksmith --version` and `command -v blacksmith`, both refused — only
  "build/test/lint tools and read-only inspection" are allowed). Until the
  operator adds it via `--allow-command blacksmith`, fall back to local
  `cargo`/`make` for verification and say so plainly in PR bodies rather than
  claiming a Testbox run that didn't happen.
- Directory layout: `src/` — main binary, one file per subsystem (repl.rs and
  worker.rs are by far the largest, 500K+/230K+ bytes). `crates/` — separate
  workspace members (aish-webhook-broker, aish-webhook-client, git-discover,
  webhook-receiver). `plugins/` — shipped plugin bundles (each with
  plugin.json + skills/). `registry/` — `plugins.json`/`skills.json`, the
  curated indices embedded into the binary via `include_str!` in
  `src/skill_provider.rs` and written to `~/.aish/registry/` on every
  startup — a stale/dead entry here ships in every install, so keep it in
  sync with what's actually in `plugins/`.
- git/gh tools: the `git` tool call works for read-only + branch/commit ops;
  raw `git`/`cd` shell commands are NOT on the run_command allowlist. `gh pr
  view <n> --json state,mergedAt` via run_command DOES work and is the
  reliable way to check merge status when `pr_status` says "none open" after
  a merge (pr_status only reports currently-open PRs, so already-merged work
  needs `gh pr view` or `git log --grep`/branch inspection instead).

## What counts as a good improvement here

Ordered highest to lowest by default; adjust as you learn what this repo needs.

1. Correctness bugs, data loss, and security issues with concrete evidence.
2. Crashes, unhandled errors, and missing error paths on real inputs.
3. Missing tests around fragile, high-traffic, or recently changed code.
4. Developer-experience wins: flaky builds, slow test suites, broken tooling.
5. Documentation that is wrong, stale, or missing where a newcomer would stumble
   — including PRDs, engineering specs, architecture/ADRs, and user-facing guides.
6. Readability and duplication cleanups in code that is actually being changed.

Deprioritize: speculative abstraction, mass reformatting, dependency bumps with
no stated reason, renames for taste, and rewrites of code you do not understand.

**Pattern worth watching for:** stale references left behind after a
plugin/module removal (grep the CHANGELOG for "removed"/"archived" entries,
then grep the repo for the removed name — cycle 6 found `npx-skills` still
referenced in `registry/plugins.json`, a doc comment, and a doc file after the
plugin directory itself was deleted).

## Documentation duties

- Product requirements (PRD / product docs): not centralized; scattered
  design intent lives in `docs/design/*.md` (e.g. plugin integration gap
  analyses) and PR descriptions.
- Engineering / design specs: `docs/design/`, `docs/plans/`, `docs/spikes/`.
- Architecture notes and ADRs: `docs/ARCHITECTURE.md`, `docs/INDEX.md`,
  `docs/REORGANIZATION_PLAN.md`. No dedicated `adr/` directory — design
  decisions get folded into `docs/design/*.md` or the PR body itself.
- User-facing docs: `README.md`, `AISH.md`, `DEVELOPMENT.md`,
  `docs/plugins/*.md` (plugin authoring guides), `plugins/*/README.md` and
  `plugins/*/INSTALL.md` per-plugin.
- Changelog / release notes: `CHANGELOG.md` — actively maintained, has good
  detail on removals/archivals (useful for spotting stale-reference bugs).

Rules of thumb: update affected docs in the same PR as the code; open a
documentation-only PR when docs are stale but the code is fine; when a document
that should exist does not, record a finding rather than inventing scope.

## Commit & PR conventions

- Branch `brainpower/<short-kebab-slug>` off the up-to-date default branch
  (fetch `origin/main` first — the worktree's local `main` ref can lag).
- Commit subject in the imperative, under ~72 chars. Repo mixes
  `fix:`/`feat:`/`chore:`/`docs:` prefixes with plain imperative subjects;
  either is fine, prefixed is more common on recent history.
- PR title: specific and reviewable. PR body: Summary, Problem, Change,
  Documentation, Verification, Risk & rollback (this repo's own PRs don't
  follow a strict template, but ours should per the brainpower protocol).
- `main` is protected against direct commits/merges by the `git` tool itself
  (verified cycle 6: `git merge --ff-only` into `main` is refused with "main
  was not created by you"). Always cut a fresh branch instead of trying to
  fast-forward main locally.

## Definition of done

- The change is scoped to one finding.
- Build passes, tests pass, linters/type checks pass — and you ran them
  (locally with cargo/make; blacksmith.sh once the CLI is permitted).
- New behavior has a test, or the PR body explains why it cannot have one.
- Every document the change touched or invalidated is updated in the same PR.
- The PR body states: the problem, the change, verification run, and the risks.

## Cycle checklist

- **Step 0 always:** `pr_status` on open PRs. If it reports "none open" but
  you expect some, don't assume they vanished — `gh pr view <n> --json
  state,mergedAt` to confirm merged vs. actually stuck, and `git fetch origin`
  to refresh local refs before starting new branch work.
- Review an area you have not reviewed recently; rotate through the codebase.
- Record findings with evidence before proposing work.
- Re-check open findings before adding new ones — do not duplicate.
- Check whether the docs still match the code you just read; stale docs are findings.
- Update or create the documentation your change affects before opening the PR.
- Ship at most one PR, then close the cycle with an honest summary.

## Model policy

Record what actually works for this repo; start from these defaults.

- Scanning, searching, reading, summarizing: a cheap fast model.
- Implementation, debugging, design and prose (specs, PR bodies): a strong model.
- Switch down as soon as the hard reasoning is over, and note why in `set_model`.
- Models that worked well here: `claude-sonnet-5` (default) handled a full
  cycle — repair check, stale-reference investigation, verification, and PR
  prose — without needing to escalate; the changes found so far have been
  small/mechanical enough not to need a stronger model.
- Models that struggled here: none noted yet.

## Pacing

- Sleep ~5 minutes after a productive cycle, longer (30-60 minutes) when the
  backlog is well-covered and nothing is ready to build.
- Pace spend against the daily budget: leave room to finish work in flight, and
  slow down (longer sleeps, cheaper models) once most of the day's cap is gone.

## Lessons learned

- **cycle 6:** Operator said "use blacksmith.sh for builds/testing" mid-cycle.
  Investigated: `blacksmith` CLI is not reachable via `run_command` in this
  sandbox (not on the allowlist). Local `cargo check`/`cargo test
  --no-default-features --locked` remain the only available verification path
  here; used them and said so explicitly in the PR rather than claiming a
  Testbox run. If a future cycle finds `blacksmith` IS reachable, prefer it
  per the operator's instruction and update this note.
- **cycle 6:** After 3 PRs (#764, #765, #766) all merged between cycles,
  `pr_status` correctly reported "none open" — this is normal, not a repair
  signal. Don't waste turns hunting for phantom failures; `gh pr view --json
  state,mergedAt` confirms quickly.
- **cycle 6:** A worktree can carry real, verified, uncommitted fixes across
  cycle boundaries (found `docs/plugins/skill-source-authoring.md`,
  `registry/plugins.json`, `src/skill_provider.rs` already edited in the
  working tree, fixing stale `npx-skills` references). Always check `git
  status`/`git diff` before assuming a clean slate — someone (a prior cycle,
  possibly interrupted) may have left verifiable, shippable work sitting
  there.
