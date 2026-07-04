//! Shared per-worker transcript ring buffer — one buffer, two read cursors
//! (TASK-298).
//!
//! A background coordinator's forwardable stderr activity used to feed TWO
//! independent structures: a bounded in-memory buffer replayed on `:attach`
//! (the "output-to-date" backfill) and the real-time forward path that prints
//! each line live when `:worker-output` / `:attach` is on. They were populated
//! side-by-side, so a future edit to one path but not the other could make an
//! `:attach` replay drift from the live stream ("attach shows old activity but
//! the live stream shows different activity").
//!
//! This type collapses both onto ONE ring buffer that is the single source of
//! truth. The coordinator's stderr stream is the sole WRITER (the write
//! pointer, [`TranscriptRing::push`]); TWO read cursors consume the same
//! entries:
//!   * the **backfill** cursor — [`TranscriptRing::backfill_tail`] returns the
//!     full retained tail, replayed the instant an operator `:attach`es; and
//!   * the **live** cursor — [`LiveCursor`] + [`TranscriptRing::read_live`]
//!     yields each entry once, in order, as it lands, for the real-time stream.
//! Because both cursors read the identical `entries`, the `:attach` replay can
//! never diverge from the live stream: whatever the live cursor emitted is
//! exactly what the backfill cursor will later replay (until eviction), and
//! whatever a fresh live cursor drains equals the backfill tail.
//!
//! Bounded oldest-first by BOTH a line count and a byte budget so a chatty
//! coordinator can't grow the PARENT's memory without limit — the same OOM
//! discipline as the rest of the worker plumbing.

use std::collections::VecDeque;

/// Max retained activity lines (line cap). Whichever of the line/byte caps trips
/// first evicts the oldest rows.
pub const MAX_LINES: usize = 1000;

/// Byte budget for the retained buffer (byte cap). Oldest rows are evicted once
/// the running total of `(suffix + text)` bytes exceeds this.
pub const MAX_BYTES: usize = 256 * 1024;

/// One retained activity entry: a monotonic write sequence plus the
/// `(suffix, text)` pair the pane renderer consumes. `suffix` is the pane-gutter
/// suffix (empty under the single-glyph convention); `text` is the rendered
/// line (the source glyph is already baked in).
#[derive(Clone, Debug)]
struct RingEntry {
    seq: u64,
    suffix: String,
    text: String,
}

/// A live read cursor: the seq of the last entry this consumer emitted. A fresh
/// cursor (`default`, `last_seq == 0`) drains the whole retained buffer on its
/// first [`TranscriptRing::read_live`], then only new entries thereafter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveCursor {
    last_seq: u64,
}

/// The single-source-of-truth ring buffer. See the module docs.
#[derive(Debug)]
pub struct TranscriptRing {
    entries: VecDeque<RingEntry>,
    /// Running total of `(suffix + text)` bytes across `entries`, for the byte cap.
    bytes: usize,
    /// The WRITE pointer: seq to assign the NEXT pushed entry. Monotonic — never
    /// reset (not even by [`clear`](Self::clear)), so a seq is never reused and a
    /// stale cursor can't accidentally re-emit a recycled entry.
    next_seq: u64,
    max_lines: usize,
    max_bytes: usize,
}

impl Default for TranscriptRing {
    fn default() -> Self {
        Self::new(MAX_LINES, MAX_BYTES)
    }
}

impl TranscriptRing {
    /// A ring bounded by `max_lines` entries AND `max_bytes` of payload. The
    /// write pointer starts at 1 (0 is reserved as "before any entry" for a
    /// fresh [`LiveCursor`]).
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            next_seq: 1,
            max_lines,
            max_bytes,
        }
    }

    /// WRITE one entry (advances the write pointer), then evict oldest-first
    /// until BOTH caps hold. Returns the seq assigned to the new entry.
    pub fn push(&mut self, suffix: &str, text: &str) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes += suffix.len() + text.len();
        self.entries.push_back(RingEntry {
            seq,
            suffix: suffix.to_string(),
            text: text.to_string(),
        });
        while self.entries.len() > self.max_lines || self.bytes > self.max_bytes {
            match self.entries.pop_front() {
                Some(e) => self.bytes -= e.suffix.len() + e.text.len(),
                None => break,
            }
        }
        seq
    }

    /// The BACKFILL read cursor: the full retained tail (`(suffix, text)`,
    /// oldest-first) — everything an `:attach` replays as the output-to-date.
    pub fn backfill_tail(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|e| (e.suffix.clone(), e.text.clone()))
            .collect()
    }

    /// The LIVE read cursor: entries not yet seen by `cursor` (seq strictly
    /// greater than its position), in order, advancing the cursor to the current
    /// write pointer. A fresh cursor drains the whole retained buffer; a
    /// caught-up cursor returns empty until the next [`push`](Self::push).
    ///
    /// Entries evicted before the cursor caught up are silently skipped — they
    /// fell outside the bounded window the backfill cursor would replay too, so
    /// the two cursors stay consistent on the retained window.
    pub fn read_live(&self, cursor: &mut LiveCursor) -> Vec<(String, String)> {
        let out: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|e| e.seq > cursor.last_seq)
            .map(|e| (e.suffix.clone(), e.text.clone()))
            .collect();
        // Advance to the write pointer even when the tail was evicted, so a
        // slow consumer never re-reads a recycled window.
        cursor.last_seq = self.next_seq.saturating_sub(1);
        out
    }

    /// The write pointer — seq of the most-recently written entry (0 before any
    /// push). Observability/test accessor.
    #[allow(dead_code)]
    pub fn write_pos(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Retained entry count (after eviction). Observability/test accessor.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the retained buffer is empty. Observability/test accessor (the
    /// attach-time "thinking…" placeholder keys off `transcript_rows().is_empty()`).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retained payload byte total. Observability/test accessor.
    #[allow(dead_code)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Drop all retained entries (an in-place resume — a fresh "thread"). The
    /// write pointer stays monotonic so seqs are never reused; a resumed run's
    /// fresh [`LiveCursor`] (position 0) simply drains from the next push.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_assigns_monotonic_seqs_and_bounds_oldest_first() {
        let mut ring = TranscriptRing::new(3, 1_000_000);
        assert_eq!(ring.push("", "a"), 1);
        assert_eq!(ring.push("", "b"), 2);
        assert_eq!(ring.push("", "c"), 3);
        assert_eq!(ring.push("", "d"), 4); // evicts "a"
        let tail: Vec<String> = ring.backfill_tail().into_iter().map(|(_, t)| t).collect();
        assert_eq!(tail, vec!["b", "c", "d"], "line cap keeps newest three");
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.write_pos(), 4, "write pointer counts every push");
    }

    #[test]
    fn byte_cap_evicts_before_line_cap() {
        // Huge line cap, tiny byte cap: eviction is driven by bytes.
        let mut ring = TranscriptRing::new(1000, 4);
        ring.push("", "xx"); // 2 bytes
        ring.push("", "yy"); // 4 bytes total — still within cap
        ring.push("", "zz"); // 6 > 4 → evict "xx"
        let tail: Vec<String> = ring.backfill_tail().into_iter().map(|(_, t)| t).collect();
        assert_eq!(tail, vec!["yy", "zz"]);
        assert!(ring.bytes() <= 4);
    }

    #[test]
    fn live_cursor_yields_each_entry_once() {
        let mut ring = TranscriptRing::default();
        let mut cur = LiveCursor::default();
        ring.push("", "one");
        ring.push("", "two");
        let first: Vec<String> = ring.read_live(&mut cur).into_iter().map(|(_, t)| t).collect();
        assert_eq!(first, vec!["one", "two"]);
        // Caught up — nothing new until the next push.
        assert!(ring.read_live(&mut cur).is_empty());
        ring.push("", "three");
        let second: Vec<String> = ring.read_live(&mut cur).into_iter().map(|(_, t)| t).collect();
        assert_eq!(second, vec!["three"]);
    }

    /// AC3: no drift between the `:attach` backfill and the live stream. The
    /// concatenation of a live cursor's incremental reads equals the sequence
    /// the buffer stored, and a fresh live cursor drains exactly the backfill
    /// tail — single source of truth.
    #[test]
    fn live_reads_never_drift_from_backfill() {
        let mut ring = TranscriptRing::new(1000, 1_000_000);
        let mut live = LiveCursor::default();
        let mut streamed: Vec<String> = Vec::new();
        for n in 0..500 {
            ring.push("", &format!("line {n}"));
            // Live stream drains as each entry lands (write-through).
            for (_, t) in ring.read_live(&mut live) {
                streamed.push(t);
            }
        }
        let stored: Vec<String> = (0..500).map(|n| format!("line {n}")).collect();
        assert_eq!(streamed, stored, "live cursor emitted every entry in order");

        // A late-attaching backfill cursor replays exactly the retained tail,
        // and a fresh live cursor draining now yields the identical rows.
        let backfill: Vec<String> = ring.backfill_tail().into_iter().map(|(_, t)| t).collect();
        let mut fresh = LiveCursor::default();
        let drained: Vec<String> = ring.read_live(&mut fresh).into_iter().map(|(_, t)| t).collect();
        assert_eq!(backfill, drained, "backfill tail == fresh live drain (no drift)");
        assert_eq!(backfill, stored, "unbounded ring retained every row");
    }

    #[test]
    fn clear_keeps_write_pointer_monotonic() {
        let mut ring = TranscriptRing::default();
        ring.push("", "a");
        ring.push("", "b");
        let pos = ring.write_pos();
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.bytes(), 0);
        let seq = ring.push("", "c");
        assert!(seq > pos, "seqs never reused after clear: {seq} > {pos}");
        // A fresh cursor after a resume drains only post-clear entries.
        let mut cur = LiveCursor::default();
        let rows: Vec<String> = ring.read_live(&mut cur).into_iter().map(|(_, t)| t).collect();
        assert_eq!(rows, vec!["c"]);
    }
}
