//! Anthropic Message Batches API client + in-session background batch jobs.
//!
//! Interactive batch mode (ported from atum_cli's `anthropic-batch.ts` +
//! `batch-tools.ts`): when on, the agent can offload a self-contained,
//! deferrable task to a background Anthropic Message Batches job (~50% cheaper,
//! asynchronous) with `run_in_background`, then fetch it on a later turn with
//! `batch_result`. Jobs live in memory for the session — the batch keeps running
//! platform-side even if aish exits, but the result handle is lost (fine for v1).
//!
//! Unlike the atum runner's map/reduce coordinator, this submits ONE request per
//! offload (no LLM-driven decomposition) — proportionate for a single-process
//! shell. The Message Batches API is Anthropic-only and GA (no beta header); it
//! needs a metered `ANTHROPIC_API_KEY` (a Claude subscription OAuth token won't
//! reach it).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const API_BASE: &str = "https://api.anthropic.com/v1/messages/batches";
const DEFAULT_MAX_TOKENS: u64 = 8192;
const POLL_INITIAL: Duration = Duration::from_secs(5);
const POLL_MAX: Duration = Duration::from_secs(60);
const POLL_WALL_CLOCK_CAP: Duration = Duration::from_secs(6 * 60 * 60); // 6h, matching atum

/// The model every background batch runs on. Batches are Anthropic-only, and the
/// user wants the strongest model for deferred work — so this is fixed to Opus
/// regardless of the interactive backend/model. Overridable via `:batch model`.
pub const DEFAULT_BATCH_MODEL: &str = "claude-opus-4-8";

/// A background batch job, tracked for the life of the session. Shared between
/// the REPL (which lists/fetches it) and the poll task (which mutates it).
pub struct BatchJob {
    /// Session-local handle, e.g. "batch_1" — how the model and `:batch` refer to it.
    pub id: String,
    pub task: String,
    inner: Mutex<JobInner>,
    /// Durable mirror. Every state change is written through here so the job
    /// survives a crash/exit and can be reattached on the next start. `None` only
    /// when the persistent store is unavailable.
    store: Option<crate::db::BatchStore>,
}

struct JobInner {
    /// "running" | a `processing_status` while polling | "done" | "failed".
    status: String,
    /// Rendered result text once the batch succeeds.
    result: Option<String>,
    /// Error message if the batch failed (auth, network, timeout, …).
    error: Option<String>,
}

pub type BatchJobs = Arc<Mutex<Vec<Arc<BatchJob>>>>;

impl BatchJob {
    fn set_status(&self, s: &str) {
        self.inner.lock().unwrap().status = s.to_string();
        if let Some(store) = &self.store {
            let _ = store.set_status(&self.id, s);
        }
    }
    fn set_anthropic_id(&self, anthropic_id: &str) {
        if let Some(store) = &self.store {
            let _ = store.set_anthropic_id(&self.id, anthropic_id);
        }
    }
    fn set_done(&self, summary: String) {
        {
            let mut i = self.inner.lock().unwrap();
            i.status = "done".into();
            i.result = Some(summary.clone());
        }
        if let Some(store) = &self.store {
            let _ = store.set_done(&self.id, &summary);
        }
    }
    fn set_failed(&self, err: String) {
        {
            let mut i = self.inner.lock().unwrap();
            i.status = "failed".into();
            i.error = Some(err.clone());
        }
        if let Some(store) = &self.store {
            let _ = store.set_failed(&self.id, &err);
        }
    }

    pub fn status(&self) -> String {
        self.inner.lock().unwrap().status.clone()
    }

    /// One line for `:batch status`.
    pub fn summary_line(&self) -> String {
        format!("{} [{}] {}", self.id, self.status(), self.task)
    }

    /// What `batch_result` returns: the rendered result, a failure note, or a
    /// still-running status.
    pub fn fetch(&self) -> String {
        let i = self.inner.lock().unwrap();
        match i.status.as_str() {
            "done" => i.result.clone().unwrap_or_else(|| "(empty result)".into()),
            "failed" => format!(
                "batch {} failed: {}",
                self.id,
                i.error.clone().unwrap_or_else(|| "unknown error".into())
            ),
            other => format!(
                "batch {} is still running (status: {other}). Call batch_result again on a later turn.",
                self.id
            ),
        }
    }
}

/// Register a new background batch and start its poll task. Returns the
/// session-local job id. `api_key` and `model` are captured up front so the
/// spawned task is self-contained. `store`, when present, persists the job so it
/// survives a restart.
pub fn spawn(
    jobs: &BatchJobs,
    task: String,
    model: String,
    api_key: String,
    store: Option<crate::db::BatchStore>,
) -> String {
    let mut guard = jobs.lock().unwrap();
    let n = guard
        .iter()
        .filter_map(|j| j.id.strip_prefix("batch_").and_then(|s| s.parse::<usize>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let id = format!("batch_{n}");
    if let Some(store) = &store {
        let _ = store.insert(&id, &task, &model);
    }
    let job = Arc::new(BatchJob {
        id: id.clone(),
        task: task.clone(),
        inner: Mutex::new(JobInner { status: "running".into(), result: None, error: None }),
        store,
    });
    guard.push(job.clone());
    drop(guard);

    tokio::spawn(run_batch(job, task, model, api_key));
    id
}

/// Rehydrate persisted batch jobs at startup: every stored job is restored into
/// the session (so `batch_result`/`:batch status` work across restarts), and any
/// still in flight — with an Anthropic id and an available API key — gets its
/// poll loop reattached so it finishes and announces as if we never exited.
pub fn rehydrate(session: &mut crate::session::Session) {
    let Some(store) = session.batch_store.clone() else {
        return;
    };
    let rows = match store.load_all() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("\x1b[2maish: couldn't load saved batch jobs: {e:#}\x1b[0m");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let mut resumed = 0usize;
    for row in rows {
        let terminal = matches!(row.status.as_str(), "done" | "failed");
        let job = Arc::new(BatchJob {
            id: row.local_id.clone(),
            task: row.task.clone(),
            inner: Mutex::new(JobInner {
                status: row.status.clone(),
                result: row.result.clone(),
                error: row.error.clone(),
            }),
            store: Some(store.clone()),
        });
        session.batch_jobs.lock().unwrap().push(job.clone());
        if terminal {
            continue;
        }
        match (row.anthropic_id, api_key.clone()) {
            (Some(aid), Some(key)) => {
                tokio::spawn(resume_batch(job, aid, key));
                resumed += 1;
            }
            // Crashed between registering and submitting — unrecoverable.
            (None, _) => job.set_failed("interrupted before submission (no Anthropic batch id)".into()),
            // No key this run: leave it running so a later start can reattach.
            (Some(_), None) => {}
        }
    }
    if resumed > 0 {
        eprintln!("\x1b[2maish: reattached {resumed} background batch job(s) from a previous session\x1b[0m");
    }
}

/// The poll task: submit one batch request, poll to completion, retrieve and
/// render the result, then announce on the user's terminal.
async fn run_batch(job: Arc<BatchJob>, task: String, model: String, api_key: String) {
    let client = BatchClient::new(api_key);
    match client.run(&task, &model, &job).await {
        Ok(summary) => job.set_done(summary),
        Err(e) => job.set_failed(format!("{e:#}")),
    }
    announce_completion(&job);
}

/// Reattach to an already-submitted batch (no `create`): poll the existing
/// Anthropic id to completion, retrieve, render, announce.
async fn resume_batch(job: Arc<BatchJob>, anthropic_id: String, api_key: String) {
    let client = BatchClient::new(api_key);
    let outcome = async {
        let ended = client.poll(&anthropic_id, &job).await?;
        let results_url = ended["results_url"]
            .as_str()
            .context("batch ended without a results_url")?;
        let results = client.results(results_url).await?;
        Ok::<_, anyhow::Error>(render_results(&results))
    }
    .await;
    match outcome {
        Ok(summary) => job.set_done(summary),
        Err(e) => job.set_failed(format!("{e:#}")),
    }
    announce_completion(&job);
}

/// Print a job's terminal state over the prompt (rustyline redraws on keypress).
fn announce_completion(job: &Arc<BatchJob>) {
    let line = match job.status().as_str() {
        "done" => format!("done — {} (batch_result \"{}\" to read it)", job.task, job.id),
        _ => format!("failed — {}", job.fetch()),
    };
    crate::tools::announce(&format!("[{}]", job.id), &line);
}

/// One flattened per-request result from a batch's `.jsonl` results.
struct BatchResult {
    custom_id: String,
    /// SDK discriminator: succeeded | errored | canceled | expired.
    kind: String,
    /// Joined text blocks when `kind == "succeeded"`.
    text: Option<String>,
}

/// Thin Message Batches client over reqwest. Mirrors aish's claude backend
/// conventions (x-api-key + anthropic-version headers, small retry on 429/5xx).
struct BatchClient {
    client: reqwest::Client,
    api_key: String,
}

impl BatchClient {
    fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            api_key,
        }
    }

    /// create → poll → retrieve results, end to end. Updates `job`'s status as it
    /// progresses so `:batch status` reflects live state.
    async fn run(&self, task: &str, model: &str, job: &Arc<BatchJob>) -> Result<String> {
        let requests = vec![json!({
            "custom_id": "task",
            "params": {
                "model": model,
                "max_tokens": DEFAULT_MAX_TOKENS,
                "messages": [{"role": "user", "content": task}],
            }
        })];

        let batch = self.create(requests).await?;
        let batch_id = batch["id"]
            .as_str()
            .context("batch create returned no id")?
            .to_string();
        // Persist the Anthropic id immediately — this is what a restart reattaches to.
        job.set_anthropic_id(&batch_id);

        let ended = self.poll(&batch_id, job).await?;
        let results_url = ended["results_url"]
            .as_str()
            .context("batch ended without a results_url")?;
        let results = self.results(results_url).await?;
        Ok(render_results(&results))
    }

    /// Submit a request set. Returns the created batch object (carries `.id`).
    async fn create(&self, requests: Vec<Value>) -> Result<Value> {
        self.post(API_BASE, &json!({ "requests": requests })).await
    }

    /// Poll until `processing_status == "ended"`, with exponential backoff
    /// clamped to POLL_MAX and a hard wall-clock cap.
    async fn poll(&self, batch_id: &str, job: &Arc<BatchJob>) -> Result<Value> {
        let url = format!("{API_BASE}/{batch_id}");
        let mut interval = POLL_INITIAL;
        let deadline = tokio::time::Instant::now() + POLL_WALL_CLOCK_CAP;
        loop {
            let v = self.get(&url).await?;
            let status = v["processing_status"].as_str().unwrap_or("");
            job.set_status(status);
            if status == "ended" {
                return Ok(v);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("batch {batch_id} did not complete within the time cap (last status: {status})");
            }
            tokio::time::sleep(interval).await;
            interval = (interval * 2).min(POLL_MAX);
        }
    }

    /// Fetch + flatten the `.jsonl` results. Each line is one BatchResult.
    async fn results(&self, url: &str) -> Result<Vec<BatchResult>> {
        let resp = self
            .client
            .get(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .context("batch results request failed")?;
        let text = resp.text().await.context("reading batch results")?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).context("malformed batch result line")?;
            out.push(flatten(&v));
        }
        Ok(out)
    }

    async fn post(&self, url: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::POST, url, Some(body)).await
    }

    async fn get(&self, url: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, url, None).await
    }

    async fn request(&self, method: reqwest::Method, url: &str, body: Option<&Value>) -> Result<Value> {
        let mut delay = Duration::from_secs(2);
        for attempt in 0..3 {
            let mut req = self
                .client
                .request(method.clone(), url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01");
            if let Some(b) = body {
                req = req.json(b);
            }
            match req.send().await {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let v: Value = r.json().await.context("batch api returned non-JSON")?;
                    if (200..300).contains(&status) {
                        return Ok(v);
                    }
                    let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
                    let kind = v["error"]["type"].as_str().unwrap_or("error");
                    if (status == 429 || status >= 500) && attempt < 2 {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    bail!("batch api {kind} ({status}): {msg}");
                }
                Err(e) if attempt < 2 => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    let _ = e;
                }
                Err(e) => return Err(e).context("batch api request failed"),
            }
        }
        unreachable!()
    }
}

/// Flatten one SDK individual-response line into a BatchResult.
fn flatten(v: &Value) -> BatchResult {
    let custom_id = v["custom_id"].as_str().unwrap_or("").to_string();
    let result = &v["result"];
    let kind = result["type"].as_str().unwrap_or("unknown").to_string();
    let mut text = None;
    if kind == "succeeded" {
        let mut s = String::new();
        if let Some(blocks) = result["message"]["content"].as_array() {
            for b in blocks {
                if b["type"] == "text" {
                    if let Some(t) = b["text"].as_str() {
                        s.push_str(t);
                    }
                }
            }
        }
        if !s.is_empty() {
            text = Some(s);
        }
    }
    BatchResult { custom_id, kind, text }
}

/// Render results for the model. A single-request offload returns just the text;
/// a multi-result batch is rendered per `custom_id`. Non-succeeded requests are
/// listed by type so nothing is silently dropped.
fn render_results(results: &[BatchResult]) -> String {
    if results.len() == 1 {
        let r = &results[0];
        return match (r.kind.as_str(), &r.text) {
            ("succeeded", Some(t)) => t.clone(),
            ("succeeded", None) => "(empty result)".into(),
            (k, _) => format!("batch request {k}"),
        };
    }
    let mut out = String::new();
    for r in results {
        match (&r.text, r.kind.as_str()) {
            (Some(t), "succeeded") => out.push_str(&format!("### {}\n{}\n", r.custom_id, t)),
            (_, k) => out.push_str(&format!("### {} — {}\n", r.custom_id, k)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_succeeded_joins_text_blocks() {
        let v = json!({
            "custom_id": "task",
            "result": {
                "type": "succeeded",
                "message": {"content": [
                    {"type": "text", "text": "hello "},
                    {"type": "thinking", "thinking": "ignored"},
                    {"type": "text", "text": "world"}
                ]}
            }
        });
        let r = flatten(&v);
        assert_eq!(r.kind, "succeeded");
        assert_eq!(r.text.as_deref(), Some("hello world"));
    }

    #[test]
    fn flatten_errored_has_no_text() {
        let v = json!({"custom_id": "task", "result": {"type": "errored", "error": {"type": "x"}}});
        let r = flatten(&v);
        assert_eq!(r.kind, "errored");
        assert_eq!(r.text, None);
    }

    #[test]
    fn render_single_succeeded_is_bare_text() {
        let rs = vec![BatchResult { custom_id: "task".into(), kind: "succeeded".into(), text: Some("answer".into()) }];
        assert_eq!(render_results(&rs), "answer");
    }

    #[test]
    fn render_single_failure_names_the_type() {
        let rs = vec![BatchResult { custom_id: "task".into(), kind: "expired".into(), text: None }];
        assert_eq!(render_results(&rs), "batch request expired");
    }

    #[test]
    fn render_multiple_groups_by_custom_id() {
        let rs = vec![
            BatchResult { custom_id: "a".into(), kind: "succeeded".into(), text: Some("A".into()) },
            BatchResult { custom_id: "b".into(), kind: "errored".into(), text: None },
        ];
        let out = render_results(&rs);
        assert!(out.contains("### a\nA"));
        assert!(out.contains("### b — errored"));
    }

    #[test]
    fn spawn_assigns_incrementing_ids() {
        let jobs: BatchJobs = Default::default();
        // No tokio runtime here, so the spawned poll task never runs — we only
        // assert the synchronous registration + id assignment.
        let mk = |jobs: &BatchJobs, id: usize| {
            jobs.lock().unwrap().push(Arc::new(BatchJob {
                id: format!("batch_{id}"),
                task: "t".into(),
                inner: Mutex::new(JobInner { status: "running".into(), result: None, error: None }),
                store: None,
            }));
        };
        mk(&jobs, 1);
        mk(&jobs, 2);
        // Next id computed the way spawn() does.
        let next = jobs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|j| j.id.strip_prefix("batch_").and_then(|s| s.parse::<usize>().ok()))
            .max()
            .unwrap_or(0)
            + 1;
        assert_eq!(next, 3);
    }

    #[test]
    fn fetch_reports_running_then_done() {
        let job = Arc::new(BatchJob {
            id: "batch_1".into(),
            task: "summarize".into(),
            inner: Mutex::new(JobInner { status: "running".into(), result: None, error: None }),
            store: None,
        });
        assert!(job.fetch().contains("still running"));
        job.set_done("the summary".into());
        assert_eq!(job.fetch(), "the summary");
        assert!(job.summary_line().contains("batch_1"));
        assert!(job.summary_line().contains("done"));
    }
}
