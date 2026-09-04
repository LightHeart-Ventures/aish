# Changelog

All notable changes to aish are documented here. Dates are the GitHub release published dates (UTC). Burned/failed release tags that never shipped valid assets (v0.18.1, v0.18.3, v0.19.0) are intentionally omitted.

## [Unreleased]

### Documentation
- **`audit_findings.md` corrected to match reality**: Findings #1 and #2 ("Memory Persistence
  Visibility" / "Silent Memory Persistence Failures") had been left marked "IN PROGRESS, waiting
  for stderr logs" since the audit tracker was last touched, but the actual fix (commit `bab395d`,
  "fix(escalate): add db fallback when session.db is None") landed the same day and has been on
  `main` ever since: `escalate()` in `src/tools.rs` now opens a database fallback when
  `session.db` is `None` and logs every memory-store attempt's outcome to stderr. Marked both
  findings RESOLVED with the confirmed root cause and verified-in-source evidence, and unblocked
  Finding #3 (coordinator stall detection), which was only deferred pending #1/#2.

### Removed
- **Vacuous `test_plugin_manifest_var_expansion` deleted**: this test in `tests/plugin_integration_tests.rs` consisted solely of `assert!(true, "placeholder for Phase 1.4 var expansion test")`, so it could never fail and provided no real coverage of `${env:VAR}` expansion. That behavior is implemented (`load_config`/`interpolate_env`/`resolve_env_refs` in `src/plugins.rs`) and is already exercised by real unit tests in `src/plugins.rs` (`env_reference_is_substituted`, `env_reference_resolves_inside_nested_structures`, `unset_env_reference_errors`, `env_default_reference_is_resolved`).
- **Vacuous `tests/golden_routing_heuristics.rs` deleted**: all 5 tests in this file (`test_looks_like_prose_english_routes_to_model`, `test_bare_yes_forces_direct`, `test_bang_prefix_forces_model`, and two others) consisted solely of `assert!(true, "...")`, so they could never fail and provided no real coverage. Routing-heuristic behavior is already exercised by the golden-snapshot test `routing_decision_snapshot` in `src/repl.rs` against `tests/golden/routing_decisions.snap`, which covers every case the deleted file only described in comments.
- **Dead per-tool/per-turn pulse tracking in `worker.rs`**: `JobInner::last_tool_outcome`/`last_turn_completion`, `WorkerJob::record_tool_outcome`/`record_turn_completion`/`latest_pulse`, and the module-level `fresh_pulse` aggregator were all unreachable — `cargo check` flagged `fresh_pulse` and `latest_pulse` as never-used. The prompt's `⟳N` badge has been state-based (running count + `fresh_terminal`) since an earlier change; this chain was leftover plumbing that recorded events nothing read. The live `Pulse` broadcast bus (`crate::pulse`, feeding `:pulse-report`) and `pulse_badge`/`fresh_terminal` are unaffected.

### Changed
- **Embedded mcpmarket skill search removed — live search now comes from the plugin**: dropped the in-process mcpmarket network search path from `skill_provider` (the `wreq`/`wreq-util` browser-impersonating HTTP client is gone from `Cargo.toml`). `:skill search` now reads the offline embedded curated index for the builtin source, while live/community search is served exclusively by the `npx-skillfish` plugin (a `provides.skill_source`). The offline index, `:skill add` (GitHub + skill.fish), and the plugin skill-source fan-out are unchanged.
- **`npx-skills` plugin removed**: npx-skills (npm registry skill search + install) is archived. Live skill import and search is now unified under `npx-skillfish` (agentskills.io/skillfish), which is more performant and upstream-maintained. Removed `plugins/npx-skills/` from the tree; documentation updated to remove references to npm-sourced skills.

## [0.45.0] - 2025-04-18

### Added
- **Voice input stack (SPR-068): full push-to-talk acquisition** (PR #748-760): aish now captures audio via `cpal`, resamples to 16 kHz using `rubato`, and feeds it to Whisper (via `whisper-rs`) for local speech-to-text. New `voice` feature gate (opt-in, disabled by default) avoids heavy native deps (ALSA/libasound2-dev, whisper.cpp C++ build) in the standard binary. Ctrl-G to record, full pipeline wired (capture→resample→transcribe→insert). Graceful degradation when audio is unavailable. See `docs/spr-068-voice-input-design.md`.

### Fixed
- **Coordinator preamble pollution in skill matching (PR #760)**: background coordinators were injecting their initialization preamble into the system context used for skill matching, causing false negatives on skills that matched the user intent perfectly. Now stripped before matching.
- **Orphaned key reader thread in voice shutdown**: eliminated a race where the voice key capture thread outlived the voice module, causing keyboard latency and delayed shutdown. Proper thread lifecycle coordination on disable.
- **Voice feature gate build isolation**: voice optional deps (`cpal`, `rubato`, `whisper-rs`, `crossterm`) are now gated correctly so they never leak into the default (Claude-only) build — reduces CI gate size and link time dramatically.

## [0.42.0] - 2026-08-22 - 2026-08-22

### Added
- **Silent GitHub fallback for skill imports (PR #740)**: when `:skill add owner/repo` fails on skill.fish (e.g., Vercel bot challenge), aish now silently tries interpreting it as a GitHub repo path before surfacing an error. Users can type `:skill add hyperb1iss/hyperskills` and it works seamlessly — skill.fish is tried first, but if that fails, GitHub takes over. Three-path fallback: skill.fish → resolve_ref_via_search → GitHub import, with error prioritization (Vercel challenge → GitHub-specific error).

### Changed
- **Per-turn call-budget defaults raised (soft 20→35, hard 30→50, PR #739)**: the cumulative per-turn tool-call budget (`loopguard::CALL_BUDGET_SOFT` / `CALL_BUDGET_HARD`) defaults to a soft advisory at 35 and a graceful hard yield at 50 (was 20/30 per the original TASK-357 card). This gives a legitimately-wide multi-file edit+build+test turn more headroom before it yields to resume with fresh context. Both ceilings remain operator-configurable at runtime via the existing `AISH_CALL_BUDGET_SOFT` / `AISH_CALL_BUDGET_HARD` env vars (resolved in `engine::call_budget`, clamped to `[1, 100000]`). The system-prompt budget guidance and the `loop-guards.md` env-var reference were updated to match.

## [0.41.1] - 2026-08-21

### Fixed
- **Ghost-worker launch race (PR #737)**: a coordinator could return and let `main` tear down its in-flight worker children in the window between `tokio::spawn` and the child's PID being set — leaving detached processes that died at spawn and surfaced as stale `coordinating ♥` rows. `engine::run_coordinator` now holds coordinator exit behind a launch-handoff barrier that waits until every sub-worker has fully detached (PID set), with a 30s ceiling that degrades to exit-anyway. Adds `worker::launching_count()` and `worker::await_launch_handoff()`.

## [0.41.0] - 2026-08-21

### Added
- **OpenAI-compatible backends (PR #735, #733)**: aish now supports OpenAI and OpenRouter as alternative LLM backends. Set `OPENAI_API_KEY` and use `:backend openai` or `:model gpt-4o` to switch. Backends are transparently integrated into the agentic loop and honor the same tool-call orchestration, system prompt layering, and streaming contract as Claude. Full parity on reasoning, function calling, and error recovery.
- **Native I/O redirection in the pipeline (PR #732)**: shell-style I/O redirection operators (`>`, `>>`, `<`, `2>`, `2>&1`, `&>`) are now first-class constructs in aish's native in-subset language. No shell is invoked; piping and redirection are part of the core pipeline grammar, enabling cleaner scripting without escaping to bash.

### Changed
- **Piping and redirection fully documented**: see `docs/plans/piping-redirection.md` for the design rationale, grammar, and examples.

### Fixed
- **Test oracle cleanup**: removed stale negative cases for redirection now that `>` is native in the in-subset grammar.

## [0.40.3] - 2026-08-19

### Fixed
- **DB health guard + self-check on launch (PR #727)**: aish now self-checks the SQLite health gate on startup (catches corrupted databases before the REPL bakes in the corruption), and enforces per-session guardrails so a DB-side write failure triggers a graceful fallback (session-local memory store, no data loss). The explicit `check_db_health()` callsite is available for future health checks.
- **Worktree lifecycle + .atum telemetry exclusion (PR #727, #728)**: fixed worktree dirty-state probes (`is_clean()`, `sweep_worktrees()`) to exclude the `.atum` directory (telemetry, session logs, transient state) from git porcelain checks. Prevents false-positive "dirty" states when `.atum/` contains untracked session files, so worktree sweeps no longer trap on noise. Also improved error thresholds on worker dispatch failures.
- **System prompt budget guidance now reflects actual tool-call limits (PR #726)**: the per-turn tool-call budget guidance in the system prompt now dynamically embeds the actual configured constants (`MAX_TOOL_CALLS_PER_TURN`, `WARN_TOOL_CALLS_THRESHOLD`) instead of stale hardcoded numbers, so the prompt always stays in sync with the engine's runtime behavior.


## [0.36.2] - 2026-07-08

### Added
- **Terminal footer resync on SIGCONT wake (PR #651)**: terminal footer + idle timer re-sync when the process wakes from `SIGCONT` (e.g., fg after Ctrl-Z + bg). Prevents stale footer state on resume.
- **Repository entry announcement (PR #654)**: aish now prints "Working with repository: <name>" on first repo entry in a session, clarifying context in multi-repo workflows.
- **Terminal footer dynamic resize (PR #654)**: terminal footer redraws on window resize within one heartbeat tick instead of waiting for the next turn, improving responsiveness.

### Changed
- **Release workflow consolidation (PR #653)**: merged `release.yml` + `release-prod.yml` → `release-production.yml`. Added `workflow_dispatch` trigger, reusable `build-release-binary.yml` to eliminate duplication, and clearer workflow naming (`release-ci.yml` → `release-ci-cd.yml`). Documented in `workflows/README.md`.
- **Codebase-memory auto-index warnings quieted (PR #652)**: moved auto-index handoff warnings from interactive output to a durable log, reducing noise in typical workflows.
- **Documentation reorganized (PR #654)**: restructured docs into a tiered hierarchy with `INDEX.md` navigation hub, consolidated release docs into `RELEASE.md`, archived completed work, organized reference/internals/formats into subdirectories, and created `archive/MANIFEST.md` for historical context.

## [0.36.0] - 2026-07-07

### Added
- **`aish-webhook-broker` — self-hosted webhook broker for the plugin system (PR #515, SPR-059 Phase 4)**: a new standalone crate (`crates/aish-webhook-broker`) shipping the `aish-webhook-broker` binary — a single self-contained server (embedded SQLite, no external services) that ingests webhooks from external producers (GitHub, Slack, GitLab, …) and fans them out to connected aish clients. Webhooks are routed by `(tenant_id, plugin_id)`, verified with constant-time **HMAC-SHA256** (GitHub-compatible `sha256=` prefix, `X-Signature`/`X-Hub-Signature-256`), persisted to a WAL-mode SQLite queue (durable source of truth — survives client disconnects and broker restarts), and delivered in real time over **WebSocket** (`GET /ws`) with an HTTP **long-poll** fallback (`GET /webhooks/:tenant/:plugin/pending?wait_secs=N`). Delivery is at-least-once (messages held until explicitly ACKed via `DELETE …/messages/:id` or a WS `ack` frame), the per-route queue is bounded (`--max-queue-size`, oldest-drops-first overflow), and undelivered messages expire on a configurable TTL (`--msg-ttl-secs`, default 7 days) purged by an hourly sweep. Endpoints: `GET /health`, `POST /clients/register`, `POST /webhooks/:tenant/:plugin`, `GET /webhooks/:tenant/:plugin/pending`, `DELETE /webhooks/:tenant/:plugin/messages/:id`, `GET /ws`. Fully configurable via CLI flags or `BROKER_*` env vars (`BROKER_LISTEN`, `BROKER_DB`, `BROKER_MAX_QUEUE_SIZE`, `BROKER_WS_HEARTBEAT_SECS`, `BROKER_POLL_TIMEOUT_SECS`, `BROKER_MSG_TTL_SECS`, `BROKER_LOG_LEVEL`), with graceful `SIGINT`/`SIGTERM` shutdown. Ships with a README plus `docs/API.md`, `docs/CONFIGURATION.md`, and `docs/DEPLOYMENT.md` (Docker, systemd, AWS EC2/ECS), and unit + in-process HTTP integration tests.
- **Broker deploy assets — first-class, shipped in-crate (PR #516, SPR-059 Phase 4)**: the `aish-webhook-broker` crate now carries ready-to-run deployment tooling instead of doc-only templates — a multi-stage `Dockerfile` (builds the binary against the runtime libc) with a `.dockerignore`, plus `deploy/docker-compose.yml`, a hardened `deploy/aish-webhook-broker.service` systemd unit, a `deploy/broker.env.example` env template, and `deploy/README.md`. `docs/DEPLOYMENT.md` now points operators at these shipped files (build/install one-liners) and keeps only the AWS ECS/EC2 task definitions as fill-in templates.
- **GitHub reference plugin for the webhook pipeline (PR #517, SPR-059)**: a complete example plugin under `examples/plugins/github/` demonstrating the end-to-end webhook path — `plugin.json` (declared `webhooks` handlers), an aish-native `hooks.json` (lifecycle + `X-GitHub-Event` → script routing), `config.json`, `login.sh`, `.mcp.json`, per-event `handlers/` (`pull_request`, `issues`, `review`), lifecycle `hooks/` (`on_init`, `on_shell_ready`, `on_webhook_url_changed`), payload `schemas/`, tool definitions (`add_comment`, `create_pr`, `list_issues`), and bundled `skills/` (issue triage, PR review). Documented in the broker crate's new `docs/PLUGINS.md`.
- **`aish-webhook-client` — the consumer side of the broker (PR #518, SPR-059 Phase 5)**: a new standalone crate (`crates/aish-webhook-client`) that aish embeds to connect to a running broker, authenticate for a `(tenant_id, plugin_id)` route, and process webhooks. Loads `~/.aish/config/broker.json` (`BrokerConfig`: `broker_url`, `tenant_id`, `plugin?`, `transport`, `enabled`, `secret?`, `client_id?` — a missing/`enabled:false` file is a soft no-op), maintains the session over WebSocket with capped exponential-backoff reconnect (`backoff.rs`) and at-least-once resume from the broker queue, and speaks a small JSON frame protocol (client `auth`/`ack`/`pong`; server `webhook`/`auth_ok`/`ping`, tolerating bare untyped envelopes). A `WebhookDispatcher` loads plugin `plugin.json` manifests (`webhooks`/`handlers`), matches each event (`"*"` = all), applies AND-combined dotted-path equality `filters`, and fork/exec's every matching handler **concurrently with full failure isolation** — no shell, payload on **stdin**, `WEBHOOK_ID`/`WEBHOOK_TENANT_ID`/`WEBHOOK_PLUGIN_ID`/`WEBHOOK_EVENT_TYPE` in the env, per-handler `timeout_secs` (default 30 s, kill-on-timeout). The connection + message loop is trait-abstracted (`Transport`) and fully tested against an in-memory `MockTransport`; the real `ws://`/`wss://` transport compiles under the `net` feature. Documented in the broker crate's new `docs/CLIENT.md`.

### Fixed
- **Container worker image self-builds the aish binary (kills the glibc-mismatch failure class)**: `Dockerfile.worker` is now a multi-stage build that compiles `aish` INSIDE a `rust:1-bookworm` builder stage and copies the resulting binary into the matching `debian:bookworm-slim` runtime, so the container's glibc always matches the binary. Previously it `COPY`ed the HOST-built `target/release/aish`, which coupled the runtime to the host's glibc — a binary built on a modern host (Ubuntu 24.04 → glibc 2.39) built + inspected fine but died at container exec with `GLIBC_2.39' not found`, surfacing to operators only as opaque failed background jobs. Added a new `.dockerignore` (keeps `target/`, `.git`, worktrees out of the build context so no host binary leaks in) and a belt-and-braces preflight in `container.rs::image_runnable` — `build_container_command` now runs a one-shot `<engine> run --rm <tag> --version` probe and degrades to the host subprocess with an actionable diagnostic if the image's binary can't exec, instead of launching a doomed container. `make worker-image` no longer depends on a host `build` and self-builds against the runtime libc. The release-time `worker-image.yml` GitHub Actions workflow was also brought in line with the self-build: it no longer installs a Rust toolchain, caches cargo, runs a runner-side `cargo build --release`, or passes the now-ignored `AISH_BIN=target/release/aish` build-arg — it just hands the source context to buildx and lets the Dockerfile compile against the runtime's libc (the in-image build is cached by Blacksmith's native layer cache). Its comments previously described the removed COPY-host-binary design, which would have tempted a maintainer to reintroduce the exact glibc bug. The in-image build is now `--locked` so the published image can't silently drift from `Cargo.lock`.
- **Shift-Tab with no coordinators is a silent no-op**: pressing Shift-Tab (the worker-cycle key) when this session has launched no coordinators no longer clears/redraws the screen or prints a hint — it now takes no action at all, leaving the prompt exactly as it was. Previously an empty cycle still wiped the screen (via `clear_screen_anchor_bottom`) before discovering there was nothing to attach to. `cycle_worker` now guards the empty case before any screen manipulation and returns whether it acted so the REPL only arms the post-cycle prompt gap when the cursor actually moved; the mid-turn `cycle_worker_live` sibling is silenced the same way.

### Changed
- **`:goal` turns now surface a per-turn `message_console` note (turn summary + any PR opened)**: the per-turn generator directive (`goal_directive`, the prompt each full-tool coordinator subprocess pursues) now instructs the worker to call `message_console` once before finishing the turn with (1) a one/two-line summary of what it did and the evidence, and (2) the number/URL and one-line summary of any pull request it opened that turn. Because the `:goal` loop runs unattended, this gives the operator always-surfaced (`📣`, `:worker-output`-gate-bypassing) live progress without polluting the stdout result the verifier judges. The reporting instructions live in the shared `GOAL_DIRECTIVE_PREFIX`, which the inverse parser (`goal_condition_from_directive`, backing `:workers` goal-turn coalescing) strips whole — so the recovered condition and its grouping key stay stable.
- **Dev release reuses same-commit CI/CD builds instead of recompiling**: `release-dev.yml` now detects when a published `ci-<run>-<sha>` release already exists for the current `main` commit and carries every asset the selected platform set needs. When it does, the build matrix is skipped and the `release` job downloads and re-publishes those byte-identical binaries under the `dev-v…` tag (the Linux release build is reproducible; the macOS builds differ only by non-deterministic ad-hoc signatures — same source). This saves ~4–6 min of runner time (and the associated CO₂) for zero output change on unchanged commits, and falls back to a normal compile whenever no complete same-commit CI release is found (CI still building, pruned, etc.). A new `force_build=true` workflow-dispatch input opts out and forces a fresh compile.
- **Collapsed tool-output activity stream + symmetric Ctrl-O toggle**: after a tool/worker call the interactive activity stream no longer echoes the last 5 output lines — it shows a single `… N lines of output — Ctrl-O to expand` summary above the running status line. Ctrl-O is now a true toggle: pressing it expands the last turn's tool results verbatim (`reveal_last_turn`), and pressing it again re-collapses them back to the line-count summary (new `engine::collapse_last_turn`). The full output is always one keystroke away instead of consuming scrollback on every call.

### Added
- **Docs: `:goal` long-horizon goals (README + `:help`)**: the README now documents the `:goal` subsystem (SPR-058) — a durable, cross-session **goal → milestones → tasks + blockers** hierarchy stored in `~/.aish/aish.db`, injected into every turn's context while active, with the full subcommand surface (`new`/`show`/`status`/`link`/`block`/`unblock`/`milestone`/`complete`) laid out in a reference table under a new "Goals" section and cross-linked from the REPL Commands list. Closes the documentation gap for the goal feature that shipped across TASK-276..279/282/283.
- **Audible finish-bell when a background worker/batch/coordinator completes**: the interactive presenter now rings a terminal bell (ASCII `BEL`, written to `/dev/tty` so it survives stderr redirection) the moment any background job reaches a terminal state — a finished coordinator entering review-mode, a completed batch/worker notice, or an armed hands-free resume. On by default; opt out with `AISH_WORKER_BELL=0` (also accepts `off`/`false`/`no`), or replace the beep with a real sound file via `AISH_WORKER_BELL_CMD` (run shell-free, fire-and-forget), e.g. `AISH_WORKER_BELL_CMD="paplay /usr/share/sounds/freedesktop/stereo/complete.oga"`. Best-effort: a missing player or non-tty never breaks the presenter. New `tools::play_finish_bell` + a pure, unit-tested toggle predicate.
- **Plugin manifest — `provides.lifecycle_hooks` (renamed from `provides.hooks`)**: the plugin `plugin.json` manifest now parses a `provides` block; plugin *lifecycle* hooks (`on_init`, `on_shell_ready`, `on_shutdown`, …) are declared under `provides.lifecycle_hooks`. The old `provides.hooks` key remains a **deprecated alias for one release** — manifests using it still load, `PluginManifest::lifecycle_hooks()` resolves the effective list (canonical `lifecycle_hooks` wins when both are set), and discovery emits a one-time deprecation warning nudging authors to rename. This frees the word "hooks" for the forthcoming event-catalog contribution surface. See `docs/PLUGIN_SYSTEM_DESIGN.md` § 0.5.1.
- **Plugin system — skill-registry expansion (first slice)**: aish now discovers plugins under `~/.aish/plugins/<id>/` and merges each enabled plugin's skills into the same catalog it advertises for `~/.aish/skills`. A plugin is any directory with a readable `plugin.json`; its skills use the standard `skills/<name>/SKILL.md` layout. Installed skills win on a name collision; disabled/malformed plugins are skipped silently. New `src/plugins.rs` + `skills::load_catalog`, wired into startup, the deferred interactive MCP handshake, and `:skill` reloads. Ships a runnable `examples/plugins/hello-world/` plugin that contributes one greeting skill as an end-to-end proof. See `docs/PLUGIN_SYSTEM_DESIGN.md` § Implementation status.

## [0.21.1] - 2026-07-01

### Changed
- **Interactive aish system-prompt refresh (LightHeart persona)**: the interactive system prompt is refreshed to the current LightHeart persona.
- **NEVER FABRICATE, ALWAYS VERIFY guardrail**: added to both the system and worker prompts — agents must confirm claims with evidence (tool output, live state) rather than asserting unverified results.

## [0.21.0] - 2026-07-01

### Added
- **Lifecycle hooks — `PreToolUse` blocking gate**: hooks can now act on lifecycle events and *block* a tool call before it runs, not just observe it (builds on the observe-phase hook foundation).
- **`:stop` coordinator stand-down channel**: a harder-than-`:tell` control channel that tells a running background coordinator to stand down, distinct from queuing a mid-flight steering message.
- **Parent session wakes when fanned-out coordinators complete**: when a session's fanned-out background coordinators finish, the parent session is woken to consume the results instead of requiring a manual "continue" prompt.
- **`read_file` 1-based line-range slicing**: read a bounded line range of a file instead of re-reading the whole thing — cuts the large-file re-read loop-guard trips seen in coordinator runs.
- **Ctrl-C interrupts an attached worker's current turn**: interrupt the in-flight turn of an attached worker without killing the whole run.
- **Local backend auto-downloads the detected GGUF from Hugging Face on first use**: the `local` inference path fetches the hardware-appropriate model on demand rather than requiring a manual download.

### Changed
- **Coordinator re-evaluates its fan-out plan after triage**: stops over-decomposing — a coordinator that has already isolated a single root cause no longer blindly fans out N sub-agents.
- **Animated ⤴️ escalation banner** in the REPL.
- **`:output` pane polish**: wrapped pane rows are hang-indented and glyphs are aligned to the rocket column.
- **Blank line before always-surfaced console notes** for clearer worker → operator console output.

### Fixed
- **Retrievable sub-job output + deterministic fan-out retrieval**: fanned-out sub-job results are now retrievable by id deterministically (with tiered routing), closing the "children reported success but output was unretrievable" gap.
- **Worker memory rlimit floored** so V8/Node-based tools (e.g. `neonctl`) can start under a background worker instead of being aborted at startup by a too-low `RLIMIT_AS`.
- **Attached coordinator's final result is surfaced live** and no longer truncated in live-attach review mode.

## [0.20.0] - 2026-06-30

### Changed
- **Release binaries now ship with the local backend built in**: the release build adds `--features local`, so the published `aish` binaries include the llama.cpp / GGUF local-inference backend (`--local`) out of the box instead of requiring a from-source rebuild to enable it.

## [0.19.3] - 2026-06-30

### Added
- **Hardware-aware local model selection** (whichllm-style): the local backend inspects the host and picks an appropriate GGUF model/parameters for the detected hardware.

### Changed
- **Final, clean release of the llama.cpp local-backend line**: consolidates the v0.19.1/v0.19.2 work onto `main` (recovering the burned v0.19.0 tag) so the local backend is production-ready on the default branch.

## [0.19.2] - 2026-06-30

### Changed
- **Stabilized the local llama.cpp backend** for production use: greedy-sampling inference with a 512-token output limit, GPU offload via `AISH_LOCAL_N_GPU_LAYERS` (default 0 / CPU-only), and `AISH_LOCAL_MODEL_PATH` for an explicit model path. Resolves the tagging issues behind the earlier v0.19.0/v0.19.1 attempts.

### Notes
- Local inference is **text-only** at this stage — tool calling is not yet supported on the local backend, and a GGUF model file must be present (downloaded separately).

## [0.19.1] - 2026-06-30

### Added
- **Local llama.cpp backend** (`--local`, shorthand for `--backend=local`; feature-gated behind `cargo build --features local`): run aish against a local **GGUF** model for fully offline inference.
  - **Mistral 7B Instruct** as the default model (4096-token context window).
  - **Lazy model loading** via a `prepare()` hook so the model loads before the spinner starts.
  - Configurable through `AISH_LOCAL_MODEL_PATH` (path to the GGUF file) and `AISH_LOCAL_N_GPU_LAYERS` (GPU layer offload).
  - Clean re-release of the llama.cpp backend after the burned v0.19.0 tag (PR #291).

## [0.18.4] - 2026-06-30

### Added
- **`:batch` subcommand**: force-batch the current work onto the asynchronous batch path on demand.
- **`:loop` command**: run inline, iterative agentic turns without leaving the REPL.
- **`message_console` channel**: a one-way coordinator → operator-console notification path so a background coordinator can surface a heads-up immediately without ending its run.
- **Serialized, Claude-only build path for coordinator / CI / multi-worktree rebuilds** (`scripts/build.sh`, `make build-fast`): two OOM mitigations bundled so every automated rebuild inherits them. (1) `--no-default-features` drops the heavy `local` (mistralrs / candle / gemm) feature — the whole opt-level=3 phase and the crate that peaks past 1.5 GB per rustc — wherever in-process inference isn't needed (already the policy in CI, release, and the Ubuntu installer; `make build-fast` and `scripts/build.sh` bring it to the local/coordinator path too). (2) A single advisory file lock — `flock /tmp/aish-build.lock` — serializes builds so the dozens of background-coordinator worktrees on one host can't overcommit RAM at once; the `.cargo/config.toml` `jobs` cap only bounds ONE build's internal parallelism, this bounds *cross-build* concurrency to 1. Every `make` build/test target now takes the lock (`LOCKED` prefix, a no-op on hosts without `flock` such as macOS). Pass `--features local` / `make test-local` to opt local inference back in.

### Changed
- **Shift-Tab cycles into an active `:goal`** loop, so you can hop straight into a running goal from the prompt.
- **429 rate limits are now ridden out in-worker** via the response `Retry-After` header instead of failing the turn.
- **Release CI fails fast on a pre-published immutable release** (guards the burned-release footgun) and ships an accompanying release runbook.
- **Removed the redundant `:quit` colon command.**

### Fixed
- **Ubuntu installer**: adds `clang` / `libclang-dev` and corrects the `rustup update` flag order so the install no longer fails silently.

## [0.18.2] - 2026-06-30

### Added
- **Coordinator task pinned verbatim into the never-compacted system prompt**: a background coordinator's original instructions are now reproduced in a part of the prompt that history compaction can't drop, so a long-running coordinator never loses its source of truth.
- **`SkillMatched` observe hook**: an installed-skill match now fires an observe-phase lifecycle hook (hook-system foundation).

### Changed
- **Test builds drop `mistralrs-core` by default**: the CI `Test` job and the new `make test` target both run `cargo test --no-default-features`, so the heavy `local` in-process model (mistralrs / mistralrs-core / candle) is no longer compiled for the unit/oracle/pty suites unless a build explicitly opts back in. Exercise the local-inference path on demand with `make test-local` or `cargo test --features local`. The CI `Test` step (previously a stubbed `exit 0` over a since-resolved openssl/btls linker note) is re-enabled now that `cargo test --no-default-features` links cleanly.
- **Cleaner markdown rendering**: boxed tables are tidied up and more markdown is humanized.
- **Internal tool progress lines name their target** (e.g. the file or host a built-in tool is acting on).
- **Dropped Ubuntu 20.04 LTS** from the supported platforms.

### Fixed
- **Removed an erroneous command echo from `run_program` output** and repaired the stale command-echo test assertions that were breaking `main` CI.
- **Parallel background-job isolation** fixed so concurrent jobs no longer interfere.
- **Tests no longer assume JSON key ordering**, removing a flaky-on-reorder failure mode.

## [0.18.0] - 2026-06-30

### Changed
- **Repo-navigation prompt prioritizes `.repospec.json`**: when analyzing a repository, agents are pointed at the repospec metadata first.

### Fixed
- **`:new` no longer bleeds the prior conversation** into the fresh session.
- **`:skill search` fetches skills from the live origin** instead of a stale `file://` index.

## [0.17.0] - 2026-06-29

### Added
- **Session-scoped job filtering** (S9.6 / TASK-251): The `:workers` command (and `background_status` tool) now accepts a `filter` argument to narrow to current-session, specific-job, or all-tenant jobs. Defaults to session scope for interactive use (`status/session`), matching the REPL's mental model. Useful when you have dozens of background coordinators across multiple sessions and want to focus on the current session's work.
- **CI conflict escalation + playbook surfacing**: When CI fails or a merge conflict is encountered, the system now surfaces the `fix-ci` and `fix-conflicts` skill recommendations alongside a hand-off to run them. Operators get guidance on where to read the root cause + proposed fix plan.
- **Repospec metadata** (`.repospec.json`): Added a standard [repospec/v1](https://github.com/LightHeart-Ventures/repospec)-compliant metadata file documenting aish's 3 entrypoints, 13 modules, 8 patterns, 5 features, 3 infrastructure layers, 6 dependencies, and 5 project goals. Agents can now read one file to understand the codebase structure instead of keyword-searching through 2500+ lines of source.

### Changed
- **Background-mode nudge tightened**: Clarified that a question — including "what's running?", "didn't we dispatch a worker for this?", or "what is the coordinator doing?" — must be answered inline (via `background_status` or a lookup), never offloaded to a fresh coordinator.

## [0.16.0] - 2026-06-29

### Added
- **`aish --version` flag and `:version` REPL command**: Query the running aish version from both CLI and the REPL. `aish --version` wires clap's version attribute to print the build version; `:version` (alias `:ver`) shows the version plus the active backend via `backend.describe()`.

### Fixed
- **Structured tool results threaded to model + Ctrl-O keeps raw view** (S7.3): The optional typed JSON payload now reaches the model as compact JSON instead of alignment-corrupted ASCII, while Ctrl-O keeps the human-readable text view unchanged.
- **curated registry index** with 20 high-value installable skills from skillfish ecosystem.

## [0.14.3] - 2026-06-29

### Added
- **The S7 structured-tool-results capability is now tested + scope-bounded** (S7.4 / TASK-142): both result paths are pinned by deterministic unit tests — the **string-only** path renders to each backend's wire format with **no** payload key, `content` verbatim, and `is_error` honoured (the exact-key-count assertions fail if a payload ever leaks onto the wire as a sibling field); the **structured** path is proven **additive** — the typed payload reaches the model (`model_content`) while `content` and the Ctrl-O raw view (`raw_body`) are byte-identical to the equivalent text-only result (the payload never substitutes the human view). A written **scope guardrail** (`docs/S7.4-tests-docs-scope.md` §3, pointed to from the `ToolResult` definition) draws the hard line: an aish tool may *describe* its result in a typed way, but aish must **never operate on those types as a programmable pipeline** — no piping/composition, no query language (jq/JSONPath/`:select`/`:where`), no persistent typed-result store, no schema registry. Tests + docs only; no new runtime surface.
- **Structured tool results are threaded to the model + Ctrl-O keeps the raw view** (S7.3 / TASK-141): the optional typed payload S7.2 attaches to record/table tools (`list_dir`, `glob_expand`, `grep_files`, `stat_file`, `diff_files`, and MCP JSON passthrough) is now sent to the model as **compact JSON** instead of the alignment/ellipsis-corrupted ASCII rendering — so the LLM parses trustworthy structure rather than re-deriving it from a table. The model-facing representation lives in one place (`ToolResult::model_content`); both the Claude and Grok renderers thread it. The split is deliberate: the **model** gets the JSON, while the **Ctrl-O raw view** (`engine::raw_body`) keeps showing the verbatim, human-readable `content` for every tool — never a JSON dump — with a structured-only fallback to pretty-printed JSON when a result has no rendered text. Plain text-only tools are unchanged for both paths. _Deferred (OQ3):_ there is no per-result-set token-cost cap yet — compact JSON for a large `grep_files`/`glob_expand` payload can be heavier than the rendered text; this is flagged with a `TODO` in `ToolResult::model_content` and will be revisited post-S7.3.
- **Offline skill-install recommendation on no local match** (`src/skill_match.rs::recommend_install`): when no INSTALLED skill fits a substantial task, aish now ranks the binary-shipped registry index (`~/.aish/registry/index.json`, read offline via `skill_provider::local_index_catalog` — no per-turn network) and, when a relevant skill clears the same name-level bar the local nudge uses, folds in a `[aish skill-awareness] … :skill add <ref>` recommendation. This closes the "no local skill → recommend installing one" half of the skill-awareness design (the local-match half already existed). Deduped per session (`Session::skill_suggested`) so the same skill is suggested at most once; gated by a skill-worthy token-count heuristic so trivial commands never trigger it. A full live mcpmarket/skill.fish search stays explicit via `:skill search`.
- **`search-skills` reference skill** (`examples/skills/search-skills/`): the user-invoked, richer-output sibling of the automatic awareness above — ranks INSTALLED skills in a table with star ratings and, on no local match, recommends an INSTALLABLE one. Reconciled to sit on top of the engine rather than compete with it: it reads the **same** offline registry index, re-states the engine's single name-weighted relevance rule (no second scoring formula), triggers on prose (it does NOT shadow the live-network `:skill search` verb), and is written for aish's Rust reality (no `invoke_skill` host call — "using" a skill is reading its `SKILL.md` and following it).
- **Hook-system foundation**: lifecycle hook infrastructure for the observe phase, plus session-management improvements (coordinator lifecycle, `:close` / `:forget`) and worker-UX polish (Shift-Tab cycling through finished/failed coordinators, screen clear, runtime tracking).
- **Claude OAuth credential support**: read Claude AI OAuth tokens from `~/.claude/.credentials.json`, with detection of expired tokens and guidance to refresh them.

### Changed
- **Background-mode nudge: answer questions inline, don't dispatch them**: the `BATCH_NUDGE` (`src/session.rs`) now draws a hard line between *work to DO* and *a question to ANSWER*. A question — including "didn't we already dispatch a worker for this?", "what is it doing?", or any ask about the status/history of existing work — must be answered inline (via `background_status` or the relevant lookup), never offloaded to a fresh coordinator. Fixes the observed misfire where, asked "didn't we dispatch a worker to build it?", aish spawned a *new* background coordinator instead of just checking and replying.
- **Skill usage is stated plainly in the prompt + nudge**: the system-prompt Skills section and the per-turn `[aish skill-awareness]` note now spell out that USING a skill simply means reading its `SKILL.md` and following its steps — there is no separate command to "invoke" a skill — so the agent stops claiming it "can't run a skill from this interface" and reaches for the installed playbook (or recommends installing one) instead of silently hand-rolling the work.

### Fixed
- **Context compaction now happens inside the agentic loop**, not between turns, and a captured result is replayed when a live-attached worker finishes.

## [0.14.2] - 2026-06-28

### Added
- **miette-backed diagnostics** (S7.1 / TASK-139): aish now has a first-class diagnostic surface (`src/diag.rs`, `AishDiagnostic`) built on [miette](https://crates.io/crates/miette) + [thiserror](https://crates.io/crates/thiserror). A forced-shell parse failure (`!cmd`), a malformed `~/.aishrc` line, or a forced command-not-found now renders with a byte-span **caret**, a stable **`aish::…` code**, and a did-you-mean **`help:`** line instead of a bare drop or an ad-hoc `eprintln!`. Six stable codes: `aish::parse::{unbalanced_quote,unsupported_meta,empty_stage,bad_var_ref}`, `aish::config::bad_export`, `aish::exec::not_found`. Rendering honors the existing color policy (`NO_COLOR` / `--no-color` / non-TTY → plain text, still caret+code+help; color on → graphical theme).
- **Span-aware tokenizer** (`rc::tokenize_diagnosed`): the one tokenizer is now span-aware; `rc::tokenize`/`tokenize_with`/`tokenize_pipeline` are `.ok()` shims over it, so the silent route-to-model path is byte-for-byte unchanged while the forced (`!`) path can explain *why* a line wasn't a command. Exec misses on a forced command surface a cheap, bounded (edit-distance ≤ 2) `$PATH` did-you-mean.
- **Ubuntu 22.04 / 24.04 LTS installation guides and one-command installer**, with fixes for two silent installer failures (rustup + cmake) and a switch from the `getaish.com` domain to `aish.sh`.

### Changed
- **`~/.aishrc` parse errors are now coded + located**: the previously side-effecting dim `eprintln!` skips in `rc::parse_into` become `aish::config::bad_export` diagnostics with a `~/.aishrc:N` header and a caret on the offending token; rc parsing still continues past a bad line (a single malformed export never drops the rest of the file). A `parse_into_diagnosed` seam makes the emission testable.
- **Shift-Tab also cycles into the active `:goal` loop**, and `run_program` output now displays the full command with its arguments.

### Fixed
- **Thinking animation is cleared when the user Shift-Tabs away mid-think.**

## [0.14.0] - 2026-06-27

### Added
- **Skill-awareness layer** (`src/skill_match.rs`): each turn, aish scores the user's request against the installed local skill catalog (`~/.aish/skills`) and, when a skill clearly fits, folds a short `[aish skill-awareness] …` note into that turn's input pointing the model at the matching `SKILL.md`. Matching is keyword-overlap based — name-token hits weigh more than description hits, with a single name match enough to surface a hint and up to two top matches named. The note goes into the turn *input* (alongside `engine::seed_context`), never the cached system prompt, so the prompt-cache prefix stays byte-stable. Registry auto-search on no-match is deliberately left to the explicit `:skill search` / `--skill-search` path (no per-turn network round-trips).
- **Relevance-ranked memory recall**: `recall` now generates keyword candidates via a new FTS5 index and re-ranks them by embedding cosine similarity, so the most relevant fact leads instead of merely the newest. The long-dormant `embedding` column / `vec_memories` index are now populated (a dependency-free local lexical embedder, pluggable for a learned model later) on every `remember` and backfilled for existing rows on open. Falls back to a substring scan + recency when FTS5/embeddings are unavailable.
- **`recall` `tag` argument**: pass `tag="context-offload"` (or query `"context-offload"`) to retrieve compacted-conversation transcripts, which are now kept out of normal curated recall.

### Changed
- **History-compaction offloads are quarantined** into a dedicated `offloads` table instead of co-mingling with curated `memories`, so a routine `recall` can never drag an MB-scale transcript in front of real facts. Existing `context-offload` rows are migrated out of `memories` on open (idempotent).
- **Every `recall` hit is truncated** to a ~2 KB cap with an elision marker — a rehydrated transcript can no longer dump a six-figure-token blob into a single tool result (the offload token-blowup fix).
- **Offload transcripts are bounded** by a keep-recent (20) + max-age (7 day) reaper run on each write, so the store can't grow without limit. Curated dedup (`organize_memories`) no longer scans transcript bytes (they live in their own table).

## [0.13.1] - 2026-06-27

### Added
- **Installed Status Display in `:skill search`**: Search results now show a green `✓ installed` indicator for skills already in your local `~/.aish/skills` directory, making it easy to see what's installed vs available
- **Exponential Backoff Retry for mcpmarket Bot Protection**: When Vercel's bot challenge (HTTP 429) blocks mcpmarket.com access, aish now retries with exponential backoff (1s, 2s, 4s delays), matching skillfish's behavior
- Debug logging for skill search retry attempts and responses

### Fixed
- Graceful fallback to embedded offline skill catalog when mcpmarket is unavailable or returns transient errors

### Changed
- Enhanced `:skill search` table layout with clearer column organization

## [0.13.0] - Previous release
