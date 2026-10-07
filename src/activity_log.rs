//! Durable per-run coordinator ACTIVITY LOG — the off-process half of the
//! transcript ring (TASK-298 / this fix).
//!
//! # Why this exists
//!
//! A coordinator's live activity reaches the operator through exactly one path:
//! the LAUNCHING process drains the child's stderr pipe in
//! [`crate::worker::stream_stderr`], lexes each line ONCE into an
//! `ActivityEvent`, computes `event.forward_text()`, records it into that
//! worker's in-memory [`crate::transcript_ring::TranscriptRing`], and prints it
//! when `:worker-output` / `:attach` says so.
//!
//! That ring lives in the PROCESS THAT OWNS THE PIPE. For a root coordinator
//! that process is the interactive REPL, so `:attach` works. For a SUBWORKER —
//! a coordinator spawned by another coordinator — the pipe (and therefore the
//! ring) belongs to the PARENT COORDINATOR process. The interactive REPL has
//! neither, so `:attach <subworker-id>` fell through to the durable
//! `coordinator_runs` fallback and attached in REVIEW mode: it replayed the
//! durable `task` + `result`, and since a live run has no result yet, the pane
//! stayed BLANK forever (the reported bug).
//!
//! This module is the durable mirror of the ring: the pipe-owning process
//! appends every FORWARDABLE row — the exact `event.forward_text()` bytes, so
//! the durable log and the live pane can never drift — to an append-only JSONL
//! file keyed by `run_id`. Any other process (another session's REPL, the same
//! REPL after the parent coordinator exited) can backfill and then TAIL that
//! file to watch a run it does not own.
//!
//! # Why JSONL and not a SQLite table
//!
//! The obvious alternative was a `coordinator_activity` table next to
//! `coordinator_runs` in `~/.aish/database/aish.db`. Rejected because of the
//! hot path: `stream_stderr` is draining a pipe, and the shared
//! [`crate::coordinator_store::CoordinatorStore`] is an `Arc<Mutex<Connection>>`
//! with `busy_timeout = 15000` — 100+ concurrent coordinators all writing
//! per-stderr-line rows through one mutex and one WAL could block a stderr
//! drain for seconds and back-pressure the child. A per-run file has NO shared
//! writer at all: exactly one process owns a given child's pipe, so there is
//! exactly one writer per file and zero cross-run contention. Tailing is a
//! `seek` to a remembered byte offset — cheaper than a repeated indexed query.
//!
//! # Hot-path discipline
//!
//! [`ActivityLogWriter::record`] does NOT touch the filesystem. It pushes the
//! row onto an unbounded `std::sync::mpsc` channel (a lock-and-push, never a
//! syscall, never an await) consumed by a DEDICATED OS THREAD that does all the
//! IO. A slow or stalled disk therefore cannot stall the stderr drain: the queue
//! grows instead. The thread batches everything available into a single
//! `write_all`, so a chatty coordinator costs roughly one syscall per burst.
//!
//! # Bounded growth
//!
//! A 6-hour fan-out must not write an unbounded file. The writer tracks the
//! file size and COMPACTS at [`MAX_BYTES`]: it rewrites the file with the most
//! recent rows (up to half the cap) via temp-file + rename, which is atomic for
//! readers. A tailer notices the file SHRANK (`len < offset`) and re-anchors to
//! the new end rather than re-printing the retained window — losing the
//! compacted slice is strictly better than spamming duplicates into a live pane.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

/// Byte budget for ONE run's activity log. When the file grows past this the
/// writer compacts it down to (at most) half, keeping the newest rows.
pub const MAX_BYTES: u64 = 1024 * 1024;

/// How long a run's activity log survives after its last write. The per-run byte
/// cap bounds ONE log; this bounds the COUNT of logs. Swept once per interactive
/// start by [`crate::coordinator::rehydrate`].
pub const RETAIN_DAYS: u64 = 7;

/// The activity-log directory, `~/.aish/activity/`. Best-effort created on every
/// call (idempotent) so callers can hand the path straight to an open.
pub fn activity_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("activity");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Filesystem-safe form of a run id (same policy the worker plumbing uses for
/// run-derived paths): keep `[A-Za-z0-9-_.]`, replace everything else with `-`.
/// Guards against a crafted id escaping the activity directory.
fn sanitize(run_id: &str) -> String {
    run_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// Path of `run_id`'s log inside `dir`.
pub fn log_path_in(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(format!("{}.jsonl", sanitize(run_id)))
}

/// Encode one forwardable row as a JSONL line (trailing `\n` included). JSON
/// escaping is what makes the round-trip byte-exact: a row carries ANSI escapes
/// and may contain anything except a raw newline, which `serde_json` escapes.
/// Pure → unit-tested.
pub fn encode_row(text: &str) -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}\n", serde_json::json!({ "ts": ms, "text": text }))
}

/// Decode one JSONL line back to its forwardable text. `None` for a blank or
/// malformed line (a torn final write from a SIGKILLed writer), so a corrupt
/// tail degrades to "that row is missing" rather than failing the whole read.
/// Pure → unit-tested.
pub fn decode_row(line: &str) -> Option<String> {
    let line = line.trim_end_matches('\r');
    if line.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("text")?.as_str().map(|s| s.to_string())
}

/// A live read cursor over a durable activity log: the byte offset of the end of
/// the last COMPLETE line this reader consumed. A fresh cursor (`default`) reads
/// from the start of the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivityTail {
    offset: u64,
}

impl ActivityTail {
    /// Anchor a cursor at the CURRENT end of the log, so a subsequent
    /// [`read_new_in`] yields only rows appended from now on. Used right after
    /// an attach backfill so the tail does not re-print what backfill showed.
    #[cfg_attr(not(test), allow(dead_code))] // attach anchors via `backfill_with_cursor` (one read, no gap).
    pub fn at_end_in(dir: &Path, run_id: &str) -> Self {
        let offset = std::fs::metadata(log_path_in(dir, run_id))
            .map(|m| m.len())
            .unwrap_or(0);
        Self { offset }
    }
}

/// State for a DURABLE-TAIL `:attach` — watching a run whose stderr pipe this
/// process does not own (a live subworker, or another session's run). Carries
/// the run id plus the tail cursor the presenter advances each tick. Held in
/// `Session::attached_durable`.
#[derive(Clone, Debug)]
pub struct DurableAttach {
    /// The durable run id being tailed (`coordinator_runs.run_id`).
    pub run_id: String,
    /// Byte cursor into that run's activity log, anchored past the backfill the
    /// attach already rendered so the tail never re-prints replayed rows.
    pub tail: ActivityTail,
}

/// Every retained row of `run_id`'s log, oldest-first — the durable analogue of
/// [`crate::transcript_ring::TranscriptRing::backfill_tail`]. An absent or
/// unreadable log reads as EMPTY (callers must render the explicit
/// "no activity recorded yet" state rather than nothing).
#[cfg_attr(not(test), allow(dead_code))] // attach uses `backfill_with_cursor_in`; this is the cursorless read.
pub fn backfill_in(dir: &Path, run_id: &str) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(log_path_in(dir, run_id)) else {
        return Vec::new();
    };
    body.lines().filter_map(decode_row).collect()
}

/// Backfill AND anchor a tail cursor in ONE read, so the attach cannot lose a
/// row to the gap between "read the file" and "anchor at its end" (a coordinator
/// writing during the attach). The cursor lands exactly past the last COMPLETE
/// line returned: every subsequent row is delivered by [`read_new_in`], none
/// twice.
pub fn backfill_with_cursor_in(dir: &Path, run_id: &str) -> (Vec<String>, ActivityTail) {
    let Ok(body) = std::fs::read_to_string(log_path_in(dir, run_id)) else {
        return (Vec::new(), ActivityTail::default());
    };
    let consumed = match body.rfind('\n') {
        Some(i) => i + 1,
        None => 0,
    };
    let rows = body[..consumed].lines().filter_map(decode_row).collect();
    (
        rows,
        ActivityTail {
            offset: consumed as u64,
        },
    )
}

pub fn backfill_with_cursor(run_id: &str) -> (Vec<String>, ActivityTail) {
    backfill_with_cursor_in(&activity_dir(), run_id)
}

/// Rows appended since `tail` was last read, advancing `tail`. Only COMPLETE
/// lines are consumed, so a read that races a partial append re-reads that row
/// next tick instead of decoding a torn line. A file that SHRANK (the writer
/// compacted) re-anchors the cursor to the new end — see the module docs.
pub fn read_new_in(dir: &Path, run_id: &str, tail: &mut ActivityTail) -> Vec<String> {
    let path = log_path_in(dir, run_id);
    let Ok(meta) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    let len = meta.len();
    if len < tail.offset {
        // Compaction (or a truncate/replace): re-anchor rather than re-emit.
        tail.offset = len;
        return Vec::new();
    }
    if len == tail.offset {
        return Vec::new();
    }
    let Ok(mut f) = File::open(&path) else {
        return Vec::new();
    };
    if f.seek(SeekFrom::Start(tail.offset)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    let consumed = match buf.rfind('\n') {
        Some(i) => i + 1,
        None => return Vec::new(), // only a partial line so far
    };
    tail.offset += consumed as u64;
    buf[..consumed].lines().filter_map(decode_row).collect()
}

pub fn read_new(run_id: &str, tail: &mut ActivityTail) -> Vec<String> {
    read_new_in(&activity_dir(), run_id, tail)
}

/// The non-blocking write handle held by the stderr drain. Dropping it closes
/// the channel, which drains and ends the writer thread.
pub struct ActivityLogWriter {
    tx: Option<Sender<String>>,
    #[cfg_attr(not(test), allow(dead_code))]
    // only `close()` (tests) joins it; production just drops the channel.
    thread: Option<JoinHandle<()>>,
}

impl ActivityLogWriter {
    /// Open (create/append) `run_id`'s log under the default [`activity_dir`]
    /// and spawn its writer thread. `None` when the file can't be opened —
    /// durable activity is best-effort and must never fail a coordinator launch.
    pub fn open(run_id: &str) -> Option<Self> {
        Self::open_in(&activity_dir(), run_id, MAX_BYTES)
    }

    /// `open` with an explicit directory + byte cap (tests drive compaction with
    /// a tiny cap and a temp dir).
    pub fn open_in(dir: &Path, run_id: &str, max_bytes: u64) -> Option<Self> {
        let path = log_path_in(dir, run_id);
        let _ = std::fs::create_dir_all(dir);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let (tx, rx) = channel::<String>();
        let thread = std::thread::Builder::new()
            .name("aish-activity-log".into())
            .spawn(move || drain_loop(file, path, rx, max_bytes))
            .ok()?;
        Some(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// Queue one forwardable row. NEVER blocks on IO: this is called from the
    /// stderr-drain hot path, so it only pushes onto an unbounded channel. A
    /// dead writer thread (disk gone) makes this a silent no-op — losing
    /// durable activity is acceptable, stalling a coordinator is not.
    pub fn record(&self, text: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(encode_row(text));
        }
    }

    /// Close the channel and WAIT for the writer thread to flush everything it
    /// was handed. Tests need this to read deterministically; production just
    /// drops the writer (same close, without the join).
    #[cfg_attr(not(test), allow(dead_code))] // production drops the writer (same close, without blocking on the join).
    pub fn close(mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ActivityLogWriter {
    fn drop(&mut self) {
        // Close the channel so the writer thread drains and exits. Deliberately
        // NOT joined here: a drop on a tokio worker thread must not block on IO.
        self.tx.take();
    }
}

/// The writer thread: block on the channel, batch everything already queued into
/// ONE `write_all`, flush, then compact if the file outgrew the cap.
fn drain_loop(mut file: File, path: PathBuf, rx: Receiver<String>, max_bytes: u64) {
    let mut bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    while let Ok(first) = rx.recv() {
        let mut batch = first;
        // Coalesce the rest of the burst — one syscall per burst, not per line.
        while let Ok(next) = rx.try_recv() {
            batch.push_str(&next);
            if batch.len() > 256 * 1024 {
                break;
            }
        }
        if file.write_all(batch.as_bytes()).is_err() {
            return;
        }
        let _ = file.flush();
        bytes += batch.len() as u64;
        if bytes > max_bytes {
            // A compaction failure (disk full, permissions) returns `None` and
            // leaves `file`/`bytes` untouched: keep appending rather than
            // dropping activity, and retry on the next burst.
            if let Some((f, n)) = compact(&path, max_bytes / 2) {
                file = f;
                bytes = n;
            }
        }
    }
    let _ = file.flush();
}

/// Rewrite `path` keeping only its most recent rows (newest-first until
/// `keep_bytes` is reached), via temp-file + rename so a concurrent reader sees
/// either the old or the new file, never a half-written one. Returns the reopened
/// append handle and the new size.
fn compact(path: &Path, keep_bytes: u64) -> Option<(File, u64)> {
    let body = std::fs::read_to_string(path).ok()?;
    let mut keep: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for line in body.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        total += line.len() + 1;
        if total as u64 > keep_bytes && !keep.is_empty() {
            break;
        }
        keep.push(line);
    }
    keep.reverse();
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut t = File::create(&tmp).ok()?;
        for line in &keep {
            t.write_all(line.as_bytes()).ok()?;
            t.write_all(b"\n").ok()?;
        }
        t.flush().ok()?;
    }
    std::fs::rename(&tmp, path).ok()?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some((file, len))
}

/// Delete `run_id`'s durable activity log (housekeeping for `:forget` / store
/// pruning). Best-effort: a missing file is success.
#[cfg_attr(not(test), allow(dead_code))] // retention runs via `sweep_older_than`; targeted removal is for tests/future `:forget`.
pub fn remove_in(dir: &Path, run_id: &str) {
    let _ = std::fs::remove_file(log_path_in(dir, run_id));
}

/// Delete logs in `dir` whose last write is older than `max_age`. Per-run files
/// are each capped at [`MAX_BYTES`], but a tenant that launches thousands of
/// coordinators would still accumulate FILES — so the REPL sweeps the directory
/// once at startup (next to the coordinator-store rehydrate). Returns how many
/// logs were removed. Best-effort: unreadable entries are skipped.
#[cfg_attr(not(test), allow(dead_code))] // production sweeps the default dir via `sweep_older_than`.
pub fn sweep_older_than_in(dir: &Path, max_age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let aged = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if aged && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Sweep the default [`activity_dir`]. Logs older than `max_age_days` go.
pub fn sweep_older_than(max_age_days: u64) -> usize {
    sweep_older_than_in(
        &activity_dir(),
        std::time::Duration::from_secs(max_age_days * 24 * 60 * 60),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "aish-activity-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn row_round_trips_byte_exactly_through_jsonl() {
        // A real forwardable row: ANSI colour, a tool glyph, a quote, a tab, and
        // a backslash — everything that would break a naive line format.
        let text = "\x1b[32m✓\x1b[0m 🔧 atum_list_tasks \"sprint\"\tdone\\now";
        let encoded = encode_row(text);
        assert!(encoded.ends_with('\n'));
        assert_eq!(encoded.matches('\n').count(), 1, "one row = one line");
        assert_eq!(decode_row(encoded.trim_end()).as_deref(), Some(text));
    }

    #[test]
    fn malformed_and_blank_lines_decode_as_absent() {
        assert_eq!(decode_row(""), None);
        assert_eq!(decode_row("   "), None);
        assert_eq!(decode_row("{\"ts\":1,\"tex"), None); // torn write
        assert_eq!(decode_row("{\"ts\":1}"), None); // no text field
    }

    #[test]
    fn writer_persists_rows_and_backfill_reads_them_in_order() {
        let dir = tmpdir("persist");
        let w = ActivityLogWriter::open_in(&dir, "w_test01", MAX_BYTES).unwrap();
        w.record("  🚀 round 1");
        w.record("✓ 🔧 read_file src/worker.rs");
        w.record("  💭 thinking…");
        w.close();
        assert_eq!(
            backfill_in(&dir, "w_test01"),
            vec![
                "  🚀 round 1".to_string(),
                "✓ 🔧 read_file src/worker.rs".to_string(),
                "  💭 thinking…".to_string(),
            ]
        );
    }

    #[test]
    fn missing_log_backfills_empty_rather_than_failing() {
        let dir = tmpdir("missing");
        assert!(backfill_in(&dir, "w_nope").is_empty());
        let mut tail = ActivityTail::default();
        assert!(read_new_in(&dir, "w_nope", &mut tail).is_empty());
        assert_eq!(tail, ActivityTail::default());
    }

    #[test]
    fn tail_cursor_yields_each_row_once_then_only_new_ones() {
        let dir = tmpdir("tail");
        let w = ActivityLogWriter::open_in(&dir, "w_tail", MAX_BYTES).unwrap();
        w.record("row one");
        w.close();
        let mut tail = ActivityTail::default();
        assert_eq!(read_new_in(&dir, "w_tail", &mut tail), vec!["row one"]);
        // Nothing new → no re-emit (the live pane must not duplicate).
        assert!(read_new_in(&dir, "w_tail", &mut tail).is_empty());
        let w = ActivityLogWriter::open_in(&dir, "w_tail", MAX_BYTES).unwrap();
        w.record("row two");
        w.close();
        assert_eq!(read_new_in(&dir, "w_tail", &mut tail), vec!["row two"]);
        assert!(read_new_in(&dir, "w_tail", &mut tail).is_empty());
    }

    #[test]
    fn at_end_cursor_skips_the_backfilled_window() {
        let dir = tmpdir("atend");
        let w = ActivityLogWriter::open_in(&dir, "w_end", MAX_BYTES).unwrap();
        w.record("already shown by backfill");
        w.close();
        let mut tail = ActivityTail::at_end_in(&dir, "w_end");
        assert!(read_new_in(&dir, "w_end", &mut tail).is_empty());
        let w = ActivityLogWriter::open_in(&dir, "w_end", MAX_BYTES).unwrap();
        w.record("brand new");
        w.close();
        assert_eq!(read_new_in(&dir, "w_end", &mut tail), vec!["brand new"]);
    }

    #[test]
    fn partial_final_line_is_not_consumed_until_complete() {
        let dir = tmpdir("partial");
        let path = log_path_in(&dir, "w_partial");
        std::fs::write(&path, "{\"ts\":1,\"text\":\"complete\"}\n{\"ts\":2,\"te").unwrap();
        let mut tail = ActivityTail::default();
        assert_eq!(read_new_in(&dir, "w_partial", &mut tail), vec!["complete"]);
        // Writer finishes the torn row.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"xt\":\"second\"}\n").unwrap();
        assert_eq!(read_new_in(&dir, "w_partial", &mut tail), vec!["second"]);
    }

    #[test]
    fn log_is_bounded_by_compaction_and_keeps_the_newest_rows() {
        let dir = tmpdir("bounded");
        // 2 KiB cap → compacts to ≤1 KiB, keeping the newest rows.
        let w = ActivityLogWriter::open_in(&dir, "w_big", 2048).unwrap();
        for i in 0..400 {
            w.record(&format!("row {i:04} {}", "x".repeat(40)));
        }
        w.close();
        let len = std::fs::metadata(log_path_in(&dir, "w_big")).unwrap().len();
        assert!(len <= 2048 + 4096, "log must stay bounded, got {len} bytes");
        let rows = backfill_in(&dir, "w_big");
        assert!(!rows.is_empty(), "compaction must retain the newest rows");
        assert!(
            rows.last().unwrap().starts_with("row 0399"),
            "newest row must survive compaction, got {:?}",
            rows.last()
        );
        assert!(
            !rows.iter().any(|r| r.starts_with("row 0000")),
            "oldest rows must be evicted"
        );
    }

    #[test]
    fn tail_reanchors_after_compaction_instead_of_replaying() {
        let dir = tmpdir("reanchor");
        let path = log_path_in(&dir, "w_shrink");
        std::fs::write(&path, encode_row("a") + &encode_row("b")).unwrap();
        let mut tail = ActivityTail::default();
        assert_eq!(read_new_in(&dir, "w_shrink", &mut tail).len(), 2);
        // Simulate compaction: file replaced by a SHORTER one.
        std::fs::write(&path, encode_row("c")).unwrap();
        assert!(
            read_new_in(&dir, "w_shrink", &mut tail).is_empty(),
            "a shrunk log re-anchors; it must not re-emit the retained window"
        );
        let w = ActivityLogWriter::open_in(&dir, "w_shrink", MAX_BYTES).unwrap();
        w.record("d");
        w.close();
        assert_eq!(read_new_in(&dir, "w_shrink", &mut tail), vec!["d"]);
    }

    #[test]
    fn run_id_is_sanitized_into_a_single_flat_file() {
        let dir = tmpdir("sanitize");
        let p = log_path_in(&dir, "../../etc/passwd");
        assert_eq!(p.parent().unwrap(), dir.as_path());
        // `/` is the dangerous byte — it is what would let a crafted id escape
        // the activity dir — so it maps to `-`. `.` is retained (run ids never
        // start with one) and is harmless once every separator is gone: the
        // result is a single flat file inside `dir`.
        assert_eq!(p.file_name().unwrap(), "..-..-etc-passwd.jsonl");
    }

    #[test]
    fn remove_deletes_the_log_and_is_idempotent() {
        let dir = tmpdir("remove");
        let w = ActivityLogWriter::open_in(&dir, "w_rm", MAX_BYTES).unwrap();
        w.record("x");
        w.close();
        assert!(log_path_in(&dir, "w_rm").exists());
        remove_in(&dir, "w_rm");
        assert!(!log_path_in(&dir, "w_rm").exists());
        remove_in(&dir, "w_rm"); // no panic on a missing file
    }
}
