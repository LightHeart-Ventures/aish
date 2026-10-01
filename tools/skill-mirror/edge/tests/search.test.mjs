// Unit tests for the pure scoring core. Zero dependencies — run with:
//   node --test tools/skill-mirror/edge/tests
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import {
  searchCatalog,
  scoreRow,
  normalizeLimit,
  parseQuery,
  starsBonus,
  DEFAULT_LIMIT,
  MAX_LIMIT,
  WEIGHTS,
} from '../src/search.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const CATALOG = JSON.parse(readFileSync(join(here, 'fixtures', 'catalog.json'), 'utf8'));

const refs = (rows) => rows.map((r) => r.reference);

// ---------------------------------------------------------------------------
// Test plan #1 — exact name beats prefix beats substring
// ---------------------------------------------------------------------------
test('exact name beats prefix beats substring', () => {
  const exact = { name: 'rust', reference: 'a/exact', stars: 0 };
  const prefix = { name: 'rust-testing', reference: 'b/prefix', stars: 0 };
  const substring = { name: 'cargo-rust-audit', reference: 'c/substring', stars: 0 };
  const weakest = { name: 'nothing', description: 'mentions rust', reference: 'd/desc', stars: 0 };

  const out = searchCatalog([substring, weakest, prefix, exact], { q: 'rust' });
  assert.deepEqual(refs(out), ['a/exact', 'b/prefix', 'c/substring', 'd/desc']);

  // and the raw scores are ordered with a wide margin
  assert.equal(scoreRow(exact, 'rust'), WEIGHTS.nameExact + WEIGHTS.namePrefix + WEIGHTS.nameSubstring);
  assert.equal(scoreRow(prefix, 'rust'), WEIGHTS.namePrefix + WEIGHTS.nameSubstring);
  assert.equal(scoreRow(substring, 'rust'), WEIGHTS.nameSubstring);
  assert.equal(scoreRow(weakest, 'rust'), WEIGHTS.description);
});

// ---------------------------------------------------------------------------
// Test plan #2 — stars break ties but never override an exact name match
// ---------------------------------------------------------------------------
test('stars break ties between equally-relevant rows', () => {
  const dim = { name: 'cargo-rust-a', reference: 'a/dim', stars: 1 };
  const bright = { name: 'cargo-rust-b', reference: 'b/bright', stars: 100000 };
  const out = searchCatalog([dim, bright], { q: 'rust' });
  assert.deepEqual(refs(out), ['b/bright', 'a/dim']);
  assert.ok(starsBonus(100000) > starsBonus(1));
});

test('stars never let a popular irrelevant row outrank an exact name match', () => {
  // 99999-star row matches only on author; the exact name match has 0 stars.
  const out = searchCatalog(CATALOG, { q: 'rust' });
  assert.equal(out[0].reference, 'ferris/rust', 'exact name match must rank first');

  const popular = CATALOG.find((r) => r.reference === 'rustaceans/very-popular-thing');
  const exactRow = CATALOG.find((r) => r.reference === 'ferris/rust');
  assert.ok(
    scoreRow(exactRow, 'rust') > scoreRow(popular, 'rust'),
    'exact name match must outscore the 99999-star author-only match',
  );

  // The stars term is bounded an order of magnitude below the exact weight.
  assert.ok(starsBonus(100000) < WEIGHTS.nameExact);
});

// ---------------------------------------------------------------------------
// Test plan #3 — empty q returns the whole catalog, truncated to limit
// ---------------------------------------------------------------------------
test('empty q returns the whole catalog truncated to limit', () => {
  assert.equal(searchCatalog(CATALOG, { q: '', limit: 100 }).length, CATALOG.length);
  assert.equal(searchCatalog(CATALOG, { q: '   ', limit: 100 }).length, CATALOG.length);
  assert.equal(searchCatalog(CATALOG, {}).length, CATALOG.length);

  const truncated = searchCatalog(CATALOG, { q: '', limit: 3 });
  assert.equal(truncated.length, 3);
  assert.deepEqual(refs(truncated), refs(CATALOG.slice(0, 3)));
});

// ---------------------------------------------------------------------------
// Test plan #4 — limit clamping
// ---------------------------------------------------------------------------
test('limit=9999 clamps to 100 and limit=abc defaults to 50', () => {
  assert.equal(normalizeLimit('9999'), MAX_LIMIT);
  assert.equal(normalizeLimit('abc'), DEFAULT_LIMIT);
  assert.equal(normalizeLimit(undefined), DEFAULT_LIMIT);
  assert.equal(normalizeLimit(''), DEFAULT_LIMIT);
  assert.equal(normalizeLimit('0'), 1);
  assert.equal(normalizeLimit('-5'), 1);
  assert.equal(normalizeLimit('1'), 1);
  assert.equal(normalizeLimit('100'), 100);
  assert.equal(normalizeLimit('101'), 100);
  assert.equal(normalizeLimit('37'), 37);

  assert.deepEqual(parseQuery('q=rust&limit=9999'), { q: 'rust', limit: 100 });
  assert.deepEqual(parseQuery('?q=rust&limit=abc'), { q: 'rust', limit: 50 });
  assert.deepEqual(parseQuery('q=rust'), { q: 'rust', limit: 50 });

  // a 9999-row catalog cannot produce more than 100 rows
  const big = Array.from({ length: 9999 }, (_, i) => ({
    name: `rust-${i}`,
    reference: `o/rust-${String(i).padStart(5, '0')}`,
    stars: 0,
  }));
  assert.equal(searchCatalog(big, parseQuery('q=rust&limit=9999')).length, 100);
});

test('unknown query params are ignored', () => {
  assert.deepEqual(parseQuery('q=rust&limit=10&page=4&sort=stars&q2=x'), { q: 'rust', limit: 10 });
});

// ---------------------------------------------------------------------------
// Test plan #5 — no matches is an empty array (handler asserts the 200)
// ---------------------------------------------------------------------------
test('no matches yields an empty array, not an error', () => {
  const out = searchCatalog(CATALOG, { q: 'definitely-not-in-the-catalog-zzz' });
  assert.deepEqual(out, []);
  assert.ok(Array.isArray(out));
});

test('rows scoring 0 are excluded', () => {
  const out = searchCatalog(CATALOG, { q: 'sqlite' });
  assert.deepEqual(refs(out), ['dwh/sqlite']);
});

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------
test('ties are broken by reference ascending for determinism', () => {
  const a = { name: 'x-rust-1', reference: 'zzz/one', stars: 5 };
  const b = { name: 'x-rust-2', reference: 'aaa/two', stars: 5 };
  assert.deepEqual(refs(searchCatalog([a, b], { q: 'rust' })), ['aaa/two', 'zzz/one']);
  assert.deepEqual(refs(searchCatalog([b, a], { q: 'rust' })), ['aaa/two', 'zzz/one']);
});

// ---------------------------------------------------------------------------
// Test plan #6 — SUPERSET PROPERTY
//
// A faithful JS port of the aish client's `skill_provider::filter_local`:
//
//   let q = query.trim().to_lowercase();
//   if q.is_empty() { return results; }
//   results.filter(|r| r.name.to_lowercase().contains(&q)
//       || r.reference.to_lowercase().contains(&q)
//       || r.author.to_lowercase().contains(&q)
//       || r.description.to_lowercase().contains(&q))
//
// Every row filter_local returns MUST appear in the endpoint's output, so the
// remote path can never be worse than offline.
// ---------------------------------------------------------------------------
function filterLocal(results, query) {
  const q = String(query ?? '').trim().toLowerCase();
  if (!q) return results;
  return results.filter(
    (r) =>
      String(r.name ?? '').toLowerCase().includes(q) ||
      String(r.reference ?? '').toLowerCase().includes(q) ||
      String(r.author ?? '').toLowerCase().includes(q) ||
      String(r.description ?? '').toLowerCase().includes(q),
  );
}

test('superset property: the endpoint returns every row filter_local would', () => {
  const queries = [
    '',
    '   ',
    'rust',
    'RUST',
    'Rust-Testing',
    'ferris',
    'cargo',
    'terraform',
    'opentelemetry',
    'sqlite',
    'python',
    'audit',
    'providers',
    'zzz-no-such-thing',
    'e', // extremely broad: matches almost everything
  ];

  for (const q of queries) {
    const expected = filterLocal(CATALOG, q);
    // limit=100 so truncation can never be the reason a row is missing
    // (the fixture is well under 100 rows).
    const actual = searchCatalog(CATALOG, { q, limit: MAX_LIMIT });
    const actualRefs = new Set(refs(actual));
    for (const row of expected) {
      assert.ok(
        actualRefs.has(row.reference),
        `q=${JSON.stringify(q)}: filter_local returned ${row.reference} but the endpoint did not`,
      );
    }
    // and the endpoint adds nothing filter_local would have rejected
    assert.equal(
      actual.length,
      expected.length,
      `q=${JSON.stringify(q)}: endpoint returned ${actual.length} rows, filter_local ${expected.length}`,
    );
  }
});

test('superset property holds for every single-character query over the fixture', () => {
  const alphabet = 'abcdefghijklmnopqrstuvwxyz0123456789-/. ';
  for (const ch of alphabet) {
    const expected = filterLocal(CATALOG, ch);
    const actualRefs = new Set(refs(searchCatalog(CATALOG, { q: ch, limit: MAX_LIMIT })));
    for (const row of expected) {
      assert.ok(actualRefs.has(row.reference), `q=${JSON.stringify(ch)}: missing ${row.reference}`);
    }
  }
});

// ---------------------------------------------------------------------------
// Robustness — the generator must never be able to 500 us
// ---------------------------------------------------------------------------
test('missing / malformed fields never throw', () => {
  const junk = [
    {},
    { name: null, reference: undefined, author: 1, description: {}, stars: 'many' },
    { name: 'rust', stars: Number.NaN },
    { name: 'rust', stars: -10 },
  ];
  assert.doesNotThrow(() => searchCatalog(junk, { q: 'rust' }));
  assert.doesNotThrow(() => searchCatalog(junk, { q: '' }));
  assert.doesNotThrow(() => searchCatalog(null, { q: 'rust' }));
  assert.deepEqual(searchCatalog(null, { q: 'rust' }), []);
  assert.equal(starsBonus('many'), 0);
  assert.equal(starsBonus(-10), 0);
});
