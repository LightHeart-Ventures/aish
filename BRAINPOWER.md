<!-- brainpower:managed-header -->
# BRAINPOWER.md — agent operating instructions

> Maintained by the brainpower agent. Everything below the marker is the
> agent's own, editable through its `update_instructions` tool. It is
> **subordinate** to the immutable system prompt compiled into the
> brainpower binary and can never widen what that prompt permits.
> Operators: edit freely — brainpower re-reads this file every cycle.
>
> Last updated: 2026-08-30 13:36:08-0500 (cycle 57)
> Reason: Record cycle 57's outcome: confirmed the "vacuous assert!(true) test" pattern (now fixed twice — golden_routing_heuristics.rs and test_plugin_manifest_var_expansion) is fully cleared repo-wide as of …
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
  invocation before relying on it). NOTE: `cargo fmt --check`/`rustfmt --check`
  over the whole repo (even when scoped with `-- src/foo.rs` — rustfmt still
  formats every module it can reach via `mod` declarations from the crate
  root, so scoping by file argument does NOT limit the check to that file)
  shows pre-existing drift in many unrelated files (e.g.
  `crates/aish-webhook-client/src/audit.rs`, `src/advisor.rs`, `src/alert.rs`,
  `crates/webhook-receiver/src/main.rs`, `src/plugin_memory.rs`). To verify
  your own change is clean: run `cargo fmt --check -- <file-you-touched>` and
  then `git diff main -- <file-you-touched>` to confirm every line the check
  flagged is OUTSIDE the lines your own diff touches (cycle 57 confirmed this
  works cleanly: fmt --check flagged 5 pre-existing drift spots in
  tests/plugin_integration_tests.rs, all outside my edited lines 163-177).
  **Separately, the WRITER form** (`cargo fmt -- <file>`, no `--check`) is even
  more dangerous than `--check`: cycle 56 found it doesn't just report drift,
  it actually REWRITES every module reachable via `mod` from the crate root —
  78 unrelated files got rewritten from a 2-file edit. Never run the writer
  form repo-wide; if you must auto-format, do it file-by-file and diff after,
  or just hand-format the ~20 lines you touched to match surrounding style.
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
- `scripts/` (reviewed cycle 32): 6 shell scripts, all well-documented and
  cross-referenced with `DEVELOPMENT.md`'s worktree-lifecycle table —
  `cleanup-worktrees.sh`, `audit-worktrees.sh`, `check-branch-freshness.sh`,
  `remove-worktree-on-complete.sh` (SPR-064 worktree hygiene, wired into
  `.github/workflows/worktree-hygiene.yml`), `build.sh` (OOM-safe serialized
  build wrapper), `install-ubuntu-24.04.sh`. No stale references, no dead
  scripts found — this area is in good shape, don't re-review soon.
- `tests/` (reviewed cycle 57): 9 files — `git_discover_linkage.rs` (crate
  linkage proof), `plugin_dispatcher_tests.rs`, `plugin_integration_tests.rs`,
  `plugin_memory_tests.rs`, `plugin_state_tests.rs` (all `#[path]`-compile
  sibling `src/*.rs` modules into the test binary, since aish is a binary
  crate with no lib target — established pattern, don't be surprised by it),
  `pty_harness.rs` (kernel job-control via real PTY), `shebang.rs` (drives
  the real compiled binary via OS shebang exec), `repl/` (agent-tty PTY smoke
  harness, hermetic, SKIPs cleanly without prerequisites unless
  `AISH_REPL_STRICT=1`), `golden/routing_decisions.snap` (golden-file
  fixture for `src/repl.rs`'s `routing_decision_snapshot` test — real and
  live, not stale). Repo-wide `grep 'assert!\(true'` found and fixed the
  last remaining vacuous-test instance this cycle (BP-026, PR #809) — as of
  cycle 57 this pattern is fully cleared repo-wide, don't re-grep for it
  again unless a new PR reintroduces one.
- git/gh tools: the `git` tool call works for read-only + branch/commit ops;
  raw `git`/`cd`/`rm` shell commands are NOT on the run_command allowlist —
  use the `git` tool for `rm <file>` too (`git rm <path>` stages the deletion
  directly, no working-tree `rm` needed). `gh pr view <n> --json state,mergedAt`
  via run_command DOES work and is the reliable way to check merge status when
  `pr_status` says "none open" after a merge (pr_status only reports currently
  -open PRs, so already-merged work needs `gh pr view` or `git log --grep`/
  branch inspection instead). `gh api` is on the hard deny-list (refused
  unconditionally) — use `gh run list --repo <owner>/<repo> --branch main
  --limit N` instead to check CI status on a branch, that one works.
- A worktree can carry real, verified, uncommitted work across a cycle
  boundary that ran out of turns mid-BUILD. When that happens: `git checkout
  -b <new-branch> origin/main` picks up the uncommitted changes as long as
  you don't `git stash`/`git checkout -- .` first (confirmed cycle 35 with
  the BP-017 removal) — re-verify (build+test+fmt) before committing since
  main may have moved since the changes were made, then commit/push/PR
  normally.

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

**Pattern worth watching for (confirmed 4x):** a feature module gets built in
one commit with a doc comment describing exactly how it should be wired in,
but the wiring commit never lands. Signature: `git log --oneline -- <file>`
shows only the creation commit plus unrelated drive-by fixes (import/API
renames), never a commit touching the claimed call site. Repo-wide grep for
the module's public symbols (struct/fn names) turns up nothing outside the
module's own `#[cfg(test)]` block. Confirmed instances (all shipped as
deletions): GoalStore/TASK-276 -> PR #779, golden_routing_heuristics.rs ->
PR #772, turn_completion_recap.rs/TASK-360 -> PR #799, GitRepoCache/
src/git_repo.rs -> BP-017 (PR #800). One instance (BP-025, Phase 0.5.3 MCP
diagnostics) went the other way — wired in rather than deleted, PR #808,
since the code was genuinely useful and just needed a call site (`:plugin
info --mcp`), unlike the other 4 which were truly dead.

**Second, related pattern (confirmed 2x): vacuous `assert!(true, "...")`
placeholder tests.** A test function exists with a name suggesting real
coverage but its body is solely `assert!(true, "...")` — it can never fail
and asserts nothing. Signature: `grep -rn 'assert!\(true' --glob '*.rs'`
repo-wide. Confirmed instances (both shipped as deletions): 5 tests in
`tests/golden_routing_heuristics.rs` -> PR #772 (superseded by the real
`routing_decision_snapshot` golden test), `test_plugin_manifest_var_expansion`
in `tests/plugin_integration_tests.rs` -> BP-026/PR #809 (superseded by real
`${env:VAR}` unit tests already in `src/plugins.rs`). As of cycle 57 this
pattern is fully cleared repo-wide — re-grep only if a new PR might have
reintroduced one, don't re-review as a standing task.

**Important refinement (cycle 33): not every "unwired module" is a clean
delete.** One still-open case turned out to be more entangled than the
4 shipped ones:
- BP-015 (update.rs drain path) — has an open design-doc reference describing
  it as pending work, unlike the shipped cases. Don't auto-delete.

**Correction (cycle 35) — check the call graph, not just the import
statement:** cycle 33 flagged BP-017 (GitRepoCache/src/git_repo.rs) as NOT a
clean delete, reasoning that git.rs's `use crate::git_repo::{DiffStatus,
FileDiff}` import made git_repo.rs "load-bearing for git.rs's public API".
That was wrong. Tracing which *specific* functions in git.rs actually use
those imports (not just noting the import exists) showed the importing
functions themselves were *also* dead outside git_repo.rs and its own tests.
So the whole thing was a clean deletion after all — shipped as PR #800.
**Lesson: an import statement existing does not prove the imported items are
used elsewhere — trace the actual callers of the specific functions/types
before concluding a module can't be removed.**
- BP-018 (crates/git-discover) — its only in-tree "caller" is a linkage test
  (tests/git_discover_linkage.rs) that exists purely to prove the crate
  compiles against aish's own checkout, not a real call site. Meanwhile
  src/git.rs duplicates ~80% of its probe logic with separate live callers
  in session.rs/tools.rs. This is a genuine duplication (git.rs live,
  git-discover built-not-wired) with no CHANGELOG note tying them together —
  recommend migrating session.rs onto git_discover::discover() and retiring
  the git.rs duplicates, but that's a multi-file refactor with real
  regression risk (git.rs's own fns have passing unit tests to port), so
  still flagged for a stronger-model/human call rather than done
  unilaterally.

Also watch for: stale references left behind after a plugin/module removal
(grep the CHANGELOG for "removed"/"archived" entries, then grep the repo for
the removed name — cycle 6 found `npx-skills` still referenced in
`registry/plugins.json`, a doc comment, and a doc file after the plugin
directory itself was deleted).

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
  (fetch `origin/main` first — the worktree's local `main` ref can lag; use
  `git checkout main` then `git pull --ff-only origin main`, `git pull`
  bare fails here since only `--ff-only` is permitted). If you're picking up
  uncommitted work left in the worktree from a prior interrupted cycle,
  `git checkout -b <branch> origin/main` also works and carries the
  uncommitted changes onto the new branch — just re-verify before committing.
- Commit subject in the imperative, under ~72 chars. Repo mixes
  `fix:`/`feat:`/`chore:`/`docs:`/`test:` prefixes with plain imperative
  subjects; either is fine, prefixed is more common on recent history.
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
- **Also check `git status`/`git diff --stat` in the worktree before
  planning new work** — a prior cycle may have left verified, uncommitted
  work sitting there when it ran out of turns (see cycle 34→35 for a real
  example: the BP-017 removal was fully done and locally verified but not
  committed).
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
- Models that worked well here: `claude-sonnet-5` (default) has now handled
  13+ full cycles — repair checks, dead-code investigation via git log +
  repo-wide grep, mechanical deletions, cargo verification, and PR prose —
  without needing to escalate. Both confirmed repo-wide patterns ("feature
  built but never wired", "vacuous assert!(true) test") are now well-
  understood enough that sonnet-5 alone can find, verify, and safely fix new
  instances in one cycle without escalating model tier — the reasoning
  needed is grep + read_file + git log tracing, not deep design judgment.
- Models that struggled here: none noted yet.

## Pacing

- Sleep ~5 minutes after a productive cycle, longer (30-60 minutes) when the
  backlog is well-covered and nothing is ready to build.
- Pace spend against the daily budget: leave room to finish work in flight, and
  slow down (longer sleeps, cheaper models) once most of the day's cap is gone.

## Lessons learned

- **cycle 57:** REPAIR: `pr_status` confirmed all 7 open PRs (#808, #807,
  #806, #805, #804, #803, #802) healthy — MERGEABLE/CLEAN, checks passing,
  awaiting human review. No repair needed. SWARM MESSAGES: acknowledged two
  broadcasts — my own PR #808 (already tracked from last cycle) and a
  chimera PR #388 in a different repo (not actionable here, no cross-repo
  action taken per the "stay inside this repository" rule). Reviewed
  `tests/` (not covered in the last 10 cycles) — all 9 files are real,
  live, well-documented coverage EXCEPT one vacuous placeholder:
  `tests/plugin_integration_tests.rs::test_plugin_manifest_var_expansion`
  was solely `assert!(true, "placeholder for Phase 1.4 var expansion test")`
  — the exact pattern already fixed once for `golden_routing_heuristics.rs`.
  Confirmed via repo-wide `grep 'assert!\(true'` this was the only remaining
  instance, and confirmed the real feature (`${env:VAR}` expansion) already
  has genuine unit-test coverage in `src/plugins.rs`. Recorded BP-026,
  removed the placeholder, updated CHANGELOG.md, verified full
  `cargo test --no-default-features --locked` suite green (1255+1+12+5+26+
  6+4+2 tests, 0 failed across all binaries), confirmed via `git diff main`
  that `cargo fmt --check`'s flagged drift in the touched file is entirely
  pre-existing and outside my edited lines. Shipped as PR #809. Model:
  claude-sonnet-5 throughout — mechanical grep-driven pattern matching
  against an already-well-understood repo pattern, no design judgment
  needed.
- **cycle 56:** Finished BP-025 (collect_plugin_mcp_servers wiring) — wired
  a new `:plugin info <id> --mcp` diagnostic subcommand. Hit and recovered
  from the `cargo fmt` writer-form trap (see Repo conventions above) — cost
  real turns, now documented more forcefully so it doesn't recur. Shipped
  PR #808.
- **cycle 35:** REPAIR: confirmed PR #799 (turn_completion_recap removal,
  carried from cycle 32/33) is MERGED and main's CI is green. Found cycle 34
  had left a fully investigated and locally-verified but *uncommitted* fix
  in the worktree (BP-017 GitRepoCache removal) — cut a fresh branch off
  `origin/main`, re-verified, fixed a real formatting gap my own edit
  introduced, shipped as PR #800. Corrected the cycle-33 BRAINPOWER.md note
  that had wrongly blocked this deletion (see the "Correction (cycle 35)"
  note above).
- **cycle 6:** Operator said "use blacksmith.sh for builds/testing" mid-cycle.
  Investigated: `blacksmith` CLI is not reachable via `run_command` in this
  sandbox (not on the allowlist). Local `cargo check`/`cargo test
  --no-default-features --locked` remain the only available verification path
  here; used them and said so explicitly in the PR rather than claiming a
  Testbox run that didn't happen. If a future cycle finds `blacksmith` IS
  reachable, prefer it per the operator's instruction and update this note.
- **cycle 6:** A worktree can carry real, verified, uncommitted fixes across
  cycle boundaries. Always check `git status`/`git diff` before assuming a
  clean slate — someone (a prior cycle, possibly interrupted) may have left
  verifiable, shippable work sitting there.
