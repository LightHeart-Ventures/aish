# Architecture review — vercel-labs/skills, and why a JSON-first fork

Reviewed at upstream `v1.5.15` (`https://github.com/vercel-labs/skills`), the
CLI behind [`skills.sh`](https://skills.sh) / `npx skills`. Reviewed against the
usual axes: correctness, security, operability, cost, and fit-for-reuse.

## What it is

A TypeScript ESM CLI (`bin/cli.mjs` → `src/cli.ts`) that installs, lists, finds,
syncs, updates, and removes **agent skills** (`SKILL.md` files: YAML frontmatter
+ markdown body) across **70+ agents** (Claude Code, Cursor, Codex, Gemini CLI,
Windsurf, Zed, …). Sources: GitHub, GitLab, generic git, local paths, and
well-known registries. ~35 `src/*.ts` modules; heavy hitters are `add.ts` (73 KB),
`installer.ts` (42 KB), `update.ts` / `use.ts` (~20 KB each).

## Strengths

- **Clean skill data model.** `Skill { name, description, path, rawContent?,
  pluginName?, metadata? }` (`src/types.ts`) is small and sufficient; `name` +
  `description` are the required contract.
- **Security-conscious frontmatter parser.** `src/frontmatter.ts` deliberately
  supports *only* the `---` YAML delimiter and refuses `---js` / `---javascript`
  to avoid the `eval()`-based RCE that ships in gray-matter's JS engine. Good call.
- **Robust discovery.** `findSkillDirs` walks in parallel, skips
  `node_modules/.git/dist/build/__pycache__`, is depth-bounded, and validates
  subpaths against path-traversal (`isSubpathSafe`). Fail-soft `try/catch` around
  every FS op.
- **Broad interop.** The agent matrix and multiple source providers make it a
  genuine ecosystem hub, not just a Claude tool.
- **Sane defaults.** Internal skills hidden unless `INSTALL_INTERNAL_SKILLS=1`;
  local lockfiles; metadata sanitization (`sanitize.ts`).

## Weaknesses / friction (from a *reuse* standpoint)

1. **Human-first output.** Most commands print ANSI-colored, truncated,
   logo-branded text to stdout (`list.ts`, `use.ts`, `cli.ts` carry 256-color
   escapes and a big ASCII logo). Great for humans, hostile to programs.
2. **`--json` is partial and installed-scoped.** Only `list` has `--json`, and it
   reports *installed* skills grouped by agent (`{name, path, scope, agents}`) —
   it can **not** parse an arbitrary directory tree into a catalog, and `find` /
   `use` have no structured mode at all.
3. **Not consumable as a library.** The parsing/discovery core (`skills.ts`,
   `frontmatter.ts`) is excellent but only reachable by running the whole CLI,
   which does telemetry init (`initTelemetry`), agent detection, and network
   fetches on many paths.
4. **Heavy to vendor.** `pnpm`, `obuild`, `vitest`, a `yaml` runtime dep, license
   generation — a lot of surface for a downstream that just wants "SKILL.md → JSON".
5. **Telemetry on by default** (`telemetry.ts`) — undesirable for an embedded/CI
   use.

None of these are *bugs* — they're the correct trade-offs for a polished
end-user CLI. They're simply the wrong trade-offs for **infrastructure that
other tools build on**, which is what aish needs.

## Decision: fork the core, JSON-first, zero-dep

Rather than (a) shell out to `npx skills` and screen-scrape formatted output, or
(b) vendor the whole TS monorepo and bolt `--json` onto three more commands, this
plugin **reimplements the discovery + parse core** (~300 lines, one file, no
deps) with a single contract: **structured JSON on stdout, nothing else.**

Rationale:

- **Reversible & additive** — a new read-only tool; it changes nothing upstream
  and nothing in aish's existing `npx-skills` skill-source plugin (which wraps
  install/search — a different job).
- **Faithful** — same `Skill` fields, same required-string validation, same
  security posture (YAML-delimiter-only frontmatter, `SKIP_DIRS`,
  `metadata.internal` gating). See the mapping table in [README.md](./README.md).
- **Lean/operable** — Node ≥18 only; no `npm install`, no `node_modules`, no
  build, no network, no telemetry. Trivial to run in CI or from a coordinator.
- **The missing library** — turns "list installed skills for a human" into "parse
  *any* skill tree into a catalog for a machine": the enabling primitive for
  tool-use routing, IDE/Atum integration, and skill-catalog diffing.

### Cost / risk

- Cost: negligible — one process spawn, pure FS reads, no network.
- Risk: the only reimplemented-from-scratch piece is a **minimal YAML subset
  parser** (upstream uses the full `yaml` package). It covers real SKILL.md
  frontmatter (flat scalars, one nested map for `metadata`, simple lists) and
  degrades to a string rather than throwing. If a skill ever needs exotic YAML in
  frontmatter, swap `parseYamlLite` for a vendored `yaml` — the seam is one
  function. Documented as a known limitation.

## Recommendation

Ship the fork as this plugin. If upstream later exposes its core as a published
library with a `--json` on `find`/`use` that can target arbitrary trees, revisit
and consider depending on it directly — until then, this lean fork is the right
call for programmatic use.
