# skill.fish integration (opt-in)

[skill.fish](https://skill.fish) is a community marketplace for AI-agent skills.
This document describes how aish optionally fetches and imports skills from it.

## Background

aish already has a **skills** system (`src/skills.rs`): a skill is a `SKILL.md`
file with YAML frontmatter (`name:`, `description:`) and a markdown body of
instructions. Skills come from two sources today:

- **Local** — `~/.aish/skills/<name>/SKILL.md`, scanned at startup.
- **MCP** — connected servers publish skills as MCP prompts; the model expands
  one on demand with `get_skill`.

skill.fish is a **third source**: a public catalog of `SKILL.md` files. The
integration is the smallest possible bridge — it turns a skill.fish reference
into a local `SKILL.md`, after which the existing **Local** path takes over.
Nothing about how skills are advertised, selected, or run changes.

## How it works

```
ref ──parse──▶ SkillRef ──raw_url──▶ HTTPS GET ──validate──▶ write to
 (url/shorthand)                      (registry)   (frontmatter)   ~/.aish/skills/<name>/SKILL.md
```

1. **Reference** — a full URL `https://skill.fish/<owner>/<name>[@<version>]`
   or the shorthand `<owner>/<name>[@<version>]`.
2. **Discovery** — the registry origin defaults to `https://skill.fish` and is
   overridable with `AISH_SKILLFISH_REGISTRY` (self-hosted mirrors, testing).
3. **Fetch** — `GET {registry}/{owner}/{name}/raw[?version=…]` returns the raw
   `SKILL.md`. The request carries an `aish/<version>` user-agent and a 20s
   timeout.
4. **Import** — the body is parsed as a `SKILL.md`; the frontmatter `name:`
   becomes the on-disk directory under `~/.aish/skills/`, and the file is
   written there verbatim. On the next launch it shows up in the catalog.

## Opt-in mechanism

The integration is **never** automatic. A skill arrives only when the user runs
the explicit command:

```
aish --skill-fetch https://skill.fish/acme/git-helper
aish --skill-fetch acme/git-helper@1.2.0          # shorthand + version pin
```

`--skill-fetch` short-circuits startup: it needs no backend and no credentials,
fetches+imports, prints a one-line result, and exits. There is no background
polling, no auto-update of imported skills, and no network call on a normal
launch.

The same capability is available to the **model** mid-session through the atum
MCP server's `atum_import_skill` tool, which already understands skill.fish
references. The CLI flag is the user-driven counterpart; both land a `SKILL.md`
in the same local catalog, so the two paths stay consistent.

## Security considerations

- **HTTPS only.** `check_url` refuses any non-`https://` origin. The sole
  exception is a loopback host (`localhost`/`127.0.0.1`), used by self-hosted
  mirrors and the integration tests.
- **Path-traversal hardening.** Both the reference segments *and* the
  frontmatter `name:` taken from the (untrusted) response are validated to
  `[A-Za-z0-9._-]` with no `/`, `\`, `.` or `..` — a malicious `name:` cannot
  escape `~/.aish/skills/`.
- **Data, not code.** A `SKILL.md` is plain instructions. aish never executes
  it; it only advertises the skill to the model, which reads it like any other
  file. Importing one therefore cannot, by itself, run anything.
- **Explicit, auditable installs.** Skills land as files on disk the user can
  read, diff, and delete. No silent updates.
- **Signature verification (future).** The registry can publish a detached
  signature alongside each skill; a later revision can verify it before writing.
  The validation seam (`import`) is where that check would slot in.

## User experience

- **Install** — `aish --skill-fetch <ref>`; prints the imported name,
  description, and on-disk path.
- **Version pinning** — `<owner>/<name>@<version>` forwards `?version=` to the
  registry, so a pinned skill is reproducible.
- **Search/discovery** — deferred to the skill.fish website for now: a user
  browses the catalog there and pastes a reference. A future `aish --skill-search
  <query>` can hit a registry search endpoint and print matches; the parsing and
  fetch plumbing here is the foundation for it.
- **Where it lands** — `~/.aish/skills/<name>/SKILL.md`, the same place a
  hand-authored skill lives, so it participates in the catalog with zero extra
  wiring.

## Implementation map

| Piece | Location |
| --- | --- |
| Reference parsing, fetch, validation, import | `src/skillfish.rs` |
| Shared `SKILL.md` frontmatter parser | `src/skills.rs::parse_frontmatter` |
| CLI flag `--skill-fetch` + dispatch | `src/main.rs` |
| Tests (parse, url, security, fetch→import flow) | `src/skillfish.rs` `#[cfg(test)]` |
