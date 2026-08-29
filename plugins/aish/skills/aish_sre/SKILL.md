---
name: aish_sre
categories: [infrastructure, troubleshooting, release]
applies-to: [aish]
unwanted-for: [design, review]
description: Site-reliability & troubleshooting playbook for aish (LightHeart-Ventures/aish), the AI-native Rust shell. Encodes hard-won lessons on the big failure classes — broken releases & ':update' (GitHub immutable releases, pre-publish footgun, tag↔Cargo.toml drift, asset-less releases, burned tags), dev vs production release channels (versioning strategy, release-dev.yml workflow, dev tag naming), cargo-build OOM in coordinator worktrees (the heavy 'local' llama.cpp feature, --no-default-features, flock serialization, jobs cap), the CI test gate (--no-default-features --locked), macOS 'zsh: killed aish' codesign kill-on-launch, runaway background coordinators (loop/turn-budget exhaustion, circuit breaker, turn-audit journals), worktree/git hygiene, command-routing surprises, and runtime/config (ANTHROPIC_API_KEY, MCP startup, modes). Includes step-by-step happy-path runbooks for cutting production (§0) and dev (§0b) releases. Read this FIRST when aish itself is misbehaving, you need to cut/tag a release, a release/CI run failed, a build got OOM-killed, or a coordinator is stuck.
allowed-tools: Bash, Read, Grep, Glob, Task
license: Proprietary
version: 1.1.0
tags: incident, sre, runbook, aish, rust, release, ci, coordinator
---

# aish SRE & Troubleshooting Playbook

You are triaging or fixing a problem in **aish itself** — the AI-native Rust
shell at `github.com/LightHeart-Ventures/aish` (main checkout:
`/home/grhohertz/projects/aish`). Match the reported symptom to a section below,
confirm the root cause with **evidence** (a failed CI run's logs, the actual
built binary, `git` state on `origin/main` vs the tag, the coordinator journal,
a `cargo` exit code — never just source code), then apply the documented fix.
Prefer a feature branch + draft PR; **never push to `main`** and **never push a
release tag yourself unless you hold org-admin / ruleset bypass** (see §1).
Sections are ordered by how often they bite.

Architecture in one breath (see `docs/ARCHITECTURE.md` for the full map —
`.repospec.json` was removed from the repo in commit 3271425, "chore: remove
.repospec.json integration (deprecation)"):
`repl.rs` (routing + `:commands`) → `engine.rs` (agentic turn loop) →
`tools.rs` (read/write/run) → `backend/` (Claude API default; `local` =
llama.cpp in-process). Background work runs as separate aish processes:
`coordinator.rs` + `worker.rs` + `container.rs`, journaled to SQLite
(`aish.db`) and `.atum/run-<id>.jsonl`. The `local` feature (llama.cpp native,
cmake, opt-level=3) is the source of most build pain and is **dropped from the
CI test gate and coordinator/worktree builds**. NOTE: the **release** build
re-added `--features local` in **v0.20.0 (PR #294)** — shipped release binaries
now DO carry llama.cpp; only CI tests + coordinator rebuilds stay Claude-only
(see the release runbook below and §3).

---

### 0. Cutting a new release — the happy-path runbook (do this EVERY time)

**When:** you've been asked to "cut a release", "tag vX.Y.Z", "ship a new
version", or "release aish". This is the proactive procedure; §1–2 are the
failure modes it exists to avoid. Canonical source: `docs/RELEASING.md`.

**Mental model:** the runtime version is baked from `Cargo.toml`
(`env!("CARGO_PKG_VERSION")` → `src/update.rs::current_version()`). `:update`
discovers the latest tag via `gh release view`, downloads the per-platform asset,
and verifies it against its `.sha256`. So three things must agree: the **tag**,
**Cargo.toml on main**, and the **published release's assets**. The
`.github/workflows/release.yml` workflow (triggered by any pushed `v*` tag) is
what builds + publishes — you only push the tag.

**Steps:**

1. **Pick the version.** Patch for fixes, minor for features. Check the last tag
   first: `gh release list --repo LightHeart-Ventures/aish --limit 5`. Do NOT
   reuse or "fill a gap" in the sequence — gaps are normal (burned tags, §1).
2. **Bump on a branch:** edit `version` in `Cargo.toml`, then **refresh the lock**
   — `cargo build` (or `cargo update -p aish --precise X.Y.Z`) so `Cargo.lock`'s
   `aish` entry matches. Commit **both** files. (`--locked` CI fails otherwise, §4.)
3. **Open `release/vX.Y.Z` PR → merge to `main`.** The bump MUST be on `main`
   before tagging (§2). After merge: `git fetch origin && git show
   origin/main:Cargo.toml | grep '^version'` — must read `X.Y.Z`.
4. **Tag the MERGE COMMIT and push ONLY the tag:**
   ```sh
   git fetch origin
   git tag vX.Y.Z $(git rev-parse origin/main)   # the merge commit on main
   git push origin vX.Y.Z                          # ONLY the tag — never `gh release create`
   ```
   Do **not** create/publish the release by hand — the workflow does it. (If you
   want hand-written notes, pre-create a **DRAFT** only: `gh release create
   vX.Y.Z --draft --notes …` — drafts stay mutable so the workflow still uploads
   assets. §1.)
5. **Watch the workflow:** `gh run watch $(gh run list --workflow=Release
   --limit 1 --json databaseId -q '.[0].databaseId') --repo
   LightHeart-Ventures/aish`. It runs `verify-version` (tag == Cargo.toml + no
   pre-published release), builds macOS x86_64/arm64 + Linux x86_64 with
   `--release --features local` (v0.20.0+, PR #294 — release binaries include
   llama.cpp), emits `.sha256` sidecars + a `SHA256SUMS` roll-up, and publishes
   via `softprops/action-gh-release@v2`.
6. **Verify the release is real and complete:**
   ```sh
   gh release view vX.Y.Z --repo LightHeart-Ventures/aish --json isDraft,assets \
     -q '{draft:.isDraft, assets:[.assets[].name]}'
   ```
   Expect `isDraft:false` and **7 assets**: 3 binaries + 3 `.sha256` + `SHA256SUMS`.
   A release with **zero assets** = broken `:update`, recover via §1.
7. **Confirm the binary self-reports the version** (catches tag↔Cargo drift, §2):
   download the platform asset and run `./aish --version` — must print `X.Y.Z`.

**Release timing/sequencing facts learned in the field:**
- **Tags can be BURNED.** v0.19.0 failed (immutable pre-publish), and after
  delete the tag couldn't be re-pushed → the line was abandoned and re-cut as
  **v0.19.1**, then stabilized through **v0.19.2 / v0.19.3**. When a tag push is
  rejected after a delete, **bump forward — never retry the same tag** (§1).
- **Phantom installed versions are real.** An installed binary may self-report a
  version that has **no matching GitHub release** (e.g. a binary reporting
  `0.18.5` when the releases are `…v0.18.4, v0.19.1, v0.19.2, v0.19.3, v0.20.0`).
  That means it was built off a local bump commit that was never released — a
  drift symptom (§2), not a `:update` bug. Reconcile `gh release list` against
  the binary's `--version` before chasing `:update`.
- **`--features local` in the release build (v0.20.0+):** release binaries are
  heavier now but ship local inference. This is safe ONLY because release builds
  run one-per-clean-runner; never copy that flag into CI tests or coordinator
  rebuilds (§3, §4).

---

### 0b. Cutting a dev release — the parallel channel for testing (do this when you need a dev snapshot)

**When:** you need to publish a development/testing snapshot without touching the stable production release line. Dev releases are **pre-releases** pulled by operators with `AISH_UPDATE_CHANNEL=dev` (or `:update dev` in the REPL). Use this for early testing, nightly builds, or unstable features before cutting a stable `vX.Y.Z`.

**Mental model:** aish maintains two release channels with different semantics:

| Aspect | Stable (vX.Y.Z) | Dev (dev-vX.Y.Z-dev.N) |
|--------|---|---|
| **Trigger** | Manual: push `vX.Y.Z` tag | Manual: workflow dispatch, or scheduled nightly |
| **Workflow** | `.github/workflows/release.yml` | `.github/workflows/release-dev.yml` |
| **Version strategy** | Exact: bumped in `Cargo.toml` before tag | Lookahead: computes next minor automatically (e.g., `0.25.1` → `0.26.0`) |
| **Tag format** | `vX.Y.Z` (semver) | `dev-vX.Y.Z-dev.N` (prerelease) |
| **Release metadata** | Stable, immutable | Pre-release flag set |
| **Platforms** | Full: Linux x86_64 + macOS x86_64/arm64 | Configurable: linux-only (default), or all |
| **Retention** | Indefinite | Pruned: auto-deletes >5 oldest dev releases |
| **Update channel** | Default (`:update` or env=unset) | Opt-in: `AISH_UPDATE_CHANNEL=dev` or `:update dev` |

**When to use each:**
- **Stable (vX.Y.Z):** Feature complete, tested, ready for all users. Cut when a coordinated set of features/fixes is complete and stable. Released via `release.yml` triggered by pushing a semver tag.
- **Dev (dev-vX.Y.Z-dev.N):** Intermediate snapshots between stable releases. Cut when you want operators to test new code without a full stable release. Released via `release-dev.yml` triggered manually or on schedule. Auto-deletes old dev releases to keep the list short.

**Steps to cut a dev release:**

1. **Ensure main is clean and up-to-date:**
   ```sh
   git fetch origin
   git show origin/main:Cargo.toml | grep '^version'  # Note the current version
   ```
   No need to bump `Cargo.toml` — the workflow computes the next version automatically.

2. **Trigger the dev release workflow manually:**
   ```sh
   # Full platforms (x86_64-linux, x86_64-macos, aarch64-macos):
   gh workflow run release-dev.yml -f platforms=all \
     --repo LightHeart-Ventures/aish
   
   # Or just Linux (faster, default for nightly):
   gh workflow run release-dev.yml -f platforms=linux \
     --repo LightHeart-Ventures/aish
   ```

3. **Watch the workflow:**
   ```sh
   gh run list --workflow=release-dev.yml --limit 1 \
     --json databaseId -q '.[0].databaseId' | \
     xargs -I {} gh run watch {} --repo LightHeart-Ventures/aish
   ```

4. **The workflow automatically:**
   - Checks out `main` (no tag push needed)
   - Parses `Cargo.toml` to get the current version (e.g., `0.25.1`)
   - Bumps the minor version (e.g., → `0.26.0`) as the "lookahead" for dev
   - Builds all requested platforms with `--release --features local`
   - Generates `.sha256` sidecars + `SHA256SUMS` aggregate
   - Creates a tagged release `dev-v0.26.0-dev.<run_number>` (marked pre-release)
   - Publishes all assets atomically
   - Verifies asset count matches the platform set (fails loudly if any missing)
   - Auto-prunes old dev releases (keeps 5 latest, deletes the rest)

5. **Verify the release is live and complete:**
   ```sh
   gh release view dev-v0.26.0-dev.4 --repo LightHeart-Ventures/aish \
     --json isDraft,assets -q '{draft:.isDraft, assets:[.assets[].name]}'
   ```
   Expect `isDraft:false` and the right asset count: `2 * platform_count + 1`
   (e.g., 7 assets for 3 platforms: 3 binaries + 3 `.sha256` + `SHA256SUMS`).

6. **Confirm the binary self-reports correctly:**
   Download one platform asset and verify:
   ```sh
   curl -L -o /tmp/aish-test \
     https://github.com/LightHeart-Ventures/aish/releases/download/dev-v0.26.0-dev.4/aish-x86_64-unknown-linux-gnu
   chmod +x /tmp/aish-test
   /tmp/aish-test --version  # Must print "0.25.1 (dev snapshot dev-v0.26.0-dev.4)" — see PR #365
   ```

**Dev release facts & anti-patterns:**

- **Version lookahead is intentional.** The `release-dev.yml` bumps the **minor** version to signal "this is a snapshot of what's coming in the next feature release". If `Cargo.toml` says `0.25.1`, the dev release will be tagged `dev-v0.26.0-dev.N`. This is correct — it lets operators who track dev know they're always on the next-version edge. (Stable releases use exact versions from `Cargo.toml`.)
- **Tag format is rigid.** Dev releases MUST be named `dev-v<X>.<Y>.<Z>-dev.<N>` so `:update dev` can discover them. If you manually create a tag that doesn't match this format, the update logic won't find it.
- **Workflow handles asset atomicity.** Unlike the stable workflow (which uses `softprops/action-gh-release@v2`), `release-dev.yml` uses raw `gh release create` with all assets in one command. This ensures the release is published immutably with all assets at once — no 422 immutable-release errors.
- **Auto-pruning only applies to dev releases.** Stable releases (`vX.Y.Z`) are never auto-deleted. Only `dev-*` tags are pruned, keeping the latest 5.
- **Do NOT bump Cargo.toml before a dev release.** The workflow checks out `main` and uses its `Cargo.toml` version. If you bump the version before triggering the workflow, the released binary will self-report that bumped version, which is usually not what you want (it breaks the "lookahead" expectation and can cause confusion). Instead, bump `Cargo.toml` as part of a PR that lands on `main`, then trigger a dev release to snapshot that code.
- **Use `--platforms all` for final testing before a stable release.** The default nightly builds only Linux. When you're about to cut a stable release, trigger a dev release with `--platforms all` to verify all three binaries build and sign correctly.

- **Dev release tag embedding (PR #365, fixed PR #367):** As of PR #365, dev binaries embed the release tag in their version string so `--version` reports `0.25.1 (dev snapshot dev-v0.26.0-dev.4)` instead of just `0.25.1`. This requires the `AISH_RELEASE_TAG` environment variable to be passed to the `cargo build` invocation. The `release-dev.yml` workflow's earlier **reuse optimization** (skipping a fresh build when CI artifacts already existed) prevented this env var from being set, so released binaries showed only the base version. **Fix (PR #367): always do a fresh build in `release-dev.yml`** — the 4–6 minute compile cost is acceptable for dev releases (infrequent), and ensures the version string is properly embedded. If you see a dev release with only the base version in `--version`, the reuse optimization was active; the next dev release should show the full tag.

---

### 1. A release shipped with no assets / `:update` is broken — GitHub **immutable releases**
### 1. A release shipped with no assets / `:update` is broken — GitHub **immutable releases**

**Symptom:** The `Release` workflow (`.github/workflows/release.yml`) fails
~4 minutes in, in the `publish release` job, with:
```
Validation Failed: target_commitish cannot be changed when release is immutable
HTTP 422: Cannot upload assets to an immutable release
```
…or a release exists for `vX.Y.Z` but has **zero binary assets**, and `:update`
(`src/update.rs`) re-downloads / errors on every startup because there is
nothing to fetch. This is exactly what burned **v0.18.3** (run 28423477622) and
**v0.19.0** (run 28434180793).

**Root cause:** GitHub publishes releases as **immutable** — the instant a
release is *published* it is frozen: metadata can't change and **no assets can be
attached afterward**. If a bare release is pre-published out-of-band (a local
release script running `gh release create vX.Y.Z --notes …` right after the tag
push), the workflow's `release` job finds that frozen release and cannot attach
the per-platform binaries `:update` depends on → 422.

- **THE ONE RULE: let the workflow create the release. Push ONLY the tag.**
  `release.yml` builds macOS (x86_64 + arm64) + Linux (x86_64), generates
  `.sha256` sidecars + a `SHA256SUMS` roll-up, and publishes with every asset in
  one shot via `softprops/action-gh-release@v2`. Do **not** `gh release create`
  before the tag push. If you want hand-written notes, create a **DRAFT**
  (`gh release create vX.Y.Z --draft --notes …`) — drafts stay **mutable**, so
  the workflow uploads assets and publishes them. The `verify-version` job's
  "Assert no published release already exists" guard now fails fast with this
  exact explanation; a pre-existing draft passes.
- **Recovering an asset-less release:**
  - *Re-cut the same version* (only if the tag isn't burned — see below):
    `gh release delete vX.Y.Z --yes && git push --delete origin vX.Y.Z &&
     git tag -d vX.Y.Z && git tag vX.Y.Z <merge-sha> && git push origin vX.Y.Z`.
  - *Bump forward* to `vX.Y.(Z+1)` and release normally — **preferred** when the
    broken release can't be deleted or its tag is burned.
- **Lesson — a tag name can be permanently BURNED.** After deleting an immutable
  release, GitHub may keep an immutable *tombstone* and the repo's tag-creation
  ruleset can block re-pushing the same tag. If `git push origin vX.Y.Z` is
  rejected after a delete, **stop re-trying** — bump to the next patch instead
  (this is why v0.19.0 was abandoned for v0.19.1). Canonical procedure lives in
  `docs/RELEASING.md`.

### 2. `verify-version` passed but `origin/main` is the OLD version — tag cut off-main

**Symptom:** A release tag built and reported version `X.Y.Z`, yet `origin/main`
still shows the previous version in `Cargo.toml`. `verify-version` went green
because it checks out the **tagged commit**, not `main`.

**Root cause:** The `chore: bump version to X.Y.Z` commit was tagged off a
side/off-main commit and **never merged**. The tag is real and consistent with
its own commit; `main` never advanced.

- **Lesson — the version bump must land on `main` FIRST, then tag the merge
  commit.** Correct order (`docs/RELEASING.md`): (1) bump `version` in
  `Cargo.toml` **and** refresh `Cargo.lock`, open `release/vX.Y.Z`, **merge**;
  (2) `git tag vX.Y.Z <merge-commit-sha> && git push origin vX.Y.Z` (only the
  tag). The `verify-version` job (`tag == Cargo.toml version`) only proves
  tag↔Cargo consistency, **not** that the bump is on `main`.
- **Verify before declaring a release done:**
  `git fetch origin && git show origin/main:Cargo.toml | grep '^version'`
  must equal the tag. If it doesn't, the bump never merged.
- **Why drift matters:** the runtime version is `env!("CARGO_PKG_VERSION")`
  (`src/update.rs::current_version()`). Tag/Cargo.toml drift ships a binary that
  reports the wrong version, so `:update` compares the new tag against a stale
  baked version forever and re-downloads on every startup.

### 3. `cargo build` gets **OOM-killed** (esp. in coordinator worktrees)

**Symptom:** `cargo build` dies with `signal: 9, SIGKILL`, "rustc killed", or the
worktree build just vanishes mid-compile. Most visible when several background
coordinators share one host (e.g. w_SlcE27iD).

**Root cause:** The **`local`** feature pulls in the llama.cpp native bindings
(`llama-cpp-2` / `llama-cpp-sys-2`) — a `cmake` build of llama.cpp plus Rust
crates compiled at **opt-level=3** (see `[profile.dev.package."*"]`). A single
optimizing `rustc` on one of those peaks **past 1.5 GB**; cargo fans codegen
across every core, and dozens of concurrent worktree builds overcommit RAM →
kernel OOM-killer.

- **Lesson — build Claude-only unless you specifically need local inference.**
  `--no-default-features` drops the entire `local` graph. This is what the CI
  test gate, `make build-fast`, and `scripts/build.sh` do for coordinator/worktree
  rebuilds. Only pass `--features local` when you are exercising the in-process
  model path. **Exception (v0.20.0, PR #294): the release workflow
  (`release.yml`) intentionally builds `cargo build --release --features local`**
  so shipped binaries include local inference — release builds run on clean
  GitHub runners (one build, not dozens of concurrent worktrees), so the OOM
  risk that bans `local` from coordinator builds doesn't apply there.
- **Two stacked OOM mitigations (both committed, inherit automatically):**
  1. **Per-build jobs cap** — `.cargo/config.toml` sets `jobs = 6` so one build
     can't saturate all cores. Override on a big box: `CARGO_BUILD_JOBS=N cargo …`.
  2. **Cross-worktree serialization** — `scripts/build.sh` / the `Makefile`
     wrap cargo in `flock /tmp/aish-build.lock`, bounding *concurrent* builds
     across worktrees to 1 (each still uses up to `jobs` cores). `flock` is
     Linux-only; on macOS it degrades to an unserialized build.
- **Blacksmith testing (NEW):** For full CI-gate test runs (`cargo test --no-default-features --locked`) that need real CI secrets or services (e.g., Atum API access), use **blacksmith.sh**. It runs builds/tests in an isolated, well-resourced environment without OOM risk. See the `blacksmith-testbox` skill for setup. This eliminates the need to worry about OOM during pre-commit validation.
- **Do this:** `make build-fast` or `scripts/build.sh --release` for any
  automated/coordinator rebuild. Reserve `make build` (full `local`) for when
  local inference is actually under test.
- **Build deps for the `local`/native path** (Ubuntu): `build-essential cmake
  clang libclang-dev pkg-config libssl-dev perl` — a missing `cmake`/`libclang`
  manifests as a llama-cpp-sys build-script failure, *not* an OOM.

### 4. CI test job red — the gate is `cargo test --no-default-features --locked`

**Symptom:** `.github/workflows/ci.yml` ("cargo test (Claude-only)") fails, or a
test passes locally but fails in CI (or vice-versa).

- **Lesson — match the CI gate exactly: `cargo test --no-default-features
  --locked`.** Test builds must **NOT** pull in `mistralrs-core` / llama.cpp —
  it's huge, slow, and irrelevant to the unit/oracle/pty suites. Use
  `make test` (Claude-only, the default) and only `make test-local`
  (`--features local`) when explicitly exercising local inference.
- **`--locked` matters:** if you changed `Cargo.toml` without refreshing
  `Cargo.lock`, CI fails on the lockfile, not your code. Run `cargo build` (or
  `cargo update -p <crate> --precise …`) to sync the lock and commit it.
- **Oracle tests** (`src/oracle.rs`) diff aish's native pipeline/direct-dispatch
  output against **real bash** on the runner. A failing oracle case usually means
  a routing/dispatch behavior change, not flakiness — read the snapshot diff.

### 5. macOS: `zsh: killed  aish` immediately on launch — ad-hoc signature

**Symptom:** On Apple Silicon, the freshly-installed binary is SIGKILLed the
instant it runs; nothing prints.

**Root cause:** A cargo-built arm64 binary carries only a **linker-signed**
ad-hoc signature. macOS AMFI SIGKILLs it the moment it's copied to a new path.

- **Lesson — always install via `make install`, never a bare `cp`.** The
  `install` target re-signs with a fresh ad-hoc signature
  (`codesign --force --sign - <dest>`) which gives a valid cdhash so it launches,
  then registers it in `/etc/shells`. A bare `cp target/release/aish
  ~/.local/bin` reintroduces the kill-on-launch bug. To repair an
  already-copied binary: `codesign --force --sign - ~/.local/bin/aish &&
  codesign -v ~/.local/bin/aish`. (No-op on Linux.)

### 6. A background coordinator runs forever / loops / never finishes

**Symptom:** A coordinator run keeps going round after round, emits the same
synthesis repeatedly, or exhausts its turn budget without finishing or declaring
a blocker. Shows as long-running in `background_status` / `:workers`.

**Root cause & the shipped guards** (`docs/coordinator-loop-guards.md`,
`src/coordinator.rs`):

- **Inspect the journal first.** Every run writes `.atum/run-<id>.jsonl`: each
  **tool call** (input+output) plus the per-round **synthesis** (`status:
  "synthesis"`). A run repeating the same synthesis round after round is visibly
  looping there even when the tool log alone hides it.
- **Round cap (the bandaid):** `AISH_COORDINATOR_MAX_ROUNDS` (default **48**,
  clamped 1–1000). Lift it without a rebuild when a *legitimate* task is starved
  — but a loop is the real problem, not the cap.
- **Pre-dispatch circuit breaker:** `coordinator::drive` refuses to start when
  the **same task text** has already terminated `failed` ≥
  `AISH_COORDINATOR_MAX_FAILED_ATTEMPTS` (default **3**, `0` disables). If a task
  "won't start", check whether it tripped the breaker — clear old `failed` rows
  or change the task text.
- **Failed rows are retained on purpose** for forensics — `clear_finished`
  purges only `done` rows; `failed` survive, bounded by
  `AISH_COORDINATOR_FAILED_KEEP` (50) + `AISH_COORDINATOR_FAILED_MAX_AGE_DAYS`
  (14). Don't be surprised that failed runs linger in `:workers`.
- **Lesson — a clearly-stated blocker is a SUCCESS.** The coordinator prompt's
  "DECISION POINTS — avoid loops" block tells the model to stop retrying a
  failing approach after ~3 attempts and declare "I'm blocked because <reason>".
  When triaging a stuck run, prefer reporting the blocker over bumping
  `MAX_ROUNDS` to brute-force it.

### 7. Coordinator worktrees, branches, and git hygiene

**Symptom:** You can't find a worker's changes, or you're tempted to merge a
worker branch from inside its worktree.

- **Where work lives:** background coordinators run in dedicated git worktrees
  under `~/.aish/worktrees/<owner>--<repo>/w_<id>/` on branch `aish/w_<id>`.
  Changes are **never auto-merged** — review/merge from the **parent** repo.
- **Lesson — never commit to or push `main`.** Commit on the worker's feature
  branch, push it, open a **draft** PR (`gh pr create --draft --fill`). The
  worktree is the durable source of truth: if a coordinator's SQLite row is
  trimmed, the run is re-derived from the worktree on next boot.
- If you discover local commits sitting on `main` (or any unexpected state),
  **STOP and surface it** — don't force-sync. Reach for the `fix-ci` /
  `fix-conflicts` skills for those specific jobs rather than hand-fixing.

### 8. A line routed the wrong way — command vs intent vs prose

**Symptom:** Typed input ran as a shell command when you meant it as intent (or
the reverse), or English got executed as a binary.

**Root cause:** `repl.rs` routing heuristics (`split_route` → `dispatch` →
`looks_like_prose`): a `:command` goes to the REPL; if word-1 resolves to a real
binary it's dispatched **directly**; otherwise it routes to the **model**.

- **Lesson — use the escape hatches.** `!` prefix **forces direct** execution;
  `?` prefix **forces the model**. If `who is alice` tried to run a `who`
  binary, prefix `?`; if a prose-looking line should run literally, prefix `!`.
- Routing changes are snapshot-tested (`routing_decision_snapshot`,
  `oracle_direct_stdout_matches_bash`) — if you change `looks_like_prose` /
  `split_route`, expect oracle/snapshot diffs and update goldens deliberately.

### 9. Runtime & config — keys, MCP startup, modes, color

- **`ANTHROPIC_API_KEY` missing/invalid** → the Claude backend can't invoke;
  set `export ANTHROPIC_API_KEY=sk-ant-…`. (Local inference via `--features
  local` / `--backend local` needs no key but needs the GGUF model in the HF
  cache.)
- **MCP servers fail to start** → check `.mcp.json` (repo) / `~/.aish/.mcp.json`.
  Servers connect over **stdio**; a server binary not on PATH, or a bad command,
  silently drops its tools/skills from the prompt. `McpHost::start` →
  `tool_defs` (`tools/list`) is the path; a server that never answers
  `tools/list` contributes nothing.
- **Confirmation gates** are per-session via `:mode <paranoid|careful|normal|
  yolo>` (`tools::Decision`): paranoid=confirm all, careful=writes, normal=write
  +delete, yolo=none. "It keeps asking me to confirm" / "it ran a delete without
  asking" is almost always the wrong mode for the session.
- **No color / garbled ANSI** → `NO_COLOR` disables styling; piped stdout
  auto-disables markdown rendering (`style.rs` / `md.rs`). Not a bug.
- **State lives in `aish.db`** (SQLite + sqlite-vec): history, memories (vector
  recall), worker/coordinator rows. A corrupt/locked db surfaces as history or
  `recall`/`remember` failures — inspect with the `sqlite3` CLI before assuming
  app logic is broken.

---

## Triage procedure

1. **Classify the symptom by surface.** Cutting a stable release → §0. Cutting a dev release → §0b. Release/`:update` broken → §1–2. Build killed →
   §3. CI red → §4 (or §2 if it's the version guard). macOS won't launch → §5.
   Worker stuck → §6–7. Wrong execution of a typed line → §8. Keys/MCP/modes →
   §9.
2. **Read the ACTUAL failure, not the source.** For CI/releases pull the run:
   `gh run view <run-id> --log-failed` (and `gh run view <run-id>` for the job
   graph). For a coordinator, read `.atum/run-<id>.jsonl`. For a build, capture
   the real `cargo` exit + last 20 log lines (a `| grep` pipe can mask the true
   failure — see `scripts/install-ubuntu-24.04.sh` for why).
3. **For any release issue, reconcile three facts before acting:** the pushed
   **tag**, `Cargo.toml` on the **tagged commit**, and `Cargo.toml` on
   **`origin/main`** (`git show origin/main:Cargo.toml`). Plus: does a
   *published* (non-draft) release already exist? `gh release view vX.Y.Z
   --json isDraft,assets`.
4. **For build OOM, confirm the feature set.** Was it `--no-default-features`?
   How many concurrent builds? Was `flock` present? Re-run with
   `scripts/build.sh --release` and watch RSS.
5. **Reproduce the CI gate locally:** `cargo test --no-default-features --locked`
   (= `make test`). Don't debug with a different feature set than CI runs.
6. **Fix on a feature branch → DRAFT PR.** Never push `main`; never push a
   release tag without org-admin bypass. Bump **both** `Cargo.toml` and
   `Cargo.lock` for any version change.
7. **Verify:** for a release, confirm the workflow published assets
   (`gh release view vX.Y.Z --json assets` shows all 3 binaries +
   `.sha256` + `SHA256SUMS`) and that `origin/main`'s version matches the tag.
   For a build/test fix, a clean `make test` + `make build-fast`.

## Anti-patterns (do NOT do these)

- **Pre-publishing a GitHub Release** (`gh release create` before the tag push)
  — it freezes immutable and the asset upload 422s, breaking `:update`. Push
  only the tag, or pre-create a **draft**.
- **Re-pushing a burned tag** in a retry loop after a delete — bump to the next
  patch instead (§1).
- **Tagging a version bump that never merged to `main`** — `verify-version`
  passes but `main` lags and `:update` drifts (§2).
- **Building with the `local` feature in CI tests / coordinator worktrees** — it
  OOM-kills the host; use `--no-default-features` there. (The **release** build
  is the deliberate exception — it uses `--features local` on clean runners since
  v0.20.0/PR #294; do NOT generalize that flag back to CI/coordinator builds. §3.)
- **Running unbounded concurrent `cargo build`s across worktrees** — use
  `scripts/build.sh` (flock) and keep the `jobs` cap.
- **Testing with a different feature set than the CI gate** (`--no-default-
  features --locked`) — then being surprised CI disagrees.
- **`cp`-ing the binary onto PATH on macOS** instead of `make install` —
  reintroduces `zsh: killed aish` (§5).
- **Brute-forcing a looping coordinator by raising `MAX_ROUNDS`** instead of
  reading `.atum/run-<id>.jsonl` and fixing/declaring the blocker (§6).
- **Committing to / pushing `main`, or merging a worker branch from inside its
  worktree** — always go through a PR from the parent repo (§7).
- **Changing `Cargo.toml` version without refreshing `Cargo.lock`** — CI fails
  on `--locked`, not on your change.
