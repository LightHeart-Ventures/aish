# Skill Registry Mirror — open, unauthenticated (design)

**Status:** proposed
**Sprint:** SPR — "Skill Registry Mirror — Open Drop-in Registry"
**Decision:** ship with **no auth**. The mirror is a public, read-only,
cacheable static catalog. Abuse control is *cost* control (edge cache + rate
limit), not identity.

---

## 1. Problem

`skill.fish` sits behind Vercel bot protection. Automated clients get
`HTTP 429 + x-vercel-mitigated: challenge` instead of a payload — aish already
detects this exact pairing (`skill_provider::is_vercel_challenge`) and prints
guidance, but `:skill search` against the live catalog is simply unavailable.
Today live/community search is delegated to the `npx-skillfish` plugin, which
has the same upstream dependency.

We want a **self-hosted mirror** that aish can point at with the existing
`AISH_SKILL_REGISTRY` override — no client changes required for it to work.

## 2. The contract (already fixed by the client)

`src/skill_provider.rs` defines the entire wire contract. The mirror is a
drop-in iff it satisfies these:

| # | Client code | Request | Response |
|---|---|---|---|
| 1 | `search_url_with_base` (`:784`) | `GET {base}/api/v1/search?q=<urlenc>&limit=50` | JSON: bare array **or** object keyed `results` \| `skills` \| `data` \| `items` \| `hits` |
| 2 | `raw_url` (`:225`) | `GET {base}/{owner}/{name}/raw[?version=<v>]` | `text/plain` raw `SKILL.md` (YAML frontmatter + body) |
| 3 | `check_url` (`:238`) | — | **https only**; `http://` allowed *only* for `localhost` / `127.0.0.1` |
| 4 | `parse_search_body` (`:836`) | — | unparsable rows are skipped; dedupe by `reference`; `[]` is valid |

`SearchResult` row shape:

```jsonc
{
  "name":        "git-helper",        // required
  "author":      "acme",
  "description": "…",
  "version":     "1.2.0",
  "reference":   "acme/git-helper",   // what :skill add consumes
  "stars":       42
}
```

A `file://…/index.json` base is ALSO supported by the client (read + filtered
in-process, `search_with_base` `:868`) — that is how the offline embedded index
works, and it is what the integration tests will exercise.

**Consequence:** the mirror needs *zero* client changes to be usable:

```bash
export AISH_SKILL_REGISTRY=https://skills.example.dev
:skill search rust
:skill add acme/git-helper
```

## 3. Architecture

```
          ┌───────────── nightly GitHub Action (cron) ─────────────┐
          │                                                         │
  GitHub repos ──crawl──▶ normalize ──▶ index.json + {owner}/{name}/raw
  (allowlist +            (frontmatter      │
   code search)            parse)           │ publish
                                            ▼
                              object store (R2 / S3)
                                            │
                                            ▼
                              edge (Worker / CloudFront)
                    ┌───────────────────────┴───────────────────────┐
                    │  GET /api/v1/search?q=&limit=  → ranked rows   │
                    │  GET /{owner}/{name}/raw       → SKILL.md      │
                    └───────────────────────────────────────────────┘
                                            ▲
                                    AISH_SKILL_REGISTRY
                                            │
                                          aish
```

### 3.1 Ingest (build-time, not request-time)

A Rust/TS generator walks an **allowlist** of GitHub repos plus an optional
code-search sweep for `SKILL.md`, parses each file's YAML frontmatter with the
SAME rules as `src/skills.rs::parse_frontmatter`, and emits:

- `index.json` — the full catalog (`SearchResult[]`), also directly consumable
  as a `file://` base.
- `{owner}/{name}/raw` — the verbatim `SKILL.md` bytes, one object per skill.

Rejects: missing `name:`/`description:`, non-`[A-Za-z0-9._-]` names (the same
path-traversal hardening the client applies), files > 256 KiB.

### 3.2 Serving

Static-first. Everything except `/api/v1/search` is a plain object read.
`/api/v1/search` is a tiny edge function doing substring + field-weighted
ranking over the in-memory index (index is small — thousands of rows, tens of
KB gzipped, cached at the edge).

### 3.3 Freshness

Nightly cron rebuild. `index.json` gets a short TTL (5 min); `raw` objects are
immutable-ish with a long TTL and content-hash invalidation.

## 4. Explicitly NOT in scope (operator decision)

- No auth: no OAuth device flow, no JWT, no API keys, no client attestation.
  A public HTTP endpoint cannot be restricted to "only aish clients" — any
  embedded secret is extractable — so we do not pretend otherwise.
- No write path. The mirror is read-only; publishing happens via the repo +
  CI, not an API.
- No signature verification (the `import` seam stays open for it later, per
  `docs/internals/skillfish-integration.md`).

## 5. Guardrails that survive "no auth"

These are **cost** controls, not access controls:

| Control | Where | Why |
|---|---|---|
| Edge cache (long TTL on `raw`, 5 min on index) | CDN | ~all traffic served from cache; origin egress ≈ 0 |
| Per-IP rate limit (e.g. 60 req/min) | edge rule | caps a runaway script |
| Bandwidth/request budget alarm | provider billing | fail loud, not expensive |
| `limit` clamped server-side (max 100) | search fn | bounds response size |

## 6. Client-side follow-on (optional, in-sprint stretch)

When `is_vercel_challenge` fires and `AISH_SKILL_REGISTRY` is unset, the error
message can name the public mirror as a ready-to-paste `export`. This is a
message change only — no default network behavior change, no auto-failover.

## 7. Test strategy

`skill_provider.rs` already spins a loopback HTTP server in its tests
(`http://127.0.0.1:{port}/acme/loopback-skill/raw`) and `check_url` exempts
loopback. The mirror's conformance suite reuses that shape:

1. serve a fixture `index.json` + one `raw` object from a temp dir,
2. point `AISH_SKILL_REGISTRY` at it,
3. assert `search` returns the fixture rows and `add` writes the `SKILL.md`.

Plus a `file://index.json` case (already covered by the existing default path).

## 8. Implementation map

| Piece | Location |
| --- | --- |
| Wire contract (search URL, raw URL, `check_url`) | `src/skill_provider.rs` |
| Frontmatter parse rules the ingest must mirror | `src/skills.rs::parse_frontmatter` |
| Vercel challenge detection + guidance text | `src/skill_provider.rs::vercel_challenge_message` |
| Existing plugin source this complements | `plugins/npx-skillfish/` |
| Mirror generator + edge fn + IaC | `tools/skill-mirror/` (new) |
