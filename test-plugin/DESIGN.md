# SigNoz Observability Plugin — Design Doc

**Status:** scaffold / architecture (not a full build)
**Target:** aish (LightHeart-Ventures/aish), the AI-native Rust shell
**Plugin id:** `signoz-observability`  ·  **Version:** 0.1.0

---

## 1. Problem & goal

Give aish ambient observability: the moment you open a repo, know whether it's
OTEL-instrumented and *which services it emits*, and after every turn get a
quiet, deduped summary of any fresh SigNoz exceptions for those services —
without ever blocking the prompt.

Three capabilities, three seams:

| Capability | aish seam used | Cost |
|---|---|---|
| Repo detection → instrumentation scan | `RepoDetected` hook (new) / `CwdChanged`+`SessionStart` (ship-now) | fork/exec, throttled 1×/hr/root |
| Post-turn exception poll | `TurnEnd` hook (exists, fire-and-forget) | curl → SigNoz REST, non-blocking |
| Turn-independent sweep | plugin timer (30s) | curl, cached to statusline |

---

## 2. aish architecture findings (discovery)

- **Hook system** (`src/hooks/`): `HookEvent` enum + `fire_observe(event)` —
  matches `hooks.json` entries and **fork/execs** the program **detached**
  (fire-and-forget; the turn never awaits it). Programs receive event fields
  as env (`AISH_EVENT_TYPE`, …) and/or JSON on stdin.
- **Existing events:** `SessionStart`, `TurnEnd`, `CwdChanged`. Crucial nuance:
  **`CwdChanged` is declared but DORMANT** — no call site emits it yet.
- **Repo-open de-facto seam:** `engine::maybe_auto_index_repo` canonicalizes
  the repo root and dedups per session via
  `session.codebase_indexed: HashSet<PathBuf>`, firing once per repo-open.
  → the natural, already-deduped place to emit a `RepoDetected` event.
- **No native plugin post-turn callback registry** exists; `TurnEnd` hook IS
  the post-turn callback surface. Good enough — and non-blocking by design.
- **MCP:** `signoz_*` tools (search_logs, search_traces, aggregate_logs, …)
  are available to the AGENT loop, **not** to fork/exec hooks. Hence the
  dual-path design (§4).
- **Secrets:** session-env injection **rejects** credential-like keys; secrets
  must come from `~/.aish/credentials [profile:*]` via `${profile:signoz}`.

---

## 3. File structure

```
signoz-observability/
├── plugin.json                     # manifest: config schema, timers, statusline, lifecycle
├── hooks.json                      # CwdChanged/SessionStart → scan ; TurnEnd → poll
├── .mcp.json                       # agent-facing signoz MCP wiring (skipped if global)
├── config/
│   └── signoz-observability.toml   # example config (no secrets)
├── bin/
│   ├── scan-repo.sh                # instrumentation scanner (language-agnostic)
│   ├── poll-exceptions.sh          # MCP-free exception poller (curl → SigNoz REST)
│   └── statusline.sh               # renders state → SecondStatusLine segment
├── skills/signoz-observability/
│   └── SKILL.md                    # agent-driven playbook (uses signoz_* MCP tools)
├── state/
│   ├── registry.json               # known repos + instrumentation profiles
│   └── signoz/                      # exceptions.txt (statusline) + seen.ndjson (dedup)
└── rust-sketch/                    # native integration sketches (core upgrade path)
    ├── 01_repo_detected_hook.rs
    ├── 02_turn_end_callback.rs
    └── 03_scanner_module.rs
```

---

## 4. Dual-path architecture (the key design decision)

Hooks can't call MCP; the agent can't run on a 30s timer while idle. So every
capability has **two implementations that write the same state files**:

```
                 ┌─────────────── SHIP-NOW (fork/exec, no core change) ──────────────┐
 repo open  ──▶  CwdChanged/SessionStart hook ──▶ bin/scan-repo.sh ──▶ registry.json
 turn ends  ──▶  TurnEnd hook ──────────────────▶ bin/poll-exceptions.sh ──▶ exceptions.txt
 idle       ──▶  plugin timer (30s) ────────────▶ bin/poll-exceptions.sh ──▶ statusline
                 └───────────────────────────────────────────────────────────────────┘

                 ┌─────────────── AGENT-DRIVEN (richer, when a turn is live) ─────────┐
 SKILL.md  ──▶ signoz_search_logs / aggregate_logs (MCP) ──▶ remember() summary
                 └───────────────────────────────────────────────────────────────────┘

                 ┌─────────────── NATIVE (optional core upgrade) ─────────────────────┐
 rust-sketch/*  ──▶ RepoDetected event + in-core scan_repo() ──▶ same registry.json
                 └───────────────────────────────────────────────────────────────────┘
```

Interchangeable because they share `state/registry.json` and
`state/signoz/exceptions.txt` schemas. Start on fork/exec; graduate to native
without touching consumers.

---

## 5. Instrumentation scanner — detection matrix

Language-agnostic filesystem pattern match (bounded depth 4, prunes
`node_modules/target/.git/dist/build/.venv/__pycache__`):

| Signal | Where | Yields |
|---|---|---|
| `@opentelemetry/*` | package.json deps | node + service (pkg `name`) |
| `opentelemetry-sdk` / `opentelemetry` | requirements.txt, pyproject.toml | python |
| `opentelemetry` | Cargo.toml | rust |
| `go.opentelemetry.io/otel` | go.mod | go |
| `io.opentelemetry` | pom.xml, build.gradle | java |
| `OTEL_SERVICE_NAME=…` | .env, *.yaml, compose, k8s manifests | service name |
| `localhost:4317/4318`, `*:4317` OTLP URLs | env/config | exporter endpoint |

→ `RepoProfile { languages[], services[], endpoints[], markers[], instrumented }`.

---

## 6. Exception poller — dedup & surfacing

- Window: last `poll_window_secs` (default 30s), severity ≥ `min_severity`.
- Fingerprint = `cksum(service | error_type|snippet)`; suppress re-alert within
  `dedup_ttl_secs` (default 300s) via `state/signoz/seen.ndjson` ledger
  (trimmed to last 500 rows).
- Output: one-line `⚠ N exception(s) [30s]: svc :: type :: snippet…` →
  `exceptions.txt` (statusline reads it); `surface=memory` path has the agent
  `remember()` it. TurnEnd hook stays silent (observe-only); timer prints to
  its cache consumer.

---

## 7. Config & secrets

`plugin.json.config_schema` (required: `signoz_endpoint`, `credential_profile`).
Key knobs: `poll_window_secs`, `poll_every`, `min_severity`, `dedup_ttl_secs`,
`scan_on_detect`, `surface`, optional `watch_services`. **Secret** lives ONLY
in `~/.aish/credentials [profile:signoz] SIGNOZ_API_KEY=…`, referenced as
`${profile:signoz}` — never in config or session env.

---

## 8. Integration checklist (to move scaffold → shipped)

1. **Ship-now:** wire the dormant `CwdChanged` emit (sketch #3 §CwdChanged) —
   one call site in `Engine::set_cwd`. Enables scanner today.
2. Confirm `TurnEnd` fires post-render (sketch #2) — already does.
3. **Clean seam:** add `RepoDetected` variant + emit from
   `maybe_auto_index_repo` first-seen branch (sketch #1) → update `hooks.json`
   to prefer it over `CwdChanged`.
4. Register plugin timer + statusline segment from `plugin.json.provides`.
5. Verify `signoz_*` MCP availability at `on_init`; else print setup (README §3).
