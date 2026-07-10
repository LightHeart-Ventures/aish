//! TASK-268 §5.5 — Handler audit trail.
//!
//! Every webhook handler outcome (executed, filtered, or failed) is turned into
//! an [`AuditRecord`] and handed to an [`AuditSink`] so plugin activity is
//! durably logged and auditable. The dispatcher owns an `Arc<dyn AuditSink>`;
//! by default it is a [`NoopAuditSink`].
//!
//! Two concrete sinks ship here:
//!
//! * [`JsonlAuditSink`] — append-only, one JSON line per record, split per
//!   plugin (`<dir>/<plugin_id>.jsonl`). This is the dependency-free, on-disk
//!   realization of the plugin-memory `{plugin_id}:webhook_inputs` trail; when
//!   aish wires the dispatcher into the REPL it can supply its own
//!   plugin-memory-backed sink implementing the same trait.
//! * [`MemoryAuditSink`] — in-memory buffer, primarily for tests and
//!   integration harnesses.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::dispatcher::HandlerOutcome;
use crate::error::Result;

/// A single auditable handler outcome. Serializable so any sink can persist it
/// verbatim (JSONL file, plugin memory store, remote log, …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRecord {
    /// Delivery id of the webhook that triggered the handler.
    pub webhook_id: String,
    pub tenant_id: String,
    pub plugin_id: String,
    pub event_type: String,
    /// Event matched the handler's subscription.
    pub matched: bool,
    /// Passed filters and was fork/exec'd.
    pub executed: bool,
    /// Process exit code (None if it never ran or was killed).
    pub exit_code: Option<i32>,
    /// `exit_code == Some(0)`.
    pub success: bool,
    /// Populated on spawn failure or timeout.
    pub error: Option<String>,
    pub duration_ms: u128,
    /// Wall-clock time the record was minted (unix epoch millis).
    pub recorded_at_ms: u128,
}

impl AuditRecord {
    /// Mint a record from a dispatch outcome plus the originating webhook's
    /// identity. Note [`HandlerOutcome`] carries the plugin/event but not the
    /// webhook id/tenant, so those are threaded in here.
    pub fn from_outcome(webhook_id: &str, tenant_id: &str, o: &HandlerOutcome) -> Self {
        Self {
            webhook_id: webhook_id.to_string(),
            tenant_id: tenant_id.to_string(),
            plugin_id: o.plugin_id.clone(),
            event_type: o.event_type.clone(),
            matched: o.matched,
            executed: o.executed,
            exit_code: o.exit_code,
            success: o.success,
            error: o.error.clone(),
            duration_ms: o.duration_ms,
            recorded_at_ms: now_ms(),
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Sink for handler audit records. Implementations must be cheap to call and
/// must not panic — the dispatcher logs and swallows any error so a broken
/// sink never blocks handler dispatch.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, rec: &AuditRecord) -> Result<()>;
}

/// Discards every record. The dispatcher's default.
#[derive(Debug, Default, Clone)]
pub struct NoopAuditSink;

#[async_trait::async_trait]
impl AuditSink for NoopAuditSink {
    async fn record(&self, _rec: &AuditRecord) -> Result<()> {
        Ok(())
    }
}

/// Append-only JSONL sink, one file per plugin under `dir`.
#[derive(Debug, Clone)]
pub struct JsonlAuditSink {
    dir: PathBuf,
}

impl JsonlAuditSink {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The on-disk path a given plugin's records land in.
    pub fn path_for(&self, plugin_id: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", sanitize_plugin_id(plugin_id)))
    }
}

/// Keep a plugin id safe as a filename component: no path separators, no
/// traversal. Anything outside `[A-Za-z0-9._-]` collapses to `_`, and a bare
/// empty/dotted id falls back to `unknown`.
fn sanitize_plugin_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[async_trait::async_trait]
impl AuditSink for JsonlAuditSink {
    async fn record(&self, rec: &AuditRecord) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut line = serde_json::to_string(rec)?;
        line.push('\n');
        tokio::fs::create_dir_all(&self.dir).await?;
        let path = self.path_for(&rec.plugin_id);
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        // tokio::fs::File buffers writes internally and its Drop cannot await a
        // flush, so without this an append can be silently lost (a subsequent
        // read sees fewer lines than were written). Flush before returning.
        f.flush().await?;
        Ok(())
    }
}

/// Default ring cap for [`MemoryAuditSink`]: retain at most this many records.
pub const DEFAULT_MEMORY_MAX_ENTRIES: usize = 100;
/// Default ring cap for [`MemoryAuditSink`]: retain at most ~1 MiB of records
/// (measured as the summed serialized-JSON byte size of retained records).
pub const DEFAULT_MEMORY_MAX_BYTES: usize = 1024 * 1024;

/// One retained record plus its cached serialized size (so eviction can
/// decrement the running byte total without re-serializing).
#[derive(Debug)]
struct MemEntry {
    rec: AuditRecord,
    size: usize,
}

/// Ring-buffer contents guarded by the sink's mutex.
#[derive(Debug, Default)]
struct MemRing {
    entries: VecDeque<MemEntry>,
    /// Running sum of retained `MemEntry::size`.
    bytes: usize,
    /// Count of records evicted (oldest-first) to stay under the caps.
    dropped: u64,
}

/// In-memory, bounded ring buffer of audit records (TASK-380). Retains the most
/// recent records up to BOTH a count cap and an approximate byte cap, evicting
/// oldest-first when either is exceeded; the newest record is never evicted.
/// Backs the plugin-memory `:webhook logs` trail and test/integration harnesses
/// without growing without bound.
#[derive(Debug)]
pub struct MemoryAuditSink {
    ring: Mutex<MemRing>,
    max_entries: usize,
    max_bytes: usize,
}

impl Default for MemoryAuditSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryAuditSink {
    /// Sink with the default caps ([`DEFAULT_MEMORY_MAX_ENTRIES`] /
    /// [`DEFAULT_MEMORY_MAX_BYTES`]).
    pub fn new() -> Self {
        Self::with_caps(DEFAULT_MEMORY_MAX_ENTRIES, DEFAULT_MEMORY_MAX_BYTES)
    }

    /// Sink with explicit caps. Each cap is clamped to a minimum of 1 so the
    /// buffer always retains at least the newest record.
    pub fn with_caps(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            ring: Mutex::new(MemRing::default()),
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
        }
    }

    /// Snapshot of everything currently retained (oldest → newest).
    pub fn records(&self) -> Vec<AuditRecord> {
        self.ring
            .lock()
            .expect("audit mutex poisoned")
            .entries
            .iter()
            .map(|e| e.rec.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.ring.lock().expect("audit mutex poisoned").entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many records have been evicted (oldest-first) to honor the caps.
    pub fn dropped(&self) -> u64 {
        self.ring.lock().expect("audit mutex poisoned").dropped
    }

    /// Approximate retained byte size (summed serialized-JSON lengths).
    pub fn byte_len(&self) -> usize {
        self.ring.lock().expect("audit mutex poisoned").bytes
    }
}

#[async_trait::async_trait]
impl AuditSink for MemoryAuditSink {
    async fn record(&self, rec: &AuditRecord) -> Result<()> {
        // Approximate footprint = serialized JSON length. Cheap and stable, and
        // it is exactly what an on-disk/JSONL realization of this trail costs.
        let size = serde_json::to_string(rec).map(|s| s.len()).unwrap_or(0);
        let mut ring = self.ring.lock().expect("audit mutex poisoned");
        ring.entries.push_back(MemEntry { rec: rec.clone(), size });
        ring.bytes = ring.bytes.saturating_add(size);
        // Evict oldest-first while over either cap, but always keep the record
        // we just pushed (len > 1 guard on the byte cap).
        while ring.entries.len() > self.max_entries
            || (ring.bytes > self.max_bytes && ring.entries.len() > 1)
        {
            match ring.entries.pop_front() {
                Some(old) => {
                    ring.bytes = ring.bytes.saturating_sub(old.size);
                    ring.dropped += 1;
                }
                None => break,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(plugin: &str, executed: bool, success: bool) -> HandlerOutcome {
        HandlerOutcome {
            plugin_id: plugin.to_string(),
            event_type: "pull_request".to_string(),
            matched: true,
            executed,
            exit_code: if executed { Some(if success { 0 } else { 1 }) } else { None },
            success,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            duration_ms: 3,
        }
    }

    #[test]
    fn record_from_outcome_threads_identity() {
        let rec = AuditRecord::from_outcome("d99", "acme", &outcome("gh", true, true));
        assert_eq!(rec.webhook_id, "d99");
        assert_eq!(rec.tenant_id, "acme");
        assert_eq!(rec.plugin_id, "gh");
        assert!(rec.matched && rec.executed && rec.success);
        assert_eq!(rec.exit_code, Some(0));
        assert!(rec.recorded_at_ms > 0);
    }

    #[test]
    fn sanitize_blocks_traversal() {
        assert_eq!(sanitize_plugin_id("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_plugin_id("gh/hub"), "gh_hub");
        assert_eq!(sanitize_plugin_id(""), "unknown");
        assert_eq!(sanitize_plugin_id(".."), "unknown");
        assert_eq!(sanitize_plugin_id("ok.plugin-1_2"), "ok.plugin-1_2");
    }

    #[tokio::test]
    async fn memory_sink_captures_records() {
        let sink = MemoryAuditSink::new();
        sink.record(&AuditRecord::from_outcome("d1", "t", &outcome("a", true, true)))
            .await
            .unwrap();
        sink.record(&AuditRecord::from_outcome("d1", "t", &outcome("b", true, false)))
            .await
            .unwrap();
        let recs = sink.records();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].plugin_id, "a");
        assert_eq!(recs[1].plugin_id, "b");
        assert!(!recs[1].success);
    }

    #[tokio::test]
    async fn memory_sink_ring_caps_entry_count() {
        // Cap at 3 entries; feed 5. Oldest two evicted, newest three retained.
        let sink = MemoryAuditSink::with_caps(3, DEFAULT_MEMORY_MAX_BYTES);
        for i in 0..5 {
            let o = outcome(&format!("p{i}"), true, true);
            sink.record(&AuditRecord::from_outcome("d", "t", &o))
                .await
                .unwrap();
        }
        let recs = sink.records();
        assert_eq!(recs.len(), 3, "retains only the cap");
        assert_eq!(sink.dropped(), 2, "two oldest evicted");
        // Oldest→newest: the surviving window is p2, p3, p4.
        assert_eq!(recs[0].plugin_id, "p2");
        assert_eq!(recs[2].plugin_id, "p4");
    }

    #[tokio::test]
    async fn memory_sink_ring_caps_bytes_and_keeps_newest() {
        // A single record serializes to well over 40 bytes, so a 40-byte cap
        // forces eviction down to the newest record every push.
        let sink = MemoryAuditSink::with_caps(DEFAULT_MEMORY_MAX_ENTRIES, 40);
        for i in 0..4 {
            let o = outcome(&format!("p{i}"), true, true);
            sink.record(&AuditRecord::from_outcome("d", "t", &o))
                .await
                .unwrap();
        }
        let recs = sink.records();
        assert_eq!(recs.len(), 1, "byte cap collapses to the newest record");
        assert_eq!(recs[0].plugin_id, "p3", "newest is never evicted");
        assert_eq!(sink.dropped(), 3);
        assert!(sink.byte_len() > 0);
    }

    #[test]
    fn memory_default_caps_are_bounded() {
        assert_eq!(DEFAULT_MEMORY_MAX_ENTRIES, 100);
        assert_eq!(DEFAULT_MEMORY_MAX_BYTES, 1024 * 1024);
        // Zero caps are clamped up to 1 so the newest record always survives.
        let sink = MemoryAuditSink::with_caps(0, 0);
        assert_eq!(sink.len(), 0);
    }

    #[tokio::test]
    async fn jsonl_sink_appends_per_plugin() {
        // Unique per invocation (pid + monotonic counter) so a reused PID or a
        // parallel run can never share this sink's directory.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("aish-audit-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = JsonlAuditSink::new(&dir);

        sink.record(&AuditRecord::from_outcome("d1", "t", &outcome("gh", true, true)))
            .await
            .unwrap();
        sink.record(&AuditRecord::from_outcome("d2", "t", &outcome("gh", true, false)))
            .await
            .unwrap();

        let path = sink.path_for("gh");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "two appended records");
        // Each line is a standalone, parseable record.
        let r0: AuditRecord = serde_json::from_str(lines[0]).unwrap();
        let r1: AuditRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r0.webhook_id, "d1");
        assert_eq!(r1.webhook_id, "d2");
        assert!(r0.success && !r1.success);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
