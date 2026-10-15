//! Operator approval for sensitive-path writes in an unattended run (SEC-2.4).
//!
//! # Why this exists
//!
//! `sensitive.rs` classifies credential/key/identity paths, and `tools.rs`
//! gates every file operation through it. In an INTERACTIVE session a sensitive
//! path confirms — the operator is at the keyboard, so aish asks. In an
//! UNATTENDED run (a background coordinator, a headless `--task`) there was
//! nobody to ask, so the only safe answer was a hard **refusal**: the worker
//! simply could not touch `~/.aws/credentials`, `.env`, `~/.ssh/*`, ever.
//!
//! That refusal is correct as a DEFAULT and wrong as an ABSOLUTE. Plenty of
//! legitimate work needs exactly one sensitive write — add a profile to
//! `~/.aws/config`, append a key to `.env`, rotate a token — and the operator
//! IS reachable: a coordinator already has an always-surfaced channel to their
//! terminal (`message_console`, the `📣` sentinel). A `bool` gate that cannot
//! tell *"no"* from *"ask"* is the bug; this module is the missing third state.
//!
//! So: instead of refusing, an unattended worker **escalates**. It parks the
//! operation, surfaces a loud request on the operator's console, and blocks
//! until the operator answers `:approve <id>` / `:deny <id>` (or the request
//! times out). Approval is **one-shot**: it authorises exactly the one
//! `(op, path)` that asked, is consumed when read, and can never be persisted
//! as an `'a'`/`'d'` grant — the same clamp `sensitive::clamp_decision` applies
//! to an interactive sensitive prompt.
//!
//! # Transport: the filesystem, deliberately
//!
//! The requester and the decider are DIFFERENT PROCESSES (a coordinator child
//! and the operator's interactive aish — possibly several levels apart, since
//! coordinators nest). They share no channel back-up: the child's stderr is
//! one-way. So the broker is a directory of small JSON files under
//! `~/.aish/approvals/`:
//!
//! ```text
//! ~/.aish/approvals/
//! ├── <id>.request   written by the WORKER, read by the operator's REPL
//! └── <id>.verdict   written by the OPERATOR's REPL, read by the worker
//! ```
//!
//! A file-backed queue is boring on purpose: it survives a restart of either
//! side, needs no socket/port/daemon, works across the whole coordinator tree
//! at any nesting depth (every process resolves the same `$HOME`), and is
//! trivially inspectable when something goes wrong. The worker polls for its
//! verdict file — approval is a human-latency event, so a 250 ms poll is free.
//!
//! # Fail-closed everywhere
//!
//! Every failure mode lands on *deny*, never on *allow*:
//! - broker directory not creatable → [`Outcome::Unavailable`] → refuse
//! - no operator reachable (not nested) → never escalate at all → refuse
//! - verdict never arrives → [`Outcome::TimedOut`] → refuse
//! - malformed verdict file → treated as a deny
//! - feature disabled (`AISH_SENSITIVE_APPROVAL=0`) → refuse, exactly as before
//!
//! The *escalate* state only ever converts a refusal into a QUESTION. It can
//! never convert a refusal into a silent allow.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Directory holding the request/verdict files: `~/.aish/approvals/`.
///
/// Best-effort created on every call (idempotent), mirroring
/// [`crate::db_paths::db_dir`]. A creation failure is NOT swallowed into an
/// allow: callers map a missing directory to [`Outcome::Unavailable`], which
/// refuses.
pub fn approvals_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("approvals");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Env var that turns the escalation path OFF, restoring the previous
/// hard-refuse behaviour. Set to `0`/`false`/`off`/`no` to disable.
///
/// The feature is ON by default: an operator who never answers a request is no
/// worse off than before (the request times out and the operation refuses),
/// while an operator who DOES answer gains the ability to authorise work that
/// was previously impossible.
pub const ENABLE_ENV: &str = "AISH_SENSITIVE_APPROVAL";

/// Env var overriding how long a worker blocks waiting for a verdict, in
/// seconds. Default [`DEFAULT_TIMEOUT_SECS`].
pub const TIMEOUT_ENV: &str = "AISH_APPROVAL_TIMEOUT_SECS";

/// Default wait for an operator verdict: 10 minutes. Long enough that a human
/// who stepped away can still answer; short enough that an abandoned worker
/// does not hang a job queue forever.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// How often the worker checks for its verdict file. Approval is a
/// human-latency event, so a quarter-second poll is imperceptible and costs
/// one `stat` per tick.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Requests older than this are swept by [`sweep_stale`] — a worker that died
/// mid-wait must not leave a request in the operator's `:approvals` list
/// forever. Generous relative to [`DEFAULT_TIMEOUT_SECS`] so a live request is
/// never swept out from under a waiting worker.
const STALE_AFTER: Duration = Duration::from_secs(6 * 3600);

/// Parse a boolean-ish env value. Anything other than an explicit falsey token
/// is true — a typo must not silently disable a security-relevant channel in
/// the permissive direction, and here "enabled" is the SAFE default (it only
/// ever adds a question).
fn env_falsey(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Whether the escalate-instead-of-refuse path is enabled at all.
pub fn enabled() -> bool {
    match std::env::var(ENABLE_ENV) {
        Ok(v) => !env_falsey(&v),
        Err(_) => true,
    }
}

/// How long to wait for a verdict. A malformed or zero override falls back to
/// the default rather than degenerating into "never wait" (which would make
/// every sensitive write refuse and silently undo the feature).
pub fn timeout() -> Duration {
    let secs = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Can this process actually REACH an operator to ask?
///
/// Escalation is only honest when there is a console on the other end. A
/// background coordinator has one: its stderr is streamed by the parent aish,
/// and the `📣` sentinel is ALWAYS surfaced there (it bypasses the
/// `:worker-output` suppression gate). A truly detached run — a headless
/// `--task` with nobody watching — has nothing, so it must keep refusing
/// rather than block for ten minutes on a question no one will ever see.
///
/// `nested` is the caller's `session.nested` (true ⇒ spawned by another aish).
/// Pure, so the policy is unit-testable without a process tree.
pub fn can_ask_operator(nested: bool) -> bool {
    nested && enabled()
}

/// What the operator decided (or failed to decide).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Operator said yes. Authorises exactly the one `(op, path)` that asked.
    Approved,
    /// Operator said no.
    Denied,
    /// No verdict within [`timeout`].
    TimedOut,
    /// The broker itself could not be used (unwritable directory, etc.).
    Unavailable(String),
}

impl Outcome {
    pub fn approved(&self) -> bool {
        matches!(self, Outcome::Approved)
    }

    /// One-line reason to show the model/operator when the op does NOT proceed.
    /// `None` for [`Outcome::Approved`].
    pub fn refusal_tail(&self) -> Option<String> {
        match self {
            Outcome::Approved => None,
            Outcome::Denied => Some("the operator DENIED this request".to_string()),
            Outcome::TimedOut => Some(format!(
                "no operator answer within {}s — request expired",
                timeout().as_secs()
            )),
            Outcome::Unavailable(why) => {
                Some(format!("could not reach the approval broker ({why})"))
            }
        }
    }
}

/// A parked sensitive operation awaiting an operator verdict.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    /// Short unique id the operator types: `:approve <id>`.
    pub id: String,
    /// Worker label (`w_xxxxxxxx`) or `local` for a non-coordinator caller.
    pub worker: String,
    /// What the worker wants to do: `read` / `write` / `delete` / `run`.
    pub op: String,
    /// The sensitive path (or the command display, for an argv request).
    pub target: String,
    /// Which denylist entry matched — the *why*.
    pub reason: String,
    /// Unix seconds the request was filed.
    pub created: u64,
}

impl Request {
    /// Serialise to the one-line-per-field flat format used on disk.
    ///
    /// Hand-rolled rather than `serde_json` for one reason: the fields are
    /// operator-visible text (a path, a denylist reason) and this format cannot
    /// be tricked into re-interpreting a `"` or a `{` in a path as structure.
    /// Newlines are the only metacharacter, and they are escaped.
    fn encode(&self) -> String {
        fn esc(s: &str) -> String {
            s.replace('\\', "\\\\").replace('\n', "\\n")
        }
        format!(
            "id\t{}\nworker\t{}\nop\t{}\ntarget\t{}\nreason\t{}\ncreated\t{}\n",
            esc(&self.id),
            esc(&self.worker),
            esc(&self.op),
            esc(&self.target),
            esc(&self.reason),
            self.created
        )
    }

    /// Parse the flat format. Unknown keys are ignored; a record missing `id`
    /// or `target` is rejected (we must never show a half-read request as if it
    /// were a real one).
    fn decode(text: &str) -> Option<Request> {
        fn unesc(s: &str) -> String {
            s.replace("\\n", "\n").replace("\\\\", "\\")
        }
        let mut r = Request {
            id: String::new(),
            worker: String::new(),
            op: String::new(),
            target: String::new(),
            reason: String::new(),
            created: 0,
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('\t') else {
                continue;
            };
            match k {
                "id" => r.id = unesc(v),
                "worker" => r.worker = unesc(v),
                "op" => r.op = unesc(v),
                "target" => r.target = unesc(v),
                "reason" => r.reason = unesc(v),
                "created" => r.created = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
        if r.id.is_empty() || r.target.is_empty() {
            return None;
        }
        Some(r)
    }

    /// The loud, self-explanatory note the operator sees on their console.
    /// Includes the exact commands to answer with, because an operator who
    /// cannot remember the verb cannot approve anything.
    pub fn console_note(&self) -> String {
        format!(
            "APPROVAL NEEDED — worker wants to {} a sensitive path\n   path:   {}\n   reason: sensitive path ({})\n   answer: :approve {}   ·   :deny {}",
            self.op, self.target, self.reason, self.id, self.id
        )
    }

    /// One row for the operator's `:approvals` table.
    pub fn row(&self) -> String {
        format!(
            "{:<10} {:<12} {:<6} {} ({})",
            self.id, self.worker, self.op, self.target, self.reason
        )
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a short, collision-resistant request id. Not a secret — it is typed by
/// a human — so it only has to be unique among the handful of live requests.
fn mint_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    format!("a{:06x}", (nanos ^ (pid << 12)) & 0xff_ffff)
}

fn request_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.request"))
}

fn verdict_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.verdict"))
}

/// This process's worker label, for the request row. A coordinator carries it
/// in `AISH_WORKER_LABEL`; a non-coordinator reports `local`.
fn worker_label() -> String {
    std::env::var("AISH_WORKER_LABEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// File a request and BLOCK until the operator answers or [`timeout`] elapses.
///
/// Emits the request as a `📣` console sentinel on stderr — the coordinator's
/// one always-surfaced channel — so the operator sees it immediately even with
/// `:worker-output` off. Returns the verdict. Never panics; every error path
/// yields a non-approving [`Outcome`].
///
/// Callers MUST treat anything but [`Outcome::Approved`] as a refusal, and must
/// NOT cache an approval: the verdict file is consumed here, so a second
/// attempt at the same path asks again. That is the point — approval is
/// one-shot, exactly like an interactive sensitive `y`.
pub fn request_blocking(op: &str, target: &str, reason: &str) -> Outcome {
    if !enabled() {
        return Outcome::Unavailable(format!("{ENABLE_ENV} is disabled"));
    }
    let dir = approvals_dir();
    if !dir.is_dir() {
        return Outcome::Unavailable(format!("{} is not a directory", dir.display()));
    }
    sweep_stale(&dir);

    let req = Request {
        id: mint_id(),
        worker: worker_label(),
        op: op.to_string(),
        target: target.to_string(),
        reason: reason.to_string(),
        created: now_secs(),
    };
    if let Err(e) = std::fs::write(request_path(&dir, &req.id), req.encode()) {
        return Outcome::Unavailable(e.to_string());
    }

    // Surface it. `message_console`'s transport is line-oriented, so physical
    // newlines are encoded as CONSOLE_LINE_SEP and decoded by the parent's
    // `console_row` into aligned continuation lines.
    let note = req
        .console_note()
        .replace('\n', &crate::worker::CONSOLE_LINE_SEP.to_string());
    eprintln!("📣 {note}");

    let deadline = std::time::Instant::now() + timeout();
    loop {
        if let Some(outcome) = take_verdict(&dir, &req.id) {
            let _ = std::fs::remove_file(request_path(&dir, &req.id));
            return outcome;
        }
        if std::time::Instant::now() >= deadline {
            let _ = std::fs::remove_file(request_path(&dir, &req.id));
            return Outcome::TimedOut;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Read-and-consume a verdict file. `None` when no verdict has been written
/// yet. A verdict that does not literally say `allow` is a DENY — fail closed
/// on a truncated or garbled file.
fn take_verdict(dir: &Path, id: &str) -> Option<Outcome> {
    let p = verdict_path(dir, id);
    let text = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    Some(if parse_verdict(&text) {
        Outcome::Approved
    } else {
        Outcome::Denied
    })
}

/// Parse a verdict body into allow(true)/deny(false). Pure — unit-tested.
/// Only an exact leading `allow` token approves.
pub fn parse_verdict(text: &str) -> bool {
    text.lines()
        .next()
        .map(|l| l.trim().eq_ignore_ascii_case("allow"))
        .unwrap_or(false)
}

/// Every pending request, oldest first — the operator's `:approvals` list.
pub fn pending() -> Vec<Request> {
    let dir = approvals_dir();
    sweep_stale(&dir);
    let mut out: Vec<Request> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("request") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some(req) = Request::decode(&text)
        {
            out.push(req);
        }
    }
    out.sort_by_key(|r| r.created);
    out
}

/// Resolve an operator-typed id (exact, or an unambiguous prefix) to a pending
/// request. Prefix matching is a convenience for typing; an AMBIGUOUS prefix is
/// an error rather than a coin flip — approving the wrong sensitive write
/// because two ids shared three characters is exactly the mistake worth one
/// extra keystroke to avoid.
pub fn resolve(id: &str) -> Result<Request, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("no request id given".to_string());
    }
    let all = pending();
    if let Some(exact) = all.iter().find(|r| r.id == id) {
        return Ok(exact.clone());
    }
    let hits: Vec<&Request> = all.iter().filter(|r| r.id.starts_with(id)).collect();
    match hits.len() {
        0 => Err(format!("no pending approval matches '{id}'")),
        1 => Ok(hits[0].clone()),
        n => Err(format!(
            "'{id}' is ambiguous — {n} pending approvals match; use the full id"
        )),
    }
}

/// Record the operator's verdict for `id`. Returns the request it answered so
/// the caller can echo what was just authorised (an operator who approves a
/// request must SEE the path they approved — a bare "ok" is how the wrong file
/// gets written).
pub fn decide(id: &str, allow: bool) -> Result<Request, String> {
    let req = resolve(id)?;
    let dir = approvals_dir();
    let body = if allow { "allow" } else { "deny" };
    std::fs::write(verdict_path(&dir, &req.id), format!("{body}\n"))
        .map_err(|e| format!("could not write verdict: {e}"))?;
    Ok(req)
}

/// Drop requests whose worker is long gone. Best-effort and silent: a sweep
/// failure must never block a live request.
fn sweep_stale(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = now_secs().saturating_sub(STALE_AFTER.as_secs());
    for entry in rd.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str());
        if ext != Some("request") && ext != Some("verdict") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The one entry point callers use: ask the operator if we can, otherwise keep
/// refusing. Folds the reachability check into the request so no call site can
/// forget it (an escalation nobody can see is just a ten-minute hang).
///
/// `nested` is `session.nested`. Returns [`Outcome::Approved`] ONLY on an
/// explicit operator `:approve`.
pub fn escalate(nested: bool, op: &str, target: &str, reason: &str) -> Outcome {
    if !can_ask_operator(nested) {
        return Outcome::Unavailable("no operator console reachable from this run".to_string());
    }
    request_blocking(op, target, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str) -> Request {
        Request {
            id: id.to_string(),
            worker: "w_test1234".to_string(),
            op: "write".to_string(),
            target: "/home/u/.aws/credentials".to_string(),
            reason: "~/.aws".to_string(),
            created: 42,
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let r = req("a0000001");
        let back = Request::decode(&r.encode()).expect("decodes");
        assert_eq!(back, r);
    }

    #[test]
    fn decode_escapes_newlines_in_target() {
        let mut r = req("a0000002");
        r.target = "/tmp/odd\nname".to_string();
        let back = Request::decode(&r.encode()).expect("decodes");
        assert_eq!(back.target, "/tmp/odd\nname");
    }

    #[test]
    fn decode_rejects_record_without_id_or_target() {
        assert!(Request::decode("worker\tw_x\nop\twrite\n").is_none());
        assert!(Request::decode("id\ta1\nop\twrite\n").is_none());
    }

    #[test]
    fn decode_ignores_unknown_keys() {
        let text = "id\ta1\ntarget\t/x\nfuture\tsomething\n";
        let r = Request::decode(text).expect("decodes");
        assert_eq!(r.id, "a1");
        assert_eq!(r.target, "/x");
    }

    #[test]
    fn only_literal_allow_approves() {
        assert!(parse_verdict("allow"));
        assert!(parse_verdict("allow\n"));
        assert!(parse_verdict("ALLOW\n"));
        assert!(!parse_verdict("deny\n"));
        assert!(!parse_verdict(""));
        assert!(!parse_verdict("allowed-ish"));
        // Truncated / garbled → deny, never allow.
        assert!(!parse_verdict("\0\0"));
    }

    #[test]
    fn falsey_env_tokens_disable() {
        for v in ["0", "false", "off", "no", " OFF "] {
            assert!(env_falsey(v), "{v} should be falsey");
        }
        for v in ["1", "true", "on", "yes", "maybe"] {
            assert!(!env_falsey(v), "{v} should not be falsey");
        }
    }

    #[test]
    fn cannot_ask_operator_when_not_nested() {
        // A detached run has no console on the other end: never escalate.
        assert!(!can_ask_operator(false));
    }

    #[test]
    fn console_note_names_path_reason_and_both_verbs() {
        let note = req("a0000003").console_note();
        assert!(note.contains("/home/u/.aws/credentials"));
        assert!(note.contains("~/.aws"));
        assert!(note.contains(":approve a0000003"));
        assert!(note.contains(":deny a0000003"));
    }

    #[test]
    fn refusal_tail_present_for_every_non_approval() {
        assert!(Outcome::Approved.refusal_tail().is_none());
        assert!(Outcome::Denied.refusal_tail().is_some());
        assert!(Outcome::TimedOut.refusal_tail().is_some());
        assert!(
            Outcome::Unavailable("x".into())
                .refusal_tail()
                .expect("tail")
                .contains('x')
        );
    }

    #[test]
    fn approved_predicate_is_exclusive() {
        assert!(Outcome::Approved.approved());
        assert!(!Outcome::Denied.approved());
        assert!(!Outcome::TimedOut.approved());
        assert!(!Outcome::Unavailable(String::new()).approved());
    }

    #[test]
    fn timeout_falls_back_on_garbage_override() {
        // Zero/garbage must NOT degenerate into "never wait".
        assert!(timeout().as_secs() > 0);
    }

    #[test]
    fn minted_ids_are_short_and_prefixed() {
        let id = mint_id();
        assert!(id.starts_with('a'), "{id}");
        assert!(id.len() <= 8, "{id}");
    }

    #[test]
    fn row_includes_id_op_and_target() {
        let row = req("a0000004").row();
        assert!(row.contains("a0000004"));
        assert!(row.contains("write"));
        assert!(row.contains(".aws/credentials"));
    }
}
