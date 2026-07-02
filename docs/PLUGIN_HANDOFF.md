# Plugin System — Handoff / Status

> Living handoff for the aish plugin-system build-out. Read this first when picking
> up plugin work across coordinator runs. The full design is in
> [`docs/PLUGIN_SYSTEM_DESIGN.md`](./PLUGIN_SYSTEM_DESIGN.md); this file tracks
> **what actually shipped**, where it lives, how to verify it, and what's next.

**Last updated:** Phase 3 complete (merged to `main` via PR #386). Version at
handoff: `0.26.0`.

---

## Phase status at a glance

| Phase | Scope | Status | Landed |
|---|---|---|---|
| 1 | Core infra — discovery, loading, lifecycle, `:plugin list/info` | ✅ Done | on `main` |
| 2 | Plugin memory & state (file-based, namespaced, 0600 auth) | ✅ Done | PR #385 |
| 0.5.x | MVP capability slices (skill registry, manifest `provides`, consolidation) | ✅ Done | PRs #384/#385 |
| **3** | **Schemas & structured-output validation** | **✅ Done** | **PR #386** |
| 4 | Self-hosted webhook broker (separate binary) | ⬜ Not started | — |
| 5 | Webhook handler registration & dispatch | ⬜ Not started | — |
| 6 | GitHub plugin (first real plugin) | ⬜ Not started | — |
| 7 | aish.sh dynamic forwarding (optional) | ⬜ Not started | — |
| 8 | Plugin config & management (`:plugin config`) | ⬜ Not started | — |
| 9 | Plugin enable/disable & reload | ⬜ Not started | — |
| 10 | Webhook testing & debugging (`:webhook test/logs/replay`) | ⬜ Not started | — |
| 11 | Error handling & robustness / audit trail | ⬜ Partial (loader is already forgiving) | — |
| 12 | Docs & examples / scaffold generator | ⬜ Partial (design + this doc + hello-world example) | — |

Legend: ✅ merged to `main` · ⬜ pending.

---

## Phase 3 — what shipped (schema discovery + validation)

Delivered in `src/plugins.rs` (dependency-free; **no `jsonschema` crate added** —
see decision below). Tasks 3.1–3.6 all landed.

**Discovery (3.1 / 3.3).**
`plugins::load_schemas(plugin_dir: &Path) -> Vec<PluginSchema>` reads every
`<plugin>/schemas/*.json` into `PluginSchema { name, schema }` where `name` is the
file stem (`schemas/greeting.json` → `greeting`), **sorted by name**. Forgiving:
absent `schemas/` → empty list; unreadable / non-`.json` / invalid-JSON /
non-object-or-boolean files are **skipped silently**. `discover()` attaches the
result to each `Plugin` as `Plugin.schemas`.

**Validator (3.2).**
`validate_json_schema(schema, instance) -> Vec<SchemaViolation>` — a pragmatic
draft-07 subset that collects **every** violation (empty vec = valid) instead of
bailing on the first. Boolean schemas honored. Keywords covered:

| Applies to | Keywords |
|---|---|
| any    | `type` (string or array), `enum`, `const` |
| object | `required`, `properties` (recurse), `additionalProperties` |
| array  | `minItems`, `maxItems`, `items` |
| string | `minLength`, `maxLength`, `pattern` |
| number | `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` |

Out of scope for the MVP: `$ref`/`$defs`, `allOf`/`anyOf`/`oneOf`/`not`, `format`,
`propertyNames`, tuple `items`, `patternProperties`, `uniqueItems`, `multipleOf`.

**Error reporting.** Each `SchemaViolation` carries a JSON-pointer-ish `path`
(RFC-6901 escaped) + human `message`; `Display` renders `"(root): …"` / `"/a/b: …"`.
Aggregate `SchemaValidationError` has `UnknownSchema(name)` and
`Failed(Vec<SchemaViolation>)`.

**Runtime seams (3.4).** Both return `Result<(), SchemaValidationError>`:
- `Plugin::validate(schema_name, value)` — validate against a loaded plugin's named schema.
- `plugins::validate_against_plugin_schema(plugins_dir, plugin_id, schema_name, value)`
  — the runtime entry point (discovers the plugin fresh; needs only dir + ids).
  These are the wiring seam for a future "validate this tool's structured return"
  enforcement point — reachable programmatically today, exercised by tests.

**Skill attachment (3.3).** `src/skills.rs` — `Skill.output_schema: Option<String>`
lifted from SKILL.md frontmatter; a dangling ref never drops the skill (it disables
validation + warns, surfaced via `unresolved_skill_schemas()`).

**CLI introspection (3.5).** `:plugin info <id> --schema` (alias `--schemas`)
renders each schema's top-level `type`, `properties` names, and `required` keys.
No `schemas/` → `(none)`; unknown id → hint to `:plugin list`.

**Example fixture.** `examples/plugins/hello-world/schemas/greeting.json` — draft-07
object schema exercising every covered keyword; the hello-world skill declares
`output_schema: greeting`, so the full lifecycle is demoable via
`:plugin info hello-world --schema`.

**Tests (3.6).** In `src/plugins.rs`: discovery
(`load_schemas_reads_json_sorted_and_skips_junk`, `load_schemas_absent_dir_is_empty`),
validator coverage (`validate_type_required_and_additional_properties`,
`validate_array_items_and_string_pattern`, `validate_enum_const_and_number_bounds`,
`validate_boolean_schema_false_rejects_everything`), plugin seam
(`plugin_validate_and_unknown_schema`, `validate_against_plugin_schema_end_to_end`),
CLI rendering (`format_plugin_info_and_schemas_render`), example
(`example_hello_world_plugin_config_resolves`).

### Files touched (Phase 3)

| File | Change |
|---|---|
| `src/plugins.rs` | `PluginSchema`, `SchemaValidationError`, `load_schemas`, `validate_json_schema`, `Plugin::{schemas,validate}`, `validate_against_plugin_schema`, `format_plugin_info` schema rendering, tests |
| `src/skills.rs` | `Skill.output_schema` + frontmatter parse + `unresolved_skill_schemas()` + tests |
| `src/skill_match.rs` | test helper updated for the new field |
| `examples/plugins/hello-world/` | ships `schemas/greeting.json`; skill declares `output_schema: greeting` |

### Verify

```
cargo check --no-default-features --locked           # clean (pre-existing dead-code warnings only)
cargo test  --no-default-features --locked schema    # schema-filter tests pass
cargo test  --no-default-features --locked plugins   # full plugins:: suite passes
```

---

## Key design decision (carry forward)

**No `jsonschema` crate.** Task 3.2 suggested `jsonschema = "0.18"` + a separate
`src/plugin_schemas.rs`. We implemented a dependency-free `validate_json_schema`
**inside `src/plugins.rs`** instead — keeps the `--no-default-features --locked`
build lean and avoids a heavy transitive tree, while covering the keyword set
plugin authors actually write for tool returns. If richer keywords (`$ref`,
`allOf`/`anyOf`/`oneOf`, `format`, JSON-path error strings) are later required,
swap the crate in **behind the same `validate_json_schema` seam** — callers
(`Plugin::validate`, `validate_against_plugin_schema`) don't change.

> Note: the module layout differs from the task's suggested `src/plugin_schemas.rs`
> — everything lives in `src/plugins.rs`. Data-structure names also differ from the
> task sketch (`PluginSchema { name, schema }` vs `{ id, filename, schema, description }`;
> `SchemaViolation`/`SchemaValidationError` vs `ValidationError`). Behavior meets
> all acceptance criteria.

---

## Next up

**Enforcement wiring (small, unblocked now).** The 3.4 seam
(`validate_against_plugin_schema`) is not yet called from a tool-return site.
Wiring it into the tool-result path in `src/tools.rs` / `src/engine.rs` (validate
declared structured returns, log-not-crash on failure) is the natural follow-up —
it's the "runtime enforcement" half of Phase 3's intent and needs no new phase.

**Phase 6 (GitHub plugin) is partially unblocked.** Schema-validated tool
definitions (`schemas/github-pr.json`, `schemas/github-issue.json`, tool
`output_schema` refs) can be authored now against the Phase 3 machinery. But the
*webhook* half of Phase 6 (`on_init` webhook config, PR/issue/review handler
dispatch) depends on **Phase 4 (broker)** + **Phase 5 (handler dispatch)**, which
are not started. Sequence: **4 → 5 → 6**, or land the GitHub plugin's
tools/skills/schemas first and bolt on webhooks once the broker exists.

**Open questions (from design doc addendum).**
1. Plugin-contributed `PreToolUse` veto — gate behind install-time consent + a
   user-controlled `policy_enforcement` config. *Leaning: yes.*
2. Catalog-event allow-list — require `plugin.json` to declare registered events in
   `provides.event_hooks` (array, auditable); loader rejects undeclared. *Leaning: yes.*
3. Managed-config push cadence — start with pull-at-`SessionStart`; a live channel
   can reuse the Phase 4 broker later.

---

## Where things live

- **Design (source of truth):** `docs/PLUGIN_SYSTEM_DESIGN.md` (§ "Phase 3 detail"
  and § "Implementation Phases" carry the authoritative spec + phase checklist).
- **Code:** `src/plugins.rs` (discovery, memory glue, schema validation),
  `src/plugin_memory.rs`, `src/plugin_state.rs`, `src/plugin_auth.rs`,
  `src/plugin_dispatcher.rs`, `src/skills.rs` (skill `output_schema`).
- **Example plugin:** `examples/plugins/hello-world/` (end-to-end proof, incl. a schema).
- **Supporting docs:** `docs/plugin-memory-schema.md`, `docs/plugin-state-schema.md`,
  `docs/plugin-webhook-events.md`.
