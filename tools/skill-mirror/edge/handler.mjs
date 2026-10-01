// Lambda@Edge (origin-request) handler for the skill-mirror search endpoint.
//
// Why Lambda@Edge and not a CloudFront Function: a CloudFront Function has NO
// network access and hard 10 KB / 1 ms limits, so it cannot fetch index.json
// from the S3 origin. Lambda@Edge (nodejs20.x, us-east-1) can, and still runs
// at the edge POP. See README.md for the full rationale.
//
// Behavioural guarantees (these are the acceptance criteria, in code):
//   * 200 application/json, BARE array of SearchResult.
//   * An empty match set is `[]` with 200 — NEVER 404. A 404 surfaces to the
//     aish user as "...returned HTTP 404" instead of "no results".
//   * Never sets `x-vercel-mitigated` (the aish client special-cases
//     429 + that header into a bot-challenge error).
//   * `cache-control: public, max-age=60`.
//
// Index caching: module scope, so it survives across invocations on a warm
// container. TTL 300 s, lazy refetch, conditional on the S3 ETag so a
// no-change refresh is ~free. If a refetch FAILS we keep serving the STALE
// index — availability over freshness.

import { searchCatalog, parseQuery } from './src/search.mjs';

/** Index TTL in milliseconds. */
export const INDEX_TTL_MS = 300_000;

const BUCKET = process.env.CATALOG_BUCKET ?? '';
const KEY = process.env.CATALOG_KEY ?? 'index.json';

// ---------------------------------------------------------------------------
// Module-scope cache — survives warm invocations.
// ---------------------------------------------------------------------------
const cache = {
  rows: null,
  etag: null,
  fetchedAt: 0,
};

/** Test seam: reset the module-scope cache. */
export function __resetCache() {
  cache.rows = null;
  cache.etag = null;
  cache.fetchedAt = 0;
}

/** Test seam: inspect the module-scope cache. */
export function __peekCache() {
  return { ...cache };
}

/**
 * Normalize an index.json payload into an array of rows. Accepts a bare array
 * or any of the wrapper keys the aish client's `parse_search_body` accepts, so
 * the generator (TASK-694) is free to emit either shape.
 */
export function normalizeIndex(parsed) {
  if (Array.isArray(parsed)) return parsed;
  if (parsed && typeof parsed === 'object') {
    for (const key of ['results', 'skills', 'data', 'items', 'hits']) {
      if (Array.isArray(parsed[key])) return parsed[key];
    }
  }
  return [];
}

/**
 * Default index fetcher: a conditional S3 GetObject against the catalog
 * bucket. The AWS SDK v3 is part of the nodejs20.x Lambda runtime, so this is
 * NOT a bundled dependency — hence the lazy dynamic import, which also keeps
 * `node --test` dependency-free (tests inject their own fetcher).
 *
 * Returns `{ notModified: true }` when the stored ETag still matches.
 */
export async function s3IndexFetcher({ bucket, key, etag }) {
  const { S3Client, GetObjectCommand } = await import('@aws-sdk/client-s3');
  const client = new S3Client({ region: process.env.CATALOG_REGION ?? 'us-east-1' });
  try {
    const res = await client.send(
      new GetObjectCommand({
        Bucket: bucket,
        Key: key,
        ...(etag ? { IfNoneMatch: etag } : {}),
      }),
    );
    return { body: await res.Body.transformToString(), etag: res.ETag ?? null };
  } catch (err) {
    // A conditional GET that still matches comes back as 304 NotModified.
    const status = err?.$metadata?.httpStatusCode;
    if (status === 304 || err?.name === 'NotModified') return { notModified: true };
    throw err;
  }
}

/**
 * Load the catalog, honouring the module cache, the 300 s TTL and the
 * conditional-ETag refetch. On refetch failure the STALE rows are returned
 * (and the timestamp is bumped so we don't hammer a broken origin on every
 * request). Returns `[]` only when we have never successfully loaded.
 */
export async function loadCatalog({
  fetcher = s3IndexFetcher,
  bucket = BUCKET,
  key = KEY,
  now = Date.now(),
  ttlMs = INDEX_TTL_MS,
} = {}) {
  const fresh = cache.rows !== null && now - cache.fetchedAt < ttlMs;
  if (fresh) return cache.rows;

  try {
    const res = await fetcher({ bucket, key, etag: cache.etag });
    if (res?.notModified) {
      // No change at the origin — the cheapest possible refresh.
      cache.fetchedAt = now;
      return cache.rows ?? [];
    }
    const rows = normalizeIndex(JSON.parse(res.body));
    cache.rows = rows;
    cache.etag = res.etag ?? null;
    cache.fetchedAt = now;
    return rows;
  } catch (err) {
    // STALE-ON-FAILURE: availability over freshness. A catalog 20 minutes out
    // of date beats a 500.
    cache.fetchedAt = now;
    if (cache.rows !== null) {
      console.warn('search: index refetch failed, serving stale index:', err?.message ?? err);
      return cache.rows;
    }
    console.error('search: index fetch failed with no cached index:', err?.message ?? err);
    return [];
  }
}

/**
 * Build the Lambda@Edge response object. Always 200; the body is always a
 * bare JSON array.
 */
export function jsonResponse(rows) {
  return {
    status: '200',
    statusDescription: 'OK',
    headers: {
      'content-type': [{ key: 'Content-Type', value: 'application/json' }],
      'cache-control': [{ key: 'Cache-Control', value: 'public, max-age=60' }],
    },
    body: JSON.stringify(rows),
  };
}

/**
 * Factory so tests can inject a fetcher and a clock without touching AWS.
 * Returns a standard Lambda@Edge origin-request handler.
 */
export function makeHandler(deps = {}) {
  return async function handler(event) {
    const request = event?.Records?.[0]?.cf?.request ?? {};
    const { q, limit } = parseQuery(request.querystring ?? '');
    const catalog = await loadCatalog({ ...deps, now: deps.now?.() ?? Date.now() });
    return jsonResponse(searchCatalog(catalog, { q, limit }));
  };
}

/** The deployed entrypoint. */
export const handler = makeHandler();
