//! Pure prompt-batching planner for the local (llama.cpp) backend.
//!
//! **Why this module exists.** The first cut of [`crate::backend::local`]
//! allocated one `LlamaBatch::new(512, 1)` and pushed *every* prompt token into
//! it. aish's own system prompt is several thousand tokens, so the 513th
//! `batch.add(..)` returned `BatchSizeExceeded` and the turn died with
//! `failed to add token to batch` — the local backend was unusable for any real
//! prompt, not because of the model or the host's memory but because of a
//! hard-coded 512-slot buffer.
//!
//! The fix is to feed the prompt in **chunks** that each fit the batch, and to
//! trim the prompt to the context window *before* feeding it. That policy is
//! pure arithmetic, so it lives here — free of `llama_cpp_2` types — and is
//! covered by unit tests that run in the default (`--no-default-features`) CI
//! build, where the `local` feature and its llama.cpp toolchain are absent.
//!
//! Trimming keeps a **head** (the system prompt, which must survive) and the
//! **tail** (the most recent turns), dropping from the middle. Positions are
//! renumbered `0..kept` over the kept tokens, which is what llama.cpp wants:
//! the KV cache indexes by position, not by original prompt offset.

// The planner's only non-test consumer is `backend::local`, which is gated
// behind the `local` feature. In a default build the items below are reachable
// only from this module's unit tests, so dead-code analysis would flag them.
#![cfg_attr(not(feature = "local"), allow(dead_code))]

/// One `decode` unit: `len` tokens starting at `src_start` in the tokenized
/// prompt, occupying llama positions `pos_start .. pos_start + len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// Offset into the original token slice.
    pub src_start: usize,
    /// Token count in this chunk. Always `>= 1` and `<= batch_capacity`.
    pub len: usize,
    /// llama position of this chunk's first token.
    pub pos_start: usize,
}

/// The full feeding plan for one prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPlan {
    /// Chunks to decode, in order.
    pub chunks: Vec<Chunk>,
    /// Tokens that will actually be fed.
    pub kept: usize,
    /// Tokens dropped from the middle to fit the context window.
    pub dropped: usize,
    /// Batch slots per decode call (the cap each chunk respects).
    pub batch_capacity: usize,
    /// Prompt tokens the context window could afford after reserving output.
    pub prompt_budget: usize,
}

impl PromptPlan {
    /// True when there is nothing to feed (empty prompt).
    pub fn is_empty(&self) -> bool {
        self.kept == 0
    }

    /// Whether any prompt token had to be dropped to fit the window.
    pub fn trimmed(&self) -> bool {
        self.dropped > 0
    }

    /// llama position of the first generated token — i.e. where sampling picks
    /// up after the prompt has been decoded.
    pub fn next_position(&self) -> usize {
        self.kept
    }
}

/// Batch slots per `decode` call. 512 matches llama.cpp's own default
/// `n_batch`; the planner treats it as a *chunk size*, not a prompt limit.
pub const DEFAULT_BATCH_CAPACITY: usize = 512;

/// Context window requested for the local context.
pub const DEFAULT_N_CTX: usize = 4096;

/// Tokens reserved for the model's reply (and thus withheld from the prompt).
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 512;

/// Clamp an optional raw string into `[min, max]`, falling back to `default`
/// when it is absent or unparseable.
///
/// Split out from [`env_usize`] so the policy is unit-testable **without**
/// mutating process environment: `std::env::set_var` is `unsafe` under edition
/// 2024 and racy across the test harness's threads.
pub fn parse_usize(raw: Option<&str>, default: usize, min: usize, max: usize) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .map(|v| v.clamp(min, max))
        .unwrap_or(default)
}

/// Read a `usize` tunable from the environment, clamped to `[min, max]`.
/// Unset/garbage values fall back to `default` rather than failing a turn.
pub fn env_usize(key: &str, default: usize, min: usize, max: usize) -> usize {
    parse_usize(std::env::var(key).ok().as_deref(), default, min, max)
}

/// Plan how to feed `total` prompt tokens through a `batch_capacity`-slot batch
/// into an `n_ctx` context that must leave `reserve_output` tokens for the
/// reply.
///
/// * `keep_head` — leading tokens that must survive trimming (the system
///   prompt). Clamped so the tail always gets at least one token.
/// * Over-budget prompts drop from the middle: head is kept, tail is kept,
///   the gap between them is reported as [`PromptPlan::dropped`].
pub fn plan_prompt(
    total: usize,
    keep_head: usize,
    n_ctx: usize,
    reserve_output: usize,
    batch_capacity: usize,
) -> PromptPlan {
    let cap = batch_capacity.max(1);
    // Always leave room for at least one generated token.
    let budget = n_ctx.saturating_sub(reserve_output).max(1);

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let (kept, dropped);

    if total <= budget {
        kept = total;
        dropped = 0;
        if total > 0 {
            ranges.push((0, total));
        }
    } else {
        // Keep the head (system prompt) but never the whole budget: the most
        // recent turns matter more than the middle of the history.
        let head = keep_head.min(total).min(budget.saturating_sub(1));
        let tail_len = budget - head;
        let tail_start = total - tail_len;
        if head > 0 {
            ranges.push((0, head));
        }
        ranges.push((tail_start, tail_len));
        kept = head + tail_len;
        dropped = tail_start - head;
    }

    let mut chunks = Vec::new();
    let mut pos = 0usize;
    for (start, len) in ranges {
        let mut off = 0usize;
        while off < len {
            let take = cap.min(len - off);
            chunks.push(Chunk {
                src_start: start + off,
                len: take,
                pos_start: pos,
            });
            pos += take;
            off += take;
        }
    }

    PromptPlan {
        chunks,
        kept,
        dropped,
        batch_capacity: cap,
        prompt_budget: budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covered(plan: &PromptPlan) -> usize {
        plan.chunks.iter().map(|c| c.len).sum()
    }

    /// Positions must be contiguous from 0 — llama.cpp indexes its KV cache by
    /// position, so a gap or repeat corrupts attention.
    fn assert_positions_contiguous(plan: &PromptPlan) {
        let mut expect = 0usize;
        for c in &plan.chunks {
            assert_eq!(c.pos_start, expect, "non-contiguous positions in {plan:?}");
            assert!(c.len >= 1, "zero-length chunk in {plan:?}");
            assert!(
                c.len <= plan.batch_capacity,
                "chunk exceeds batch capacity in {plan:?}"
            );
            expect += c.len;
        }
        assert_eq!(expect, plan.kept, "chunk coverage != kept in {plan:?}");
    }

    #[test]
    fn empty_prompt_plans_nothing() {
        let p = plan_prompt(0, 0, 4096, 512, 512);
        assert!(p.is_empty());
        assert!(p.chunks.is_empty());
        assert_eq!(p.dropped, 0);
    }

    #[test]
    fn short_prompt_is_one_chunk() {
        let p = plan_prompt(100, 50, 4096, 512, 512);
        assert_eq!(p.chunks.len(), 1);
        assert_eq!(
            p.chunks[0],
            Chunk {
                src_start: 0,
                len: 100,
                pos_start: 0
            }
        );
        assert!(!p.trimmed());
        assert_positions_contiguous(&p);
    }

    #[test]
    fn exactly_batch_capacity_is_one_chunk() {
        let p = plan_prompt(512, 0, 4096, 512, 512);
        assert_eq!(p.chunks.len(), 1);
        assert_eq!(covered(&p), 512);
        assert_positions_contiguous(&p);
    }

    /// The original bug: 513 tokens must NOT be a single over-capacity batch.
    #[test]
    fn one_past_capacity_splits_instead_of_overflowing() {
        let p = plan_prompt(513, 0, 4096, 512, 512);
        assert_eq!(p.chunks.len(), 2);
        assert_eq!(p.chunks[0].len, 512);
        assert_eq!(
            p.chunks[1],
            Chunk {
                src_start: 512,
                len: 1,
                pos_start: 512
            }
        );
        assert!(!p.trimmed());
        assert_positions_contiguous(&p);
    }

    /// A realistic aish system prompt (~3.5k tokens) fits the window and is fed
    /// as seven full batches — this is the case that used to abort the turn.
    #[test]
    fn large_in_window_prompt_is_chunked_not_trimmed() {
        let p = plan_prompt(3584, 2048, 4096, 512, 512);
        assert_eq!(p.chunks.len(), 7);
        assert_eq!(covered(&p), 3584);
        assert_eq!(p.kept, 3584);
        assert!(!p.trimmed());
        assert_positions_contiguous(&p);
    }

    #[test]
    fn over_budget_prompt_keeps_head_and_tail() {
        // budget = 4096 - 512 = 3584; head 1000 + tail 2584, dropping the middle.
        let p = plan_prompt(10_000, 1000, 4096, 512, 512);
        assert_eq!(p.prompt_budget, 3584);
        assert_eq!(p.kept, 3584);
        assert_eq!(p.dropped, 10_000 - 3584);
        assert!(p.trimmed());
        // Head starts at 0; the tail range ends at the final prompt token.
        assert_eq!(p.chunks[0].src_start, 0);
        let last = p.chunks.last().unwrap();
        assert_eq!(last.src_start + last.len, 10_000);
        assert_positions_contiguous(&p);
    }

    #[test]
    fn keep_head_larger_than_budget_still_leaves_tail() {
        let p = plan_prompt(10_000, 9_000, 1024, 512, 512);
        assert_eq!(p.prompt_budget, 512);
        assert_eq!(p.kept, 512);
        // head clamped to budget-1 so the newest token always survives.
        assert_eq!(p.chunks[0].len, 511);
        let last = p.chunks.last().unwrap();
        assert_eq!(last.src_start + last.len, 10_000);
        assert_eq!(last.len, 1);
        assert_positions_contiguous(&p);
    }

    #[test]
    fn zero_keep_head_keeps_only_the_tail() {
        let p = plan_prompt(5_000, 0, 2048, 512, 512);
        assert_eq!(p.kept, 1536);
        assert_eq!(p.chunks[0].src_start, 5_000 - 1536);
        assert_positions_contiguous(&p);
    }

    #[test]
    fn degenerate_capacity_and_window_do_not_panic() {
        let p = plan_prompt(5, 10, 0, 0, 0);
        assert_eq!(p.batch_capacity, 1);
        assert_eq!(p.prompt_budget, 1);
        assert_eq!(p.kept, 1);
        assert_eq!(p.chunks.len(), 1);
        assert_positions_contiguous(&p);

        // reserve larger than the window must not underflow.
        let p = plan_prompt(100, 0, 512, 4096, 32);
        assert_eq!(p.prompt_budget, 1);
        assert_eq!(p.kept, 1);
        assert_positions_contiguous(&p);
    }

    #[test]
    fn next_position_follows_the_prompt() {
        let p = plan_prompt(700, 0, 4096, 512, 512);
        assert_eq!(p.next_position(), 700);
        let p = plan_prompt(10_000, 100, 4096, 512, 512);
        assert_eq!(p.next_position(), p.kept);
    }

    #[test]
    fn parse_usize_clamps_and_falls_back() {
        // Absent / unparseable -> default.
        assert_eq!(parse_usize(None, 512, 1, 8192), 512);
        assert_eq!(parse_usize(Some("not-a-number"), 512, 1, 8192), 512);
        assert_eq!(parse_usize(Some(""), 512, 1, 8192), 512);
        // Parsed values are honoured, with surrounding whitespace tolerated.
        assert_eq!(parse_usize(Some("1024"), 512, 1, 8192), 1024);
        assert_eq!(parse_usize(Some("  1024  "), 512, 1, 8192), 1024);
        // Out-of-range values clamp instead of failing the turn.
        assert_eq!(parse_usize(Some("999999"), 512, 1, 8192), 8192);
        assert_eq!(parse_usize(Some("0"), 512, 1, 8192), 1);
    }
}
