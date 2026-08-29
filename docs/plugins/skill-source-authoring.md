# Authoring a skill-source plugin (`provides.skill_source`)

A **skill source** is a plugin that joins the `:skill search` fan-out and the
`:skill add` priority-resolution — letting a plugin contribute live search
results and own a `:skill add <ref>` namespace **without recompiling aish**.

This guide is the practical companion to the design
(`docs/design/plugin-skill-sources.md`). The bundled
[`plugins/npx-skillfish`](../../plugins/npx-skillfish) is a complete, working
reference implementation.

---

## 1. Declare the source in `plugin.json`

Add a `skill_source` block to your manifest's `provides`:

```json
{
  "id": "acme-catalog",
  "name": "ACME private skill catalog",
  "version": "1.0.0",
  "provides": {
    "skill_source": {
      "id": "acme",
      "priority": 120,
      "search": "search.sh",
      "add": "add.sh",
      "handles": ["acme:*", "acme/*"]
    }
  }
}
```

| Field | Meaning | Default |
|---|---|---|
| `id` | Label shown in the SOURCE column and `:skill sources` | owning plugin id |
| `priority` | Merge/precedence rank — higher wins dedup ties and is tried first on `add` | `0` |
| `search` | Handler script answering `:skill search`; omit for an add-only source | — |
| `add` | Handler script resolving `:skill add`; omit for a search-only source | — |
| `handles` | Glob/prefix patterns of `reference` namespaces this source claims for add-routing | `[]` |

The built-in `skillfish` façade sits at **priority 100**. Rank above it to take
precedence, below it to act as a supplementary source.

`handles` globs decide which source a `:skill add <ref>` is offered to first.
Use a **namespaced** prefix (`acme:*`) so you don't shadow the built-in
`owner/name` and `github:*` refs unless you mean to.

---

## 2. Write the `search.sh` handler

Contract (identical env/stdout shape as `login.sh`):

- **env in:** `AISH_SKILL_QUERY`, `AISH_SKILL_LIMIT`, `AISH_PLUGIN_ID`,
  `AISH_TENANT_ID`, `AISH_CREDENTIALS_FILE` (read `${profile:<id>}` for authed
  catalogs).
- **stdout:** a JSON array of `SearchResult` objects. Field names (all optional,
  aliases accepted): `name`, `author` (or `owner`), `description` (or
  `summary`), `version`, `reference` (or `ref`/`slug`/`id`), `stars` (or
  `github_stars`).
- **exit:** non-zero or non-JSON is treated as **no results** in the fan-out —
  search never fails because one source misbehaved. **Be fail-soft:** print `[]`
  and exit 0 on any internal error.

```bash
#!/usr/bin/env bash
set -uo pipefail
q="${AISH_SKILL_QUERY:-}"
[ -z "$q" ] && { echo '[]'; exit 0; }
# ... query your catalog, map to the SearchResult shape ...
printf '[{"name":"demo","author":"acme","description":"A demo skill","reference":"acme:demo","stars":0}]\n'
```

---

## 3. Write the `add.sh` handler

Contract:

- **env in:** `AISH_SKILL_REF`, `AISH_SKILLS_DIR`, `AISH_PLUGIN_ID`,
  `AISH_CREDENTIALS_FILE`.
- **stdout:** EITHER the raw SKILL.md text (single skill) OR a JSON array of
  `{ "path": "<name>", "content": "<SKILL.md text>" }` (multi-skill import).
- **exit:** non-zero **surfaces** to the user (unlike search, `add` is a
  resolution the user explicitly requested). Be fail-loud with a clear stderr
  message.

The REPL owns writing the returned SKILL.md(s) via `skill_provider::import` and
reloading the catalog — your handler only **produces** content.

```bash
#!/usr/bin/env bash
set -euo pipefail
ref="${AISH_SKILL_REF:?}"
spec="${ref#acme:}"
# ... fetch the SKILL.md for $spec ...
cat "/path/to/${spec}/SKILL.md"   # single-skill: raw SKILL.md on stdout
```

---

## 4. How the shell uses your source

- **`:skill search <q>`** fans out across the built-in source + every discovered
  `skill_source` plugin **in parallel**, each bounded by a per-source timeout;
  results merge/dedupe on the `owner/name` reference and render with a **SOURCE**
  column. A slow/broken source contributes `[]`.
- **`:skill add <ref>`** collects the sources whose `handles` match `<ref>`,
  ordered by `priority` desc, and tries each until one returns a SKILL.md;
  otherwise it falls through to the built-in resolver.
- **`:skill sources`** lists every registered source (built-in + plugins) with
  its `id`, `priority`, and whether it answers `search`/`add`.

---

## 5. Checklist

- [ ] `provides.skill_source` block with a stable `id` and deliberate `priority`.
- [ ] `handles` globs use a namespaced prefix you own.
- [ ] `search.sh` is **fail-soft** (`[]` + exit 0 on error), emits valid JSON.
- [ ] `add.sh` is **fail-loud**, emits raw SKILL.md or `{path,content}[]`.
- [ ] Both scripts are executable (`chmod +x`).
- [ ] Test end-to-end: `:skill sources`, `:skill search`, `:skill add`.

See [`plugins/npx-skillfish`](../../plugins/npx-skillfish) for a full working example.
