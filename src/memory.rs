//! Memory-store helpers — the pure, testable core behind the `remember`/`recall`
//! tools and history offload (see [`crate::db`]).
//!
//! Three concerns live here so [`crate::db`] stays thin SQL plumbing:
//!   * **Token budgeting** ([`truncate_hit`]) — a single `recall` hit is capped
//!     so a rehydrated transcript can never dump a six-figure-token blob into a
//!     tool result (the offload-rehydration footgun).
//!   * **Retention** ([`OFFLOAD_KEEP_RECENT`] / [`OFFLOAD_MAX_AGE_DAYS`]) — bounds
//!     on the compaction-offload table so it can't grow without limit.
//!   * **Embeddings** ([`embed`]) — a dependency-free local lexical embedding so
//!     `recall` ranks by *relevance* (cosine over the query) instead of pure
//!     recency. The hashing embedder needs no network/model; it is intentionally
//!     pluggable — swap in a learned embedder later for true synonym matching and
//!     the recall path (candidate gen → cosine rank) is unchanged.

/// The reserved tag a history-compaction transcript is stored under (see
/// [`crate::context`]). Such rows live in their OWN `offloads` table, never in
/// `memories`, so a routine `recall` of curated facts never drags an MB-scale
/// transcript in front of them.
pub const OFFLOAD_TAG: &str = "context-offload";

/// Hard cap (in characters) on a single `recall` hit fed back to the model. A
/// curated fact is a sentence; an offload transcript can be ~2 MB. Capping each
/// hit keeps even a worst-case recall well under the context window — the head
/// is returned with a marker pointing at how to get more.
pub const RECALL_HIT_MAX_CHARS: usize = 2_000;

/// Keep at most this many of the most-recent offload transcripts (mirrors the
/// coordinator's failed-run keep-recent bound). Older ones are reaped.
pub const OFFLOAD_KEEP_RECENT: usize = 20;

/// Reap any offload transcript older than this many days regardless of the
/// keep-recent count.
pub const OFFLOAD_MAX_AGE_DAYS: i64 = 7;

/// Candidate fan-out for keyword recall before relevance re-ranking: pull this
/// many keyword matches, then rank them by embedding cosine and keep `limit`.
pub const RECALL_CANDIDATE_CAP: usize = 64;

/// Dimensionality of the local lexical embedding — matches the `vec_memories`
/// `float[384]` mirror so a learned 384-d embedder can drop in unchanged.
pub const EMBED_DIM: usize = 384;

/// Truncate one recall hit to `max` characters, appending a marker that states
/// how much was elided and how to get it. A hit at or under the cap is returned
/// unchanged (no allocation surprise). The marker keeps the model honest: it
/// learns the row is larger and that a tighter query (or the offload tag) pages
/// the rest, instead of silently seeing a clipped fact.
pub fn truncate_hit(content: &str, max: usize) -> String {
    // Count by chars (not bytes) so we never split a UTF-8 boundary.
    if content.chars().count() <= max {
        return content.to_string();
    }
    let head: String = content.chars().take(max).collect();
    let elided_bytes = content.len().saturating_sub(head.len());
    let kb = elided_bytes.div_ceil(1024);
    format!("{head}… [+{kb} KB elided — recall with a tighter query or the `context-offload` tag]")
}

/// Build a forgiving FTS5 MATCH expression from free-form user text: each
/// alphanumeric token becomes a prefix term (`tok*`) and they are OR-joined, so
/// `recall` matches any token prefix without the caller learning FTS syntax and
/// without a stray `:`/`"`/`%` ever becoming a syntax error. Returns `None` when
/// the query has no usable token (all punctuation / empty) so the caller can
/// fall back to a substring scan.
pub fn fts_match_query(user: &str) -> Option<String> {
    let toks: Vec<String> = user
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("{}*", t.to_ascii_lowercase()))
        .collect();
    if toks.is_empty() {
        None
    } else {
        Some(toks.join(" OR "))
    }
}

/// A dependency-free local embedding: feature-hash the text's tokens into a
/// fixed [`EMBED_DIM`]-dimensional vector (signed hashing trick), then L2-
/// normalize so cosine similarity is a plain dot product. No network, no model
/// weights — deterministic and instant. It captures token-overlap relevance
/// (the recency→relevance upgrade); a learned embedder swapped in here would add
/// true semantic/synonym matching with no change to the recall pipeline. Returns
/// an all-zero vector for text with no tokens (treated as "no embedding").
pub fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; EMBED_DIM];
    let mut any = false;
    for tok in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        any = true;
        let lower = tok.to_ascii_lowercase();
        let h = fnv1a(lower.as_bytes());
        let idx = (h % EMBED_DIM as u64) as usize;
        // A second hash bit picks the sign so distinct tokens don't only ever add.
        let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
        v[idx] += sign;
    }
    if any {
        l2_normalize(&mut v);
    }
    v
}

/// Cosine similarity of two equal-length vectors. With [`embed`]'s L2-normalized
/// output this is a dot product; we divide by the norms anyway so the function
/// is correct for un-normalized inputs too. Returns 0.0 when either vector is
/// zero (no shared basis) or the lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Serialize an embedding to a little-endian `f32` BLOB for the `memories.embedding`
/// column. Inverse of [`blob_to_embed`].
pub fn embed_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Parse a little-endian `f32` BLOB back into an embedding. Returns `None` when
/// the byte length isn't a whole number of `f32`s (a corrupt/foreign blob), so
/// the caller treats the row as un-embedded rather than panicking.
pub fn blob_to_embed(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut v = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        v.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(v)
}

/// JSON array form of an embedding — the textual vector format `sqlite-vec`
/// accepts for a `vec0` column, used to mirror the embedding into `vec_memories`.
pub fn embed_to_json(v: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{f}"));
    }
    s.push(']');
    s
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// 64-bit FNV-1a — a fast, well-distributed non-cryptographic hash for the
/// feature-hashing embedder and token bucketing. Stable across runs/platforms.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_hit_keeps_short_and_marks_long() {
        // A short hit is returned byte-for-byte.
        assert_eq!(truncate_hit("a tiny fact", 2000), "a tiny fact");
        // A long hit is clipped to the cap with a marker that names the elision.
        let big = "x".repeat(5000);
        let out = truncate_hit(&big, 2000);
        assert!(out.chars().count() < big.chars().count());
        assert!(out.starts_with(&"x".repeat(2000)));
        assert!(out.contains("KB elided"));
        assert!(out.contains("context-offload"));
        // ~3 KB elided (5000 - 2000 = 3000 bytes → 3 KB).
        assert!(out.contains("+3 KB"), "marker should state elided size: {out}");
    }

    #[test]
    fn truncate_hit_never_splits_utf8() {
        // Multi-byte chars: capping by char count must not panic or corrupt.
        let s = "é".repeat(100);
        let out = truncate_hit(&s, 10);
        assert!(out.starts_with(&"é".repeat(10)));
        assert!(out.contains("KB elided"));
    }

    #[test]
    fn fts_match_query_builds_prefix_or_and_is_syntax_safe() {
        assert_eq!(fts_match_query("fix the build").as_deref(), Some("fix* OR the* OR build*"));
        // Punctuation/operators are stripped to barewords — never an FTS syntax error.
        assert_eq!(fts_match_query("error: NEAR(x)").as_deref(), Some("error* OR near* OR x*"));
        // All-punctuation / empty → None so the caller falls back to a scan.
        assert_eq!(fts_match_query("   "), None);
        assert_eq!(fts_match_query("%%%"), None);
    }

    #[test]
    fn embed_is_deterministic_normalized_and_overlap_aware() {
        let a = embed("the rust compiler is fast");
        let b = embed("the rust compiler is fast");
        // Deterministic.
        assert_eq!(a, b);
        // L2-normalized (unit length, modulo float error).
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "embedding should be unit length: {norm}");
        // Token overlap drives similarity: a near-identical sentence ranks far
        // above an unrelated one.
        let related = embed("the rust compiler is very fast");
        let unrelated = embed("bananas are yellow tropical fruit");
        assert!(
            cosine(&a, &related) > cosine(&a, &unrelated),
            "overlapping text must score higher"
        );
        // Empty / token-less text → zero vector → zero similarity.
        let zero = embed("   ");
        assert!(zero.iter().all(|&x| x == 0.0));
        assert_eq!(cosine(&a, &zero), 0.0);
    }

    #[test]
    fn embed_blob_roundtrips() {
        let v = embed("round trip me");
        let blob = embed_to_blob(&v);
        assert_eq!(blob.len(), EMBED_DIM * 4);
        let back = blob_to_embed(&blob).expect("valid blob");
        assert_eq!(v, back);
        // Corrupt blobs decode to None rather than panicking.
        assert!(blob_to_embed(&[1, 2, 3]).is_none());
        assert!(blob_to_embed(&[]).is_none());
    }

    #[test]
    fn embed_json_is_vec0_shaped() {
        let j = embed_to_json(&[1.0, -0.5, 0.0]);
        assert!(j.starts_with('['));
        assert!(j.ends_with(']'));
        assert_eq!(j.matches(',').count(), 2);
    }
}
