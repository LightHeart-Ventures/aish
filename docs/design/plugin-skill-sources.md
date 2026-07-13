# Plugin-Provided Skill Sources — `:skill search` / `:skill add` — Design

Status: **Draft for review** · Owner: aish core · Scope: high-level design (no implementation) · Tracks: **FR-332** (plugin distribution / skill-source federation)

This document designs a capability surface that lets **plugins contribute
results to `:skill search` and `:skill add`**, and shows how the shell's
built-in **skill.fish / mcpmarket** integration can be re-expressed as the first
such plugin. It is grounded in the code that ships today
(`src/repl.rs` `:skill` handler, `src/skill_provider.rs`, `src/plugins.rs`,
`src/plugin_auth.rs`) and deliberately reuses the **`provides.login` →
`login.sh`** handler pattern as its template.

It complements [`fly-plugin-integration.md`](./fly-plugin-integration.md) §9,
which names the *distribution* gap (no plugin registry). This design is the
mirror-image opportunity on the **capability** axis: today plugins *feed skills
into* the registry as static files on disk, but they cannot participate in the
**live search/fetch** path that `:skill` drives. Closing that turns skill
discovery from a single hard-wired source (mcpmarket) into an **open,
plugin-extensible federation** — and lets the built-in source become "just
another plugin."

---

## 1. How `:skill search` / `:skill add` work today

Verified against `src/repl.rs` (`handle_skill_command`, `skill_search`,
`skill_add`) and `src/skill_provider.rs`.

```
:skill search <q>   → repl::skill_search(q)  → skill_provider::search(q)   → Vec<SearchResult>
:skill add <ref>    → repl::skill_add(ref)   → skill_provider::add(ref, …) → Vec<ImportedSkill>
:skill list                                   → skills::load(~/.aish/skills)
:skill remove <n>                             → rm -rf ~/.aish/skills/<n> + reload
```

**Search** (`skill_provider::search`) has a fixed, hard-coded source precedence:

1. `AISH_SKILL_REGISTRY` override — authoritative when set.
2. **mcpmarket.com** live skills API (dynamic primary), merged with…
3. the curated **binary-embedded** index (`registry/index.json`) — offline
   fallback.

Results merge/dedupe via `merge_results` (keyed on `SearchResult::ref_or_synth`)
and rank by `stars`.

**Add** (`skill_provider::add`) resolves a `reference` shaped as one of:
`owner/name[@version]` (skill.fish), `github:owner/repo[/path][@ref]`, a
`github.com/…` URL, or a `raw.githubusercontent.com/…/SKILL.md` URL — fetches the
SKILL.md(s), imports via `skill_provider::import`, and the REPL reloads the
catalog.

**The constraint:** every source is *compiled into* `skill_provider.rs`. There is
no seam for a plugin to add a source, redirect a ref namespace, or supply its own
catalog. mcpmarket + skill.fish + the embedded index are the only three sources
that will ever answer, and their precedence is frozen in code.

### 1.1 What plugins can already do (and the gap)

`src/plugins.rs` `discover()` already lets a plugin contribute **skills as
files** (`<plugin>/skills/<name>/SKILL.md`), which `plugin_skills()` flattens into
the same catalog `~/.aish/skills` feeds. That is a *static, install-time*
contribution: the skill is on disk before the shell starts.

What no plugin can do today is participate in the **interactive discovery/fetch**
path — answer a `:skill search <q>` with live results, or resolve a `:skill add
<ref>` from a namespace it owns. That is the gap this design closes.

---

## 2. The template: `provides.login` → `login.sh`

The shell already ships one plugin-provided *interactive command* handler, and it
is the exact shape we want. From `src/plugin_auth.rs`:

| Element | `login` implementation | What we reuse |
|---|---|---|
| Manifest opt-in | `provides.login: "<name>"` | a new `provides.skill_source` block |
| Router | `login_at()` finds the plugin whose `login_command()` matches | a resolver that finds skill-source plugins |
| Handler | `<plugin>/login.sh`, run via `run_login_handler` | `<plugin>/search.sh` + `<plugin>/add.sh` |
| I/O contract | env in (`AISH_PLUGIN_ID`, `AISH_LOGIN_NAME`, `AISH_TENANT_ID`, …), **JSON on stdout**, stderr/stdin inherited, non-zero = error | identical: env in, JSON on stdout, non-zero = error |
| ETXTBSY-hardened exec, retry/backoff | already in `run_login_handler` | factor into a shared `run_plugin_handler` |

The login path proves the whole contract: discover a plugin by a `provides.*`
key, exec a named script in its dir with a curated env, capture stdout as JSON,
fail loudly on non-zero. **We generalize that one proven mechanism** rather than
invent a new plugin IPC.

---

## 3. Proposed capability: `provides.skill_source`

Add a `skill_source` block to the manifest `Provides` struct (`src/plugins.rs`):

```json
{
  "id": "skillfish",
  "name": "skill.fish + mcpmarket",
  "version": "1.0.0",
  "provides": {
    "skill_source": {
      "id": "skillfish",
      "priority": 100,
      "search": "search.sh",
      "add": "add.sh",
      "handles": ["*", "skillfish:*", "*/*"]
    }
  }
}
```

Fields:

| Field | Meaning |
|---|---|
| `id` | Source label shown in the SOURCE column of results and in `:skill sources`. Defaults to plugin id. |
| `priority` | Merge/precedence rank (higher wins on ref/name-dedup ties, and orders `add` attempts). Built-in embedded index sits at a low fixed priority so plugins can outrank it. |
| `search` | Handler script (relative to plugin dir) that answers `:skill search`. Optional — a source may be add-only. |
| `add` | Handler script that resolves + emits a SKILL.md for `:skill add`. Optional — a source may be search-only. |
| `handles` | Glob/prefix patterns of `reference` namespaces this source claims for `add` routing (e.g. `"github:*"`, `"acme/*"`). Drives *which* plugin(s) a given `:skill add <ref>` is offered to, in priority order. |

Only `skill_source` is new; every other manifest field is unchanged. serde's
unknown-key-dropping means older binaries ignore it and newer binaries ignore its
absence — same forward/backward-compat story as every prior plugin phase.

### 3.1 Handler contract

Both handlers mirror `login.sh`: exec'd in the plugin dir with a curated env,
**JSON on stdout**, stderr/stdin inherited (so an interactive/token flow can
prompt), non-zero exit = error (skipped in search fan-out; surfaced in add).

**`search.sh`** — query in, result array out:

```
env:  AISH_PLUGIN_ID, AISH_SKILL_QUERY="<query>", AISH_SKILL_LIMIT, AISH_TENANT_ID,
      AISH_CREDENTIALS_FILE   (so it can read ${profile:<id>} for authed catalogs)
stdout (JSON):
  [ { "name": "...", "author": "...", "description": "...",
      "version": "...", "reference": "...", "stars": 0 }, … ]
```

The object shape is **exactly** `skill_provider::SearchResult` (which already
accepts the alias soup `owner`/`ref`/`slug`/`github_stars`/…), so plugin output
deserializes through the existing struct with zero new parsing.

**`add.sh`** — reference in, SKILL.md out (or a fetch plan):

```
env:  AISH_PLUGIN_ID, AISH_SKILL_REF="<reference>", AISH_SKILLS_DIR, AISH_CREDENTIALS_FILE
stdout: EITHER the raw SKILL.md text (single skill)
        OR a JSON array of { "path": "<name>", "content": "<SKILL.md text>" } (multi-skill import)
```

The REPL/`skill_provider` side writes the returned SKILL.md(s) via the existing
`skill_provider::import` + `session.reload_skills_from` — the write/reload half is
already factored and untouched.

### 3.2 Why scripts, not an in-process trait

A compiled `SkillSource` trait would be faster but requires linking every source
into the binary — which is exactly the coupling we're removing. Scripts:

- match the **already-shipping** `login.sh` contract (one IPC to reason about),
- let a source be authored/updated **without recompiling aish**,
- inherit the security posture already accepted for `login.sh` / `webhook_command`
  (plugins run local code by design; this adds no new trust surface),
- keep the built-in Rust `skill_provider` search/add as an **in-process fallback
  source** (§5) so the feature degrades to today's behavior with zero plugins.

---

## 4. Aggregation model

### 4.1 `:skill search` — fan-out + merge

```
skill_search(q):
    sources = [ built-in in-process source ] ++ discover_skill_source_plugins()
    results = parallel_map(sources, |s| s.search(q))   // per-source timeout, failures → []
    merged  = fold(results, sort by priority desc, merge_results dedupe on ref_or_synth)
    print_results_table(q, merged)   // + a SOURCE column
```

- Reuses `skill_provider::merge_results` (already dedupes on `ref_or_synth` and is
  order-preserving) and `print_results_table` (extended with a SOURCE column).
- A slow/broken plugin source is bounded by a per-source timeout and contributes
  `[]` — search never hangs or fails because one source misbehaved (same
  fail-soft posture as `search()`'s current mcpmarket→embedded degradation).
- `AISH_SKILL_REGISTRY` still short-circuits the **built-in** source to the
  override; plugin sources are independent of it.

### 4.2 `:skill add` — priority-ordered resolution

```
skill_add(ref):
    candidates = sources_whose_`handles`_match(ref) sorted by priority desc
    for s in candidates:
        try s.add(ref) → SKILL.md(s) → import + reload → done
    if none matched/resolved: fall through to built-in skill_provider::add(ref)
```

- `handles` globs route a ref to the right namespace owner first (e.g. a future
  `fly` plugin could own `fly:*` skill refs), while a catch-all built-in still
  resolves `owner/name` / `github:…` refs exactly as today.
- First source that returns a SKILL.md wins; a non-zero/empty source is tried-next
  (add is a resolution race, unlike search's union).

### 4.3 New surface: `:skill sources`

A small read-only subcommand listing registered skill sources (built-in +
plugins), their `id`, `priority`, and whether they answer `search`/`add` — the
discoverability analogue of `:plugin list`. Pure addition; no behavior change.

---

## 5. Migrating skill.fish / mcpmarket to a plugin

The built-in source stays in the binary as the **fallback in-process source**
(so a zero-plugin install behaves identically to today), but its *public identity*
becomes a bundled plugin so it is visible, reorderable, and replaceable.

Two viable shapes:

| Shape | What ships | Trade-off |
|---|---|---|
| **A. Manifest-only façade (recommended first)** | A bundled `~/.aish/plugins/skillfish/plugin.json` declaring `provides.skill_source` whose `search`/`add` are handled **in-process** by the existing `skill_provider` code (a reserved handler sentinel, e.g. `"search": "@builtin"`). | Zero new script, zero network-code duplication; the existing Rust path *becomes* a named, reorderable source. Cleanest migration. |
| **B. Real script plugin** | `search.sh` / `add.sh` that shell out to `curl` against mcpmarket + skill.fish, plus the embedded index shipped as a data file. | Fully decouples the source from the binary (can update without a release) but reimplements `search_mcpmarket` / `merge_results` in shell — more surface, more drift risk. |

**Recommendation: ship A now, offer B later.** Shape A reframes the built-in as
"the `skillfish` skill source, priority 100, handled in-process" — a one-file
manifest plus a `@builtin` handler dispatch in the resolver. It gets the *model*
(sources are plugins; the built-in is one of them) in place with no behavioral
risk, and leaves the door open to later lift the network code into a true script
plugin (B) once the distribution/registry gap (`fly` §9) makes shipping updatable
plugins ergonomic.

### 5.1 End state

```
~/.aish/plugins/skillfish/
└── plugin.json         # provides.skill_source { id:"skillfish", priority:100,
                        #   search:"@builtin", add:"@builtin", handles:["*","*/*","github:*"] }
```

`:skill search foo` now shows a **SOURCE = skillfish** column; `:skill sources`
lists it; a third-party plugin declaring `priority: 200` with its own `search.sh`
transparently ranks ahead of it — the exact extensibility the current hard-wired
path can't offer.

---

## 6. Wiring changes (where the code touches)

| # | Change | File | Size |
|---|---|---|---|
| 1 | Add `SkillSource` struct + `skill_source` field to `Provides` | `src/plugins.rs` | small |
| 2 | `discover_skill_sources()` — collect plugins with a `skill_source` block, id-/priority-sorted | `src/plugins.rs` | small |
| 3 | Factor `run_login_handler`'s exec (ETXTBSY retry, env, stdout-capture) into a shared `run_plugin_handler(script, env)` | `src/plugin_auth.rs` → shared helper | small |
| 4 | A `SkillSource` façade: in-process `@builtin` variant (calls today's `skill_provider::search`/`add`) + script variant (runs `search.sh`/`add.sh`) | `src/skill_provider.rs` (new module `skill_sources.rs`) | medium |
| 5 | `skill_search`/`skill_add` fan-out + merge + priority-resolve | `src/repl.rs` | small |
| 6 | `SearchResult` gains a non-wire `source` label; `print_results_table` gains a SOURCE column | `src/skill_provider.rs` | small |
| 7 | `:skill sources` subcommand + `SkillCmd::Sources` variant | `src/repl.rs` | small |
| 8 | Bundle the `skillfish` façade plugin manifest (install at first run, like other bundled assets) | packaging | small |

No change to `import`, `reload_skills_from`, `skills::load`, the embedded index,
or the credential/env-resolution machinery. The blast radius is: the manifest
struct, one discovery helper, one shared exec helper, a thin source-façade module,
and the two `:skill` verb handlers.

---

## 7. Phasing

| Phase | Deliverable | Unlocks |
|---|---|---|
| 1 | `provides.skill_source` parsed; `discover_skill_sources()`; shared `run_plugin_handler` | Manifest surface + resolver exist |
| 2 | `@builtin` façade + bundle the `skillfish` plugin; `:skill sources` | Built-in becomes a named, listable source (no behavior change) |
| 3 | Search fan-out + SOURCE column; `add` priority-resolution via `handles` | Plugins can *add results* to search and *own ref namespaces* for add — **the headline feature** |
| 4 | Reference **script** skill-source plugin (e.g. a private/enterprise catalog) + docs | Third-party sources without recompiling aish |
| 5 | (Optional) lift mcpmarket/skill.fish network code into a true script plugin (shape B) | Update the built-in source without an aish release |

Phases 1–2 are pure refactor-with-a-façade (zero user-visible change beyond a new
column/subcommand); Phase 3 is where plugins actually start contributing
search/add results.

---

## 8. Backward compatibility & risks

- **Zero-plugin installs are unchanged.** With no `skill_source` plugin present,
  the built-in in-process source answers exactly as today; `AISH_SKILL_REGISTRY`
  and the mcpmarket→embedded degradation are preserved.
- **Old binaries / old manifests interoperate.** `skill_source` is dropped by
  serde on old binaries; absent on old manifests. Same contract as every prior
  plugin phase.
- **Fail-soft search.** A per-source timeout + failures-become-`[]` guarantees one
  bad plugin can't hang or error the search — inheriting `search()`'s existing
  graceful-degradation posture.
- **Trust surface.** Skill-source handlers run local plugin code — identical to
  the already-accepted `login.sh` / `webhook_command` model. No *new* trust
  boundary; the existing "plugins execute local code by design" posture applies.
  (A future capability-gating/`:plugin trust` layer, if built, covers this and
  every other handler uniformly.)
- **Add-collision determinism.** `handles` + `priority` make ref routing
  deterministic (highest-priority matching source first, built-in catch-all
  last), so two plugins claiming overlapping namespaces resolve predictably.
- **Distribution still gated by `fly` §9.** This design makes the built-in source
  a plugin and lets *installed* plugins federate discovery; it does **not** add a
  way to *install* third-party skill-source plugins. Shape-B script plugins and
  Phase 4 third-party sources still land on disk by hand until the plugin registry
  (fly §9 / gap #8) exists. The two efforts are orthogonal and compounding: this
  is the *capability*, that is the *distribution*.

---

## 9. Recommendation

1. **Now:** land Phases 1–2 — parse `provides.skill_source`, add the shared
   `run_plugin_handler`, and ship the `skillfish` **façade** plugin (`@builtin`
   handlers). Reframes the built-in as a named, reorderable source with **no
   behavioral change** and gets `:skill sources` in front of users.
2. **Next (headline):** Phase 3 — search fan-out with a SOURCE column and
   priority-ordered `add` resolution. This is the moment plugins can actually
   *provide results* to `:skill search` / `:skill add`, exactly as asked.
3. **Then:** Phase 4 reference script plugin + docs, so enterprises/teams can wire
   a private skill catalog behind `${profile:*}` auth without touching aish.
4. **Reuse, don't reinvent:** every step generalizes the proven `provides.login`
   handler contract and the existing `merge_results` / `import` / `SearchResult`
   plumbing — the net-new code is a manifest field, a resolver, a shared exec
   helper, and a thin source façade.

---

## Appendix: source references

| Claim | Evidence |
|---|---|
| `:skill` routing (`add`/`search`/`list`/`remove`) | `src/repl.rs` `parse_skill_command`, `handle_skill_command` |
| Search source precedence (override → mcpmarket → embedded), fail-soft | `src/skill_provider.rs` `search`, `merge_results` |
| Add ref shapes (skill.fish / github / raw URL) + import/reload | `src/skill_provider.rs` `add`, `run_fetch`, `import` |
| `SearchResult` wire shape + alias soup + `ref_or_synth` dedup key | `src/skill_provider.rs` `SearchResult` |
| Plugin discovery, skills/schemas/`.mcp.json` contribution | `src/plugins.rs` `discover`, `plugin_skills`, `read_plugin_mcp` |
| Manifest `Provides` surface (`lifecycle_hooks`, `login`) | `src/plugins.rs` `PluginManifest`, `Provides` |
| `provides.login` handler contract (env in, JSON stdout, ETXTBSY retry, non-zero=err) | `src/plugin_auth.rs` `login_at`, `run_login_handler` |
| Distribution gap (no plugin registry; skills have one) | `docs/design/fly-plugin-integration.md` §9 |
| Tracking feature request (plugin distribution / skill-source federation) | FR-332 |
</content>
</invoke>
