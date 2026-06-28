# search-skills — design notes & relationship to the engine

`search-skills` is a **user-invoked playbook** (a `SKILL.md` the model reads and
follows). It is the explicit, richer-output sibling of aish's **automatic**
skill-awareness, which lives in the Rust engine (`src/skill_match.rs`). This file
records how the two relate so the skill never drifts back into competing with the
binary.

## Two layers, one feature

| | Engine (`skill_match`, automatic) | `search-skills` (this playbook, explicit) |
|---|---|---|
| Layer | Rust, in-binary, every turn | Markdown playbook the model follows |
| Trigger | Per-turn token match on the task | User asks "find / recommend a skill" |
| Installed match | Prepends an `[aish skill-awareness]` note | Step 2–5: ranked table + stars |
| No local match | `recommend_install` → `:skill add <ref>` | Step 4: same recommendation, richer copy |
| Registry source | offline `~/.aish/registry/index.json` (`skill_provider::local_index_catalog`) | **same** offline index, via `scripts/` |
| Relevance rule | `skill_match::relevance` (name-weighted) | **same** rule, re-stated in `discover-skills.sh` |

Because this skill is installed, the engine's automatic awareness will itself
surface it when a task mentions "find / search / recommend a skill" — i.e. the
binary becomes the auto-trigger for the playbook.

## Three reconciliations baked into this skill (vs. the original draft)

1. **No `:skill search` collision.** `:skill search <query>` is a built-in that
   does a **live network search** of skill.fish / mcpmarket (`repl.rs` →
   `SkillCmd::Search` → `skill_provider::search`). This skill does **not** claim
   that verb; it triggers on prose and points users at `:skill search` for live
   search.
2. **Defer to the engine's matching.** Rather than ship a second, divergent
   scoring formula, the helper scripts re-state the engine's single
   name-weighted relevance rule so the ranked table agrees with the automatic
   banner. Star ratings are presentation only.
3. **Rust reality, not TypeScript.** aish's router is Rust (`parse_skill_command`
   in `src/repl.rs`); there is no `invoke_skill` host call. "Using" a skill is
   reading its `SKILL.md` and following it — there is no separate invoke step.

## Scripts

| Script | Purpose |
|--------|---------|
| `scripts/discover-skills.sh "<task>"` | Scan `~/.aish/skills`, score each installed skill by the engine's name-weighted relevance rule, print ranked JSON. |
| `scripts/registry-stars.sh <name…>` | Best-effort star lookup from the offline registry index; 0 when absent. Never hits the network. |
| `scripts/registry-candidates.sh "<task>"` | Rank INSTALLABLE registry entries (the same offline index the engine's `recommend_install` reads) for the no-local-match path. |

All scripts are POSIX `bash`, dependency-light (`jq` used when present, with a
grep/sed fallback), and degrade to empty output rather than erroring when the
registry index or skills directory is missing.

## Installing this skill

This copy lives in the repo under `examples/skills/search-skills/` as a
reference. To use it locally, copy it into `~/.aish/skills/search-skills/` (the
directory aish scans on startup) — or `:skill add` it once it's published to a
registry.
