# SigNoz Observability — aish plugin

Ambient observability for aish: **detect** a repo the moment you open it,
**scan** it for OpenTelemetry instrumentation, and **poll SigNoz** for fresh
exceptions after every turn — surfaced quietly on the SecondStatusLine and in
memory. Never blocks the prompt.

> Scaffold / architecture only. See `DESIGN.md` for the full design and
> `rust-sketch/` for the native integration points.

---

## 1. What it does

| When | What happens |
|---|---|
| You `cd` into / open a repo | `scan-repo.sh` scans for OTEL markers → writes `state/registry.json` |
| A turn ends | `poll-exceptions.sh` queries SigNoz for exceptions in the last 30s for that repo's services |
| You're idle | 30s timer runs the same poll → statusline stays fresh |
| A turn is live & you ask | `SKILL.md` uses `signoz_*` MCP tools for a richer, deduped summary |

Detects: **Node, Python, Rust, Go, Java** instrumentation; service names
(`OTEL_SERVICE_NAME`, package name); OTLP endpoints (`:4317/:4318`).

---

## 2. Install

```
# already scaffolded at:
~/.aish/plugins/signoz-observability/

# make the scripts executable
chmod +x ~/.aish/plugins/signoz-observability/bin/*.sh

# (optional) enable via the plugin manager once available
:plugin enable signoz-observability
:plugin list
```

Requires `jq` and `curl` on PATH (both standard). `git` used opportunistically
to resolve the repo root.

---

## 3. SigNoz MCP setup (agent-driven path)

The plugin auto-detects whether `signoz_*` MCP tools are already registered. If
they are (global `~/.aish/.mcp.json`), the bundled `.mcp.json` is redundant and
skipped. Otherwise, wire the endpoint + key (pattern mirrors `aws-mcp-setup`):

```
# 1) endpoint (non-secret) — env or config/signoz-observability.toml
export SIGNOZ_MCP_URL="https://signoz.example.com"     # or http://localhost:3301

# 2) secret — NEVER in config/env; put it in the credentials file:
#    ~/.aish/credentials
#      [profile:signoz]
#      SIGNOZ_API_KEY=sk_...

# referenced at spawn time as ${profile:signoz} — value never enters the convo
```

The **fork/exec poller** (`bin/poll-exceptions.sh`) is MCP-free: it curls the
SigNoz REST query API directly, reading `SIGNOZ_ENDPOINT` + `SIGNOZ_API_KEY`
from the spawn env (injected from `${profile:signoz}`).

---

## 4. Configure

Edit `config/signoz-observability.toml`. Highlights:

```toml
signoz_endpoint    = "http://localhost:3301"
credential_profile = "signoz"
poll_window_secs   = 30          # look-back per poll
min_severity       = "ERROR"     # WARN | ERROR | FATAL
dedup_ttl_secs     = 300         # suppress duplicate (service,fingerprint)
surface            = "both"      # statusline | memory | both
# watch_services   = ["cost-mvp-api","dashboard"]   # else uses scanned services
```

---

## 5. Example workflow

```
$ cd ~/projects/cost-mvp-api
  # → CwdChanged hook fires scan-repo.sh
  # → registry.json: { languages:["node"], services:["cost-mvp-api"],
  #                     endpoints:["localhost:4317"], instrumented:true }

$ aish> refactor the pricing handler
  # …turn runs, answer renders…
  # → TurnEnd hook fires poll-exceptions.sh (detached, non-blocking)
  # → SigNoz: 2 ERRORs for cost-mvp-api in last 30s
  # → SecondStatusLine:  ⚠ 2 exception(s) [30s]: cost-mvp-api :: TypeError :: Cannot read 'tier' of undefined …

$ aish> what are those exceptions?
  # → SKILL.md path: signoz_search_logs(service=cost-mvp-api, severity=ERROR, 30s)
  # → deduped summary + remember() so it persists across the session
```

Idle? The 30s timer keeps the statusline current even with no turns.

---

## 6. State files

| File | Purpose |
|---|---|
| `state/registry.json` | known repos → instrumentation profiles |
| `state/signoz/exceptions.txt` | latest one-line summary (statusline reads this) |
| `state/signoz/seen.ndjson` | dedup ledger (fingerprint + ts), trimmed to 500 rows |

---

## 7. Roadmap → shipped

Fork/exec path runs **today** once `CwdChanged` is emitted (one call site —
`rust-sketch/03_scanner_module.rs §CwdChanged`). The clean seam is a
`RepoDetected` event from `maybe_auto_index_repo`
(`rust-sketch/01_repo_detected_hook.rs`). Native in-core scanner is the
optional upgrade (`rust-sketch/03`). Full checklist: `DESIGN.md §8`.
