//! Background goal loop — a stopping-oracle modeled on Claude Code's `/goal`,
//! adapted to run as background batch work (non-blocking) and gated on `:batch`.
//!
//! Generator/verifier split: each turn a full-tool **worker** (the generator)
//! pursues the condition, then a separate **judge** call on the batch model (the
//! verifier) reads the worker's output and decides — yes/no + a one-line reason —
//! whether the goal is demonstrably met. A "no" feeds the reason forward as
//! guidance for the next turn; a "yes" delivers the result and stops.
//!
//! Stop/safety: like `/goal`, the real bound is a turn/time clause the user puts
//! in the condition (the judge reads it from the transcript). Because this runs
//! UNATTENDED in the background, we add a hard `MAX_TURNS` backstop so a
//! misjudged loop can't spend forever.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Unattended runaway backstop. `/goal` itself has none (the user can Ctrl-C);
/// a background loop can't be watched, so we cap it.
const MAX_TURNS: usize = 25;

const MESSAGES_API: &str = "https://api.anthropic.com/v1/messages";
/// Cap the work output handed to the judge so a chatty turn can't blow the
/// verifier's context.
const JUDGE_INPUT_CAP: usize = 16_000;

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Active,
    Achieved,
    Failed,
    Cleared,
}

pub struct Goal {
    pub condition: String,
    inner: Mutex<Inner>,
}

struct Inner {
    status: Status,
    turns: usize,
    last_reason: Option<String>,
    cancel: bool,
}

pub type Handle = Arc<Goal>;

impl Goal {
    /// True while the loop is still pursuing the goal.
    pub fn is_active(&self) -> bool {
        let i = self.inner.lock().unwrap();
        i.status == Status::Active && !i.cancel
    }

    /// Request the loop stop (it checks between turns; a worker turn already in
    /// flight finishes first).
    pub fn clear(&self) {
        let mut i = self.inner.lock().unwrap();
        i.cancel = true;
        if i.status == Status::Active {
            i.status = Status::Cleared;
        }
    }

    /// One-line `:goal` status report.
    pub fn status_line(&self) -> String {
        let i = self.inner.lock().unwrap();
        let state = match i.status {
            Status::Active => "active",
            Status::Achieved => "achieved",
            Status::Failed => "failed",
            Status::Cleared => "cleared",
        };
        let reason = i
            .last_reason
            .as_deref()
            .map(|r| format!(" · last check: {r}"))
            .unwrap_or_default();
        format!("goal [{state}] · {} turn(s) · {}{reason}", i.turns, self.condition)
    }

    fn set(&self, status: Status, reason: Option<String>) {
        let mut i = self.inner.lock().unwrap();
        i.status = status;
        if reason.is_some() {
            i.last_reason = reason;
        }
    }
}

/// Start pursuing `condition` in the background. The work runs as full-tool
/// coordinator subprocesses (`worker::run_once`); the verifier judges on
/// `model` (the batch model). Returns a handle the REPL reads for `:goal` status.
pub fn spawn(
    condition: String,
    spec: crate::worker::WorkerSpec,
    model: String,
    api_key: String,
) -> Handle {
    let goal = Arc::new(Goal {
        condition,
        inner: Mutex::new(Inner {
            status: Status::Active,
            turns: 0,
            last_reason: None,
            cancel: false,
        }),
    });
    tokio::spawn(run_goal(goal.clone(), spec, model, api_key));
    goal
}

async fn run_goal(goal: Handle, spec: crate::worker::WorkerSpec, model: String, api_key: String) {
    announce(&format!("started — {}", goal.condition));
    let mut guidance: Option<String> = None;

    loop {
        // Stop checks between turns.
        let turn = {
            let mut i = goal.inner.lock().unwrap();
            if i.cancel {
                return;
            }
            if i.turns >= MAX_TURNS {
                i.status = Status::Failed;
                drop(i);
                announce(&format!("stopped — hit the {MAX_TURNS}-turn backstop without meeting the goal"));
                return;
            }
            i.turns += 1;
            i.turns
        };

        // Generator: a full-tool worker pursues the goal with the latest guidance.
        let directive = match &guidance {
            Some(g) => format!(
                "Work toward this goal, then report what you did and the evidence:\n{}\n\n\
                 The goal is NOT yet met — last check said: {}",
                goal.condition, g
            ),
            None => format!(
                "Work toward this goal, then report what you did and the evidence:\n{}",
                goal.condition
            ),
        };
        announce(&format!("turn {turn}: working…"));
        let run_id = format!("goal-{}", uuid::Uuid::new_v4());
        let output = match crate::worker::run_once(&spec, &directive, &run_id).await {
            Ok(o) => o,
            Err(e) => {
                goal.set(Status::Failed, Some(e.clone()));
                announce(&format!("failed — {e}"));
                return;
            }
        };
        if !goal.is_active() {
            return;
        }

        // Verifier: the batch model judges whether the output demonstrates the goal.
        announce(&format!("turn {turn}: checking…"));
        match judge(&api_key, &model, &goal.condition, &output).await {
            Ok((true, reason)) => {
                goal.set(Status::Achieved, Some(reason.clone()));
                deliver(&goal, turn, &reason, &output);
                return;
            }
            Ok((false, reason)) => {
                goal.set(Status::Active, Some(reason.clone()));
                guidance = Some(reason);
            }
            Err(e) => {
                // Couldn't verify — keep going but record why; don't silently stop.
                let note = format!("could not verify this turn: {e}");
                goal.set(Status::Active, Some(note.clone()));
                guidance = Some(note);
            }
        }
    }
}

/// Ask the verifier (batch model) whether the goal is demonstrably met. Returns
/// `(met, reason)`. A strict judge: evidence in the output, not mere claims.
async fn judge(
    api_key: &str,
    model: &str,
    condition: &str,
    work: &str,
) -> Result<(bool, String), String> {
    let work = if work.chars().count() > JUDGE_INPUT_CAP {
        let head: String = work.chars().take(JUDGE_INPUT_CAP).collect();
        format!("{head}\n…(truncated)")
    } else {
        work.to_string()
    };
    let body = json!({
        "model": model,
        "max_tokens": 512,
        "system": "You are a strict completion judge for an autonomous agent. Decide whether the \
GOAL is DEMONSTRABLY met by the WORK OUTPUT — judge only what the output shows as evidence (command \
results, file contents, exit codes), never what is merely asserted without proof. If the goal \
states a turn/time bound, honor it. Reply with ONLY a JSON object, no prose: \
{\"met\": true|false, \"reason\": \"<one sentence>\"}.",
        "messages": [{
            "role": "user",
            "content": format!("GOAL:\n{condition}\n\nWORK OUTPUT:\n{work}")
        }]
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(MESSAGES_API)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("judge request failed: {e}"))?;
    let v: Value = resp.json().await.map_err(|e| format!("judge returned non-JSON: {e}"))?;
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(format!("judge api error: {msg}"));
    }
    let text = v["content"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find_map(|b| if b["type"] == "text" { b["text"].as_str() } else { None })
        })
        .unwrap_or("")
        .trim();
    // The judge should return bare JSON, but tolerate prose around a {...}.
    let parsed: Value = serde_json::from_str(text)
        .or_else(|_| {
            match (text.find('{'), text.rfind('}')) {
                (Some(s), Some(e)) if e > s => serde_json::from_str(&text[s..=e]),
                _ => serde_json::from_str(text), // re-raise the original error
            }
        })
        .map_err(|e| format!("couldn't parse judge verdict: {e} (got: {text})"))?;
    let met = parsed["met"].as_bool().unwrap_or(false);
    let reason = parsed["reason"].as_str().unwrap_or("(no reason given)").to_string();
    Ok((met, reason))
}

/// Print a transient `[goal]` progress/announce line over the prompt.
fn announce(line: &str) {
    crate::tools::announce("[goal]", line);
}

/// Deliver the achieved outcome over the prompt (rendered markdown), like a
/// finished batch result.
fn deliver(goal: &Handle, turns: usize, reason: &str, output: &str) {
    use std::io::Write;
    print!("\r\x1b[2K");
    println!("\x1b[2m── goal achieved ({turns} turn(s)) ──\x1b[0m");
    println!("\x1b[2m{reason}\x1b[0m");
    println!("{}", crate::md::render_stdout(output.trim()));
    let _ = goal; // handle kept for symmetry / future status integration
    std::io::stdout().flush().ok();
}
