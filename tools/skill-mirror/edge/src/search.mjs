// Pure scoring/ranking core for the skill-mirror search endpoint.
//
// ZERO AWS imports, zero I/O, zero globals. Everything in here is a pure
// function of its arguments so the runtime seam (Lambda@Edge today, a
// Cloudflare Worker or a plain Node server tomorrow) stays swappable and the
// algorithm stays unit-testable with `node --test`.
//
// Contract:  GET /api/v1/search?q=<urlencoded>&limit=50
// Response:  a BARE JSON array of SearchResult rows (see ../README.md).

/** Client-side default when `limit` is absent or unparsable. */
export const DEFAULT_LIMIT = 50;
/** Hard server-side ceiling — cost control (TASK-700). */
export const MAX_LIMIT = 100;
/** Hard server-side floor. */
export const MIN_LIMIT = 1;

/**
 * Field weights. A match is a case-insensitive substring test, except
 * `nameExact` (full equality) and `namePrefix` (startsWith).
 *
 * Weights are SUMMED, so an exact name match necessarily also scores the
 * prefix and substring weights (100 + 50 + 30 = 180). That is intentional:
 * it makes exact > prefix > substring true by construction, with a margin
 * far larger than the stars tiebreaker can ever close.
 */
export const WEIGHTS = Object.freeze({
  nameExact: 100,
  namePrefix: 50,
  nameSubstring: 30,
  reference: 20,
  author: 10,
  description: 5,
});

/** Multiplier on the log10(stars + 1) popularity tiebreaker. */
export const STARS_MULTIPLIER = 2;

function lower(value) {
  return typeof value === 'string' ? value.toLowerCase() : '';
}

/**
 * Clamp a raw `limit` query-string value to [MIN_LIMIT, MAX_LIMIT].
 * Unparsable / missing / non-finite → DEFAULT_LIMIT.
 */
export function normalizeLimit(raw) {
  if (raw === undefined || raw === null || raw === '') return DEFAULT_LIMIT;
  // parseInt deliberately: "50abc" is a best-effort 50, "abc" is NaN.
  const n = Number.parseInt(String(raw), 10);
  if (!Number.isFinite(n)) return DEFAULT_LIMIT;
  if (n < MIN_LIMIT) return MIN_LIMIT;
  if (n > MAX_LIMIT) return MAX_LIMIT;
  return n;
}

/**
 * Parse the query string of a request into the normalized `{ q, limit }` pair.
 * Accepts a raw query string ("q=rust&limit=10", with or without a leading
 * "?"), a URLSearchParams, or a plain object. Unknown params are ignored.
 */
export function parseQuery(input) {
  let params;
  if (input instanceof URLSearchParams) {
    params = input;
  } else if (typeof input === 'string') {
    params = new URLSearchParams(input.startsWith('?') ? input.slice(1) : input);
  } else if (input && typeof input === 'object') {
    params = new URLSearchParams(
      Object.entries(input).map(([k, v]) => [k, v === undefined || v === null ? '' : String(v)]),
    );
  } else {
    params = new URLSearchParams();
  }
  return {
    q: (params.get('q') ?? '').trim(),
    limit: normalizeLimit(params.get('limit')),
  };
}

/**
 * The popularity tiebreaker: log10(stars + 1) * STARS_MULTIPLIER.
 * Bounded in practice at ~10 (100k stars), i.e. an order of magnitude below
 * the exact-name weight, so a popular irrelevant row can never outrank an
 * exact name match.
 */
export function starsBonus(stars) {
  const s = Number(stars);
  if (!Number.isFinite(s) || s <= 0) return 0;
  return Math.log10(s + 1) * STARS_MULTIPLIER;
}

/**
 * Score one catalog row against an already-lowercased, non-empty query.
 * Returns 0 when nothing matched (caller excludes those rows).
 */
export function scoreRow(row, q) {
  if (!q) return 0;
  let score = 0;

  const name = lower(row?.name);
  if (name) {
    if (name === q) score += WEIGHTS.nameExact;
    if (name.startsWith(q)) score += WEIGHTS.namePrefix;
    if (name.includes(q)) score += WEIGHTS.nameSubstring;
  }
  if (lower(row?.reference).includes(q)) score += WEIGHTS.reference;
  if (lower(row?.author).includes(q)) score += WEIGHTS.author;
  if (lower(row?.description).includes(q)) score += WEIGHTS.description;

  if (score === 0) return 0;
  return score + starsBonus(row?.stars);
}

function referenceKey(row) {
  // Mirrors the client's SearchResult::ref_or_synth dedup/sort key closely
  // enough for a deterministic tiebreak.
  const r = typeof row?.reference === 'string' ? row.reference.trim() : '';
  if (r) return r;
  const author = typeof row?.author === 'string' ? row.author : '';
  const name = typeof row?.name === 'string' ? row.name : '';
  if (author && name) return `${author}/${name}`;
  return name;
}

/**
 * Rank `catalog` against `{ q, limit }` and return the response rows.
 *
 * - empty / whitespace-only `q` → the WHOLE catalog truncated to `limit`
 *   (mirrors the aish client's `filter_local`, which returns everything on an
 *   empty query).
 * - rows scoring 0 are EXCLUDED.
 * - sort: score desc, then `reference` asc for determinism.
 * - truncated to `limit`.
 *
 * Always returns an array — never null, never an error, never a 404 signal.
 */
export function searchCatalog(catalog, { q = '', limit = DEFAULT_LIMIT } = {}) {
  const rows = Array.isArray(catalog) ? catalog : [];
  const effectiveLimit = normalizeLimit(limit);
  const needle = String(q ?? '').trim().toLowerCase();

  if (!needle) return rows.slice(0, effectiveLimit);

  const scored = [];
  for (const row of rows) {
    const score = scoreRow(row, needle);
    if (score > 0) scored.push({ row, score, key: referenceKey(row) });
  }

  scored.sort((a, b) => {
    if (b.score !== a.score) return b.score - a.score;
    return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
  });

  return scored.slice(0, effectiveLimit).map((s) => s.row);
}

/**
 * One-shot convenience: query string in, response rows out.
 * `search('q=rust&limit=10', catalog)`.
 */
export function search(queryString, catalog) {
  return searchCatalog(catalog, parseQuery(queryString));
}
