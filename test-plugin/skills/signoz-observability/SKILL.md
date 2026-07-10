---
name: signoz-observability
description: >
  Agent-driven half of the SigNoz Observability plugin. Use when a repo has
  just been detected/opened and you want an OpenTelemetry instrumentation
  profile, or after a turn to check SigNoz for fresh exceptions in the active
  repo's services. Triggers on: "scan this repo for instrumentation", "any
  new exceptions?", "check signoz", "what services does this repo emit",
  "observability status". Pairs with the fork/exec scripts in bin/ (which
  handle the MCP-free REST path); THIS file is what YOU (the agent) follow
  when the signoz MCP tools are available.
---

# SigNoz Observability

Two cooperating halves:

| Half | Runs as | SigNoz access | When |
|------|---------|---------------|------|
| **Scripts** (`bin/*.sh`) | fork/exec hooks + timer | curl → SigNoz REST | every CwdChanged / SessionStart / TurnEnd / 30s timer — **no MCP** (hooks can't call MCP) |
| **This skill** (agent) | you, in-loop | `mcp__signoz__*` tools | when the operator asks, or you proactively check after a risky turn |

## 0. Preflight — is SigNoz MCP wired?
Confirm the tools exist before relying on them:
- Look for `mcp__signoz__signoz_search_logs`, `..._search_traces`, `..._aggregate_logs` in your tool list.
- If **missing**: tell the operator to add the SigNoz MCP server (see `README.md` → *SigNoz MCP setup*, mirrors the `aws-mcp-setup` skill pattern). Do NOT fabricate results — say it's not wired and stop.
- Credentials: the key lives in `~/.aish/credentials` `[profile:signoz]` as `SIGNOZ_API_KEY`, referenced as `${profile:signoz}`. Never read or echo it.

## 1. Instrumentation scan (on repo detect)
When a new repo is opened (or the operator asks), build/refresh its profile:
1. Read the current profile from `state/registry.json` keyed by the canonical repo root (`git rev-parse --show-toplevel`).
2. If stale/absent, the `bin/scan-repo.sh` hook will have populated it — read it back. To do a richer scan yourself, grep for: `@opentelemetry/*` (Node), `opentelemetry-sdk|opentelemetry-instrumentation` (Python `requirements.txt`/`pyproject.toml`), `opentelemetry|tracing-opentelemetry|opentelemetry-otlp` (Rust `Cargo.toml`), `go.opentelemetry.io/otel` (Go `go.mod`), `io.opentelemetry` (Java pom/gradle); plus `OTEL_SERVICE_NAME`, `:4317`/`:4318`, collector URLs.
3. Report a table: **language(s) · service name(s) · instrumented? · exporter endpoint(s) · markers**.

## 2. Exception check (post-turn / on demand)
For each service in the active repo's profile (`state/registry.json → repos[<root>].services`):
1. `mcp__signoz__signoz_search_logs` with `service=<svc>`, `severity="ERROR"`, `timeRange="30s"` (or the operator's window). Use `mcp__signoz__signoz_aggregate_logs` grouped by `service.name` + error type for counts.
2. Dedup: skip any `(service, error-fingerprint)` already noted in `state/signoz/seen.ndjson` within `dedup_ttl_secs` (default 300).
3. If nothing fresh → say "clean" and stop. If hits → emit a compact summary: **service · count · error type · 1-line stack snippet**.
4. Surface per `surface` config:
   - `statusline`/`both` → write the one-liner to `state/signoz/exceptions.txt` (the statusline segment renders it).
   - `memory`/`both` → `remember()` a durable note tagged `signoz,exception` with the service, type, and first-seen timestamp so the pattern survives compaction.

## 3. Deeper triage (optional)
If the operator wants root cause, escalate to the bundled MCP skills:
- `debug_service_errors {service, timeRange}` — error logs + error-trace aggregation + top ops.
- `latency_analysis {service}` / `incident_triage {alertId}` for latency or alert-driven digs.

## Guardrails
- Hooks are observe-only and MUST stay silent on stdout except the timer's cache line — never let a poll block or spam the prompt.
- Never invent exception data. If a SigNoz query errors or times out, report the failure and the query you ran.
- Keep every poll inside its timeout budget; a slow SigNoz must degrade to "unknown", not hang the turn.
