//! `:schedule` — deferred + recurring background tasks.
//!
//! Lets the operator queue work to run later, either once (`in 5 minutes …`)
//! or on a recurring cadence (a 5-field cron expression, or `every 10 minutes
//! …`). Every fire ALWAYS spawns a full background coordinator (a `worker`),
//! exactly like `:dispatch` — scheduled work never runs inline. The scheduler:
//!
//!   * updates the 2nd status line ("SecondStatusLine") when a task fires and
//!     again when it finishes (see [`Scheduler::status_line`], folded into the
//!     footer by `repl::coordinator_status_message`), and
//!   * prints a console line above the prompt on fire AND a result-summary line
//!     on finish (`crate::tools::print_above_prompt`).
//!
//! Parsing accepts two shapes:
//!   1. cron  — `0 12 * * * run playwright test on http://mysite.com`
//!   2. natural language — `in 5 minutes execute this` / `every 30s ping api`.
//!
//! A single 1-Hz tick task (started lazily on the first `:schedule` add) drives
//! everything: it owns the shared [`SchedState`] and a snapshot of the launching
//! session's spawn context so it can build a `WorkerSpec` at fire time without
//! borrowing the live `Session`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Spawn context — a self-contained snapshot for building a WorkerSpec at fire
// time. Captured (refreshed) on every `:schedule` add so the tick task never
// touches the live Session.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SpawnCtx {
    pub exe: PathBuf,
    pub cwd: PathBuf,
    pub backend: String,
    pub model: String,
    pub env: Vec<(String, String)>,
    pub launch_session_id: String,
    pub launch_session_name: Option<String>,
    pub show_output: Arc<AtomicBool>,
    pub attached: Arc<Mutex<Option<String>>>,
    pub worker_jobs: crate::worker::WorkerJobs,
    /// Launching session's durable coordinator store, cloned so each scheduled
    /// coordinator spawn can reconcile its own orphaned `coordinator_runs` row
    /// after the child exits (mirrors `:dispatch`). `None` when no store is wired.
    pub coordinator_store: Option<crate::db::CoordinatorStore>,
}

// ---------------------------------------------------------------------------
// Schedule kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// Fire exactly once at the stored instant, then drop the job.
    Once,
    /// Fire every `Duration`, re-arming `next_fire += period` after each fire.
    Every(Duration),
    /// Fire on every minute matching the cron expression.
    Cron(Cron),
}

impl Schedule {
    fn describe(&self) -> String {
        match self {
            Schedule::Once => "once".to_string(),
            Schedule::Every(d) => format!("every {}", fmt_dur(*d)),
            Schedule::Cron(c) => format!("cron `{}`", c.src),
        }
    }
    fn recurring(&self) -> bool {
        !matches!(self, Schedule::Once)
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s % 86400 == 0 && s >= 86400 {
        format!("{}d", s / 86400)
    } else if s % 3600 == 0 && s >= 3600 {
        format!("{}h", s / 3600)
    } else if s % 60 == 0 && s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

// ---------------------------------------------------------------------------
// A scheduled job
// ---------------------------------------------------------------------------

pub struct ScheduledJob {
    pub num: u64,
    /// Full task text handed to the coordinator.
    pub task: String,
    /// Compact one-line hint for status/console lines.
    pub hint: String,
    pub schedule: Schedule,
    pub next_fire: SystemTime,
    pub runs: u64,
    /// Worker id of the most recent fire, until its completion is reported.
    pending_worker: Option<String>,
}

// ---------------------------------------------------------------------------
// Scheduler — the session-held handle
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct Scheduler {
    inner: Arc<Mutex<SchedState>>,
    status: Arc<Mutex<Option<String>>>,
}

#[derive(Default)]
struct SchedState {
    jobs: Vec<ScheduledJob>,
    next_num: u64,
    ctx: Option<SpawnCtx>,
    running: bool,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current schedule status snippet for the 2nd status line, if any.
    pub fn status_line(&self) -> Option<String> {
        self.status.lock().unwrap().clone()
    }

    /// Parse `spec_and_task` and enqueue a job. Refreshes the spawn context and
    /// starts the tick task if needed. Returns the human message to print.
    pub fn add(&self, spec_and_task: &str, ctx: SpawnCtx) -> String {
        let (schedule, task, first_fire) = match parse(spec_and_task) {
            Ok(v) => v,
            Err(e) => return format!("schedule: {e}"),
        };
        let hint = one_line(&task, 60);
        let num;
        {
            let mut st = self.inner.lock().unwrap();
            st.ctx = Some(ctx);
            st.next_num += 1;
            num = st.next_num;
            st.jobs.push(ScheduledJob {
                num,
                task,
                hint: hint.clone(),
                schedule: schedule.clone(),
                next_fire: first_fire,
                runs: 0,
                pending_worker: None,
            });
        }
        self.ensure_running();
        let when = fmt_when(first_fire);
        format!(
            "\x1b[2mscheduled\x1b[0m \x1b[1;36m#{num}\x1b[0m \x1b[2m({}) — next fire {when}. \x1b[0m\x1b[36m:schedule\x1b[0m\x1b[2m to list, \x1b[0m\x1b[36m:schedule clear {num}\x1b[0m\x1b[2m to cancel.\x1b[0m — {hint}",
            schedule.describe()
        )
    }

    /// Human listing of pending jobs for bare `:schedule`.
    pub fn list(&self) -> String {
        let st = self.inner.lock().unwrap();
        if st.jobs.is_empty() {
            return "no scheduled tasks. usage: :schedule <cron|in N unit|every N unit> <task>"
                .to_string();
        }
        let mut out = String::from("scheduled tasks:\n");
        for j in &st.jobs {
            out.push_str(&format!(
                "  #{}  {}  next {}  (runs {})  — {}\n",
                j.num,
                j.schedule.describe(),
                fmt_when(j.next_fire),
                j.runs,
                j.hint
            ));
        }
        out.trim_end().to_string()
    }

    /// Cancel one job (`Some(num)`) or all (`None`). Returns a message.
    pub fn clear(&self, num: Option<u64>) -> String {
        let mut st = self.inner.lock().unwrap();
        match num {
            None => {
                let n = st.jobs.len();
                st.jobs.clear();
                format!("cleared {n} scheduled task(s)")
            }
            Some(n) => {
                let before = st.jobs.len();
                st.jobs.retain(|j| j.num != n);
                if st.jobs.len() < before {
                    format!("cancelled scheduled task #{n}")
                } else {
                    format!("no scheduled task #{n}")
                }
            }
        }
    }

    /// Start the 1-Hz tick task exactly once.
    fn ensure_running(&self) {
        {
            let mut st = self.inner.lock().unwrap();
            if st.running {
                return;
            }
            st.running = true;
        }
        let inner = self.inner.clone();
        let status = self.status.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                tick(&inner, &status);
            }
        });
    }
}

/// One scheduler pass: fire due jobs, report finished ones.
fn tick(inner: &Arc<Mutex<SchedState>>, status: &Arc<Mutex<Option<String>>>) {
    let now = SystemTime::now();
    // 1) Fire due jobs. Collect spawn requests under the lock, spawn outside it.
    struct Fire {
        num: u64,
        hint: String,
        task: String,
        spec: crate::worker::WorkerSpec,
    }
    let mut fires: Vec<Fire> = Vec::new();
    let mut finished: Vec<(u64, String, String)> = Vec::new(); // (num, hint, worker_id)
    {
        let mut st = inner.lock().unwrap();
        let ctx = st.ctx.clone();
        // Detect completions of in-flight scheduled workers.
        let jobsnap: Vec<(usize, u64, String, String)> = st
            .jobs
            .iter()
            .enumerate()
            .filter_map(|(i, j)| {
                j.pending_worker
                    .as_ref()
                    .map(|w| (i, j.num, j.hint.clone(), w.clone()))
            })
            .collect();
        for (i, num, hint, wid) in jobsnap {
            if let Some(worker) = ctx.as_ref().and_then(|c| find_worker(&c.worker_jobs, &wid)) {
                let s = worker.status();
                if s == "done" || s == "failed" {
                    st.jobs[i].pending_worker = None;
                    finished.push((num, hint, wid));
                }
            }
        }
        // Fire due jobs.
        if let Some(ctx) = ctx {
            let mut i = 0;
            while i < st.jobs.len() {
                if st.jobs[i].next_fire <= now {
                    let spec = build_spec(&ctx);
                    fires.push(Fire {
                        num: st.jobs[i].num,
                        hint: st.jobs[i].hint.clone(),
                        task: st.jobs[i].task.clone(),
                        spec,
                    });
                    // Re-arm or drop.
                    let recurring = st.jobs[i].schedule.recurring();
                    if recurring {
                        let nf = next_fire_after(&st.jobs[i].schedule, now);
                        st.jobs[i].next_fire = nf;
                        st.jobs[i].runs += 1;
                    }
                }
                i += 1;
            }
        }
        // We handle drop-of-Once AFTER spawning (need the worker id first), so
        // mark them by leaving next_fire in the past and removing below.
    }

    // 2) Report finishes (console + status line) outside the lock.
    for (num, hint, wid) in &finished {
        let (icon, result) = {
            let w = {
                let st = inner.lock().unwrap();
                st.ctx
                    .as_ref()
                    .and_then(|c| find_worker(&c.worker_jobs, wid))
            };
            match w {
                Some(job) => {
                    let ok = job.status() == "done";
                    (if ok { "✓" } else { "✗" }, job.fetch())
                }
                None => ("✓", String::new()),
            }
        };
        let summary = one_line(&result, 160);
        *status.lock().unwrap() = Some(format!("⏰ #{num} finished {icon} — {hint}"));
        crate::tools::print_above_prompt(format!(
            "\x1b[2m⏰ scheduled #{num} finished {icon} — {hint} · `:result {wid}` for details\x1b[0m\n\x1b[2m   {summary}\x1b[0m\n"
        ));
    }

    // 3) Spawn fires outside the lock, then record worker ids / drop one-shots.
    for f in fires {
        let ctx_jobs = {
            let st = inner.lock().unwrap();
            st.ctx.as_ref().map(|c| c.worker_jobs.clone())
        };
        let Some(jobs) = ctx_jobs else { continue };
        let wid = crate::worker::spawn(&jobs, f.task.clone(), f.spec);
        *status.lock().unwrap() = Some(format!("⏰ #{} running — {}", f.num, f.hint));
        crate::tools::print_above_prompt(format!(
            "\x1b[2m⏰ scheduled #{} fired — {} → coordinator \x1b[0m\x1b[1;36m{wid}\x1b[0m\x1b[2m (`:attach {wid}` to watch)\x1b[0m\n",
            f.num, f.hint
        ));
        // Attach worker id to the job (for finish detection) or drop one-shots.
        let mut st = inner.lock().unwrap();
        if let Some(j) = st.jobs.iter_mut().find(|j| j.num == f.num) {
            j.pending_worker = Some(wid);
            if !j.schedule.recurring() {
                j.runs += 1;
            }
        }
        // Remove completed one-shots (next_fire not re-armed → still <= now).
        st.jobs
            .retain(|j| j.schedule.recurring() || j.pending_worker.is_some());
    }
}

fn build_spec(ctx: &SpawnCtx) -> crate::worker::WorkerSpec {
    crate::worker::WorkerSpec {
        exe: ctx.exe.clone(),
        cwd: ctx.cwd.clone(),
        backend: ctx.backend.clone(),
        model: ctx.model.clone(),
        env: ctx.env.clone(),
        // Scheduled/recurring work runs in the shared cwd (like `:dispatch`) —
        // no per-fire worktree churn.
        isolate: false,
        base: "main".to_string(),
        launch_session_id: ctx.launch_session_id.clone(),
        launch_session_name: ctx.launch_session_name.clone(),
        show_output: ctx.show_output.clone(),
        attached: ctx.attached.clone(),
        coordinator_store: ctx.coordinator_store.clone(),
    }
}

fn find_worker(
    jobs: &crate::worker::WorkerJobs,
    id: &str,
) -> Option<Arc<crate::worker::WorkerJob>> {
    jobs.lock().unwrap().iter().find(|j| j.id == id).cloned()
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse `<spec> <task>` into (schedule, task, first_fire).
pub fn parse(input: &str) -> Result<(Schedule, String, SystemTime), String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("expected `<cron|in N unit|every N unit> <task>`".to_string());
    }
    let lower_first = s.split_whitespace().next().unwrap_or("").to_lowercase();

    // Natural language: "in N unit …" (one-shot) / "every N unit …" (recurring).
    if lower_first == "in" || lower_first == "every" {
        let toks: Vec<&str> = s.split_whitespace().collect();
        let (dur, rest_idx) = parse_rel(&toks[1..])?;
        if rest_idx + 1 >= toks.len() {
            return Err("missing task after the time spec".to_string());
        }
        let task = toks[1 + rest_idx..].join(" ");
        if task.trim().is_empty() {
            return Err("missing task after the time spec".to_string());
        }
        if lower_first == "in" {
            return Ok((Schedule::Once, task, SystemTime::now() + dur));
        } else {
            return Ok((Schedule::Every(dur), task, SystemTime::now() + dur));
        }
    }

    // Cron: first 5 tokens must be valid cron fields.
    let toks: Vec<&str> = s.split_whitespace().collect();
    if toks.len() > 5 {
        if let Some(cron) = Cron::parse(&toks[..5]) {
            let task = toks[5..].join(" ");
            if task.trim().is_empty() {
                return Err("missing task after the cron expression".to_string());
            }
            let first = next_fire_after(&Schedule::Cron(cron.clone()), SystemTime::now());
            return Ok((Schedule::Cron(cron), task, first));
        }
    }
    Err("unrecognized schedule — use `in N minutes <task>`, `every N min <task>`, or a 5-field cron `M H DoM Mon DoW <task>`".to_string())
}

/// Parse a relative duration from leading tokens. Handles `5 minutes`, `30s`,
/// `2h`. Returns (duration, number_of_tokens_consumed).
fn parse_rel(toks: &[&str]) -> Result<(Duration, usize), String> {
    if toks.is_empty() {
        return Err("missing duration".to_string());
    }
    // Fused form: "30s", "5m", "2h", "1d".
    if let Some((n, unit)) = split_fused(toks[0]) {
        let d = unit_dur(n, &unit)?;
        return Ok((d, 1));
    }
    // Split form: "5 minutes".
    let n: u64 = toks[0]
        .parse()
        .map_err(|_| format!("bad number `{}`", toks[0]))?;
    let unit = toks.get(1).ok_or("missing time unit (s/min/hour/day)")?;
    let d = unit_dur(n, unit)?;
    Ok((d, 2))
}

fn split_fused(tok: &str) -> Option<(u64, String)> {
    let digits: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest: String = tok.chars().skip(digits.len()).collect();
    if rest.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok().map(|n| (n, rest))
}

fn unit_dur(n: u64, unit: &str) -> Result<Duration, String> {
    let u = unit.to_lowercase();
    let secs = match u.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => n,
        "m" | "min" | "mins" | "minute" | "minutes" => n * 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        _ => return Err(format!("unknown time unit `{unit}`")),
    };
    if secs == 0 {
        return Err("duration must be > 0".to_string());
    }
    Ok(Duration::from_secs(secs))
}

// ---------------------------------------------------------------------------
// Cron (5-field, local time)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    min: Field,
    hour: Field,
    dom: Field,
    mon: Field,
    dow: Field,
    src: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Field {
    any: bool,
    values: Vec<u32>,
}

impl Field {
    fn matches(&self, v: u32) -> bool {
        self.any || self.values.contains(&v)
    }
}

impl Cron {
    fn parse(fields: &[&str]) -> Option<Cron> {
        if fields.len() != 5 {
            return None;
        }
        let min = parse_field(fields[0], 0, 59)?;
        let hour = parse_field(fields[1], 0, 23)?;
        let dom = parse_field(fields[2], 1, 31)?;
        let mon = parse_field(fields[3], 1, 12)?;
        let mut dow = parse_field(fields[4], 0, 7)?;
        // Normalize Sunday=7 to 0 so tm_wday (0-6) matching works.
        if dow.values.contains(&7) {
            dow.values.retain(|v| *v != 7);
            if !dow.values.contains(&0) {
                dow.values.push(0);
            }
        }
        Some(Cron {
            min,
            hour,
            dom,
            mon,
            dow,
            src: fields.join(" "),
        })
    }

    fn matches(&self, tm: &libc::tm) -> bool {
        let min = tm.tm_min as u32;
        let hour = tm.tm_hour as u32;
        let dom = tm.tm_mday as u32;
        let mon = (tm.tm_mon + 1) as u32;
        let dow = tm.tm_wday as u32; // 0-6, Sun=0
        let day_ok = match (self.dom.any, self.dow.any) {
            (true, true) => true,
            (false, true) => self.dom.matches(dom),
            (true, false) => self.dow.matches(dow),
            // Both restricted: cron matches if EITHER day-of-month or day-of-week hits.
            (false, false) => self.dom.matches(dom) || self.dow.matches(dow),
        };
        self.min.matches(min) && self.hour.matches(hour) && self.mon.matches(mon) && day_ok
    }
}

fn parse_field(tok: &str, lo: u32, hi: u32) -> Option<Field> {
    if tok == "*" {
        return Some(Field {
            any: true,
            values: Vec::new(),
        });
    }
    let mut values: Vec<u32> = Vec::new();
    for part in tok.split(',') {
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (start, end) = if range_part == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (a.parse().ok()?, b.parse().ok()?)
        } else {
            let n: u32 = range_part.parse().ok()?;
            (n, n)
        };
        if start < lo || end > hi || start > end {
            return None;
        }
        let mut v = start;
        while v <= end {
            values.push(v);
            v += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    Some(Field { any: false, values })
}

/// Next fire strictly after `after` for a recurring schedule.
fn next_fire_after(sched: &Schedule, after: SystemTime) -> SystemTime {
    match sched {
        Schedule::Once => after, // never re-armed
        Schedule::Every(d) => after + *d,
        Schedule::Cron(c) => {
            let base = after.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            // Truncate to the minute, start from the next minute.
            let mut cand = (base / 60) * 60 + 60;
            let limit = cand + 366 * 24 * 60 * 60; // search up to ~1 year
            while cand < limit {
                if let Some(tm) = broken_down(cand) {
                    if c.matches(&tm) {
                        return UNIX_EPOCH + Duration::from_secs(cand as u64);
                    }
                }
                cand += 60;
            }
            // Fallback: an hour out, so a mis-parsed cron doesn't hot-loop.
            after + Duration::from_secs(3600)
        }
    }
}

fn broken_down(epoch: i64) -> Option<libc::tm> {
    unsafe {
        let t: libc::time_t = epoch as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            None
        } else {
            Some(tm)
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Collapse whitespace and truncate to `max` chars with an ellipsis.
fn one_line(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let t: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", t.trim_end())
    }
}

/// Relative "in Ns / at HH:MM" style hint for a future instant.
fn fmt_when(t: SystemTime) -> String {
    match t.duration_since(SystemTime::now()) {
        Ok(d) => {
            let s = d.as_secs();
            if s < 90 {
                format!("in {s}s")
            } else if s < 5400 {
                format!("in {}m", (s + 30) / 60)
            } else if s < 172800 {
                format!("in {}h", (s + 1800) / 3600)
            } else {
                format!("in {}d", s / 86400)
            }
        }
        Err(_) => "now".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_in_minutes() {
        let (s, task, _) = parse("in 5 minutes execute this").unwrap();
        assert_eq!(s, Schedule::Once);
        assert_eq!(task, "execute this");
    }

    #[test]
    fn parse_fused_and_every() {
        let (s, task, _) = parse("every 30s ping api").unwrap();
        assert_eq!(s, Schedule::Every(Duration::from_secs(30)));
        assert_eq!(task, "ping api");
    }

    #[test]
    fn parse_cron_expr() {
        let (s, task, _) = parse("0 12 * * * run playwright test on http://mysite.com").unwrap();
        match s {
            Schedule::Cron(c) => {
                assert!(c.min.matches(0));
                assert!(!c.min.matches(1));
                assert!(c.hour.matches(12));
            }
            _ => panic!("expected cron"),
        }
        assert_eq!(task, "run playwright test on http://mysite.com");
    }

    #[test]
    fn cron_field_ranges_and_steps() {
        let f = parse_field("*/15", 0, 59).unwrap();
        assert!(f.matches(0) && f.matches(15) && f.matches(30) && f.matches(45));
        assert!(!f.matches(10));
        let f = parse_field("1-3", 0, 59).unwrap();
        assert!(f.matches(1) && f.matches(3) && !f.matches(4));
        assert!(parse_field("99", 0, 59).is_none());
    }

    #[test]
    fn cron_sunday_seven_normalizes() {
        let c = Cron::parse(&["0", "0", "*", "*", "7"]).unwrap();
        assert!(c.dow.matches(0)); // Sunday as 0
    }

    #[test]
    fn bad_specs_error() {
        assert!(parse("in 5 minutes").is_err()); // no task
        assert!(parse("garbage here").is_err());
    }
}
