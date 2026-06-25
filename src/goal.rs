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

use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// What the loop is doing right now within a turn (work → check), or between turns.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    Idle,
    Working,
    Checking,
}

pub struct Goal {
    pub condition: String,
    /// When the goal was first spawned — basis for total elapsed time.
    started: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    status: Status,
    turns: usize,
    last_reason: Option<String>,
    cancel: bool,
    /// When the current turn began — basis for per-turn elapsed time.
    turn_started: Option<Instant>,
    /// Current phase within the loop.
    phase: Step,
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

    /// One-line `:goal` status report — includes overall + current-turn elapsed
    /// and the current phase while active; a finished goal shows just the final
    /// state and total elapsed.
    pub fn status_line(&self) -> String {
        let i = self.inner.lock().unwrap();
        let total = fmt_duration(self.started.elapsed());
        let reason = i
            .last_reason
            .as_deref()
            .map(|r| format!(" · last check: {r}"))
            .unwrap_or_default();
        let condition = truncate_condition(&self.condition);

        if i.status == Status::Active {
            let phase = match i.phase {
                Step::Working => "working",
                Step::Checking => "checking",
                Step::Idle => "starting",
            };
            // Per-turn elapsed only makes sense once a turn is under way.
            let this_turn = match i.turn_started {
                Some(t) => format!("{} this turn / {} total", fmt_duration(t.elapsed()), total),
                None => format!("{total} total"),
            };
            format!(
                "goal [active · {phase}] · turn {} · {this_turn} · {condition}{reason}",
                i.turns
            )
        } else {
            let state = match i.status {
                Status::Achieved => "achieved",
                Status::Failed => "failed",
                Status::Cleared => "cleared",
                Status::Active => unreachable!(),
            };
            format!(
                "goal [{state}] · {} turn(s) · {total} total · {condition}{reason}",
                i.turns
            )
        }
    }

    fn set(&self, status: Status, reason: Option<String>) {
        let mut i = self.inner.lock().unwrap();
        i.status = status;
        if reason.is_some() {
            i.last_reason = reason;
        }
    }
}

/// Human-readable elapsed time: `45s`, `4m12s`, `1h03m`. Coarse on purpose —
/// seconds drop off once we're past an hour.
fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Keep the condition snippet on one line — long goals get an ellipsis.
fn truncate_condition(condition: &str) -> String {
    const MAX: usize = 60;
    if condition.chars().count() > MAX {
        let head: String = condition.chars().take(MAX).collect();
        format!("{}…", head.trim_end())
    } else {
        condition.to_string()
    }
}

/// Start pursuing `condition` in the background. The work runs as full-tool
/// coordinator subprocesses (`worker::run_once`); the verifier judges on
/// `model` (the batch model). Returns a handle the REPL reads for `:goal` status.
pub fn spawn(
    condition: String,
    spec: crate::worker::WorkerSpec,
    model: String,
    cred: crate::backend::claude::Credential,
) -> Handle {
    let goal = Arc::new(Goal {
        condition,
        started: Instant::now(),
        inner: Mutex::new(Inner {
            status: Status::Active,
            turns: 0,
            last_reason: None,
            cancel: false,
            turn_started: None,
            phase: Step::Idle,
        }),
    });
    tokio::spawn(run_goal(goal.clone(), spec, model, cred));
    goal
}

async fn run_goal(
    goal: Handle,
    spec: crate::worker::WorkerSpec,
    model: String,
    cred: crate::backend::claude::Credential,
) {
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
                announce(&format!(
                    "stopped — hit the {MAX_TURNS}-turn backstop without meeting the goal"
                ));
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
        {
            let mut i = goal.inner.lock().unwrap();
            i.turn_started = Some(Instant::now());
            i.phase = Step::Working;
        }
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
        goal.inner.lock().unwrap().phase = Step::Checking;
        match judge(&cred, &model, &goal.condition, &output).await {
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
    cred: &crate::backend::claude::Credential,
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
        // Shaped per credential (OAuth needs the Claude Code identity block).
        "system": cred.system_value(
            "You are a strict completion judge for an autonomous agent. Decide whether the \
    GOAL is DEMONSTRABLY met by the WORK OUTPUT — judge only what the output shows as evidence (command \
    results, file contents, exit codes), never what is merely asserted without proof. If the goal \
    states a turn/time bound, honor it. Reply with ONLY a JSON object, no prose: \
    {\"met\": true|false, \"reason\": \"<one sentence>\"}.",
        ),
        "messages": [{
            "role": "user",
            "content": format!("GOAL:\n{condition}\n\nWORK OUTPUT:\n{work}")
        }]
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let req = client
        .post(MESSAGES_API)
        .header("anthropic-version", "2023-06-01");
    let resp = cred
        .apply(req)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("judge request failed: {e}"))?;
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("judge returned non-JSON: {e}"))?;
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(format!("judge api error: {msg}"));
    }
    let text = v["content"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|b| {
                if b["type"] == "text" {
                    b["text"].as_str()
                } else {
                    None
                }
            })
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
    let reason = parsed["reason"]
        .as_str()
        .unwrap_or("(no reason given)")
        .to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_buckets() {
        assert_eq!(fmt_duration(Duration::from_secs(0)), "0s");
        assert_eq!(fmt_duration(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_duration(Duration::from_secs(59)), "59s");
        // 4m12s
        assert_eq!(fmt_duration(Duration::from_secs(4 * 60 + 12)), "4m12s");
        // seconds zero-padded within a minute
        assert_eq!(fmt_duration(Duration::from_secs(60 + 5)), "1m05s");
        // 1h03m — past an hour, seconds drop off, minutes zero-padded
        assert_eq!(
            fmt_duration(Duration::from_secs(3600 + 3 * 60 + 9)),
            "1h03m"
        );
        assert_eq!(fmt_duration(Duration::from_secs(3600)), "1h00m");
    }

    #[test]
    fn truncate_condition_ellipsizes() {
        assert_eq!(truncate_condition("short goal"), "short goal");
        let long = "a".repeat(80);
        let out = truncate_condition(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 61); // 60 + ellipsis
    }

    /// Build a Goal directly (bypassing spawn's background loop) to assert wording.
    fn goal_with(status: Status, phase: Step, turn_started: bool) -> Goal {
        Goal {
            condition: "Complete the work".to_string(),
            started: Instant::now(),
            inner: Mutex::new(Inner {
                status,
                turns: 3,
                last_reason: Some("not yet".to_string()),
                cancel: false,
                turn_started: turn_started.then(Instant::now),
                phase,
            }),
        }
    }

    #[test]
    fn status_line_active_shows_phase_and_turn() {
        let g = goal_with(Status::Active, Step::Checking, true);
        let line = g.status_line();
        assert!(line.contains("goal [active · checking]"), "got: {line}");
        assert!(line.contains("turn 3"), "got: {line}");
        assert!(line.contains("this turn /"), "got: {line}");
        assert!(line.contains("total"), "got: {line}");
        assert!(line.contains("Complete the work"), "got: {line}");
        assert!(line.contains("last check: not yet"), "got: {line}");
        assert!(!line.contains('\n'), "must be one line: {line}");
    }

    #[test]
    fn status_line_finished_omits_phase_and_perturn() {
        let g = goal_with(Status::Achieved, Step::Idle, false);
        let line = g.status_line();
        assert!(line.contains("goal [achieved]"), "got: {line}");
        assert!(line.contains("3 turn(s)"), "got: {line}");
        assert!(line.contains("total"), "got: {line}");
        assert!(!line.contains("this turn"), "got: {line}");
        assert!(!line.contains("checking"), "got: {line}");
        assert!(!line.contains("working"), "got: {line}");
    }
}
