// Unit tests for the Lambda@Edge handler: response shape, the 200-not-404
// guarantee, header hygiene, and the index cache / TTL / stale-on-failure
// policy. No network, no AWS SDK — the fetcher is injected.
import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import {
  makeHandler,
  loadCatalog,
  normalizeIndex,
  jsonResponse,
  __resetCache,
  __peekCache,
  INDEX_TTL_MS,
} from '../handler.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const CATALOG = JSON.parse(readFileSync(join(here, 'fixtures', 'catalog.json'), 'utf8'));

beforeEach(() => __resetCache());

/** Minimal CloudFront origin-request event. */
function cfEvent(querystring) {
  return {
    Records: [{ cf: { request: { uri: '/api/v1/search', querystring, method: 'GET' } } }],
  };
}

function okFetcher(rows, etag = '"v1"') {
  const fn = async () => {
    fn.calls += 1;
    return { body: JSON.stringify(rows), etag };
  };
  fn.calls = 0;
  return fn;
}

const headerValue = (res, name) => res.headers[name]?.[0]?.value;

// ---------------------------------------------------------------------------
// Response contract
// ---------------------------------------------------------------------------
test('returns 200 application/json with a BARE array', async () => {
  const handler = makeHandler({ fetcher: okFetcher(CATALOG) });
  const res = await handler(cfEvent('q=rust&limit=50'));

  assert.equal(res.status, '200');
  assert.equal(headerValue(res, 'content-type'), 'application/json');
  assert.equal(headerValue(res, 'cache-control'), 'public, max-age=60');

  const body = JSON.parse(res.body);
  assert.ok(Array.isArray(body), 'body must be a bare array, not a wrapper object');
  assert.equal(body[0].reference, 'ferris/rust');
});

// Test plan #5 — the single most important error case on this card.
test('no matches returns [] with status 200, NEVER 404', async () => {
  const handler = makeHandler({ fetcher: okFetcher(CATALOG) });
  const res = await handler(cfEvent('q=zzz-definitely-no-such-skill'));
  assert.equal(res.status, '200');
  assert.notEqual(res.status, '404');
  assert.deepEqual(JSON.parse(res.body), []);
});

test('an empty catalog still returns [] with 200, never 404', async () => {
  const handler = makeHandler({ fetcher: okFetcher([]) });
  const res = await handler(cfEvent('q=rust'));
  assert.equal(res.status, '200');
  assert.deepEqual(JSON.parse(res.body), []);
});

test('a total index failure degrades to [] with 200, never a 5xx', async () => {
  const handler = makeHandler({
    fetcher: async () => {
      throw new Error('S3 is on fire');
    },
  });
  const res = await handler(cfEvent('q=rust'));
  assert.equal(res.status, '200');
  assert.deepEqual(JSON.parse(res.body), []);
});

test('never emits x-vercel-mitigated (the client reads it as a bot challenge)', async () => {
  const handler = makeHandler({ fetcher: okFetcher(CATALOG) });
  for (const qs of ['q=rust', '', 'q=zzz', 'q=&limit=9999']) {
    const res = await handler(cfEvent(qs));
    const names = Object.keys(res.headers).map((k) => k.toLowerCase());
    assert.ok(!names.includes('x-vercel-mitigated'), `x-vercel-mitigated leaked for qs=${qs}`);
    assert.notEqual(res.status, '429');
  }
  const bare = jsonResponse([]);
  assert.ok(!Object.keys(bare.headers).some((k) => k.toLowerCase() === 'x-vercel-mitigated'));
});

test('empty querystring returns the whole catalog truncated to the default limit', async () => {
  const handler = makeHandler({ fetcher: okFetcher(CATALOG) });
  const res = await handler(cfEvent(''));
  assert.deepEqual(JSON.parse(res.body).length, CATALOG.length); // fixture < 50
});

test('limit is clamped server-side through the handler', async () => {
  const big = Array.from({ length: 500 }, (_, i) => ({
    name: `rust-${i}`,
    reference: `o/rust-${String(i).padStart(4, '0')}`,
    stars: 0,
  }));
  const handler = makeHandler({ fetcher: okFetcher(big) });
  assert.equal(JSON.parse((await handler(cfEvent('q=rust&limit=9999'))).body).length, 100);
  assert.equal(JSON.parse((await handler(cfEvent('q=rust&limit=abc'))).body).length, 50);
  assert.equal(JSON.parse((await handler(cfEvent('limit=0'))).body).length, 1);
});

// ---------------------------------------------------------------------------
// Index cache: module scope, 300 s TTL, conditional ETag, stale-on-failure
// ---------------------------------------------------------------------------
test('index TTL is 300 seconds', () => {
  assert.equal(INDEX_TTL_MS, 300_000);
});

test('the index is fetched once on cold start and reused while warm', async () => {
  const fetcher = okFetcher(CATALOG);
  const t0 = 1_000_000;
  assert.equal((await loadCatalog({ fetcher, now: t0 })).length, CATALOG.length);
  await loadCatalog({ fetcher, now: t0 + 1 });
  await loadCatalog({ fetcher, now: t0 + INDEX_TTL_MS - 1 });
  assert.equal(fetcher.calls, 1, 'must not refetch inside the TTL');
});

test('past the TTL it refetches lazily, passing the stored ETag', async () => {
  const seen = [];
  const fetcher = async ({ etag }) => {
    seen.push(etag);
    return { body: JSON.stringify(CATALOG), etag: '"v1"' };
  };
  const t0 = 2_000_000;
  await loadCatalog({ fetcher, now: t0 });
  await loadCatalog({ fetcher, now: t0 + INDEX_TTL_MS + 1 });
  assert.deepEqual(seen, [null, '"v1"'], 'second call must send If-None-Match');
});

test('a notModified refetch keeps the cached rows and costs nothing', async () => {
  let call = 0;
  const fetcher = async () => {
    call += 1;
    return call === 1 ? { body: JSON.stringify(CATALOG), etag: '"v1"' } : { notModified: true };
  };
  const t0 = 3_000_000;
  await loadCatalog({ fetcher, now: t0 });
  const rows = await loadCatalog({ fetcher, now: t0 + INDEX_TTL_MS + 1 });
  assert.equal(rows.length, CATALOG.length);
  assert.equal(__peekCache().etag, '"v1"');
});

test('STALE-ON-FAILURE: a failed refetch keeps serving the stale index', async () => {
  let call = 0;
  const fetcher = async () => {
    call += 1;
    if (call === 1) return { body: JSON.stringify(CATALOG), etag: '"v1"' };
    throw new Error('origin 503');
  };
  const t0 = 4_000_000;
  await loadCatalog({ fetcher, now: t0 });
  const stale = await loadCatalog({ fetcher, now: t0 + INDEX_TTL_MS + 1 });
  assert.equal(stale.length, CATALOG.length, 'must serve the stale index, not throw or empty out');

  // and the request path still answers 200 with real results
  const handler = makeHandler({ fetcher, now: () => t0 + INDEX_TTL_MS * 4 });
  const res = await handler(cfEvent('q=rust'));
  assert.equal(res.status, '200');
  assert.ok(JSON.parse(res.body).length > 0);
});

test('a failed refetch bumps the timestamp so a broken origin is not hammered', async () => {
  let fetches = 0;
  const fetcher = async () => {
    fetches += 1;
    if (fetches === 1) return { body: JSON.stringify(CATALOG), etag: '"v1"' };
    throw new Error('origin 503');
  };
  const t0 = 5_000_000;
  await loadCatalog({ fetcher, now: t0 });
  await loadCatalog({ fetcher, now: t0 + INDEX_TTL_MS + 1 }); // fails -> stale
  await loadCatalog({ fetcher, now: t0 + INDEX_TTL_MS + 2 }); // inside new TTL, no fetch
  assert.equal(fetches, 2);
});

test('malformed index JSON degrades to [] rather than throwing', async () => {
  const rows = await loadCatalog({ fetcher: async () => ({ body: 'not json', etag: null }) });
  assert.deepEqual(rows, []);
});

test('normalizeIndex accepts a bare array or any parse_search_body wrapper key', () => {
  assert.deepEqual(normalizeIndex([{ name: 'a' }]), [{ name: 'a' }]);
  for (const key of ['results', 'skills', 'data', 'items', 'hits']) {
    assert.deepEqual(normalizeIndex({ [key]: [{ name: key }] }), [{ name: key }]);
  }
  assert.deepEqual(normalizeIndex({ nope: 1 }), []);
  assert.deepEqual(normalizeIndex(null), []);
});
