# Changelog

All notable changes to aish are documented here. Dates are the GitHub release published dates (UTC). Burned/failed release tags that never shipped valid assets (v0.18.1, v0.18.3, v0.19.0, v0.48.0) are intentionally omitted. `v0.48.0` is permanently unusable: the tag name was consumed by an immutable release that was later deleted, so GitHub's pre-receive hook now rejects any attempt to recreate the ref (`tag_name was used by an immutable release` / `Cannot create ref due to creations being restricted`). The 0.48.0 work shipped as **0.48.1**.

## [Unreleased]

### Fixed
- **Statusline stops disappearing while the model is thinking** (PR #944): the footer heartbeat — the background thread that repaints the statusline from cache to heal a footer that scrolled out of view — only ran while the REPL was parked at the prompt (`READING_LINE`). For the whole duration of a turn (thinking + tool calls, often minutes) nothing repainted the footer, so anything that scrolled it away or overwrote it during that window left it gone until the turn ENDED: a mouse-wheel/trackpad scroll (which sends aish no input at all, so the shell never learns the viewport moved), a window resize leaving DECSTBM stale, or a full-screen program's leftovers. The `READING_LINE` requirement was never a safety property — the repaint is cursor-safe (DECSC/DECRC) and mid-turn type-ahead lives in `MIDTURN_INPUT`, which the repaint itself draws. The one genuine hazard, rustyline rendering a partially-typed line (`INPUT_DIRTY`), is now scoped to the prompt instead of blocking unconditionally, so the heartbeat keeps healing the footer straight through a turn. The gate is extracted as a pure `heartbeat_should_paint(HeartbeatState)` with regression tests covering mid-turn healing, resize/animation bypasses, the in-progress-prompt-line hold-off, `:attach` and foreground-TTY-child (`vim`/`sudo`) yielding, and the no-change quiet case.

## [0.53.2] - 2026-10-08

The TUI-layout release. The footer and statusline stop being rigid row stacks and become pure layout solvers that shed zones by priority, so a short window degrades gracefully instead of falling off a cliff. On top of that: the `:workers` modal wraps and scrolls instead of cropping, worker-pane bursts carry a stable per-worker hue and a seam rule, the prompt prefix is budgeted so there is always room to type, and the escalation banner stops flickering through unrelated emoji.

### Added
- **Footer solved as flex zones that degrade by priority** (PR #920): the footer was a rigid bottom-up stack whose height was derived in two independent places, and below `MIN_FOOTER_ROWS` the *entire* footer — statusline included — was dropped. `FooterLayout::solve(rows, want_banner_rows)` is now a pure solver that sheds in priority order (escalation banners → separator rule → status message; the statusline never sheds while any footer exists) and packs survivors contiguously upward. Every consumer — DECSTBM bottom margin, body-home row, resume choreography, teardown clear, and the paint itself — reads that one struct, so region and paint cannot disagree by construction. New `MIN_FOOTER_ROWS_DEGRADED` floor means a 3-row window still gets a statusline.
- **Statusline solved as priority-shedding zones with a fair segment budget** (PR #921): the statusline is now a zone grid that sheds lowest-value segments first and shares the available width budget between segments instead of letting one long segment crowd out the rest.
- **`:workers` modal wraps and scrolls instead of cropping** (PR #924): the modal hard-clipped the Task column and rendered every row unconditionally, so long tasks lost their tail and a 20-worker fan-out scrolled off-screen unreachably. Adds `wrap_cell()` (word-boundary wrapping, hard-split for overlong tokens, max-lines cap with an explicit truncation marker), `viewport()` (scrolling window that keeps the selection visible and clamps at list bounds), and `PageUp`/`PageDown`/`Home`/`End` parsing alongside the existing arrow/`j`/`k` bindings.
- **Worker-pane bursts are attributed to a speaker** (PR #925): with `:output on` every pane row carried the same uniformly-dim `[w_…]` tag, so two interleaving workers were a wall of identical rows. The gutter tag is now tinted from a stable FNV-1a hash into a 6-entry muted palette (a worker keeps its hue for the whole session, including across `:attach`/`:detach`), and a thin attribution rule is emitted **once per burst** on speaker change rather than per row. Visible widths are unchanged, so existing gutter/wrap math holds; the attached single-speaker stream is untouched.
- **Prompt prefix is width-budgeted so there's always room to type** (PR #926): three prompt segments were unbounded (goal title, cwd depth, attach id), so on a 60- or 80-column terminal the prefix could eat the whole row. `fit_prompt_prefix` fits the prefix to the real terminal width and sheds least-load-bearing first — goal badge shrinks (sigil + percent always survive) then drops, cwd elides on `/` boundaries keeping the leaf, while attach + pulse + `❯` are incompressible. 32 columns are always reserved for input, with a 12-column prefix floor. **Unknown width falls back to the legacy prefix byte-for-byte**, so piped runs and tests are unchanged.

### Fixed
- **Escalation banner stops its emoji slideshow** (PR #922): the pinned banner animated its head glyph through four unrelated pictographs at ~4.5 fps, which read as the banner changing identity rather than one thing moving. The glyph is now a stable identity sigil with a braille spinner beside the heartbeat heart.
- **`:close` now clears the worker's escalation banner immediately** (PR #923): closing a coordinator removed it from `:workers` and the Shift-Tab rotation but left its pinned escalation banner animating in the footer tray. `escalation::sweep` could never retire it either — sweep only retires a banner that reached a terminal state and then outlived its dwell, which a still-running closed worker never does from the session's point of view — so the rows outlived the worker indefinitely. `:close` now calls the new `escalation::unpin(id)`, which is strictly id-scoped (live siblings stay pinned) and a no-op for a worker that never escalated; the confirmation line names the tray only when a banner was really there.

## [0.53.1] - 2026-10-07

Patch release: clearing the clippy backlog and gating it in CI, plus three correctness fixes in the worker/REPL surface — mid-turn `Shift-Tab`, the worker status line, and worker skill-hint matching.

### Changed
- **Clippy backlog cleared and gated in CI** (PR #915): the 539-diagnostic `cargo clippy` backlog is fixed (autofixes, `manual_strip`, `unused_unsafe`, `doc_lazy_continuation`/`doc_overindented_list_items`, dead assignments, nested-if collapses in the Linux-gated `hwdetect` paths) and CI now runs `clippy -D warnings`, so the debt cannot regrow. Also clears 6 new Rust 1.99.0 lints that were blocking CI.

### Fixed
- **Mid-turn `Shift-Tab` actually switches the view** (PR #916): pressing `Shift-Tab` during a thinking turn flipped internal state but the view never changed; the keypress now switches the view the moment it's issued.
- **Worker status line no longer reads "starting up" forever** (PR #917): a worker's activity summary was only written once at dispatch, so `:workers` and the escalation lines kept showing `starting up` for the whole run. The summary is now refreshed mid-round, so the row reflects what the worker is actually doing.
- **Worker skill hints matched the quoted conversation digest instead of the task** (PR #918): workers *do* receive a skill hint, but `skill_match::match_text` was fed the entire turn input — including the quoted `=== Recent conversation context ===` digest — so a brief could match a skill from an unrelated older conversation while the skill that fits the real task never surfaced. Matching now strips preamble, digests, and already-rendered hint blocks and scores only the TASK region, with an `already_hinted` dedup gate. The interactive path is unchanged.

## [0.53.0] - 2026-10-07

The plugin-and-broker release. The webhook broker reaches feature-complete (stats, OTel, load-tested WS core), the plugin system gets config/enable-disable/reload/error-handling/docs, GitHub push-PR-issue events land, and the worker/escalation UI gets durable activity so `:attach` and `:workers` tell you what a subworker is actually doing.

### Added
- **GitHub plugin: push, PR, and issue events** (PR #897, TASK-373): the GitHub plugin now handles push events, pull requests, and issues end-to-end through the webhook broker
- **Webhook testing & debugging tools** (PR #898, TASK-273): local webhook replay/test tooling and a delivery log so plugin handlers can be exercised without waiting on a real provider event
- **`:plugin` enable / disable / reload** (PR #899, TASK-272): plugins can be toggled and hot-reloaded at runtime, with the enabled/disabled state persisted across sessions
- **Broker `/stats` endpoint + hardening** (PR #901, TASK-314): operational stats for the webhook broker plus input/connection hardening
- **Plugin configuration & management** (PR #902, TASK-271): `:plugin config` view/set/reset with schema validation and secret redaction
- **Plugin error handling & robustness** (PR #903, TASK-274): invalid-config quarantine state, hook timeout warnings, a per-plugin errors log, and health surfaced in `:plugin list`
- **Broker core close-out** (PR #904, TASK-371): WebSocket test coverage, a load benchmark, backlog stats, and Fly volume/deploy documentation
- **Broker observability** (PR #905, TASK-375): OTel/SigNoz metrics, structured JSON logs, and per-handler counters for plugin testing and production triage
- **Plugin developer documentation** (PR #906, TASK-275): developer enablement docs covering the plugin manifest, hooks, webhook handlers, config schema, and local testing loop
- **Escalation heartbeat** (PR #908): escalation banners carry a heartbeat liveness heart so a stalled worker is visually distinguishable from a working one
- **Worker activity summaries** (PR #911): `:workers` rows and escalation lines show a width-derived summary of what each worker is currently doing instead of a bare id
- **Durable subworker activity + tail-mode `:attach`** (PR #913): subworker activity is written to a durable activity log, so `:attach` shows live progress from any session — including attaching after the fact and tailing from where the worker is now

### Changed
- **Faster CI** (PR #910): cargo jobs run in parallel, Blacksmith caches moved off GitHub's cache quota, and `restore-keys` added so a cold key still warm-starts

### Fixed
- **Webhook broker client now speaks the broker's protocol** (TASK-449): the client registers over HTTP (`POST /clients/register`) to get a `session_token`, authenticates the WebSocket with `{"type":"auth","session_token"}`, and acks with `{"type":"ack","webhook_id"}` — previously the broker rejected every auth and never saw an ack. `auth_error` now fails fast and triggers re-registration. `wss://` brokers are dialable (rustls). **New required env var `WEBHOOK_PLUGIN_ID`** alongside `WEBHOOK_BROKER_URL`; `WEBHOOK_BROKER_SECRET` is now the registration secret (the broker then requires signed webhooks). A contract test (`crates/aish-webhook-broker/tests/client_contract.rs`) runs the real broker against the real client. The bundled hello-world plugin declares a `ping` webhook handler, and its `handlers/ping.sh` now actually reads the payload's `message`.
- **Duplicate "thinking…" row on resume** (PR #895): resuming a finished worker printed two `thinking…` rows; now one
- **Engine artifacts leaked into dispatched worker briefs** (PR #907): the offload path no longer quotes engine/digest artifacts into a worker's task brief, so a dispatched coordinator sees only the real task
- **Every queued escalation is shown** (PR #909): the escalation stack rendered only the newest ask and silently dropped the rest; all queued escalations now display
- **`:tell` and `:stop` run immediately mid-turn** (PR #912): both were queued until the current turn ended, which defeated the point of a mid-flight course-correction or stand-down; they now take effect the moment they're issued

## [0.52.1] - 2026-10-07

Shipped as 0.52.1: the `v0.52.0` tag is unusable. The release for it was pre-published by hand instead of by the Release workflow, GitHub marked it immutable with zero assets, and the workflow's `Assert no published release already exists for this tag` gate then (correctly) refused to attach the `:update` binaries. Same failure mode as `v0.48.0`.

### Added
- **Cleaner attached-pane rows** (PR #891): the duplicated `[w_…]` / `[goal]` gutter is dropped from attached pane rows — the worker id and goal are already carried by the pane header, so repeating them per row was pure noise

### Fixed
- **Escalation banner anchoring** (PR #890): the escalation banner is now anchored ABOVE the statusline's top rule instead of colliding with it
- **Mid-turn input controls act immediately** (PR #893): Shift-Tab and `:attach` / `:detach` now flip the stream gate the moment they're pressed during a thinking turn, rather than being queued until the turn ends. Mid-turn colon commands (`:dispatch` et al.) route through the `OpsCtx` path and run immediately

## [0.51.0] - 2026-10-07

### Added
- **OSC 8 hyperlink terminal support** (PR #886): markdown URLs now render as clickable terminal hyperlinks via OSC 8 escape sequences when the terminal supports them; non-supporting terminals fall back to plain text with the URL visible
- **Escalation banner UI** (PR #887): animated escalation banner pinned above the statusline showing live worker status and pending operator asks, with visual severity indicators
- **Reasoning telemetry instrumentation**: escalate(), batch runs, and coordinator turnarounds now emit structured reasoning events for observability and decision-tree analysis

### Fixed
- **Voice configuration wiring** (PR #885): all five voice.* config keys now honored in the REPL dictation path; Whisper initialization stderr suppressed for cleaner output

## [0.50.2] - 2026-10-06

### Fixed
- **`llama-cpp-2` v0.1.158+ API migration** (PR #883): the `local` backend failed to build against current `llama-cpp-2` because token/vocab accessors moved off `LlamaModel` onto `LlamaVocab`. `src/backend/local.rs` now routes tokenization through `model.vocab()` (`str_to_token`, `is_eog_token`, token→bytes) and uses the `Special` enum in place of the old boolean argument. Unblocks the 0.50.2 release build with `--features local`.

## [0.50.1] - 2026-10-06

### Fixed
- **Release tag protection**: v0.50.0 tag orphaned by immutable release deletion; bumped to v0.50.1 to bypass protected ref restrictions

## [0.50.0] - 2026-10-06

### Added
- **Coordinator Plan-DAG infrastructure (SPR-113)**: `PlanGraph` types with topological ordering, ready-set computation, and cycle detection enable deterministic work scheduling
- **Delta artifact storage**: Phase 1 DISCOVERY output `(ask − state)` now persisted and re-injected on resume, survives context compaction
- **Plan persistence layer**: `coordinator_store` extended with plan graph and delta artifact tables, keyed by `scope_key` for scope isolation
- **Node-keyed work-package leases**: In-flight work identification upgraded from run-shape inference to exact plan node id, eliminates resume ambiguity and enables safe parallel sibling dispatch
- **Derived fan-out logic**: Parallel fan-out is now a computed property of the dependency DAG (`ready_set` + file-disjointness) rather than a discretionary call, replacing the anti-decompose heuristic that was suppressing 87-call runaways
- **Comprehensive test coverage (TASK-809)**: Unit tests for ready-set, topological sort, cycle detection, serde round-trips; prompt-constant assertions lock down Phase 1 delta directive, Phase 2 plan object, and fan-out derivation rule
- **Reference documentation**: `docs/reference/coordinator/patterns.md` extended with five-step loop explanation, delta and plan shapes, fan-out derivation table, and incident history (87-call runaway motivating case)

### Fixed
- **Batch tier availability check**: removed duplicate `metered_key` field definition; `batch::available()` now correctly gates on both backend kind and API key presence (PR #879)

## [0.49.1] - 2025-01-22

### Fixed
- **Voice config wiring**: honour all five voice.* config keys in the REPL dictation path (TASK-368, PR #859)
- **Whisper initialization stderr**: suppress Whisper model loading diagnostic output to keep stderr clean (TASK-368)

## [0.49.0] - 2026-09-30

### Fixed
- **Unbounded reveal buffer + non-ASCII token undercount** (PR #841, ISS-409754 / ISS-409759): reported as "unbounded `session.history` growth", but review showed `history` was *not* the leaking structure — compaction drains it every pass. Two other defects were the real causes. (1) `Session::last_turn_tools` was pushed on every tool call but cleared only at the *start* of a user turn, so a single long agentic turn grew it without bound; critically, context compaction offloads `history` but not this buffer, making it the one accumulator that survived every compaction pass. All writes now route through `Session::record_turn_tool`, which applies a FIFO cap at `LAST_TURN_TOOLS_CAP`, retains the tail (so a reveal still shows what the turn just did), and shrinks capacity so the allocation is actually released rather than just logically truncated. (2) `estimate_text_tokens` counted `chars()` rather than bytes, undercounting non-ASCII input by up to 4x, so the compaction trigger fired late — exactly when the context window was already blown. It now estimates from byte `len()`, which never undercounts for UTF-8.
- **`webhook-receiver` test module rewritten to match its real API**: `crates/webhook-receiver/src/tests.rs` referenced a `crate::models::Webhook` type, `crate::signing::{generate_signature, verify_signature}` functions, and a `Database` struct with `memory()`/`store_webhook`/`get_webhook`/`list_webhooks` methods — none of which exist. `main.rs` is a single-file design (inline `sqlx` queries against a `webhooks` table, a free `verify_signature()` fn); the tests were broken from the crate's creation commit and never caught because `.github/workflows/ci.yml` never builds or tests this crate. Tests now exercise the real schema (via an in-memory `SqlitePool`) and the real `verify_signature()` directly. Also switched the crate's `sqlx` TLS backend from `runtime-tokio-native-tls` to `runtime-tokio-rustls` so it builds without a system OpenSSL dev package, which is what made this bug undetectable in this sandbox until now.
- **CI test flakiness traced to stale incremental-compilation cache, not test flakiness (BP-020)**: `.github/workflows/ci.yml` and `ci-testbox.yml` cache `target/` keyed only on `hashFiles('**/Cargo.lock')`, with `cancel-in-progress: true` and no `restore-keys`/rustc-version component in the key — a `target/` dir can be cached mid-write by a cancelled run and later restored verbatim onto an unrelated PR that shares the same lockfile hash. Investigation of a real CI failure (`pipeline::tests::parse_splits_stages`, a pure zero-I/O test) showed an assertion mismatch against content that didn't match the checked-out source, consistent with a stale incremental unit surviving cache reuse rather than genuine flakiness. Both workflows now set `CARGO_INCREMENTAL: '0'`, the standard fix — CI always builds from scratch regardless, so incremental compilation bought nothing there and only added this correctness risk.
- **Stray `[DEBUG escalate]` eprintln left in production `escalate()` tool**: commit `bab395d` (the `session.db` fallback fix) added a debug print to `stderr` on every single `escalate` tool call and never removed it before merging — `src/tools.rs`'s `escalate()` printed `[DEBUG escalate] session.db is Some=<bool>` on every escalation regardless of outcome, noise with no caller. Removed; the surrounding memory-persistence success/failure logging (`[aish] stored escalation memory id=...` / `[aish] WARNING: failed to store escalation memory...`) is unaffected and remains the real diagnostic signal for that code path.

- **Stale doc links left by the `docs/` reorganization**: `AISH.md` and `README.md` linked to `docs/SKILL-FORMAT.md` and `docs/telemetry-efficiency.md`, both moved to `docs/formats/skill-format.md` and `docs/internals/telemetry-efficiency.md` respectively during the reorg (`docs/REORGANIZATION_PLAN.md`). Also fixed the same class of stale reference inside `docs/reference/coordinator/patterns.md` (relative links to now-nonexistent `coordinator-loop-guards.md`/`coordinator-stale-row-prevention.md`/bare `telemetry-efficiency.md` instead of the consolidated `loop-guards.md`/`stale-row-prevention.md`/`../../internals/telemetry-efficiency.md`), `docs/reference/plugins/state.md` (`DATABASE_PATHS.md` → `../database.md`), four `docs/design/*-plugin-integration.md` files (`docs/plugin-state-schema.md` → `docs/reference/plugins/state.md`), `plugins/aish/skills/aish_sre/SKILL.md` (`docs/RELEASING.md` → `docs/RELEASE.md`, `docs/coordinator-loop-guards.md` → `docs/reference/coordinator/loop-guards.md`), and doc comments in `src/plugin_memory.rs`, `src/plugin_state.rs`, `src/db_paths.rs`, `src/coordinator.rs` pointing at bare pre-reorg filenames that no longer exist at the repo root.
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
- **Vacuous `test_plugin_manifest_var_expansion` deleted**: this test in `tests/plugin_integration_tests.rs` consisted solely of `assert!(true, "placeholder for Phase 1.4 var expansion test")`, so it could never fail and provided no real coverage of `` expansion. That behavior is implemented (`load_config`/`interpolate_env`/`resolve_env_refs` in `src/plugins.rs`) and is already exercised by real unit tests in `src/plugins.rs` (`env_reference_is_substituted`, `env_reference_resolves_inside_nested_structures`, `unset_env_reference_errors`, `env_default_reference_is_resolved`).
- **Dead example fixture `examples/registry-index.json` deleted**: this file was added alongside the original `registry/index.json` (PR #173, "ship curated skill registry index") as a 3-entry illustrative example, but nothing in the repo ever read it — no Rust code, test, script, or doc referenced `examples/registry-index.json`. When the registry later split into `registry/skills.json` + `registry/plugins.json` (commit `bf29105`, "plugin registry as JSONL"), the example was left behind pointing at a schema shape (`{"results": [...]}`) that still matches `skills.json` today but has no reader. The real, live registry examples are `registry/skills.json`/`registry/plugins.json` themselves (embedded via `include_str!` in `src/skill_provider.rs`) and the unit-test fixtures inline in `src/skill_provider.rs::tests`.
- **Vacuous `tests/golden_routing_heuristics.rs` deleted**: all 5 tests in this file (`test_looks_like_prose_english_routes_to_model`, `test_bare_yes_forces_direct`, `test_bang_prefix_forces_model`, and two others) consisted solely of `assert!(true, "...")`, so they could never fail and provided no real coverage. Routing-heuristic behavior is already exercised by the golden-snapshot test `routing_decision_snapshot` in `src/repl.rs` against `tests/golden/routing_decisions.snap`, which covers every case the deleted file only described in comments.
- **Dead per-tool/per-turn pulse tracking in `worker.rs`**: `JobInner::last_tool_outcome`/`last_turn_completion`, `WorkerJob::record_tool_outcome`/`record_turn_completion`/`latest_pulse`, and the module-level `fresh_pulse` aggregator were all unreachable — `cargo check` flagged `fresh_pulse` and `latest_pulse` as never-used. The prompt's `⟳N` badge has been state-based (running count + `fresh_terminal`) since an earlier change; this chain was leftover plumbing that recorded events nothing read. The live `Pulse` broadcast bus (`crate::pulse`, feeding `:pulse-report`) and `pulse_badge`/`fresh_terminal` are unaffected.

### Added
- **`:plugin info <id> --mcp` diagnostic**: renders which of a plugin's declared `.mcp.json` servers will actually connect vs. lose a name-collision (first-plugin-wins / config-wins policy), by wiring the existing Phase 0.5.3 `collect_plugin_mcp_servers` into a new `format_plugin_mcp` report alongside the existing `--schema` diagnostic. Previously this collision-resolution logic was only exercised by unit tests, with no way for a user to see it applied to their own plugins.

### Changed
- **CI now tests `git-discover` and `aish-webhook-broker`**: the root `Cargo.toml` is a mixed manifest (both `[workspace]` and `[package]`), so the plain `cargo test --no-default-features --locked` step in `.github/workflows/ci.yml` only ever tested the `aish` binary package itself — every other workspace member needed an explicit `-p <name>` to be reached at all. `git-discover`'s own unit test module never ran in CI (only a compile-linkage smoke test in the root package touched it). `aish-webhook-broker` is a deliberately separate Cargo workspace (own `Cargo.lock`, to keep aish's heavy `local`-feature deps out of its build graph) and so was invisible to every `-p` flag and had never been built or tested in CI at all. Added `cargo test -p git-discover --no-default-features --locked` and `cargo test --manifest-path crates/aish-webhook-broker/Cargo.toml --locked` steps. `webhook-receiver` (also untested by CI, tracked separately as BP-024) is intentionally not added yet — its `sqlx` dependency needs the `runtime-tokio-rustls` fix from PR #806 first; see the comment left in `ci.yml` for the follow-up.
- **Embedded mcpmarket skill search removed — live search now comes from the plugin**: dropped the in-process mcpmarket network search path from `skill_provider` (the `wreq`/`wreq-util` browser-impersonating HTTP client is gone from `Cargo.toml`). `:skill search` now reads the offline embedded curated index for the builtin source, while live/community search is served exclusively by the `npx-skillfish` plugin (a `provides.skill_source`). The offline index, `:skill add` (GitHub + skill.fish), and the plugin skill-source fan-out are unchanged.
- **`npx-skills` plugin removed**: npx-skills (npm registry skill search + install) is archived. Live skill import and search is now unified under `npx-skillfish` (agentskills.io/skillfish), which is more performant and upstream-maintained. Removed `plugins/npx-skills/` from the tree; documentation updated to remove references to npm-sourced skills.

## [0.48.4] - 2026-09-29

### Fixed
- **Footer ate a command's last lines on resume** (PR #829): returning to the prompt after a long-running command could repaint the status footer over the tail of that command's output, permanently clobbering the last lines a user needed to read. `src/terminal.rs` now restores the scroll region and reserves the footer row before resuming output.
- **`background_status` reported a live worker as done** (PR #830): a coordinator still mid-run could be listed as `done`, so an operator would stop watching a worker that was in fact burning turns. The liveness check no longer resolves a running child to a terminal status.
- **Truncated judge verdict burned the whole goal turn** (PR #831): the goal loop's verifier called `serde_json::from_str` on the judge model's reply and hard-failed on any parse error, producing `couldn't parse judge verdict: EOF while parsing …`. Two real shapes caused it — a verdict cut off mid-string (`max_tokens: 512` with no bound on the reason and `stop_reason` never inspected) and an empty reply where `.unwrap_or("")` handed serde a zero-length string. Both collapsed to `met=false` with the parse error as the reason, and that error text then became the *next* turn's guidance, so the worker was steered by the verifier's malfunction instead of by the goal. Adds `salvage_verdict()` (recovers the `met` bit and a usable reason prefix from a truncated reply, walking escape sequences by hand because the closing quote may never arrive), reports `stop_reason` on an empty reply, caps the reason at 240 chars, raises `max_tokens` to 1024, retries once (`JUDGE_ATTEMPTS = 2`), and stops a parse failure from overwriting the last genuine guidance.
- **Every goal iteration reported the id `goal`, leaving `stop` with no kill switch** (PR #833): `background_status` listed 47+ separate goal-loop turns all sharing the literal id `goal`, so an operator watching a runaway loop could not target a single run to stand it down. The rows were always distinct — the id was minted as `goal-{uuid}` and `batch::short_id` truncates at the first `-`, rendering them all as the bare label. Goal turns now get `new_goal_id()` → `g_########` (the `new_worker_id()` base62 convention, which survives `short_id`), so `stop g_aB3xK9pQ` or an unambiguous prefix resolves to exactly one run. The stable `GOAL_STREAM_LABEL = "goal"` attach handle is untouched, `worker::run_kind()` adds a real Kind column so goal turns stay readable at a glance, and `coordinator::id_matches()` replaces four copy-pasted id-resolution closures in `tools.rs` and `repl.rs`. Pre-existing `goal-<uuid>` rows in an existing `aish.db` keep their old unaddressable ids — no backfill migration was written.


## [0.48.3] - 2026-09-22

### Fixed
- **Detached background coordinators killed by `EPIPE`** (PR #825): a detached coordinator whose parent terminal went away took `SIGPIPE`/`EPIPE` on its next write and died mid-run. Detached coordinators are now shielded from broken-pipe death so background work survives the parent shell exiting.
- **Unreaped zombie children counted as live workers** (PR #826): the coordinator's liveness check treated an unreaped zombie pid as a running child, so finished-but-unwaited workers kept stale `coordinating` rows alive forever. Zombie pids are now treated as dead.
- **GGUF model downloads are resumable and retryable** (PR #827): an interrupted local-model fetch restarted from zero. `modelfetch` now resumes partial downloads and retries transient failures.


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
