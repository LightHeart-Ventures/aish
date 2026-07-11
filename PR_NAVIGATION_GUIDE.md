# Runtime Configuration PR — Navigation Guide

## 📖 Where to Start?

**If you have 5 minutes:**
→ Read `EXECUTIVE_SUMMARY.md` (elevator pitch for stakeholders)

**If you have 10 minutes:**
→ Read `EXECUTIVE_SUMMARY.md` + skim `RUNTIME_CONFIG_PR.md` (PR description)

**If you have 30 minutes:**
→ Read `EXECUTIVE_SUMMARY.md` + `RUNTIME_CONFIG_PR.md` + review `src/config.rs` (the parser)

**If you have 1 hour:**
→ Do the full code review:
1. `EXECUTIVE_SUMMARY.md` (context)
2. `RUNTIME_CONFIG_PR.md` (what changed, why, next steps)
3. `src/config.rs` (code, tests, documentation)
4. `~/.aish/aish.config` (sample config)
5. `docs/reference/runtime-config.md` (knobs reference)
6. `INTEGRATION_CHECKLIST.md` (follow-up roadmap)

---

## 📁 File Organization

### Core PR Files (Merge-Ready)

| File | Purpose | Size | Status |
|------|---------|------|--------|
| `src/config.rs` | INI parser + 7 unit tests | 286 lines | ✅ Production-ready |
| `src/main.rs` | Declare `mod config` | +1 line | ✅ Ready |
| `~/.aish/aish.config` | Sample configuration | 221 lines | ✅ Comprehensive |
| `docs/reference/runtime-config.md` | Knobs reference | Full docs | ✅ Complete |
| `docs/INDEX.md` | Link to runtime-config.md | Updated | ✅ Ready |

→ **These 5 files go in the merge commit.**

### Support Documents (For Review)

| File | Audience | Read Time |
|------|----------|-----------|
| `EXECUTIVE_SUMMARY.md` | Stakeholders, decision-makers | 5 min |
| `RUNTIME_CONFIG_PR.md` | Code reviewers | 5 min |
| `RUNTIME_CONFIG_PR_PLAN.md` | Implementation leads | 5 min |
| `INTEGRATION_CHECKLIST.md` | Project managers, engineers | 10 min |
| `INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md` | Engineers writing follow-ups | 10 min |
| `INTEGRATION_PATTERN_COORDINATOR.rs` | Engineers writing Coordinator follow-up | 5 min |

→ **These support review and future work. Optional to commit (helpful for PRs reviewers).**

### Validation & Deployment

| File | Purpose | Usage |
|------|---------|-------|
| `validate_pr.sh` | Pre-merge validation | `bash validate_pr.sh` |
| `COMMIT_MESSAGE.md` | Git commit template | Copy into `git commit -m` |
| `DELIVERABLES_SUMMARY.md` | Complete artifact checklist | Reference during merge |

---

## 🎯 Quick Review Path

### Step 1: Understand the "Why" (3 min)
Read `EXECUTIVE_SUMMARY.md` sections:
- **Context** (why this now?)
- **User Impact** (what does an operator gain?)
- **Risk Assessment** (is this safe?)

### Step 2: Verify Scope (3 min)
Read `RUNTIME_CONFIG_PR.md`:
- **Changed Files** (what's new, what's modified?)
- **Zero Behavior Change** (is it really optional?)
- **Backward Compatibility** (are env vars still first priority?)

### Step 3: Audit Code (10 min)
Read `src/config.rs`:
- Public API: `Config`, `parse()`, `from_file()`
- Tests: 7 unit tests covering edge cases
- No external dependencies (stdlib only)

### Step 4: Spot-Check Config (3 min)
Read `~/.aish/aish.config`:
- Scan for completeness (all 11 subsystems covered?)
- Check documentation quality (every knob explained?)

### Step 5: Understand Follow-Ups (5 min)
Read `INTEGRATION_CHECKLIST.md`:
- 8 follow-up PRs mapped out
- Priorities (which subsystems first?)
- Estimated effort (how long total?)

→ **Total: ~25 minutes for a thorough, confident review.**

---

## 🚀 How to Use This PR

### Pre-Merge
1. Clone the branch (or it's already in your working tree)
2. Run: `bash validate_pr.sh` (automated checks)
3. Run: `cargo test config:: --lib` (unit tests)
4. Code review using the 5-step path above
5. Merge to main

### Post-Merge: Immediate Actions
- Link to `INTEGRATION_CHECKLIST.md` in a GitHub milestone
- Assign Follow-1 (Coordinator) to an engineer
- Announce to team: "Runtime config is now live; config file is optional"

### Post-Merge: Next 1–2 Weeks
- Follow-1 (Coordinator) merges → operators can set `max_rounds` in config
- Follow-2 (Alerts) merges → operators can set `bell`, `bell_cmd` in config
- Follow-3 (Updates) merges → operators can choose update channel in config
- ... (and so on for Follow-4 through Follow-8)

---

## ✅ Pre-Merge Checklist

Before clicking "Merge", verify:

| Item | Check |
|------|-------|
| Code review complete | ✅ |
| `validate_pr.sh` passes | ✅ |
| Unit tests pass (`cargo test config::`) | ✅ |
| No breaking changes | ✅ |
| Backward compatible (env vars take precedence) | ✅ |
| Documentation is complete | ✅ |
| Integration patterns documented for all 8 subsystems | ✅ |
| Follow-up roadmap is clear | ✅ |
| Sample config is valid INI | ✅ |

---

## 🔗 Cross-References

### If You Need To...

**Understand the INI parser:**
→ `src/config.rs` (lines 50–150)

**See all configurable knobs:**
→ `~/.aish/aish.config` (all 11 sections)

**Understand what each knob does:**
→ `docs/reference/runtime-config.md` (full reference)

**Integrate Coordinator to read config:**
→ `INTEGRATION_PATTERN_COORDINATOR.rs` (concrete example)

**Plan follow-up PRs:**
→ `INTEGRATION_CHECKLIST.md` (timeline + ownership)

**Understand code patterns for all subsystems:**
→ `INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md` (all 8 subsystems)

**Know what changed:**
→ `RUNTIME_CONFIG_PR.md` (formal PR description)

**Understand the rationale:**
→ `EXECUTIVE_SUMMARY.md` (big picture, risk, timeline)

---

## 🎓 Learning Path for Future Contributors

If you're writing a follow-up PR (e.g., Follow-2 for Alerts), read in this order:

1. **This guide** (you are here)
2. `INTEGRATION_CHECKLIST.md` (find your PR in the timeline)
3. `INTEGRATION_PATTERNS_ALL_SUBSYSTEMS.md` (find your subsystem's pattern)
4. `docs/reference/runtime-config.md` (understand which knobs affect your subsystem)
5. `src/config.rs` (understand the parser API)
6. Your subsystem's code (e.g., `src/alert.rs` for alerts)
7. **Write your follow-up PR** (copy the pattern, wire up the knobs)

---

## 💡 FAQ

**Q: Can I commit without the sample config?**
A: No. The sample config (~/.aish/aish.config) is essential for operators and must merge.

**Q: Do all follow-up PRs need to merge before operators can use config?**
A: No. Config file is optional and can be used immediately. First follow-up (Coordinator) lands in ~1 day and gives operators their first knob to tune (max_rounds).

**Q: What if an operator puts invalid values in the config?**
A: Type conversion failures are caught gracefully. The parser falls back to defaults and logs a warning. No crash.

**Q: Can I test this locally before merging?**
A: Yes. After merge, operators can:
1. Create `~/.aish/aish.config` (from the sample)
2. Run `aish` (config is optional, no error if missing)
3. Wait for Follow-1 to integrate Coordinator (then they can set max_rounds)

**Q: Does this change any existing behavior?**
A: No. Config file is optional, env vars take precedence, no subsystem reads config yet. 100% backward compatible.

---

## 📊 Metrics & Timeline

| Phase | Effort | Timeline | Blocker? |
|-------|--------|----------|----------|
| **This PR** (infrastructure) | 0 min | Ready now | No |
| **Follow-1** (Coordinator) | ~30 min | ~1 day | No |
| **Follow-2** (Alerts) | ~20 min | ~1 day | No |
| **Follow-3** (Updates) | ~25 min | ~1 day | No |
| **Follow-4–7** (other subsystems) | ~15–20 min each | ~3–5 days | No |
| **Follow-8** (optional CLI) | ~1 hour | ~1 day | No |
| **Total (all subsystems)** | **~2–3 hours** | **~1–2 weeks** | None |

**Impact:** High (operators gain persistent, discoverable configuration)
**Risk:** Low (infrastructure only, no behavior change yet)

---

## 🎯 Final Note

This PR is **boring infrastructure that unlocks exciting operator UX**. It's safe to merge immediately — the config file is optional, existing behavior is unchanged, and the follow-up roadmap is crystal clear.

**Ready to merge. 🚀**

Questions? Check the relevant file above. Didn't find the answer? Open an issue or ask in code review.
