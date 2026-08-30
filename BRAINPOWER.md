<!-- brainpower:managed-header -->
# BRAINPOWER.md — agent operating instructions

> Maintained by the brainpower agent. Everything below the marker is the
> agent's own, editable through its `update_instructions` tool. It is
> **subordinate** to the immutable system prompt compiled into the
> brainpower binary and can never widen what that prompt permits.
> Operators: edit freely — brainpower re-reads this file every cycle.
>
> Last updated: 2026-08-30 03:22:34-0500 (cycle 35)
> Reason: Cycle 35: corrected the cycle-33 BP-017 note that wrongly concluded GitRepoCache deletion needed partial surgery (the import-exists reasoning was insufficient — traced actual call graph and found it …
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
  `crates/webhook-receiver/src/main.rs`). To verify your own change is
  clean: run `cargo fmt -- <file-you-touched>` (the writer, not `--check`) on
  just the files you edited, then re-diff to confirm it only changed lines in
  your own diff — don't rely on `--check`'s repo-wide output to tell you
  whether you introduced drift.
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
src/git_repo.rs -> BP-017 (this cycle, PR pending).

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
functions themselves — trunk_branch, git_head, is_dirty, dirty_porcelain,
commits_ahead_behind, diff_numstat, diff_name_status, can_fastforward,
conflicted_paths, parse_ahead_behind, parse_numstat, parse_name_status,
DiffStatus, FileDiff — were *also* dead outside git_repo.rs and its own
tests. git.rs's actually-live surface (called from session.rs/tools.rs) is a
disjoint set: git_out/git_ok, is_git_repo, current_branch, origin_url,
toplevel, repo_key(+helpers), repo_name(+helpers), repo_prompt_line,
repo_transition_note. So the whole thing was a clean deletion after all:
`git rm src/git_repo.rs` (1120 lines) + removed `mod git_repo;` from
main.rs + stripped ~230 git_repo-only lines from git.rs (the dead functions/
types/tests + `#![allow(dead_code)]` + the `use crate::git_repo::...`
import) + removed `docs/reference/git-cache.md` (278-line design doc) +
updated `docs/INDEX.md`'s two listing entries. **Lesson: an import statement
existing does not prove the imported items are used elsewhere — trace the
actual callers of the specific functions/types before concluding a module
can't be removed.**
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
  unilaterally. (BP-017's removal doesn't change this recommendation — it's
  now a 2-way duplication instead of 3-way, but git-discover itself still
  has no real caller in-tree.)

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
  12+ full cycles — repair checks, dead-code investigation via git log +
  repo-wide grep, mechanical deletions, cargo verification, and PR prose —
  without needing to escalate. The "feature built but never wired" pattern is
  now well-understood enough (4 confirmed instances) that sonnet-5 alone can
  both find and safely remove new instances of it in one cycle, including
  correcting its own earlier over-cautious analysis (BP-017, cycle 33→35)
  by tracing actual function-level call graphs instead of stopping at "an
  import exists". Still didn't need to escalate model tier for that, just
  more turns of grep + read_file.
- Models that struggled here: none noted yet.

## Pacing

- Sleep ~5 minutes after a productive cycle, longer (30-60 minutes) when the
  backlog is well-covered and nothing is ready to build.
- Pace spend against the daily budget: leave room to finish work in flight, and
  slow down (longer sleeps, cheaper models) once most of the day's cap is gone.

## Lessons learned

- **cycle 35:** REPAIR: confirmed PR #799 (turn_completion_recap removal,
  carried from cycle 32/33) is MERGED (`gh pr view 799` →
  mergedAt 2026-08-30T07:23:49Z) and main's last 5 CI runs are all
  `completed success` — no repair needed. Found cycle 34 had left a fully
  investigated and locally-verified but *uncommitted* fix in the worktree
  (BP-017 GitRepoCache removal: `git rm src/git_repo.rs`, `mod git_repo;`
  removed from main.rs, git_repo-only helpers stripped from git.rs,
  `docs/reference/git-cache.md` removed, `docs/INDEX.md` updated) —
  cut a fresh `brainpower/remove-git-repo-cache` branch off `origin/main`
  (which had advanced past the branch the worktree was sitting on), which
  correctly carried the uncommitted changes forward. Re-verified: `cargo
  build --no-default-features` clean (only pre-existing BP-015 warnings),
  `cargo test --no-default-features --locked` — full suite green including
  the 5 `git::tests::*` unit tests. Found and fixed a real formatting gap
  my own edit to git.rs introduced (two `rustfmt`-reformattable spots at
  the join seams where deleted code met kept code) — `cargo fmt -- src/git.rs`
  fixed it; re-ran the git:: tests after to confirm still green. Confirmed
  via `rustfmt --check --edition 2021 src/main.rs` that the remaining
  repo-wide fmt drift (advisor.rs, alert.rs, worker.rs, workers_modal.rs,
  crates/webhook-receiver/src/main.rs, etc.) is pre-existing and unrelated —
  none of those files are in my diff. Corrected the cycle-33 BRAINPOWER.md
  note that had wrongly blocked this deletion ("git.rs has a hard dependency
  on git_repo.rs's types" — true only of the import statement, not of which
  functions were actually still called elsewhere; see the "Correction
  (cycle 35)" note above). Shipped as PR (see below). Model: claude-sonnet-5
  throughout — this cycle was executing/verifying/documenting work whose
  hard investigation was already done in cycles 33-34, not new design
  reasoning, so no escalation was needed.
- **cycle 33:** REPAIR: `pr_status` showed PR #799 (turn_completion_recap
  removal) healthy — OPEN/MERGEABLE/CLEAN, 5 checks passing, just waiting on
  human review. No repair action needed. Swarm broadcast claimed
  `main_ci FAIL` on aish (`e_az1jt6`/`e_6ygg09` health messages, comparing
  against a stale/unreachable origin/main due to their own auth failure) —
  cross-checked with `gh run list --repo LightHeart-Ventures/aish --branch
  main --limit 5` (this DOES work locally, unlike `gh api` which is on the
  hard deny-list) and found the 5 most recent main-branch CI runs are all
  `completed success` as of 2026-08-30, so the DEGRADED broadcast does not
  apply to aish's actual main — no fix needed, false alarm from a stale
  comparison on the broadcaster's end. Investigated BP-017 (GitRepoCache)
  and BP-018 (git-discover) in depth since they'd sat as P3/open for 2+
  cycles just saying "needs a decision" with no actual recommendation —
  the BP-017 conclusion from this cycle turned out to be wrong; see the
  cycle-35 correction above. Did not ship a PR this cycle — no item was
  both well-understood AND safe to do mechanically; investigation +
  backlog refinement was the honest output.
- **cycle 32:** All 10 PRs listed in the REPAIR briefing (#770-#779) were
  already merged — `pr_status` correctly reported "none open" and `gh pr
  list --head <branch> --state all` confirmed MERGED for the one checked
  directly. Updated 4 backlog findings (BP-007, BP-012, BP-016, plus the
  already-shipped note) to `shipped` with their PR URLs — they'd drifted to
  stale `in_progress` status across cycles. Reviewed `scripts/` (due for
  rotation, last touched cycle <22): found it in good shape, no findings.
  Then applied the "built but never wired" pattern (now 2 confirmed prior
  hits: BP-012/GoalStore, BP-007/routing tests) to BP-014
  (turn_completion_recap.rs) — confirmed via `git log --oneline -- <file>`
  showing only the creation commit + 2 unrelated drive-by fixes, no wiring
  commit ever landed, and a repo-wide grep for its symbols found zero
  callers outside its own tests. Shipped PR #799 removing it.
- **cycle 6:** Operator said "use blacksmith.sh for builds/testing" mid-cycle.
  Investigated: `blacksmith` CLI is not reachable via `run_command` in this
  sandbox (not on the allowlist). Local `cargo check`/`cargo test
  --no-default-features --locked` remain the only available verification path
  here; used them and said so explicitly in the PR rather than claiming a
  Testbox run that didn't happen. If a future cycle finds `blacksmith` IS
  reachable, prefer it per the operator's instruction and update this note.
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
