# Runtime Configuration Support for aish — Executive Summary

## What This Is

A **runtime configuration file** (`~/.aish/aish.config`) that lets operators set aish's operational knobs **once**, instead of exporting environment variables for every session.

**Key insight:** Config file + env vars allows **zero-breaking-change upgrades** to operator control surfaces.

---

## User Impact (Why They Care)

### Before (Current)
```bash
export AISH_COORDINATOR_MAX_ROUNDS=100
export AISH_ALERT_BELL=true
export AISH_UPDATE_CHANNEL=dev
aish
# Next session: re-export all three variables again
```

### After (This PR + Follow-Ups)
```bash
# Set once in ~/.aish/aish.config
cat >> ~/.aish/aish.config <<EOF
[coordinator]
max_rounds = 100

[alerts]
bell = true

[updates]
channel = dev
EOF

# Now just run aish; all settings persist
aish
```

### Bonus: Precedence
Environment variables still work and **take precedence**, so scripts don't break:
```bash
AISH_COORDINATOR_MAX_ROUNDS=200 aish  # Overrides config file value (200 > 100)
```

---

## Technical Overview

### Scope (This PR)
- ✅ `src/config.rs` — Lightweight INI parser (stdlib only, no external deps)
- ✅ `~/.aish/aish.config` — Sample configuration file (user-facing)
- ✅ `docs/reference/runtime-config.md` — Comprehensive knobs reference
- ✅ Integration patterns for all subsystems (so follow-ups are identical)
- ✅ Zero behavior change (just infrastructure, no subsystem integrations yet)

### What It Does
Reads `~/.aish/aish.config`, parses it as INI (sections + key=value), and exposes a `Config` struct with typed fields for all major knobs.

### What It Doesn't Do Yet
**Doesn't integrate any subsystem yet.** This PR is pure foundation. Follow-up PRs will wire each subsystem (Coordinator, Alerts, Updates, etc.) to read from the config.

---

## Precedence (How It Works)

Every knob follows this order (highest to lowest):
1. **Environment variable** (e.g., `AISH_COORDINATOR_MAX_ROUNDS=100`)
2. **Config file** (e.g., `~/.aish/aish.config` has `max_rounds = 50`)
3. **Code default** (e.g., hardcoded `const MAX_ROUNDS: usize = 48`)

**Example:**
```bash
# ~/.aish/aish.config has:
# [coordinator]
# max_rounds = 50

# Session 1: uses config file value
aish  # max_rounds = 50

# Session 2: env var overrides config file
AISH_COORDINATOR_MAX_ROUNDS=100 aish  # max_rounds = 100

# Session 3: back to config file
aish  # max_rounds = 50
```

---

## Testing & Validation

### This PR
```bash
cd /home/grhohertz/projects/aish
cargo test config::  # All INI parser tests pass
```

### Follow-Up PRs (Template)
```bash
# Test the integrated subsystem reads config
cargo test <subsystem>::  

# Verify precedence
mkdir -p ~/.aish
echo "[coordinator]" > ~/.aish/aish.config
echo "max_rounds = 100" >> ~/.aish/aish.config
AISH_COORDINATOR_MAX_ROUNDS=200 aish  # Should use 200 (env > config)
```

---

## Files Delivered

### Code
| File | Size | Purpose |
|------|------|---------|
| `src/config.rs` | ~290 loc | INI parser with unit tests |
| `src/main.rs` | +1 line | Declare `mod config` |

### Documentation
| File | Purpose |
|------|---------|
| `~/.aish/aish.config` | Sample config (user-facing) |
| `docs/reference/runtime-config.md` | Comprehensive knobs reference |
| `docs/INDEX.md` | Link to runtime-config.md |
| `RUNTIME_CONFIG_PR.md` | This PR's description for reviewers |
| `INTEGRATION_CHECKLIST.md` | Timeline for follow-ups |
| `INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md` | Code pattern for all follow-ups |

---

## Backward Compatibility

✅ **100% backward compatible:**
- Config file is **optional** (missing = all defaults)
- Environment variables **still work and take precedence**
- Existing scripts **unaffected**
- No new CLI commands required (optional polish in future PR)

---

## Follow-Up Work (Separate PRs)

### High-Impact (Do Soon)
| PR | Subsystem | Knobs | Impact |
|----|-----------|-------|--------|
| Follow-1 | Coordinator | `max_rounds`, `max_failed_attempts`, `failed_keep`, `failed_max_age_days` | 🔴 Critical (most-used knobs) |
| Follow-2 | Alerts | `bell`, `bell_cmd`, `bell_worker` | 🟡 High (operator comfort) |
| Follow-3 | Updates | `channel`, `repo` | 🟡 High (release management) |

### Medium-Impact
| PR | Subsystem | Knobs | Impact |
|----|-----------|-------|--------|
| Follow-4 | Serial Chain | `yield_depth` | 🟡 Medium (loop guards) |
| Follow-5 | Worker | `runtime`, `cpus`, `network`, `state_dir`, `worktree_dir` | 🟡 Medium (container config) |
| Follow-6 | Session | `launch_session_name`, `startup_digest` | 🟢 Low (convenience) |
| Follow-7 | Inference | `local_model_path`, `gpu_layers`, `hf_base`, `hf_revision` | 🟢 Low (experimental) |

### Optional Polish
| PR | Feature | Purpose |
|----|---------|---------|
| Follow-8 | CLI commands | `:config show`, `:config edit`, `:config validate` |

---

## Success Metrics

### This PR ✅
- [x] Config parser compiles and tests pass
- [x] Sample config is valid and documented
- [x] Zero behavior change (no subsystem reads config yet)
- [x] Backward compatible (env vars still work)

### Follow-1 (Coordinator) 🎯
- [ ] Coordinator reads config file
- [ ] Precedence works (env > config > default)
- [ ] Tests verify all three paths
- [ ] Operator can set `max_rounds` once in config and it sticks

### All Follow-Ups
- [ ] All subsystems read config
- [ ] Complete operator control surface via `~/.aish/aish.config`
- [ ] Operator docs explain each knob
- [ ] No behavior regressions

---

## Risk Assessment

| Risk | Severity | Mitigation |
|------|----------|-----------|
| Config file parsing crashes aish | Low | Parser catches errors; missing file returns empty config |
| Operator sets invalid values | Low | Docs explain valid ranges; `:config validate` cmd in Follow-8 |
| Environment variables break | None | Env vars still take precedence; no scripts break |
| Config file gets out of sync | Low | Simple INI format; easy to audit manually |

---

## Decision: Merge or Hold?

### ✅ Recommend: Merge Immediately
1. **Zero risk** — config file is optional, no behavior change
2. **Enables follow-ups** — foundation is ready for integration
3. **Unblocks operators** — they can create `~/.aish/aish.config` now (it'll be read in Follow-1)
4. **Easy to review** — pure infrastructure, no complex logic changes

**Blocker:** None. Ship it.

---

## Next Steps for Operator

1. **After this PR merges:**
   - Config module lands in main
   - Create `~/.aish/aish.config` if you want to (optional)

2. **Await Follow-1 (Coordinator):**
   - Coordinator reads config
   - You can now set `max_rounds` once instead of exporting env var every session

3. **Prioritize Follow-2 & Follow-3:**
   - Alerts and Updates are high-value
   - Can be parallelized with Coordinator

4. **Optional: Follow-8 (CLI):**
   - `:config show` command for visibility
   - `:config validate` command for peace of mind

---

## Questions?

**Q: Why INI format and not YAML?**
A: INI is simpler to parse (stdlib only, no deps), easier for operators to understand, and sufficient for flat key-value config.

**Q: Why not TOML or JSON?**
A: Same reason — INI is the lightest option. No external dependencies = faster startup + lower maintenance burden.

**Q: Will this slow down aish startup?**
A: No. Config parsing is `O(n)` over file size (~200 lines = negligible). File is only read at startup (once).

**Q: Can I commit `.aish/aish.config` to git?**
A: Yes! It's safe to commit and share. Just remember env vars override it, so developers can customize locally.

**Q: What if I want to reset to defaults?**
A: Delete `~/.aish/aish.config` (or the relevant section). All code defaults apply.

---

## Summary

This PR delivers **the plumbing** for operator-controlled configuration. It's boring infrastructure, but it unlocks:
- Zero-breaking-change operator UX improvements
- Discoverable configuration (one file, docs, not scattered env vars)
- Foundation for future knob additions

**Estimated timeline:** This PR + Follow-1 = ~2-3 days to unlock the highest-value knobs (Coordinator). Full integration across all subsystems = 1-2 weeks.

**Impact:** High. Operators gain persistent, discoverable control over aish's behavior.

---

## Files Ready for Review

```
src/config.rs                      ✅ Full parser with tests
src/main.rs                        ✅ mod config declaration
~/.aish/aish.config                ✅ Sample config (221 lines, well-documented)
docs/reference/runtime-config.md   ✅ Comprehensive knobs reference
RUNTIME_CONFIG_PR.md               ✅ PR description for reviewers
INTEGRATION_CHECKLIST.md           ✅ Follow-up timeline & patterns
```

**Status: Ready to merge.** 🚀
