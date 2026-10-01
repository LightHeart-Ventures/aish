# skill-mirror edge search endpoint

`GET /api/v1/search?q=<urlencoded>&limit=50` over the mirror's `index.json`.

This is the **one piece of compute** the skills mirror needs — everything else
(`index.json`, raw `SKILL.md` fetches) is a static object read served straight
from S3 via CloudFront.

- Provider: **AWS S3 + CloudFront**, hostname **skills.aish.sh** (TASK-696).
- Deploy artifact: **Lambda@Edge, `nodejs20.x`, `us-east-1`, origin-request**.

## Layout

| File | Role |
|---|---|
| `src/search.mjs` | PURE scoring/ranking core. Zero AWS imports, zero I/O. |
| `handler.mjs` | Lambda@Edge origin-request handler: index cache + response shaping. |
| `tests/` | `node:test` suite, 27 tests (zero dependencies, no lockfile). |
| `../infra/search_function.tf` | Lambda, IAM role, cache policy, CloudFront wiring. |
| `../infra/variables_search.tf` | The knobs. |

## Why Lambda@Edge and not a CloudFront Function

A **CloudFront Function** is the cheaper, faster edge compute — and it is *not
viable here*:

| Constraint | CloudFront Function | What search needs |
|---|---|---|
| Network access | **none** | must read `index.json` from the S3 origin |
| Max runtime | 1 ms | JSON parse + scan over thousands of rows |
| Max code size | 10 KB | handler + scoring core |
| Runtime | restricted ES5.1-ish JS | ESM, `URLSearchParams`, AWS SDK |

No network access alone is fatal: the catalog cannot be fetched, and bundling it
into the function would make it stale on every publish *and* blow the 10 KB
limit. **Lambda@Edge** keeps the compute at the edge POP, can fetch from S3,
gets `nodejs20.x`, and holds the parsed index in module scope across warm
invocations.

The card's engineering spec recommended a **Cloudflare Worker**, following the
then-undecided TASK-696 provider choice. That decision was ratified as **AWS
S3 + CloudFront** on 2026-09-30 (closing `oq_d8412c5cc29c`), so the runtime
became Lambda@Edge. **The algorithm in `src/search.mjs` is unchanged** — the
spec explicitly anticipated this swap, and the scoring core has zero AWS
imports precisely so the runtime seam stays swappable.

## Request contract

```
GET /api/v1/search?q=<urlencoded>&limit=50
```

Exactly what the aish client's `search_url_with_base` emits (`limit` is
hardcoded to 50 client-side today; the endpoint does not assume that).

- `q` absent or empty/whitespace → the **whole catalog**, truncated to `limit`.
  This mirrors the client's own `skill_provider::filter_local`, which returns
  everything on an empty query.
- `limit` parsed as an int and **clamped server-side to `[1, 100]`** (cost
  control, TASK-700). Unparsable → default **50**.
- Unknown query params are ignored.

## Response contract

`200 application/json`, a **bare JSON array** of `SearchResult`:

```json
[{ "name": "rust", "author": "ferris", "description": "...",
   "version": "1.0.0", "reference": "ferris/rust", "stars": 12 }]
```

A bare array is the minimum payload, and the client's `parse_search_body`
accepts it via its `v.as_array()` fallback (wrapper keys
`results`/`skills`/`data`/`items`/`hits` are also accepted by the client — we
simply don't need one).

Headers: `content-type: application/json`, `cache-control: public, max-age=60`.

### An empty match set is `[]` with **200** — never 404

This is the single most important error case on the card. A 404 surfaces to the
aish user as `"…returned HTTP 404"` instead of "no results". The handler has
**no code path that returns 404 or 5xx**: a missing index, an unparsable index,
or a total S3 outage all still answer `200 []`.

### Never `x-vercel-mitigated`

The aish client special-cases `429` + `x-vercel-mitigated` into a
bot-challenge error message (that header pair is skill.fish's Vercel bot
protection). This endpoint never sets it. Throttling (TASK-700) is attached at
the route **in front of** this handler and answers `429` + `retry-after`, so a
throttled request never reaches the search code — and never gets that header.

## Scoring

Case-insensitive substring, field-weighted. Weights are **summed**:

| Field | Weight |
|---|---|
| `name` exact (`== q`) | 100 |
| `name` prefix | 50 |
| `name` substring | 30 |
| `reference` substring | 20 |
| `author` substring | 10 |
| `description` substring | 5 |

```
score = sum(matched weights) + log10(stars + 1) * 2
```

Because the weights sum, an exact name match necessarily scores
`100 + 50 + 30 = 180`, a prefix `80`, a bare substring `30` — so
**exact > prefix > substring** holds by construction.

`stars` is a **tiebreaker only**. `log10(100000 + 1) * 2 ≈ 10`, an order of
magnitude below the exact-name weight, so a wildly popular but irrelevant skill
can never outrank an exact name match. This is asserted in the tests.

Rows scoring **0 are excluded**. Sort by score desc, then `reference` asc for
determinism. Truncate to `limit`.

### Strict superset of `filter_local`

The matched fields are exactly the four `skill_provider::filter_local` matches
on (`name`, `reference`, `author`, `description`), so every row the offline
client would return scores `> 0` here. The remote path can never be *worse*
than offline. `tests/search.test.mjs` ports `filter_local` to JS and asserts
the property over a fixture catalog and a sample of queries (plus every
single-character query).

## Index caching

1. **Cold start**: conditional `GetObject` for `index.json` from the catalog
   bucket (AWS SDK v3 — part of the `nodejs20.x` Lambda runtime, *not* a bundled
   dependency; imported lazily so the tests stay dependency-free).
2. Parsed once and held in **module scope**, so it survives across invocations
   on a warm container.
3. **TTL 300 s** against a stored timestamp, refetched lazily past the TTL.
4. The stored S3 **`ETag`** is sent as `If-None-Match`, so a no-change refresh
   is a 304 and costs ~nothing.
5. **Stale-on-refetch-failure**: if a refetch fails we keep serving the stale
   index. Availability over freshness — a catalog 20 minutes out of date is
   vastly better than a 500. The timestamp is bumped on failure too, so a broken
   origin is not hammered once per request.

### Size ceiling

5,000 rows × ~200 B ≈ **1 MB** raw — trivial in Lambda's memory, and the
substring scan over it is `O(rows × fields)`, fine at thousands of rows. Past
roughly **10 MB** the right move is a prebuilt inverted index generated by
TASK-694 instead of a linear scan. **Explicitly out of scope** — documented so
the ceiling is known.

## Tests

Zero dependencies, zero lockfile — Node's built-in runner:

```sh
node --test "tools/skill-mirror/edge/tests/*.test.mjs"
```

(Node ≥22 takes glob/file positionals here, not a bare directory.)

Covers the spec's test plan:

1. exact name beats prefix beats substring
2. stars break ties but never override an exact name match
3. empty `q` → whole catalog, truncated to `limit`
4. `limit=9999` → clamped to 100; `limit=abc` → 50
5. no matches → `[]` with status **200**
6. **superset property** vs a JS port of `filter_local`

plus the index cache (TTL, conditional ETag, 304, stale-on-failure,
don't-hammer-a-broken-origin), header hygiene (never `x-vercel-mitigated`), and
malformed-row robustness.

## Deploy

`../infra/search_function.tf` is **stacked on TASK-696**, which owns
`main.tf` / `providers.tf` / `s3.tf` / `cloudfront.tf` / `variables.tf` /
`outputs.tf`. Until that lands, `terraform validate` in `../infra` fails because
the resources this file references do not exist yet. `terraform fmt -check` is
clean.

One deliberate integration seam: the `ordered_cache_behavior` for
`/api/v1/search*` must live **inside** TASK-696's
`aws_cloudfront_distribution` resource, which is in *their* file. This module
exposes it as `local.search_ordered_cache_behavior` plus the
`search_ordered_cache_behavior` output, so wiring it up is a one-line
`dynamic "ordered_cache_behavior"` block in `cloudfront.tf` — see the WIRING
comment at the bottom of `search_function.tf`.
