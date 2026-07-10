#!/usr/bin/env bash
# signoz-observability :: exception poller (script-native, MCP-free path)
# ---------------------------------------------------------------------------
# Fired by (a) the TurnEnd hook and (b) the turn-independent timer. Hooks are
# fork/exec programs and CANNOT call MCP tools, so this path queries SigNoz
# over its REST query API directly with curl. The agent-driven equivalent
# lives in SKILL.md (uses mcp__signoz__signoz_search_logs).
#
# Reads the active repo's services from the registry, searches the last
# poll_window_secs for ERROR/FATAL logs, dedups by (service,fingerprint), and
# writes a one-line summary to state/signoz/exceptions.txt (consumed by the
# statusline) plus an NDJSON audit trail in state/signoz/seen.ndjson.
#
# Secrets: SIGNOZ_API_KEY comes from the environment at spawn time, injected
# from ${profile:signoz}. NEVER hard-code it here.
# ---------------------------------------------------------------------------
set -euo pipefail

PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGISTRY="$PLUGIN_DIR/state/registry.json"
STATE="$PLUGIN_DIR/state/signoz"
OUT="$STATE/exceptions.txt"
SEEN="$STATE/seen.ndjson"
mkdir -p "$STATE"

ENDPOINT="${SIGNOZ_ENDPOINT:-http://localhost:3301}"
API_KEY="${SIGNOZ_API_KEY:-}"
WINDOW="${SIGNOZ_POLL_WINDOW_SECS:-30}"
MIN_SEV="${SIGNOZ_MIN_SEVERITY:-ERROR}"
DEDUP_TTL="${SIGNOZ_DEDUP_TTL_SECS:-300}"

# --- resolve services for the active repo ----------------------------------
cwd="$PWD"
repo_root="$(git -C "$cwd" rev-parse --show-toplevel 2>/dev/null || echo "$cwd")"
repo_root="$(cd "$repo_root" 2>/dev/null && pwd -P || echo "$repo_root")"
services=()
if [ -f "$REGISTRY" ]; then
  while IFS= read -r s; do [ -n "$s" ] && services+=("$s"); done \
    < <(jq -r --arg r "$repo_root" '.repos[$r].services[]? // empty' "$REGISTRY" 2>/dev/null || true)
fi
# fall back to explicit watch list
if [ "${#services[@]:-0}" -eq 0 ] && [ -n "${SIGNOZ_WATCH_SERVICES:-}" ]; then
  IFS=',' read -r -a services <<< "$SIGNOZ_WATCH_SERVICES"
fi
[ "${#services[@]:-0}" -eq 0 ] && exit 0   # nothing instrumented here — quiet exit

# --- time window (SigNoz wants epoch ms) -----------------------------------
end_ms=$(( $(date -u +%s) * 1000 ))
start_ms=$(( end_ms - WINDOW * 1000 ))

now_epoch="$(date -u +%s)"
total=0
declare -a lines=()

for svc in "${services[@]}"; do
  # SigNoz v4 logs query — filter service.name + severity. The exact body
  # depends on the SigNoz version; this is the v4 query_range logs shape.
  body="$(jq -n --arg svc "$svc" --arg sev "$MIN_SEV" --argjson s "$start_ms" --argjson e "$end_ms" '{
    start:$s, end:$e, step:60,
    compositeQuery:{ queryType:"builder", panelType:"list",
      builderQueries:{ A:{ queryName:"A", dataSource:"logs", expression:"A",
        filters:{ op:"AND", items:[
          {key:{key:"service.name",type:"resource"},op:"=",value:$svc},
          {key:{key:"severity_text"},op:"IN",value:[$sev,"FATAL"]}
        ]}, limit:20, orderBy:[{columnName:"timestamp",order:"desc"}] } } } }')"

  resp="$(curl -fsS --max-time 6 \
      -H 'Content-Type: application/json' \
      ${API_KEY:+-H "SIGNOZ-API-KEY: $API_KEY"} \
      -X POST "$ENDPOINT/api/v4/query_range" \
      -d "$body" 2>/dev/null || true)"
  [ -z "$resp" ] && continue

  # Pull (error_type, body-snippet) rows; shape varies, so be defensive.
  while IFS=$'\t' read -r etype snippet; do
    [ -z "$etype$snippet" ] && continue
    fp="$(printf '%s|%s' "$svc" "${etype:-$snippet}" | cksum | cut -d' ' -f1)"
    # dedup within TTL
    if [ -f "$SEEN" ]; then
      last_seen="$(grep -F "\"fp\":$fp" "$SEEN" 2>/dev/null | tail -1 | jq -r '.ts // 0' 2>/dev/null || echo 0)"
      if [ "$(( now_epoch - ${last_seen:-0} ))" -lt "$DEDUP_TTL" ]; then continue; fi
    fi
    total=$((total+1))
    lines+=("$svc :: ${etype:-log} :: $(printf '%.80s' "$snippet")")
    printf '{"ts":%s,"fp":%s,"service":"%s","type":"%s"}\n' "$now_epoch" "$fp" "$svc" "${etype:-log}" >> "$SEEN"
  done < <(printf '%s' "$resp" | jq -r '
      (.data.result[0].list // .data.newResult.data.result[0].list // [])[]?
      | [ (.data["exception.type"] // .data.error_type // .data["error.type"] // "error"),
          (.data.body // .body // "") ] | @tsv' 2>/dev/null || true)
done

# --- surface summary --------------------------------------------------------
if [ "$total" -gt 0 ]; then
  {
    printf '⚠ %d exception(s) [%ss]: ' "$total" "$WINDOW"
    printf '%s | ' "${lines[@]}" | sed 's/ | $//'
    printf '\n'
  } > "$OUT"
else
  : > "$OUT"   # clear — no fresh exceptions
fi

# trim seen ledger to last 500 rows
if [ -f "$SEEN" ] && [ "$(wc -l < "$SEEN")" -gt 500 ]; then
  tail -500 "$SEEN" > "$SEEN.tmp" && mv "$SEEN.tmp" "$SEEN"
fi

# TurnEnd hook stays silent (observe-only); the timer path lets the statusline
# read $OUT. When surface=memory, the agent-side SKILL.md reads $OUT and calls
# remember(). Emit the summary on stdout ONLY for the timer cache consumer.
if [ "${1:-}" = "--source=timer" ] && [ "$total" -gt 0 ]; then
  cat "$OUT"
fi
exit 0
