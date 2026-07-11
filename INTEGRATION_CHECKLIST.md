# Runtime Configuration PR — Complete Integration Checklist

## This PR (MVP / Foundation)

### ✅ Completed
- [x] `src/config.rs` — Full INI parser with unit tests (no external deps)
- [x] `~/.aish/aish.config` — Sample configuration file (user-facing reference)
- [x] `docs/reference/runtime-config.md` — Comprehensive knobs reference
- [x] `docs/INDEX.md` — Link to runtime-config.md
- [x] `src/main.rs` — Declare `mod config`
- [x] PR description + plan (RUNTIME_CONFIG_PR.md)
- [x] Integration patterns for all subsystems (INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md)
- [x] Coordinator integration pattern (INTEGRATION_PATTERN_COORDINATOR.rs)

### ✅ Tests Passing
- [x] `cargo test config::` — All INI parser tests pass
- [x] No behavior change (config loading is optional, all defaults work)
- [x] Backward compatible (env vars still work, take precedence)

---

## Follow-Up PRs (Integration — One Subsystem Per PR)

### Timeline & Ownership

| # | Subsystem | Knobs | Est. Lines | Priority | Status |
|---|-----------|-------|-----------|----------|--------|
| **Follow-1** | **Coordinator** | AISH_COORDINATOR_{MAX_ROUNDS, MAX_FAILED_ATTEMPTS, FAILED_KEEP, FAILED_MAX_AGE_DAYS} | ~40 | 🔴 HIGH | Ready to integrate |
| Follow-2 | Serial Chain (engine.rs) | AISH_SERIAL_CHAIN_YIELD_DEPTH | ~15 | 🟡 MED | Pending Follow-1 |
| Follow-3 | Alerts (tools.rs) | AISH_ALERT_BELL, AISH_ALERT_BELL_CMD, AISH_ALERT_BELL_WORKER | ~25 | 🟡 MED | Pending Follow-1 |
| Follow-4 | Updates (update.rs) | AISH_UPDATE_CHANNEL, AISH_UPDATE_REPO, AISH_UPDATE_GITHUB_RAW_BASE | ~30 | 🟡 MED | Pending Follow-1 |
| Follow-5 | Session (session.rs) | AISH_LAUNCH_SESSION_NAME, AISH_STARTUP_DIGEST | ~20 | 🟢 LOW | Pending Follow-1 |
| Follow-6 | Worker (worker.rs) | AISH_WORKER_{RUNTIME, CPUS, NETWORK, STATE_DIR, WORKTREE_DIR} | ~40 | 🟡 MED | Pending Follow-1 |
| Follow-7 | Inference (modelfetch.rs, oracle.rs) | AISH_INFERENCE_{LOCAL_MODEL_PATH, GPU_LAYERS, HF_BASE, HF_REVISION} | ~25 | 🟢 LOW | Pending Follow-1 |
| Follow-8 | Optional: Main CLI | `:config show`, `:config edit`, `:config validate` | ~80 | 🟢 LOW | UI Polish |

---

## PR Template for Each Follow-Up

Each follow-up PR follows this checklist:

### Code Changes
- [ ] Read config in subsystem's initialization function
- [ ] Respect precedence: env > config file > code default
- [ ] Use `load_<subsystem>_config()` helper function (following the pattern)
- [ ] Minimal changes — only add loading logic, don't refactor the subsystem

### Tests
- [ ] Unit test: load config with knob set, verify value is read
- [ ] Integration test: set env var to override config file value, verify precedence
- [ ] Regression test: run existing subsystem tests, ensure all pass
- [ ] Manual test: create temp `~/.aish/aish.config`, verify subsystem reads it

### Documentation
- [ ] Update `docs/reference/runtime-config.md` if adding new knobs
- [ ] Add comment in code explaining precedence (env > config > default)
- [ ] Link issue/PR to this parent PR (RUNTIME_CONFIG_PR.md)

### Review Checklist
- [ ] Config loading is optional (missing file = all defaults)
- [ ] No breaking changes (env vars still work)
- [ ] No new external dependencies
- [ ] Tests pass cleanly
- [ ] Code follows the established pattern (see INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md)

---

## Testing & Validation

### Pre-Merge This PR
```bash
cd /home/grhohertz/projects/aish
cargo test config:: --lib
```

### Pre-Merge Each Follow-Up
```bash
# Test the integrated subsystem
cargo test <subsystem>:: --lib

# Regression test
cargo test

# Manual verification
mkdir -p ~/.aish
cat > ~/.aish/aish.config <<EOF
[coordinator]
max_rounds = 100
EOF
# Run aish, observe max_rounds is 100 (can verify via `:config show` in Follow-8)
```

---

## Sample Config Files for Testing

### Test 1: Coordinator (Follow-1)
```ini
[coordinator]
max_rounds = 100
max_failed_attempts = 5
failed_keep = 100
failed_max_age_days = 30
```

### Test 2: Multiple Sections
```ini
[coordinator]
max_rounds = 100

[alerts]
bell = true
bell_cmd = paplay /path/to/alert.oga

[updates]
channel = dev
repo = fork/aish
```

### Test 3: Env Override
```bash
export AISH_COORDINATOR_MAX_ROUNDS=200  # overrides config file
# aish reads: env=200 (takes precedence over config file)
```

---

## Delivery Milestones

### Milestone 1: Foundation (THIS PR)
- Config parser lands
- No behavior changes yet
- Foundation ready for integration

### Milestone 2: Coordinator Integration (Follow-1) — **CRITICAL PATH**
- Coordinator reads config
- Unlocks operator ergonomics for the most-used knobs
- High-impact followup

### Milestone 3: Full Integration (Follow-2 through Follow-7)
- All major subsystems read config
- Complete operator control surface

### Milestone 4: Polish (Follow-8, optional)
- `:config show` command
- `:config edit` command
- `:config validate` command
- Operator-friendly UI

---

## Backward Compatibility Guarantees

✅ This PR (and all follow-ups) maintain **100% backward compatibility**:
- Config file is **optional** — missing `~/.aish/aish.config` causes zero issues
- Environment variables **still work and take precedence** — no `AISH_*` scripts break
- Code defaults **unchanged** — old behavior is the fallback
- **No new CLI commands** required to use config (optional in Follow-8)

---

## Risk Assessment

| Risk | Severity | Mitigation |
|------|----------|-----------|
| Config file parsing error crashes aish | Low | Parse errors are caught; missing file returns empty config |
| Typo in config file goes unnoticed | Low | `:config validate` command in Follow-8; docs are clear |
| Operator sets conflicting env + config | Low | Documented precedence; env always wins |
| Complex config file hard to understand | Low | Sample file provided; docs explain each knob |

---

## Git & PR Workflow

1. **This PR**: Merge to `main` once tests pass
   - Establishes config module
   - No behavior change yet
   - Safe to ship immediately

2. **Follow-1** (Coordinator integration):
   - Branch from `main` after this PR merges
   - Small, focused, testable
   - High impact (most-used knobs)
   - ~1-2 day turnaround

3. **Follow-2 through Follow-7**:
   - Sequential or parallel (independent subsystems)
   - Each ~30 mins to 1 hour per PR
   - Can be batched if desired

4. **Follow-8** (optional CLI):
   - Last; requires all subsystems to read config
   - Operator-facing polish only

---

## Success Criteria

✅ This PR is done when:
1. `src/config.rs` compiles and all tests pass
2. `src/main.rs` declares `mod config`
3. Sample `~/.aish/aish.config` is present and documented
4. `docs/reference/runtime-config.md` is comprehensive
5. No behavior change (all subsystems still use code defaults)
6. Backward compatible (env vars still work)

✅ Follow-1 is done when:
1. Coordinator reads `AISH_COORDINATOR_MAX_ROUNDS` from env > config file > default
2. Tests verify precedence
3. Operator can set knob in `~/.aish/aish.config` and it works
4. Regression tests pass

---

## Questions & Next Steps

**Q: When should I merge this PR?**
A: Once `cargo test config::` passes and code review approves. No behavior change, safe to ship immediately.

**Q: When should I start Follow-1?**
A: After this PR merges to main. Follow-1 unlocks the highest-value knobs (coordinator).

**Q: Can I skip some subsystems?**
A: Yes! Each subsystem is independent. You can integrate Coordinator first, then decide which others are worth it.

**Q: How do I test locally before merging?**
A: Run `cargo test config::` to verify the parser. Create a sample `~/.aish/aish.config` and verify it loads without error (which it will, since no subsystem reads it yet in this PR).

---

## Files in This PR

```
✅ src/config.rs                                 (+290 lines, with tests)
✅ src/main.rs                                   (mod config declared)
✅ ~/.aish/aish.config                           (sample config)
✅ docs/reference/runtime-config.md              (comprehensive reference)
✅ docs/INDEX.md                                 (link to runtime-config.md)
✅ RUNTIME_CONFIG_PR.md                          (this PR's description)
✅ RUNTIME_CONFIG_PR_PLAN.md                     (detailed implementation plan)
✅ INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md        (follow-up pattern)
✅ INTEGRATION_PATTERN_COORDINATOR.rs            (coordinator example)
```

---

## Ready to Ship? 🚀

This PR is **production-ready** once:
1. ✅ Code review approves
2. ✅ All tests pass
3. ✅ Sample config is valid INI

**No blockers.** Safe to merge to main immediately.
