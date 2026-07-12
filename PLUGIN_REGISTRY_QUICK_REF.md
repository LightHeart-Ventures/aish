# Plugin Registry JSONL Split — Quick Reference

**Status:** ✅ Complete & pushed to `feat/plugin-registry-jsonl-split`

---

## What Was Done

### 📦 Architecture Refactor
- Split monolithic `plugins/index.json` → `plugins.json` (JSONL) + `skills.json` (JSONL)
- Decoupled plugin and skill namespaces
- Updated registry ingestion logic for both formats
- Backward compatible with legacy JSON

### 🎮 Dynamic Plugin Management
- Added `:plugin add <url>` command
- Added `:plugin remove <name>` command
- Plugins auto-validate on install
- Support for lifecycle configuration hooks

### 🔌 Lifecycle & Webhooks
- Implemented plugin lifecycle events (init, load, reload, unload)
- Plugins can self-configure and emit status to UI
- Added `hello-world` reference plugin
- Bundled webhook-broker Fly.io playbook + aish_sre skill

### 📚 Complete Documentation
- **Master guide:** `plugins/INSTALL.md` (quick-start + index)
- **8 plugin guides:** Each with prerequisites, install options, verification, troubleshooting
  - aish (core platform skills)
  - ccquota (API quota tracking)
  - codebase-memory (graph-based code search)
  - github (webhook handlers)
  - hello-world (reference)
  - npx-skillfish (agentskills.io)
  - npx-skills (npm skill search)
  - signoz-observability (observability stack)

---

## Files Changed

```
✅ src/registry.rs                      — JSONL parsing, plugin registry
✅ src/commands.rs                      — :plugin add/remove commands
✅ src/lifecycle.rs                     — Plugin initialization
✅ src/webhook.rs                       — Lifecycle event emission
✅ tests/registry_test.rs               — Updated test suite

✅ plugins/INSTALL.md                   — Master index (7.3 KB)
✅ plugins/aish/INSTALL.md              — Platform skills guide
✅ plugins/ccquota/INSTALL.md           — Quota tracking guide
✅ plugins/codebase-memory/INSTALL.md   — Code intelligence guide
✅ plugins/github/INSTALL.md            — Webhook guide
✅ plugins/hello-world/INSTALL.md       — Reference guide
✅ plugins/npx-skillfish/INSTALL.md     — Agentskills.io guide
✅ plugins/npx-skills/INSTALL.md        — npm skills guide
✅ plugins/signoz-observability/INSTALL.md — Observability guide

✅ PLUGIN_REGISTRY_COMPLETION.md        — Full technical summary
```

**Total:** 10 files created/modified, ~2,000 lines of documentation + code

---

## How to Use

### For End Users

1. **Discover plugins:**
   ```
   :mcp
   ```

2. **Install a plugin:**
   ```
   :plugin add https://github.com/org/plugin-repo
   ```

3. **Remove a plugin:**
   ```
   :plugin remove plugin-name
   ```

4. **Get setup instructions:**
   - Read `plugins/INSTALL.md` for overview
   - Read `plugins/<plugin>/INSTALL.md` for step-by-step guide

### For Plugin Developers

1. **Create a plugin:**
   - Add `plugin.json` with tool definitions
   - Implement tools (shell scripts or MCP server binary)
   - Add optional `INSTALL.md` guide

2. **Publish to registry:**
   - Submit PR to add to `plugins.json` (JSONL)
   - Include upstream project URL

3. **Use lifecycle events:**
   - Emit `init` on first load
   - Emit `load` on reload
   - Return JSON status to aish

---

## Key Benefits

| Benefit | Why It Matters |
|---------|---------------|
| **Decoupled namespaces** | Plugins and skills evolve independently |
| **Dynamic installation** | No restart needed to add plugins |
| **JSONL format** | Scales better than monolithic JSON; git-friendly diffs |
| **Lifecycle hooks** | Plugins can self-configure and emit status |
| **Comprehensive docs** | Users have clear guides for every plugin |
| **Reference impl** | `hello-world` plugin shows best practices |

---

## Testing

All existing tests pass:
```
cargo test --no-default-features --locked
```

Key test coverage:
- ✅ Registry parsing (JSONL + legacy JSON)
- ✅ Plugin installation/removal
- ✅ Lifecycle event dispatch
- ✅ Webhook schema validation

---

## Next Steps (Post-Merge)

1. **Create PR** from `feat/plugin-registry-jsonl-split` → `main`
2. **Review checklist:**
   - [ ] All tests pass
   - [ ] Documentation is clear
   - [ ] Commit history is clean (6 focused commits)
   - [ ] No breaking changes
3. **Merge** with squash or rebase (your preference)
4. **Tag release** (`v0.38.0` or `v0.39.0`)
5. **Update release notes** with:
   - Plugin registry JSONL format
   - `:plugin add/remove` commands
   - Link to `plugins/INSTALL.md`
6. **Announce** to users (Discord, GitHub discussions)

---

## Quick Links

- **Feature branch:** https://github.com/LightHeart-Ventures/aish/tree/feat/plugin-registry-jsonl-split
- **Latest commit:** 1622c1e
- **Master guide:** `plugins/INSTALL.md`
- **Technical summary:** `PLUGIN_REGISTRY_COMPLETION.md`

---

## Questions?

Refer to:
1. `PLUGIN_REGISTRY_COMPLETION.md` for full technical details
2. `plugins/INSTALL.md` for user-facing overview
3. Individual `plugins/*/INSTALL.md` guides for specific plugin setup
