#!/usr/bin/env bash
# signoz-observability :: instrumentation scanner
# ---------------------------------------------------------------------------
# Fired by hooks.json on CwdChanged / SessionStart. Receives the hook payload
# JSON on stdin (fields: event, session_id, agent, cwd, mode, timestamp_ms).
# Language-agnostic, file-system pattern matching. Self-dedups against the
# registry so it is cheap to fire every cwd change / session start.
#
# Emits nothing on stdout by default (observe-only hook). Writes/updates the
# repo instrumentation profile into state/registry.json.
# ---------------------------------------------------------------------------
set -euo pipefail

PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REGISTRY="$PLUGIN_DIR/state/registry.json"
IGNORE='-name node_modules -o -name target -o -name .git -o -name dist -o -name build -o -name .venv -o -name __pycache__'
MAX_DEPTH=4

# --- 0. locate repo root (git top-level, else cwd) -------------------------
payload="$(cat || true)"
cwd="$(printf '%s' "$payload" | jq -r '.cwd // empty' 2>/dev/null || true)"
[ -z "$cwd" ] && cwd="$PWD"
repo_root="$(git -C "$cwd" rev-parse --show-toplevel 2>/dev/null || echo "$cwd")"
repo_root="$(cd "$repo_root" 2>/dev/null && pwd -P || echo "$repo_root")"

# --- 1. dedup: skip if scanned within the last hour ------------------------
now_iso="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [ -f "$REGISTRY" ]; then
  last="$(jq -r --arg r "$repo_root" '.repos[$r].last_scanned // empty' "$REGISTRY" 2>/dev/null || true)"
  if [ -n "$last" ]; then
    # re-scan at most once per hour
    last_epoch="$(date -u -d "$last" +%s 2>/dev/null || echo 0)"
    if [ $(( $(date -u +%s) - last_epoch )) -lt 3600 ]; then
      exit 0
    fi
  fi
fi

# --- 2. language + instrumentation detection -------------------------------
langs=()   ; services=() ; endpoints=() ; markers=()
add(){ local -n arr=$1; shift; case " ${arr[*]:-} " in *" $1 "*) ;; *) arr+=("$1");; esac; }

scanfile(){ find "$repo_root" -maxdepth "$MAX_DEPTH" \( $IGNORE \) -prune -o -type f -name "$1" -print 2>/dev/null; }

# Node
if [ -n "$(scanfile package.json | head -1)" ]; then
  while IFS= read -r pj; do
    if grep -qE '@opentelemetry/(sdk-node|api|auto-instrumentations)' "$pj" 2>/dev/null; then
      add langs node; add markers "package.json:@opentelemetry"
      svc="$(jq -r '.name // empty' "$pj" 2>/dev/null || true)"; [ -n "$svc" ] && add services "$svc"
    fi
  done < <(scanfile package.json)
fi
# Python
if [ -n "$(scanfile requirements.txt | head -1)$(scanfile pyproject.toml | head -1)" ]; then
  if grep -rqiE 'opentelemetry-(sdk|api|instrumentation)' "$repo_root" --include=requirements.txt --include=pyproject.toml 2>/dev/null; then
    add langs python; add markers "requirements:opentelemetry"
  fi
fi
# Rust
if [ -n "$(scanfile Cargo.toml | head -1)" ]; then
  if grep -rqE '(opentelemetry|tracing-opentelemetry|opentelemetry-otlp)' "$repo_root" --include=Cargo.toml 2>/dev/null; then
    add langs rust; add markers "Cargo.toml:opentelemetry"
  fi
fi
# Go
if [ -n "$(scanfile go.mod | head -1)" ]; then
  if grep -rqE 'go.opentelemetry.io/otel' "$repo_root" --include=go.mod 2>/dev/null; then
    add langs go; add markers "go.mod:go.opentelemetry.io/otel"
  fi
fi
# Java
if grep -rqE 'io.opentelemetry' "$repo_root" --include=pom.xml --include=build.gradle 2>/dev/null; then
  add langs java; add markers "pom/gradle:io.opentelemetry"
fi

# --- 3. service names + endpoints from env/config --------------------------
while IFS= read -r hit; do
  val="${hit#*=}"; [ -n "$val" ] && add services "$val"; add markers "OTEL_SERVICE_NAME"
done < <(grep -rhoE 'OTEL_SERVICE_NAME[=: ]+["'\'']?[A-Za-z0-9_.-]+' "$repo_root" \
          --include=*.env --include=.env* --include=docker-compose*.y*ml \
          --include=*.ts --include=*.js --include=*.py --include=*.rs --include=*.go 2>/dev/null | head -20 | sed -E 's/.*[=: ]+["'\'']?//')

while IFS= read -r ep; do add endpoints "$ep"; done < <(grep -rhoE 'https?://[A-Za-z0-9_.-]+:4317|https?://[A-Za-z0-9_.-]+:4318|localhost:4317|localhost:4318' "$repo_root" \
          --include=*.env --include=.env* --include=*.ts --include=*.js --include=*.py --include=*.rs --include=*.go --include=*.y*ml 2>/dev/null | sort -u | head -10)

# Check if markers array has items; set instrumented flag
instrumented=false
if [ ${#markers[@]:-0} -gt 0 ] 2>/dev/null; then
  instrumented=true
fi

# --- 4. merge profile into registry (atomic) -------------------------------
mkdir -p "$(dirname "$REGISTRY")"
[ -f "$REGISTRY" ] || echo '{"schema":"signoz-observability/registry/v1","repos":{}}' > "$REGISTRY"

to_json_arr(){ printf '%s\n' "${@}" | jq -R . | jq -cs .; }
profile="$(jq -n \
  --arg detected "$now_iso" --arg scanned "$now_iso" \
  --argjson langs "$(to_json_arr "${langs[@]:-}")" \
  --argjson services "$(to_json_arr "${services[@]:-}")" \
  --argjson endpoints "$(to_json_arr "${endpoints[@]:-}")" \
  --argjson markers "$(to_json_arr "${markers[@]:-}")" \
  --argjson instrumented "$instrumented" \
  '{detected_at:$detected,last_scanned:$scanned,languages:($langs-[""]),services:($services-[""]),endpoints:($endpoints-[""]),markers:($markers-[""]),instrumented:$instrumented}')"

tmp="$(mktemp)"
jq --arg r "$repo_root" \
   --argjson p "$profile" \
   '.repos[$r] = (if .repos[$r] then (.repos[$r] + $p | .detected_at = .repos[$r].detected_at) else $p end)' \
   "$REGISTRY" > "$tmp" && mv "$tmp" "$REGISTRY"

exit 0
