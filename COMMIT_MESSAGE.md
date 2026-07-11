Runtime configuration file support (aish.config) — foundation PR

Introduces ~/.aish/aish.config — a lightweight INI file for persistent,
operator-friendly control of aish's operational knobs. Operators can now
set defaults once and have them persist across sessions, without exporting
environment variables.

## What's New

- src/config.rs: Lightweight INI parser (stdlib only, no external deps)
  - Full INI parsing: comments, blank lines, sections, key=value pairs
  - 7 unit tests covering parsing, edge cases, type conversions
  - Gracefully handles missing files (optional config)

- mod config declared in src/main.rs

- ~/.aish/aish.config: Sample configuration file (221 lines)
  - Well-documented examples for all major knobs
  - Covers: Coordinator, Alerts, Updates, Worker, Session, Inference, etc.

- docs/reference/runtime-config.md: Comprehensive knobs reference
  - Explains each knob, defaults, valid ranges, examples
  - Cross-references to subsystem docs

- Integration patterns documented:
  - INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md (code pattern for all follow-ups)
  - INTEGRATION_PATTERN_COORDINATOR.rs (concrete example)
  - INTEGRATION_CHECKLIST.md (timeline for follow-up PRs)

## Precedence (Highest to Lowest)

1. Environment variable (e.g., AISH_COORDINATOR_MAX_ROUNDS=100)
2. Config file (~/.aish/aish.config)
3. Code default (hardcoded constant)

## Zero Behavior Change

This PR is pure infrastructure:
- Config file is optional (missing = all defaults apply)
- No subsystem reads config yet (follow-up PRs will integrate each one)
- All existing tests pass unchanged
- 100% backward compatible (env vars still work, take precedence)

## Follow-Up Work (Separate PRs)

Follow-1 (Coordinator): Wire coordinator.rs to read AISH_COORDINATOR_* knobs
Follow-2 (Alerts): Wire alerts to read AISH_ALERT_* knobs
... (and so on for each subsystem)

Each follow-up is small, focused, and testable independently.
No new CLI commands required yet (optional `:config show` in a later PR).

## Testing

cargo test config:: --lib  # All INI parser tests pass

Validation script: bash /home/grhohertz/projects/aish/validate_pr.sh

## Files Changed

- src/config.rs (NEW) — +290 lines, full INI parser with tests
- src/main.rs — +1 line (declare mod config)
- ~/.aish/aish.config (NEW) — +221 lines, sample config
- docs/reference/runtime-config.md (NEW) — Comprehensive reference
- docs/INDEX.md — Link to runtime-config.md

## Risk Assessment

Low. Config file is optional, env vars take precedence, no subsystem
changes. If an operator doesn't create ~/.aish/aish.config, aish
behaves identically to before this PR.

## Questions?

See EXECUTIVE_SUMMARY.md for full details, rationale, and roadmap.

Resolves #XXX (runtime configuration support)
