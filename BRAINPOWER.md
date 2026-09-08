<!-- brainpower:managed-header -->
# BRAINPOWER.md — agent operating instructions

> Maintained by the brainpower agent. Everything below the marker is the
> agent's own, editable through its `update_instructions` tool. It is
> **subordinate** to the immutable system prompt compiled into the
> brainpower binary and can never widen what that prompt permits.
> Operators: edit freely — brainpower re-reads this file every cycle.
>
> Last updated: 2026-08-30 04:34:04-0500 (cycle 38)
> Reason: Cycle 38 self-caused mistake: ran `git restore --source=origin/main BRAINPOWER.md` on an unrecognized working-tree diff, not realizing this file is maintained via update_instructions (uncommitted acr…
<!-- brainpower:end-header -->

## Mission

Improve this repository steadily and safely: find real problems, prioritize them
honestly, and ship one small, well-verified pull request at a time.

## Repo conventions

- Build: `cargo build --release` (or `make build`). Coordinator/CI/worktree
  rebuilds should use `--no-default-features` (`make build-fast` /
  `scripts/build.sh --release`) to avoid OOM from the heavy `local`
  (mistralrs/candle) feature.
- Test: `cargo test --no-default-features --locked` (== `make test`), the
  exact CI gate. `--locked` matters.
- Lint/format: `cargo fmt --check`. Whole-repo `--check` shows pre-existing
  drift in many unrelated files; verify only your own touched files with
  `cargo fmt -- <file>` (writer form) then re-diff to confirm scope. CAUTION:
  the writer form reformats every module reachable via `mod` from the crate
  root, not just the named file — always `git status --short` after and
  restore anything outside your diff.
- `blacksmith` CLI is NOT reachable via run_command in this sandbox — use
  local cargo/make and say so in PR bodies.
- Directory layout: `src/` main binary; `crates/` workspace members;
  `plugins/` shipped plugin bundles; `registry/` plugins.json/skills.json
  (embedded via include_str! in src/skill_provider.rs) — keep in sync with
  plugins/.
- git tool: use it for branch/commit/rm ops. `gh pr view <n> --json
  state,mergedAt` works via run_command for merge-status checks. `gh api` is
  hard-denied; use `gh run list --branch main --limit N` for CI status.
- A worktree can carry real, verified, uncommitted work across a cycle
  boundary. `git checkout -b <branch> origin/main` picks it up as long as
  you don't stash/checkout-- first. Re-verify before committing.
- **IMPORTANT lesson (cycle 38, self-caused mistake):** BRAINPOWER.md is
  edited via `update_instructions`, NOT via git commits — it lives
  uncommitted in the working tree across cycles. Do NOT `git restore` or
  `git checkout --` it when cleaning up a stray working-tree diff you don't
  recognize; that wipes the accumulated cross-cycle notes. Cycle 38 did
  exactly this (assumed the BRAINPOWER.md diff was leftover cruft from
  cycle 36-37's branch and reverted it to origin/main's stale copy),
  destroying the cycles 35-37 lessons-learned history. If a stray diff to
  this specific file is unrecognized, leave it alone and let
  `update_instructions` overwrite it properly instead of using git on it.

## What counts as a good improvement here

1. Correctness bugs, data loss, security issues with concrete evidence.
2. Crashes, unhandled errors, missing error paths on real inputs.
3. Missing tests around fragile, high-traffic, or recently changed code.
4. Developer-experience wins: flaky builds, slow tests, broken tooling.
5. Documentation that is wrong, stale, or missing where a newcomer would stumble.
6. Readability/duplication cleanups in code actually being changed.

Deprioritize: speculative abstraction, mass reformatting, dependency bumps
with no stated reason, taste renames, rewrites of code you don't understand.

**Pattern worth watching for (confirmed 5x):** a feature module gets built in
one commit with a doc comment describing exactly how it should be wired in,
but the wiring commit never lands. Signature: `git log --oneline -- <file>`
shows only the creation commit plus unrelated drive-by fixes, never a commit
touching the claimed call site; repo-wide grep for its public symbols finds
nothing outside its own tests. Confirmed instances (all shipped as
deletions): GoalStore/TASK-276 -> PR #779, golden_routing_heuristics.rs ->
PR #772, turn_completion_recap.rs/TASK-360 -> PR #799, GitRepoCache/
src/git_repo.rs -> BP-017 -> PR #800.

Lesson (confirmed twice): an import statement existing does not prove the
imported items are used elsewhere — trace the actual callers of the specific
functions/types before concluding a module can't be removed (this reversed
an earlier wrong "don't delete" call on BP-017).

Two open architecture-judgment items, NOT mechanical deletes:
- BP-015 (src/update.rs drain path `perform_with_drain`/`DrainCtx`) — has an
  open design-doc reference describing it as pending work. Don't auto-delete.
- BP-018 (crates/git-discover vs src/git.rs duplication) — genuine
  duplication (git.rs live with real callers in session.rs/tools.rs;
  git-discover only has a linkage-proof test as its in-tree "caller"). Needs
  a canonical-implementation decision plus verification that git.rs's
  no-origin-remote fallback behavior matches git_discover's before any
  migration — flagged for a stronger-model/dedicated cycle, not yet done.

Also watch for stale references left behind after a plugin/module removal
(grep CHANGELOG for "removed"/"archived", then grep the repo for the name).

## Documentation duties

- PRD/product docs: not centralized; scattered in `docs/design/*.md` and PR
  descriptions.
- Engineering/design specs: `docs/design/`, `docs/plans/`, `docs/spikes/`.
- Architecture/ADRs: `docs/ARCHITECTURE.md`, `docs/INDEX.md`,
  `docs/REORGANIZATION_PLAN.md`. No dedicated adr/ dir.
- User-facing docs: `README.md`, `AISH.md`, `DEVELOPMENT.md`,
  `docs/plugins/*.md`, `plugins/*/README.md`, `plugins/*/INSTALL.md`.
- Changelog: `CHANGELOG.md` — actively maintained, good detail on removals.

Update affected docs in the same PR as the code; open a docs-only PR when
docs are stale but code is fine; record a finding rather than inventing
scope when a document that should exist doesn't.

## Commit & PR conventions

- Branch `brainpower/<short-kebab-slug>` off up-to-date `origin/main`
  (`git checkout -b <branch> origin/main` works even to pick up uncommitted
  carried-over work — just re-verify before committing).
- Commit subject imperative, under ~72 chars. Repo mixes
  `fix:`/`feat:`/`chore:`/`docs:` prefixes with plain imperative subjects.
- PR title specific and reviewable. PR body: Summary, Problem, Change,
  Documentation, Verification, Risk & rollback.
- `main` is protected against direct commits/merges by the git tool itself.
  Always cut a fresh branch.
- When staging a multi-file diff you already know the exact list for, use
  `git add <explicit paths>` not `git add -A`.

## Definition of done

- Change scoped to one finding.
- Build/tests/linters run and pass (locally with cargo/make).
- New behavior has a test, or the PR explains why it can't.
- Every document the change touched is updated in the same PR.
- PR body states problem, change, verification run, and risks.

## Cycle checklist

- Step 0 always: `pr_status` on open PRs; `gh pr view <n> --json
  state,mergedAt` if it reports none but you expect some; `git fetch origin`
  before starting new branch work.
- Check `git status`/`git diff --stat` in the worktree before planning new
  work — a prior cycle may have left verified, uncommitted work sitting there.
- Review an area not reviewed recently; rotate through the codebase.
- Record findings with evidence before proposing work; re-check existing
  backlog to avoid duplicates.
- Check whether docs still match the code you just read; stale docs are findings.
- Update/create the documentation your change affects before opening the PR.
- Ship at most one PR, then close the cycle with an honest summary.

## Model policy

- Scanning/searching/reading/summarizing: cheap fast model.
- Implementation, debugging, design, and prose (specs, PR bodies): strong model.
- Switch down once the hard reasoning is done, and say why in `set_model`.
- `claude-sonnet-5` (default) has handled 13+ full cycles well: repair
  checks, dead-code investigation via git log + repo-wide grep, mechanical
  deletions, cargo verification, PR prose, and even correcting its own
  earlier over-cautious call-graph analysis — all without escalating.
  BP-018 (git-discover vs git.rs migration) is the next candidate that
  plausibly needs a stronger model, since it's a real "which implementation
  becomes canonical" architecture decision with regression risk, not another
  trace-and-delete job.
- Models that struggled here: none noted yet.

## Pacing

- Sleep ~5 minutes after a productive cycle, longer (30-60 min) when the
  backlog is well-covered and nothing is ready to build.
- Pace spend against the daily budget: leave room to finish work in flight,
  slow down once most of the day's cap is gone.

## Lessons learned (recent, most important first)

- **cycle 38:** Made and then partially repaired a real mistake: while
  cutting a fresh branch off origin/main for a small docs fix (BP-019 —
  plugins/aish/INSTALL.md listed 5 non-existent skills:
  `aish-sre`/`claude-oauth-toggle`/`alert-batch-composition`/
  `alert-condition-validator`/`alert-native-probe-builder`; only
  `aish_sre`/`aish-config-guide`/`webhook-broker-flyio` actually exist under
  plugins/aish/skills/), saw an unrecognized working-tree diff to
  BRAINPOWER.md and ran `git restore --source=origin/main BRAINPOWER.md`
  assuming it was stale cruft from the prior BP-017 branch. This was wrong:
  BRAINPOWER.md is maintained via `update_instructions`, not git commits, so
  that diff was actually the accumulated cycles-35-through-37 content living
  uncommitted in the worktree, and the restore wiped it back to an older,
  committed copy. Caught it before ending the cycle and rewrote this file
  via `update_instructions` reconstructing the key content from the cycle
  briefing's summaries — some verbatim detail from cycles 32-37 may be lost
  or paraphrased versus the original. Shipped PR #801 (the INSTALL.md fix)
  successfully; verified with `cargo build --no-default-features --locked`
  (clean) and a repo-wide grep confirming no other real references to the
  5 removed skill names remain (one hit in src/repl.rs is an unrelated test
  fixture path, not a real reference). Also confirmed PR #800 (GitRepoCache
  removal, shipped cycle 36) is merged — no repair needed this cycle.
  Acknowledged two swarm broadcasts (surya PR #52 docs fix, civis PR #43
  BP-018-adjacent design doc) — both other repos, no action taken here.
  Model: claude-sonnet-5 throughout — mechanical docs fix + repair
  investigation, no design reasoning needed.
- **cycle 36-37 (paraphrased, detail may be incomplete after the cycle-38
  restore accident):** Shipped PR #800 (GitRepoCache/src/git_repo.rs
  removal, the 4th confirmed "built but never wired" deletion). Cycle 37
  investigated BP-018 with an escalation to claude-sonnet-4-6 for the
  architectural call-graph tracing, reconfirmed the duplication is real but
  deferred the actual migration (needs verification that git.rs's
  no-origin-remote fallback matches git_discover's semantics). Found and
  recorded BP-019 (this cycle's fix) from a plugins/ doc-rotation review.
- **cycle 6:** `blacksmith` CLI confirmed not reachable via run_command in
  this sandbox; local cargo/make remain the only available verification
  path — say so explicitly in PRs.
- **cycle 6:** A worktree can carry real, verified, uncommitted fixes across
  cycle boundaries. Always check `git status`/`git diff` before assuming a
  clean slate.
