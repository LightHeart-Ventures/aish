use crate::backend::Backend;
use crate::editor::{LineEditor, ReadOutcome};
use crate::engine;
use crate::pipeline;
use crate::rc;
use crate::session::Session;
use crate::tools;
use anyhow::Result;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Install a background MCP connect's result into the session the moment it's
/// ready (non-blocking). Replaces the placeholder host with the connected one
/// and swaps in the skills catalog that includes the servers' published skills.
/// A no-op once installed or while still connecting. See `mcp_rx` in `run`.
fn install_mcp_if_ready(
    rx: &mut Option<tokio::sync::oneshot::Receiver<(crate::mcp::McpHost, String)>>,
    session: &mut Session,
) {
    let Some(receiver) = rx.as_mut() else {
        return;
    };
    match receiver.try_recv() {
        Ok((host, skills_prompt)) => {
            let n = host.server_names().len();
            session.mcp = host;
            session.skills_prompt = skills_prompt;
            *rx = None;
            if n > 0 {
                eprintln!(
                    "\x1b[2mmcp: ready — {n} server{} connected\x1b[0m",
                    if n == 1 { "" } else { "s" }
                );
            }
        }
        // Still connecting, or the connect task died — leave the placeholder.
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => *rx = None,
    }
}

/// Blocking y/N/a prompt on the controlling TTY. `a` ("always") allows this
/// call and persists the tool/command so it never prompts again.
pub fn confirm_tty(prompt: &str) -> tools::Decision {
    use tools::Decision;
    // If a mid-turn key watcher is active it holds stdin in cbreak (ECHO off);
    // pause it for the length of this prompt so the answer echoes + line-edits.
    let _pause = crate::keywatch::pause_for_prompt();
    print!("\x1b[33mrun?\x1b[0m {prompt} \x1b[33m[y/N/a/d]\x1b[0m ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Decision::Deny;
    }
    match line.trim() {
        "y" | "Y" | "yes" => Decision::AllowOnce,
        "a" | "A" | "always" => Decision::AlwaysAllow,
        // 'd' = allow this permission for the whole directory, recursively. Only
        // meaningful on a read/write/delete prompt (where a path is in scope);
        // elsewhere the gate treats it as a one-time allow.
        "d" | "D" | "dir" => Decision::AllowDir,
        _ => Decision::Deny,
    }
}

/// Emit an OSC 0 (icon name & window title) sequence to set the terminal window title.
fn set_window_title(title: &str) {
    print!("\x1b]0;{}\x07", title);
    let _ = std::io::stdout().flush();
}

/// Emit an OSC 0 sequence with an empty string to reset the terminal window title.
fn reset_window_title() {
    print!("\x1b]0;\x07");
    let _ = std::io::stdout().flush();
}

/// Clear the screen and home the cursor so the next output starts at the top —
/// but only on an interactive terminal (a piped stdout stays free of escape
/// noise). Used by the Shift-Tab worker cycle so each attach/detach view opens
/// on a fresh screen instead of scrolling under the prior one.
fn clear_screen() {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } == 1 {
        print!("\x1b[2J\x1b[H");
        let _ = std::io::stdout().flush();
        // If a bottom-anchored footer is installed, the clear just wiped its
        // rows and homed the cursor to the top — re-assert the region and
        // repaint the footer so it stays pinned to the bottom.
        crate::terminal::restore_after_clear();
    }
}

pub async fn run(
    mut backend: Backend,
    mut session: Session,
    aliases: HashMap<String, Vec<String>>,
    mcp_config_paths: Vec<PathBuf>,
    skills_dir: PathBuf,
) -> Result<()> {
    // Job-control signal disposition (TASK-115): aish ignores SIGINT/QUIT/TSTP/
    // TTOU/TTIN so a Ctrl-C/Ctrl-\/Ctrl-Z reaches the foreground child's process
    // group (run_on_tty hands it the terminal) instead of killing or suspending
    // the shell; foreground children restore the default disposition in pre_exec.
    // SIGINT is also observed by the ctrl_c task below for model-turn aborts.
    // With real process-group ownership (TASK-113/114) the terminal delivers a
    // Ctrl-C straight to the foreground child's process group, so aish only ever
    // receives SIGINT when IT owns the terminal — i.e. during a model turn. That
    // makes the old tty_handoff disambiguation flag unnecessary; TASK-116 removed it.
    tools::ignore_job_control_signals();

    // Load the lifecycle-hook registry (~/.aish/hooks.json + ./.aish/hooks.json)
    // now that the session cwd is established, then fire SessionStart. Both are
    // no-ops when no hook is configured — the empty registry spawns nothing.
    session.load_hooks();
    if session.hooks.has(crate::hooks::HookEvent::SessionStart) {
        let p = session.hook_payload(crate::hooks::HookEvent::SessionStart);
        session
            .hooks
            .fire_observe(crate::hooks::HookEvent::SessionStart, p);
    }

    // Install the process-wide SIGINT handler up front. A Ctrl-C during a
    // direct-dispatch child is delivered by the terminal straight to that child's
    // own process group (it leads its own pgrp and owns the tty via tcsetpgrp), so
    // this task just keeps a tokio SIGINT handler installed — aish is never killed.
    tokio::spawn(async {
        loop {
            let _ = tokio::signal::ctrl_c().await;
        }
    });

    // ~/.aishrc is already loaded in main (its exports are in session.env, used
    // for credential resolution before the backend is built); we just take the
    // aliases it parsed.
    let aliases = Arc::new(aliases);

    // Deferred MCP connect. The handshake (a remote HTTP server's
    // initialize → tools/list → prompts/list) is the dominant startup cost and
    // used to run before this REPL existed, freezing the shell for seconds.
    // Instead connect in the background and hand the result back over a oneshot;
    // the loop installs the tools + skills catalog when it arrives. Until then
    // the prompt is live and everything except MCP tools works. `engine::run_turn`
    // reads `session.mcp.tool_defs()` fresh each turn, so a late arrival simply
    // becomes available on the next turn.
    let mut mcp_rx: Option<tokio::sync::oneshot::Receiver<(crate::mcp::McpHost, String)>> =
        if mcp_config_paths.is_empty() {
            None
        } else {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let refs: Vec<&Path> = mcp_config_paths.iter().map(|p| p.as_path()).collect();
                let host = crate::mcp::McpHost::start(&refs).await;
                // The interactive REPL is a light-touch router: advertise only
                // the routing skills (interactive_mcp_skills), keeping the heavy
                // code-work / agent-dispatch skills for background coordinators.
                let skills_prompt = crate::skills::render_prompt_section(
                    &crate::skills::load_catalog(&skills_dir),
                    &crate::skills::interactive_mcp_skills(&host.skills()),
                );
                let _ = tx.send((host, skills_prompt));
            });
            Some(rx)
        };

    // Best-effort self-update check. Runs the `gh` release query OFF the startup
    // critical path; when a newer release exists for this platform the result
    // lands on `update_rx`, and the loop prints a one-line notice + stashes it in
    // `pending_update` so `:update` can install it without a second network call.
    // Silent on any failure (no gh, offline, no releases, no matching asset).
    let mut update_rx: Option<tokio::sync::oneshot::Receiver<crate::update::UpdateInfo>> =
        if crate::update::gh_available() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                if let Ok(Some(info)) = crate::update::check_cached().await {
                    let _ = tx.send(info);
                }
            });
            Some(rx)
        } else {
            None
        };
    let mut pending_update: Option<crate::update::UpdateInfo> = None;

    // The line editor is driven through the `LineEditor` trait (S5.1/TASK-130)
    // so the concrete editor — rustyline today, reedline-capable tomorrow — is a
    // one-line swap at this construction site. The rustyline impl configures
    // list-style completion (the whole candidate menu on the first TAB), installs
    // the `AishHelper` (completion + the live `:`-palette Hinter + the
    // route-preview Highlighter), loads history, and binds Ctrl-O / Esc.
    let helper = AishHelper::new(session.cwd.clone(), session_path(&session), aliases.clone());
    let mut editor = crate::editor::RustylineEditor::new(helper, dirs_history_path())?;

    // The `:`-command palette renders live as an inline hint below the prompt
    // (see `palette_hint` and the Hinter impl): type `:` and the menu appears,
    // and each extra char filters it in place — rustyline redraws the hint
    // without re-printing the prompt or stacking menus. No key bindings are
    // needed; `:` and the name chars self-insert normally, and TAB still
    // completes a `:`-prefix via the Completer.

    // Start on a clean screen (interactive terminals only — keep piped output clean).
    clear_screen();

    // Bottom-anchored footer (S: DECSTBM scroll region). On a tall enough
    // interactive terminal we pin a three-row footer to the bottom: a solid rule,
    // a coordinator status message, and the live statusline. Everything else
    // scrolls above it. On a short terminal (height ≤ 4) or off a tty, `term` is
    // `None`/disabled and we fall back to inline statusline printing (the prior
    // behavior). A panic hook resets the region on unwind; `Drop` resets it on a
    // clean exit. A SIGWINCH watcher flips `resized` so the loop re-establishes
    // the region on the next idle pass (and every `draw_footer` re-asserts it too).
    let mut term = crate::terminal::Terminal::detect();
    let footer_active = term.as_ref().map(|t| t.footer_enabled()).unwrap_or(false);
    let resized = Arc::new(AtomicBool::new(false));
    if footer_active {
        crate::terminal::install_panic_hook();
        if let Some(t) = term.as_mut() {
            t.init_scroll_region();
        }
        // Idle-timeout heartbeat: repaints the footer after 3s parked at the
        // prompt so a terminal scroll that carried it out of view self-heals
        // (the "I scrolled and the footer disappeared" complaint).
        crate::terminal::spawn_footer_heartbeat();
        // SIGWINCH → set the resize flag; the loop drains it before the prompt.
        let resized_sig = resized.clone();
        tokio::spawn(async move {
            if let Ok(mut sig) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            {
                loop {
                    if sig.recv().await.is_none() {
                        break;
                    }
                    resized_sig.store(true, Ordering::SeqCst);
                }
            }
        });
    }

    // Live statusline (version + model on the left, date/time on the right),
    // then the help hint. Interactive terminals only — never leak escape/status
    // noise into piped/redirected output. With the footer active the statusline
    // lives in the pinned bottom row; otherwise it's refreshed inline directly
    // above the prompt on each idle pass (see the LoopTick::Idle arm).
    if unsafe { libc::isatty(1) } == 1 {
        if footer_active {
            if let Some(t) = term.as_mut() {
                let statusline = crate::style::statusline(
                    crate::update::current_version(),
                    &backend.describe(),
                );
                t.draw_footer(&coordinator_status_message(&session), &statusline);
            }
        } else {
            println!(
                "{}",
                crate::style::statusline(crate::update::current_version(), &backend.describe())
            );
        }
        println!(
            "\x1b[2m:help for commands — :workers to monitor background tasks\x1b[0m"
        );
    }

    // The startup block above already printed the statusline, and the first
    // `LoopTick::Idle` pass reprints it directly above the first prompt — which
    // would show the header twice. Suppress exactly that first idle refresh so
    // the header appears once at startup; every later idle pass refreshes it as
    // usual (updated date/time above each prompt). Irrelevant in footer mode
    // (the footer is idempotent and never scrolls into the body).
    let mut suppress_statusline_once = unsafe { libc::isatty(1) } == 1 && !footer_active;

    let mut prev_dir: Option<PathBuf> = None;
    let mut needs_gap = false; // blank line between previous output and the prompt
    // A command the user accepted from a rewrite preview (S6.4 / TASK-138) is
    // parked here and consumed at the top of the next iteration so it flows
    // through the SAME dispatch path as a typed line — no logic is duplicated.
    let mut injected: Option<String> = None;

    // Background-result presenter. When interactive, finished batch/worker jobs
    // queue their results (present::enable_deferred) and this task prints them
    // ABOVE the prompt via rustyline's ExternalPrinter — but only at a pause in
    // work (`busy == false`), so a result never blurts over a command in flight
    // or the user's typing. ExternalPrinter redraws the prompt after printing.
    // If the terminal can't provide a printer, we leave inline printing on.
    let busy = Arc::new(AtomicBool::new(false));
    if let Some(printer) = editor.take_printer() {
        crate::present::enable_deferred();
        // Share the one ExternalPrinter process-wide (#354). The live `:attach`
        // / Shift-Tab coordinator stream forwards through `tools::announce_raw`;
        // installing the printer here makes that path prompt-preserving too,
        // instead of the raw `\r\x1b[2K` write that stranded the prompt above
        // the stream. This presenter then prints its own notices through the
        // same slot (`tools::print_above_prompt`), so both share one serialised
        // printer.
        crate::tools::install_external_printer(printer);
        let busy = busy.clone();
        let batch_jobs = session.batch_jobs.clone();
        let worker_jobs = session.worker_jobs.clone();
        let attached = session.attached.clone();
        let attach_review_announced = session.attach_review_announced.clone();
        // Shared with the auto-resume wake hook: this presenter observes child
        // completions off the main thread and arms a coalesced resume; the main
        // loop drains it on its next idle pass. See session::ResumeState.
        let resume = session.resume.clone();
        // `:alert` monitor plumbing shared with this presenter: the store to
        // poll native alerts / claim fired ones, and the banner slot the
        // SecondStatusLine reads. Clones are cheap (Arc handles).
        let alert_store = session.alert_store.clone();
        let alert_banner = session.alert_banner.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(400));
            loop {
                tick.tick().await;
                if busy.load(Ordering::SeqCst) {
                    continue; // mid-command/turn — hold results until a pause
                }
                // ── `:alert` monitors ──────────────────────────────────────
                // Evaluate due native alerts (file/command probes) token-free,
                // then surface every fired alert (native OR semantic ones a
                // delegated coordinator flipped) exactly once. `take_fired`
                // advances each row armed→fired→done so it never repeats.
                if let Some(store) = &alert_store {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    if let Ok(armed) = store.armed_alerts() {
                        for a in armed {
                            if a.due(now) {
                                let _ = store.touch_alert_check(a.id, now);
                                if let Some(fired) = crate::alert::poll(&a) {
                                    let _ =
                                        store.mark_alert_fired(a.id, &fired.detail, &fired.short);
                                }
                            }
                        }
                    }
                    if let Ok(fired) = store.take_fired() {
                        let mut ring = false;
                        for (_id, detail, short, audible) in fired {
                            crate::tools::print_above_prompt(format!("{detail}\n"));
                            if let Ok(mut b) = alert_banner.lock() {
                                *b = Some(short);
                            }
                            ring = ring || audible;
                        }
                        if ring {
                            crate::alert::play_alert_bell();
                        }
                    }
                }

                // Set by any finish-branch below so we ring the audible bell
                // ONCE per tick (never a double-beep when two branches fire).
                let mut finished_bell = false;
                // A live `:attach` leaves the main loop BLOCKED in `read_line`, so
                // `announce_attach_review` (which prints the coordinator's FINAL
                // answer + review notice) can't run until the operator presses
                // Enter. Surface it HERE instead — the moment the attached worker
                // goes terminal — via the ExternalPrinter, so the last message is
                // never "cut off" while watching. The atomic claim de-dupes
                // against the main-loop path (whichever observes the finish first
                // prints; the other no-ops).
                if let Some(run_id) = attached.lock().unwrap().clone() {
                    let terminal = worker_jobs
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|w| w.id == run_id)
                        .map(|w| matches!(w.status().as_str(), "done" | "failed"))
                        .unwrap_or(false);
                    if terminal && claim_attach_announce(&attach_review_announced, &run_id) {
                        for line in attached_result_lines(&run_id, &worker_jobs) {
                            crate::tools::print_above_prompt(format!("{line}\n"));
                        }
                        let short = crate::batch::short_id(&run_id);
                        crate::tools::print_above_prompt(format!(
                            "\x1b[2m⇄ {short} finished — review mode: type a message to resume it, or :detach to return to your shell.\x1b[0m\n"
                        ));
                        finished_bell = true;
                    }
                }
                // Notify (one line per finished job), don't dump the full result
                // over the prompt — the user views it on demand with `:result`.
                let mut notices = crate::batch::notify_pending(&batch_jobs);
                notices.extend(crate::worker::notify_pending(&worker_jobs));
                if !notices.is_empty() {
                    finished_bell = true;
                }
                for n in notices {
                    crate::tools::print_above_prompt(format!("{n}\n"));
                }
                // Auto-resume wake hook: observe this session's fanned-out
                // coordinators. Snapshot every worker's terminal-ness and the
                // count still outstanding, then let ResumeState coalesce — it
                // dedupes repeat observations and only ARMS once the LAST
                // outstanding child has finished. Passing the full terminal set
                // each tick is intentional: `observe` folds only newly-terminal
                // ids into the pending list (see session::ResumeState). When it
                // freshly arms, surface a one-line heads-up; the main loop drains
                // the armed resume on its next idle pass (Session::take_resume_tick).
                {
                    let (terminal_ids, outstanding) = {
                        let jobs = worker_jobs.lock().unwrap();
                        let mut terminal = Vec::new();
                        let mut outstanding = 0usize;
                        for w in jobs.iter() {
                            if matches!(w.status().as_str(), "done" | "failed") {
                                terminal.push(w.id.clone());
                            } else {
                                outstanding += 1;
                            }
                        }
                        (terminal, outstanding)
                    };
                    let freshly_armed = resume
                        .lock()
                        .map(|mut r| r.observe(&terminal_ids, outstanding))
                        .unwrap_or(false);
                    if freshly_armed {
                        crate::tools::print_above_prompt(
                            "\x1b[2m⤵ background workers finished — resuming to synthesize their results…\x1b[0m\n"
                                .to_string(),
                        );
                        // Hands-free drain: if the main loop is parked inside a
                        // blocking read_line right now, arming alone won't fire
                        // the resume until the human presses a key. Push a
                        // newline into the terminal's input queue so read_line
                        // returns and the next loop pass drains the armed resume
                        // via take_resume_tick. Best-effort (see
                        // editor::nudge_terminal_return): a no-op on kernels that
                        // gate TIOCSTI or when stdin isn't a tty, in which case
                        // we fall back to resuming on the user's next Enter.
                        crate::editor::nudge_terminal_return();
                        finished_bell = true;
                    }
                }
                // Ring the audible finish-bell once if any worker/batch/coordinator
                // reached a terminal state this tick. Best-effort + opt-out via
                // AISH_WORKER_BELL / custom via AISH_WORKER_BELL_CMD.
                if finished_bell {
                    crate::tools::play_finish_bell();
                }
            }
        });
    }

    loop {
        // Install MCP tools/skills as soon as the background connect finishes
        // (see `mcp_rx` above). Checked here and again right before a model turn
        // so a handshake that lands while the user is typing is still available
        // for that very turn.
        install_mcp_if_ready(&mut mcp_rx, &mut session);
        // Surface a discovered upgrade exactly once, above the prompt. The user
        // chooses whether to act on it with `:update`.
        if pending_update.is_none() {
            if let Some(rx) = update_rx.as_mut() {
                match rx.try_recv() {
                    Ok(info) => {
                        println!(
                            "\x1b[1;32m✨ aish {} is available\x1b[0m (you have {}) — type \x1b[1m:update\x1b[0m to upgrade",
                            info.version,
                            crate::update::current_version()
                        );
                        needs_gap = true;
                        pending_update = Some(info);
                        update_rx = None;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => update_rx = None,
                }
            }
        }
        if needs_gap {
            println!();
            println!();
            needs_gap = false;
        }
        // If the coordinator we're attached to has reached a terminal state,
        // announce it once and flip into review mode (stay attached so a typed
        // line resumes it). See `announce_attach_review`.
        announce_attach_review(&mut session);
        // TASK-282 AC2: fold any finished linked coordinator into its goal's
        // progress rollup (idempotent — fires the notice once, on the pass that
        // first sees the finish).
        for notice in session.sync_goal_rollup() {
            crate::tools::print_above_prompt(format!("{notice}\n"));
        }
        // Tab completion resolves against the session's cwd, which `cd` mutates.
        editor.set_cwd(&session.cwd);
        // We're about to idle at the prompt — let the presenter flush results.
        busy.store(false, Ordering::SeqCst);
        // Combined background-job tally — Anthropic batches + re-exec'd workers
        // + durable coordinator runs the in-memory tallies miss (goal-loop turns,
        // other-session runs, runs reattached after a restart). This is the ⟳N
        // badge count; `:update` reuses the SAME helper to warn about version
        // skew before swapping the binary (ISS-2045).
        let running = background_running_count(&session);
        // Colour the ⟳N badge by the most recent background-worker event
        // (green ✓ tool success, red ✗ tool failure, magenta ⟳ turn
        // completion), fading back to dim ⟳N after worker::PULSE_FADE.
        let pulse = crate::worker::fresh_pulse(&session.worker_jobs);
        let badge = crate::worker::pulse_badge(running, pulse);
        // The session name (`:rename`) is no longer shown in the prompt — it's
        // right-justified on the 2nd statusline instead (see
        // `coordinator_status_message`).
        // `:attach`ed coordinator indicator — bold yellow `⇄<id>` makes it obvious
        // that what you type is steered to that coordinator, not run here.
        let attach = match &*session.attached.lock().unwrap() {
            Some(run_id) => format!("\x1b[1;33m⇄{} \x1b[0m", crate::batch::short_id(run_id)),
            None => String::new(),
        };
        // TASK-282: surface the active goal + rollup % just before the cwd so it
        // rides along every prompt (empty when there's no active goal).
        let goal_badge = session
            .goal_badge()
            .map(|b| format!("\x1b[35m{b}\x1b[0m "))
            .unwrap_or_default();
        let prompt = format!(
            "{attach}{badge}{goal_badge}\x1b[36m{}\x1b[0m ❯ ",
            short_cwd(&session)
        );
        
        // Consume an accepted-rewrite line first, then an active `:loop`
        // iteration, before falling back to the editor. A loop iteration is fed
        // to the model inline via the `?` route escape so it runs as an agentic
        // turn — never shell-dispatched or auto-offloaded to a coordinator.
        let outcome = match injected.take() {
            Some(l) => ReadOutcome::Line(l),
            // Auto-resume drain: when the last fanned-out coordinator of this
            // session has finished, the presenter armed a coalesced resume (see
            // session::ResumeState). Consume it here — BEFORE the blocking
            // read_line — and run the synthetic continuation as a model turn via
            // the `?` route escape, exactly like a `:loop` tick, so the parent
            // reads + synthesizes the workers' results without the human typing
            // "continue". A session parked inside a blocking read_line when the
            // resume arms is woken hands-free by the presenter, which pushes a
            // newline into the terminal via editor::nudge_terminal_return()
            // (TIOCSTI) so read_line returns and this drain runs on the next pass.
            // Residual: on kernels that gate TIOCSTI (Linux ≥6.2
            // dev.tty.legacy_tiocsti=0) or when stdin isn't a tty, the nudge is a
            // no-op and the resume still drains on the user's next keypress.
            None => match session.take_resume_tick() {
                Some(body) => {
                    println!("\x1b[2m⤵ auto-resume — reading finished background workers\x1b[0m");
                    ReadOutcome::Line(format!("?{body}"))
                }
                None => match session.next_loop_tick() {
                crate::session::LoopTick::Run { index, total, body } => {
                    println!("\x1b[2m↻ loop {index}/{total}\x1b[0m");
                    ReadOutcome::Line(format!("?{body}"))
                }
                crate::session::LoopTick::Done { total } => {
                    println!(
                        "\x1b[2m✓ loop complete ({total} iteration{})\x1b[0m",
                        if total == 1 { "" } else { "s" }
                    );
                    continue;
                }
                crate::session::LoopTick::Idle => {
                    // Refresh the live statusline (updated date/time + model).
                    // Interactive terminals only.
                    //
                    // Footer mode: the statusline + coordinator status live in the
                    // pinned bottom rows. Re-draw them here (cheap, idempotent) so
                    // the clock and attach state stay current above each prompt,
                    // and re-establish the scroll region first if a SIGWINCH fired.
                    //
                    // Inline mode (short terminal / non-footer): print the
                    // statusline directly above the prompt. The startup block
                    // already printed it, so skip the very first refresh to avoid a
                    // duplicated header (see `suppress_statusline_once`).
                    if unsafe { libc::isatty(1) } == 1 {
                        if footer_active {
                            if let Some(t) = term.as_mut() {
                                if resized.swap(false, Ordering::SeqCst) {
                                    t.handle_resize();
                                }
                                let statusline = crate::style::statusline(
                                    crate::update::current_version(),
                                    &backend.describe(),
                                );
                                t.draw_footer(
                                    &coordinator_status_message(&session),
                                    &statusline,
                                );
                            }
                        } else if suppress_statusline_once {
                            suppress_statusline_once = false;
                        } else {
                            println!(
                                "{}",
                                crate::style::statusline(
                                    crate::update::current_version(),
                                    &backend.describe()
                                )
                            );
                        }
                    }
                    editor.read_line(&prompt)
                }
                },
            },
        };
        // The in-place Ctrl-O toggle block (engine::render_raw_toggle) can only
        // erase itself while it's still the last thing on screen — i.e. the
        // prompt sits directly below it. Any other outcome scrolls fresh output
        // in, so drop the erase anchor; only a follow-up Ctrl-O keeps it.
        if !matches!(outcome, ReadOutcome::CtrlO) {
            session.raw_view_rows = 0;
        }
        match outcome {
            ReadOutcome::Line(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                editor.add_history(&line);
                needs_gap = true;
                // Now working — hold background results until the next pause.
                busy.store(true, Ordering::SeqCst);

                // Inline AI command-rewrite preview (S6.4 / TASK-138): `:rewrite
                // <intent>` (alias `:rw`) asks the model to translate intent into
                // ONE concrete command, shows it pre-filled in the editor, and runs
                // it only after the user accepts (Enter) or edits it. Nothing runs
                // unconfirmed — the candidate is only ever placed in the buffer.
                if let Some(intent) = crate::rewrite::parse_invocation(&line) {
                    let intent = intent.to_string();
                    if intent.is_empty() {
                        println!(
                            "\x1b[2musage: :rewrite <what you want to do>  (alias :rw)\x1b[0m"
                        );
                        continue;
                    }
                    // S8.2 / TASK-144: stream the rewrite token-by-token where the
                    // static `⚙ rewriting…` spinner used to sit, so the candidate
                    // assembles live before it lands in the editor.
                    let label = "rewriting…";
                    let mut preview = crate::stream_render::LivePreview::new(
                        label,
                        crate::stream_render::preview_width(5 + label.chars().count()),
                    );
                    print!("{}", crate::stream_render::frame(label, ""));
                    std::io::stdout().flush().ok();
                    let candidate = {
                        let mut on_text = |t: &str| {
                            if let Some(fr) = preview.push(t) {
                                print!("{fr}");
                                std::io::stdout().flush().ok();
                            }
                        };
                        // S8.3 / TASK-145: race the stream against a stdin cancel
                        // watcher — a new keystroke drops the future and returns
                        // to the prompt cleanly.
                        crate::stream_cancel::with_cancel(
                            crate::rewrite::rewrite_to_command_streaming(
                                &backend, &session, &intent, &mut on_text,
                            ),
                        )
                        .await
                    };
                    print!("{}", crate::stream_render::CLEAR_LINE); // wipe the transient preview line
                    std::io::stdout().flush().ok();
                    match candidate {
                        crate::stream_cancel::StreamOutcome::Cancelled => {
                            println!("\x1b[33m^C\x1b[0m rewrite cancelled");
                            continue;
                        }
                        crate::stream_cancel::StreamOutcome::Done(Ok(Some(cmd))) => {
                            println!(
                                "\x1b[2m  candidate — edit, Enter to run, Ctrl-C to cancel:\x1b[0m"
                            );
                            match editor.read_line_with_initial(&prompt, &cmd) {
                                ReadOutcome::Line(edited) => {
                                    let edited = edited.trim().to_string();
                                    if !edited.is_empty() {
                                        // Re-dispatch as if typed (history + routing).
                                        injected = Some(edited);
                                    }
                                }
                                ReadOutcome::Interrupted => {
                                    println!("\x1b[33m^C\x1b[0m rewrite cancelled")
                                }
                                ReadOutcome::CtrlO => toggle_raw_output(&mut session),
                                // Shift-Tab is a worker-cycle key, not meaningful
                                // while editing a candidate — dismiss it.
                                ReadOutcome::ShiftTab => {}
                                ReadOutcome::Eof => break,
                                ReadOutcome::Error(e) => eprintln!("aish: readline error: {e}"),
                            }
                        }
                        crate::stream_cancel::StreamOutcome::Done(Ok(None)) => println!(
                            "\x1b[2m  couldn't express that as a single command — try \x1b[0m?{intent}\x1b[2m to let the model work it out\x1b[0m"
                        ),
                        crate::stream_cancel::StreamOutcome::Done(Err(e)) => {
                            eprintln!("\x1b[31maish:\x1b[0m rewrite failed: {e:#}")
                        }
                    }
                    continue;
                }

                // Model-based next-command suggestion (S6.3 / TASK-137):
                // `:suggest` (alias `:sg`) asks the model for the single most
                // plausible NEXT command given the session so far (recent
                // commands + last output), renders it pre-filled in the editor,
                // and runs it only after the user accepts (Enter) or edits it —
                // the same trust surface as the rewrite preview. An optional
                // trailing hint nudges it (`:sg now run the tests`). Nothing runs
                // unconfirmed: the candidate is only ever placed in the buffer.
                if let Some(hint) = crate::suggest::parse_invocation(&line) {
                    let hint = hint.to_string();
                    // S8.2 / TASK-144: stream the suggestion token-by-token where the
                    // static `⚙ suggesting…` spinner used to sit.
                    let label = "suggesting…";
                    let mut preview = crate::stream_render::LivePreview::new(
                        label,
                        crate::stream_render::preview_width(5 + label.chars().count()),
                    );
                    print!("{}", crate::stream_render::frame(label, ""));
                    std::io::stdout().flush().ok();
                    let candidate = {
                        let mut on_text = |t: &str| {
                            if let Some(fr) = preview.push(t) {
                                print!("{fr}");
                                std::io::stdout().flush().ok();
                            }
                        };
                        // S8.3 / TASK-145: race the stream against a stdin cancel
                        // watcher — a new keystroke drops the future cleanly.
                        crate::stream_cancel::with_cancel(
                            crate::suggest::suggest_next_command_streaming(
                                &backend, &session, &hint, &mut on_text,
                            ),
                        )
                        .await
                    };
                    print!("{}", crate::stream_render::CLEAR_LINE); // wipe the transient preview line
                    std::io::stdout().flush().ok();
                    match candidate {
                        crate::stream_cancel::StreamOutcome::Cancelled => {
                            println!("\x1b[33m^C\x1b[0m suggestion dismissed");
                            continue;
                        }
                        crate::stream_cancel::StreamOutcome::Done(Ok(Some(cmd))) => {
                            println!(
                                "\x1b[2m  suggestion — edit, Enter to run, Ctrl-C to cancel:\x1b[0m"
                            );
                            match editor.read_line_with_initial(&prompt, &cmd) {
                                ReadOutcome::Line(edited) => {
                                    let edited = edited.trim().to_string();
                                    if !edited.is_empty() {
                                        // Re-dispatch as if typed (history + routing).
                                        injected = Some(edited);
                                    }
                                }
                                ReadOutcome::Interrupted => {
                                    println!("\x1b[33m^C\x1b[0m suggestion dismissed")
                                }
                                ReadOutcome::CtrlO => toggle_raw_output(&mut session),
                                // Shift-Tab is a worker-cycle key, not meaningful
                                // while editing a candidate — dismiss it.
                                ReadOutcome::ShiftTab => {}
                                ReadOutcome::Eof => break,
                                ReadOutcome::Error(e) => eprintln!("aish: readline error: {e}"),
                            }
                        }
                        crate::stream_cancel::StreamOutcome::Done(Ok(None)) => println!(
                            "\x1b[2m  no next-command suggestion from the current context — keep going, or \x1b[0m?<intent>\x1b[2m to ask the model\x1b[0m"
                        ),
                        crate::stream_cancel::StreamOutcome::Done(Err(e)) => {
                            eprintln!("\x1b[31maish:\x1b[0m suggestion failed: {e:#}")
                        }
                    }
                    continue;
                }

                if let Some(cmd) = line.strip_prefix(':') {
                    if handle_colon(cmd, &mut backend, &mut session, &mut pending_update).await {
                        break;
                    }
                    continue;
                }

                // Attached to a coordinator: a plain line is operator input steered
                // to it (the `:tell` channel), not a local command or a model turn.
                // `:`-commands above still run — `:detach` ends it.
                let attached_run = session.attached.lock().unwrap().clone();
                if let Some(run_id) = attached_run {
                    send_to_attached(&run_id, &line, &mut session);
                    continue;
                }

                if let Some(db) = &session.db {
                    db.record("input", &session.cwd.to_string_lossy(), &line);
                }

                // Explicit routing escape hatches: `!cmd` forces direct
                // execution, `?text` forces the model.
                let (line, route) = split_route(line);
                if line.is_empty() {
                    continue;
                }

                // Shell-first: when the first word is a real executable (or a
                // builtin/alias), run it directly on the terminal like any
                // shell would. Everything else routes to the model.
                if route != Route::Model {
                    match dispatch(
                        &line,
                        route == Route::Direct,
                        &mut session,
                        &aliases,
                        &mut prev_dir,
                    )
                    .await
                    {
                        Dispatch::Handled => continue,
                        Dispatch::Quit => break,
                        Dispatch::NotACommand => {}
                    }
                }

                // Auto-offload heavy work to a background coordinator. A
                // model-bound line — one the shell-first dispatch above did NOT run
                // as a real command, and that wasn't forced inline with `?` — that
                // carries a troubleshooting or coding/work signal ("troubleshoot",
                // "write", "refactor", "implement", "fix", "migrate", "setup",
                // "build", …) is open-ended engineering that shouldn't tie up the
                // prompt or run inline on the light-touch interactive agent. It
                // goes to a full-tool worker whose result auto-delivers. Real
                // commands (`cargo build`, `make`, …) were already peeled off by the
                // dispatch above, so only genuine prose intent reaches here.
                // Skipped inside a coordinator (no nested coordinators).
                if route == Route::Auto && !session.nested && mentions_work_signal(&line) {
                    println!();
                    // The agent is escalating this work to a background worker on
                    // its own (a work-signal line, outside an explicit :dispatch).
                    // The worker boots with ONLY its task string and never sees
                    // this conversation, so a line like "build a fix for THIS and
                    // open a PR" would strand it — the antecedent of "this" lived
                    // in turns it can't read. Prepend a short, bounded digest of
                    // the most recent turns so deixis resolves. No-op when there's
                    // no prior context (the digest adds nothing on a cold prompt).
                    let task = enrich_task_with_context(&line, &session.history);
                    dispatch_background(&task, &mut session, true);
                    continue;
                }

                // Pick up a just-completed MCP connect so this turn has its tools.
                install_mcp_if_ready(&mut mcp_rx, &mut session);

                // Model turn: set the activity/reply apart from the typed line
                // (direct commands above stay shell-immediate on purpose).
                println!();

                // Agentic turn. A Ctrl-C aborts it. Real process-group ownership
                // means a foreground child (run_interactive) receives terminal
                // signals directly via its own pgrp, so aish only gets SIGINT here
                // when it owns the terminal — i.e. the turn itself is what to abort.
                // No tty_handoff flag needed (TASK-116).
                let pre_len = session.history.len();
                let mut aborted = false;
                let mut reply: Option<String> = None;
                {
                    // Clone the attach-cursor handles up front so a mid-turn
                    // Shift-Tab can cycle the operator's view WITHOUT touching
                    // `&mut session` (the running turn borrows it exclusively).
                    let cyc_workers = session.worker_jobs.clone();
                    let cyc_attached = session.attached.clone();
                    let cyc_review = session.attach_review_announced.clone();
                    let mut confirm = confirm_tty;
                    // Put stdin in cbreak for the turn so Shift-Tab is seen while
                    // the model thinks / is mid tool-call. The RAII guard restores
                    // cooked mode on drop; `confirm_tty` coordinates via the
                    // keywatch gate so y/N prompts still echo + line-edit, and the
                    // reader parks itself during `run_interactive` TTY hand-offs.
                    // Hand the reader thread a direct Shift-Tab action bound to
                    // cloned attach-cursor handles. It fires on the reader thread
                    // itself, so cycling works even during a blocking/CPU-bound
                    // stretch of the turn (heavy escalate/tool work) that never
                    // yields to let `select!` drain the channel — the class of
                    // "Shift-Tab does nothing while thinking/escalating/long tool
                    // run" the channel-only path missed.
                    let cb_workers = cyc_workers.clone();
                    let cb_attached = cyc_attached.clone();
                    let cb_review = cyc_review.clone();
                    let on_shift_tab: crate::keywatch::ShiftTabFn =
                        std::sync::Arc::new(move || {
                            cycle_worker_live(&cb_workers, &cb_attached, &cb_review);
                        });
                    let mut keywatch =
                        crate::keywatch::TurnKeyWatch::install(Some(on_shift_tab));
                    let turn = engine::run_turn(&backend, &mut session, line, &mut confirm);
                    tokio::pin!(turn);
                    loop {
                        tokio::select! {
                            res = &mut turn => {
                                match res {
                                    Ok(text) => reply = Some(text),
                                    Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e:#}"),
                                }
                                break;
                            }
                            _ = tokio::signal::ctrl_c() => {
                                aborted = true;
                                break;
                            }
                            Some(k) = keywatch.recv() => match k {
                                // Shift-Tab pressed mid-turn. The reader thread's
                                // direct callback (installed above) already ran
                                // `cycle_worker_live`, so this branch just drains
                                // the mirrored channel event to keep the receiver
                                // empty — cycling here too would double-advance the
                                // attach cursor. Kept so the branch stays wired for
                                // any future mid-turn key that needs `&mut turn`
                                // ownership the reader thread can't take.
                                crate::keywatch::TurnKey::ShiftTab => {}
                            },
                        }
                    }
                }
                if aborted {
                    // A half-finished turn can leave a dangling tool_use with no
                    // tool_result — the next request would 400. Roll it back.
                    session.history.truncate(pre_len);
                    println!("\x1b[33m^C\x1b[0m turn aborted");
                    // A Ctrl-C also abandons any in-flight `:loop` — the operator
                    // wanted out, not just this one iteration.
                    if session.clear_loop() {
                        println!("\x1b[33m↻\x1b[0m loop cancelled");
                    }
                } else if let Some(text) = reply {
                    if !text.trim().is_empty() {
                        println!("{}", crate::md::render_stdout(text.trim()));
                    }
                    if let Some(db) = &session.db {
                        db.record("output", &session.cwd.to_string_lossy(), &text);
                    }
                }
            }
            ReadOutcome::CtrlO => {
                // The Ctrl-O key handler requested a raw-output toggle; perform it.
                toggle_raw_output(&mut session);
                continue;
            }
            ReadOutcome::ShiftTab => {
                // Shift-Tab cycles the attach cursor across this session's
                // running coordinators (interactive → worker₁ → … → interactive).
                // `cycle_worker` prints the attach status line, the backfilled
                // tail (last 40 transcript rows), and any finished-result pane —
                // ALL of it must land BEFORE the next prompt. Arm `needs_gap` so
                // the loop emits a blank line between that last output row and the
                // redrawn prompt, matching the command path (never a prompt jammed
                // flush against the cycled coordinator's output).
                //
                // `cycle_worker` returns false when there's nothing to separate
                // from the next prompt — no workers to attach to (silent no-op),
                // OR the hop that wraps back to the interactive prompt (it prints
                // nothing and re-anchors the cursor itself). Either way, DON'T arm
                // the gap in those cases.
                if cycle_worker(&mut session) {
                    needs_gap = true;
                }
                continue;
            }
            ReadOutcome::Interrupted => {
                // Ctrl-C at the prompt. When `:attach`ed to a LIVE coordinator,
                // forward it to that worker's process group so its in-flight turn
                // aborts (mirrors the local model-turn abort). Otherwise it just
                // clears the current line and loops, as before.
                interrupt_attached_worker(&session);
                continue;
            }
            ReadOutcome::Eof => break,            // Ctrl-D: exit
            ReadOutcome::Error(e) => {
                eprintln!("aish: readline error: {e}");
                break;
            }
        }
    }

    // Don't leave managed jobs — especially stopped ones — orphaned on exit:
    // hang them up (SIGCONT + SIGHUP) so they terminate with the shell (TASK-123).
    tools::hangup_jobs_on_exit(&session.jobs);

    // Fire SessionEnd and AWAIT it: the process is about to exit, so a detached
    // task would be killed before it ran (hence run_observe_blocking, bounded by
    // each hook's timeout). No-op when no SessionEnd hook is configured.
    if session.hooks.has(crate::hooks::HookEvent::SessionEnd) {
        let p = session.hook_payload(crate::hooks::HookEvent::SessionEnd);
        session
            .hooks
            .run_observe_blocking(crate::hooks::HookEvent::SessionEnd, p)
            .await;
    }

    // If the operator exits while attached to a worker view, close it so the
    // parting line + shell prompt trail the output cleanly. The attach view
    // renders on the primary buffer, so there is no alternate buffer to pop.
    crate::terminal::close_attach_view();

    // Tear down the bottom-anchored footer before we print the parting line, so
    // "bye" lands in a normal full-height screen instead of above a stale pinned
    // footer. `Drop` would also reset it, but doing it explicitly here keeps the
    // exit output clean and ordered.
    if let Some(mut t) = term.take() {
        t.reset_scroll_region();
    }

    editor.save_history();

    // `:restart` (or the post-`:update` auto-restart) asked us to reload rather
    // than exit. Do it as the very last act, after all the exit cleanup above
    // (jobs hung up, SessionEnd fired, footer torn down, history saved), by
    // replacing this process image with a fresh aish carrying the SAME argv.
    // `reexec_aish` only returns if the exec fails.
    // End-of-session dispatch-efficiency digest: if this session offloaded any
    // background work, print a one-block summary so the loop closes without the
    // operator having to remember `:dispatch-stats`. Best-effort + silent when
    // nothing was dispatched (feature 3). Skipped on `:restart` (a reload, not a
    // session end) — and it must run before the drop(session) below.
    if !session.restart_requested {
        print_session_dispatch_digest(&session);
    }

    if session.restart_requested {
        drop(session); // close the DB / release locks before handing off the image
        reexec_aish();
    }

    println!("bye");
    Ok(())
}

/// Print the end-of-session dispatch-efficiency digest for THIS session's
/// background jobs, or nothing when the session dispatched none / the store is
/// unavailable. Kept separate from `:dispatch-stats` so the exit path stays
/// terse (no scope hint, no "unavailable" chatter).
fn print_session_dispatch_digest(session: &Session) {
    let Some(store) = &session.coordinator_store else {
        return;
    };
    let Ok(rows) = store.load_all() else {
        return;
    };
    let refs: Vec<&crate::db::CoordinatorRow> = rows
        .iter()
        .filter(|r| r.session_id.as_deref() == Some(session.session_id.as_str()))
        .collect();
    if refs.is_empty() {
        return;
    }
    let st = crate::dispatch_stats::summarize(&refs);
    for line in crate::dispatch_stats::render(&st, "this session") {
        println!("{line}");
    }
}

/// Reload aish in place: replace the current process image with a fresh run of
/// the same binary and the SAME argv it was launched with (`:restart`, and the
/// post-`:update` auto-restart). Resolving via `current_exe()` means an update
/// that swapped the on-disk binary is picked up automatically.
///
/// On Unix this `exec`s, so it never returns on success — the new aish inherits
/// the terminal directly. It only returns here if the exec call itself failed,
/// in which case the caller falls through to a normal exit.
#[cfg(unix)]
fn reexec_aish() {
    use std::io::Write;
    println!("\x1b[2m↻ restarting aish…\x1b[0m");
    let _ = std::io::stdout().flush();
    // `restart_in_place` canonicalizes `current_exe()` and `exec`s — it replaces
    // the process image and only returns an io::Error if the re-exec FAILED.
    let err = crate::update::restart_in_place();
    eprintln!("\x1b[31maish:\x1b[0m restart failed: {err}");
}

/// Non-Unix fallback: spawn a fresh aish and exit the current one. Best-effort —
/// the two processes briefly overlap, unlike the Unix `exec` hand-off.
#[cfg(not(unix))]
fn reexec_aish() {
    println!("\u{21bb} restarting aish…");
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("aish"));
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => eprintln!("aish: restart failed: {e}"),
    }
}

/// Build the coordinator status line shown in the footer's middle row (row H-1)
/// — the "2nd statusline".
///
/// - When attached to a coordinator it mirrors the Shift-Tab attach
///   announcement — `⇄ attached to <id> (i/n · Shift-Tab to cycle, :detach to
///   stop)` — with a review-mode variant for a coordinator that has finished.
/// - When NOT attached but coordinators exist to cycle into, it shows the
///   detached hint `⇄ detached — back to interactive (Shift-Tab to cycle into a
///   coordinator)` so the 2nd statusline reflects the interactive state instead
///   of going blank.
/// - Blank only when there are no coordinators at all.
///
/// The returned string is already colorized (ANSI when color is enabled); the
/// footer renderer clips it to width via `clip_visible`, which is escape-aware.
fn coordinator_status_message(session: &Session) -> String {
    let attached = session.attached.lock().unwrap().clone();
    // Same ordering as `cycle_worker`: newest coordinator first (spawn order
    // reversed), each paired with a terminal (done/failed) flag.
    let workers: Vec<(String, bool, String)> = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .rev()
        .map(|w| {
            (
                w.id.clone(),
                matches!(w.status().as_str(), "done" | "failed"),
                w.task.clone(),
            )
        })
        .collect();
    let color_on = crate::style::colors_enabled();
    let mut left = coordinator_status_line(attached.as_deref(), &workers, color_on);
    // A fired `:alert` claims the head of this row until the next prompt — the
    // compact banner the presenter stashed. Yellow so it stands apart from the
    // worker badges; the full detail already printed above the prompt.
    if let Some(banner) = session.alert_banner.lock().ok().and_then(|b| b.clone()) {
        let badge = if color_on {
            format!("\x1b[1;33m{banner}\x1b[0m")
        } else {
            banner
        };
        left = if left.trim().is_empty() {
            badge
        } else {
            format!("{badge}  {left}")
        };
    }
    // Right-justify the session name (`:rename`) on this row, directly above the
    // clock on the statusline below — in bold magenta. Blank when unnamed.
    crate::style::second_statusline_at(
        &left,
        session.name.as_deref(),
        crate::style::footer_width(),
        color_on,
    )
}

/// Max visible width of the task-hint suffix appended to an attached
/// coordinator's status line (`… - <hint>`). Kept short so the footer row stays
/// readable; [`clip_visible`] handles the hard terminal-width clamp afterward.
const TASK_HINT_MAX: usize = 48;

/// Compress a worker's task into a compact one-line hint for the attach status
/// line: collapse all internal whitespace/newlines to single spaces, trim, and
/// truncate to `max` chars with a trailing ellipsis. Returns an empty string for
/// an empty/whitespace-only task (caller then omits the ` - <hint>` suffix).
fn short_task_hint(task: &str, max: usize) -> String {
    let collapsed = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated.trim_end())
    }
}

/// The ` - <hint>` suffix appended to a Shift-Tab / cycle attach announcement,
/// or an empty string when the task is blank. Shares [`short_task_hint`] with the
/// footer status line so both surfaces render the same compact task snippet.
fn attach_task_suffix(task: &str) -> String {
    let hint = short_task_hint(task, TASK_HINT_MAX);
    if hint.is_empty() {
        String::new()
    } else {
        format!(" - {hint}")
    }
}

/// Pure form of [`coordinator_status_message`]: caller supplies the attach
/// cursor, the worker list, and the color decision so the rendering (including
/// the detached-back-to-interactive hint and colorization) is unit-testable
/// without a live `Session` or a TTY.
fn coordinator_status_line(
    attached: Option<&str>,
    workers: &[(String, bool, String)],
    color_on: bool,
) -> String {
    use crate::style::{paint_with, Color};
    let n = workers.len();
    match attached {
        // Detached: back at the interactive prompt. Show the cycle hint on the
        // 2nd statusline whenever there is at least one coordinator to cycle
        // into; otherwise leave it blank.
        None => {
            if workers.is_empty() {
                String::new()
            } else {
                paint_with(
                    "⇄ detached — back to interactive (Shift-Tab to cycle into a coordinator)",
                    Color::Cyan,
                    color_on,
                )
            }
        }
        Some(id) => {
            let short = crate::batch::short_id(id);
            let msg = match workers.iter().position(|(w, _, _)| w == id) {
                Some(i) => {
                    let idx = i + 1;
                    let base = if workers[i].1 {
                        format!(
                            "⇄ attached to {short} (finished) ({idx}/{n} · review mode: type to resume, Shift-Tab to cycle, :detach to stop)"
                        )
                    } else {
                        format!(
                            "⇄ attached to {short} ({idx}/{n} · Shift-Tab to cycle, :detach to stop)"
                        )
                    };
                    // Append a compact hint of the task this worker is on, so the
                    // 2nd statusline says WHAT you're attached to, not just which id.
                    let hint = short_task_hint(&workers[i].2, TASK_HINT_MAX);
                    if hint.is_empty() {
                        base
                    } else {
                        format!("{base} - {hint}")
                    }
                }
                // Attached to a run no longer in the local worker list (e.g.
                // reattached across a restart) — still reflect the attach state.
                None => format!("⇄ attached to {short} (:detach to stop)"),
            };
            paint_with(&msg, Color::Yellow, color_on)
        }
    }
}

// ---------------------------------------------------------------------------
// Tab completion — filenames and directories, against the session's cwd
// ---------------------------------------------------------------------------

pub(crate) struct AishHelper {
    cwd: PathBuf,
    /// Session PATH (rc exports + process PATH) used for command-name completion.
    path: String,
    /// Aliases from ~/.aishrc — offered as command names alongside PATH + builtins.
    aliases: Arc<HashMap<String, Vec<String>>>,
    /// Lazily-populated, TTL-refreshed cache of executable names on PATH.
    cmd_cache: Arc<Mutex<Option<CmdCache>>>,
    /// Single-entry memo of the last route-preview decision, keyed on the exact
    /// `(line, cwd)` it was computed for (S5.4 / TASK-133). The Highlighter calls
    /// `route_preview` on *every* repaint, and rustyline repaints on each
    /// keystroke, cursor move, forced refresh, and hint toggle. `route_preview`
    /// walks PATH (`resolve_program` stats each dir) and tokenizes the line, so
    /// recomputing it for an unchanged buffer is the per-repaint cost that reads
    /// as flicker/lag at typing speed. Keyed on `(line, cwd)`, this serves a
    /// repeated paint of the same buffer in O(1); any edit (a new line) or a `cd`
    /// (new cwd) misses and recomputes exactly once.
    route_cache: Mutex<Option<RouteMemo>>,
}

/// One memoized route-preview decision, keyed on the exact `(line, cwd)` it was
/// computed for (S5.4 / TASK-133). `Preview` is `Copy`, so a cache hit is a
/// cheap compare + return with no allocation. Held behind a `Mutex` because the
/// Highlighter only sees `&self`; the lock is uncontended (readline is
/// single-threaded).
struct RouteMemo {
    line: String,
    cwd: PathBuf,
    preview: Preview,
}

impl AishHelper {
    /// Build the helper that backs completion, the live `:`-palette Hinter, and
    /// the route-preview Highlighter. `path` is the session PATH (rc exports +
    /// process PATH); the PATH-scan cache starts empty.
    pub(crate) fn new(
        cwd: PathBuf,
        path: String,
        aliases: Arc<HashMap<String, Vec<String>>>,
    ) -> Self {
        Self {
            cwd,
            path,
            aliases,
            cmd_cache: Arc::new(Mutex::new(None)),
            route_cache: Mutex::new(None),
        }
    }

    /// Re-point completion/highlighting at `cwd` (the session's cwd, which `cd`
    /// mutates between reads).
    pub(crate) fn set_cwd(&mut self, cwd: &Path) {
        self.cwd.clear();
        self.cwd.push(cwd);
    }
}

/// Builtins aish handles itself — always offered as command-name completions.
const BUILTINS: &[&str] = &[
    "cd", "exit", "logout", "login", "jobs", "fg", "bg", "wait", "pwd", "unset", "set", "umask",
    "source", ".", "exec",
];

/// How long a PATH scan stays cached before it's re-scanned (picks up newly
/// installed binaries without re-statting every PATH dir on each TAB).
const CMD_CACHE_TTL: Duration = Duration::from_secs(10);

/// A cached PATH scan, keyed on the PATH string it was taken from.
struct CmdCache {
    path: String,
    scanned_at: Instant,
    names: Vec<String>,
}

// ---------------------------------------------------------------------------
// Colon command palette — completion for `:` REPL meta-commands
// ---------------------------------------------------------------------------

/// The `:`-command catalog: `(name, one-line description)`, kept in alphabetical
/// order so the popup menu reads tidily. Surfaced as a menu when the user types
/// `:` at the start of a line (or TABs a `:` prefix). Keep this in sync with the
/// arms of `handle_colon` and the `:help` text.
const COLON_COMMANDS: &[(&str, &str)] = &[
    ("allow", "list / revoke always-allowed tools & dir grants"),
    (
        "attach",
        "watch + steer a coordinator, or `goal` to watch the goal",
    ),
    ("backend", "switch backend (claude|grok|local)"),
    ("batch", "background batch mode (on|off|status)"),
    (
        "close",
        "remove the attached (or named) coordinator from the worker list + Shift-Tab rotation",
    ),
    ("compact", "compact history, offload to memory"),
    ("context", "show context-window usage"),
    ("detach", "stop watching the attached coordinator"),
    ("dispatch", "launch a background coordinator"),
    (
        "dispatch-stats",
        "background-job dispatch efficiency report (all = every session)",
    ),
    (
        "forget",
        "remove an exited worker (--all-exited: every exited one)",
    ),
    ("goal", "pursue a goal in the background"),
    ("help", "show command help"),
    ("hooks", "list lifecycle hooks + provenance (list|reload)"),
    ("jobs", "list background jobs"),
    ("kill", "kill a background job"),
    ("loop", "re-run a prompt N times inline (status|stop)"),
    ("mcp", "manage MCP servers"),
    ("memories", "stored memories / organize"),
    ("mode", "set confirmation level"),
    ("model", "switch model (opus|sonnet|haiku)"),
    ("model-detect", "pick the best local model for this machine"),
    ("new", "clear conversation history"),
    ("output", "stream coordinators' activity"),
    ("plugin", "plugin provenance (list|info <id>)"),
    ("quit", "exit aish"),
    ("reasoning", "show reasoning-quality telemetry (escalate vs guess)"),
    ("rename", "rename this session"),
    (
        "restart",
        "reload aish with the same command it started with",
    ),
    ("result", "view a finished job's result"),
    (
        "rewrite",
        "AI-rewrite intent into a command (edit/accept before run)",
    ),
    ("skill", "manage skills (add|search|list|remove)"),
    (
        "stop",
        "stand down an in-flight coordinator — harsher than :tell (--any: cross-session)",
    ),
    (
        "suggest",
        "AI-suggest the next command from context (edit/accept before run)",
    ),
    (
        "tell",
        "message an in-flight coordinator (--any: cross-session)",
    ),
    ("update", "upgrade aish to the latest release"),
    ("version", "show aish version + backend"),
    (
        "workers",
        "list this session's coordinators (all = every session)",
    ),
    ("yolo", "toggle yolo mode"),
];

/// Catalog command names whose name starts with `prefix` (the text after the
/// leading `:`), in catalog order. Pure + unit-tested.
fn colon_command_matches(prefix: &str) -> Vec<&'static str> {
    COLON_COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, _)| *name)
        .collect()
}

/// Build the palette completion candidates for the text after a leading `:`.
/// `start` is 0 — the whole `:<prefix>` token is replaced. The display carries
/// the description (what the menu shows); the replacement is `:<name> ` (the
/// space leaves the cursor ready for any argument).
fn complete_colon(after: &str) -> (usize, Vec<Pair>) {
    let pairs = colon_command_matches(after)
        .into_iter()
        .map(|name| {
            let desc = COLON_COMMANDS
                .iter()
                .find(|(n, _)| *n == name)
                .map_or("", |(_, d)| *d);
            Pair {
                display: format!(":{name:<14} {desc}"),
                replacement: format!(":{name} "),
            }
        })
        .collect();
    (0, pairs)
}

/// Rows the `:`-palette hint may occupy, derived from the live terminal height.
///
/// The prompt is anchored to the bottom body row, so a hint of N rows below it
/// scrolls the body up by N. If N reaches the body height the prompt itself
/// scrolls off-screen and rustyline's *relative* cursor arithmetic (it never
/// uses absolute positioning) desyncs — the redraw then clears the wrong rows
/// and the prompt "disappears" until the palette clears on the next cursor move.
/// Capping the palette to the body height less a safety margin keeps the prompt
/// on-screen so the relative math stays consistent. Falls back to a conservative
/// constant off a tty (tests, pipes).
fn palette_max_rows() -> usize {
    const FALLBACK: usize = 12;
    const MARGIN: u16 = 2; // keep the prompt row + one line of breathing room
    match crate::terminal::screen_rows() {
        Some(rows) => rows
            .saturating_sub(crate::terminal::FOOTER_ROWS)
            .saturating_sub(MARGIN)
            .max(1) as usize,
        None => FALLBACK,
    }
}

/// Build the live `:`-command palette shown as an inline hint below the prompt.
/// Returns the menu text — a leading newline, then one dim row per matching
/// command (`:name   description`) — when the buffer is a bare `:`-token with the
/// cursor at its end, or `None` otherwise. rustyline recomputes this on every
/// keystroke and redraws the hint in place (clearing the prior rows), so the
/// list filters as you type WITHOUT re-printing the prompt or stacking menus.
///
/// `max_rows` bounds how tall the menu may grow (see [`palette_max_rows`]): when
/// the match set overflows the cap, the last row becomes a dim `… and N more`
/// summary so the hint never scrolls the bottom-anchored prompt off-screen.
fn palette_hint(line: &str, pos: usize, max_rows: usize) -> Option<String> {
    // Only while typing the command name: cursor at the end of a bare `:`-token
    // (no space yet — once an argument starts, the palette is done).
    if pos != line.len() {
        return None;
    }
    let after = line.strip_prefix(':')?;
    if after.contains(char::is_whitespace) {
        return None;
    }
    let names = colon_command_matches(after);
    if names.is_empty() {
        return None;
    }
    let max_rows = max_rows.max(1);
    // When the list overflows the cap, reserve the final row for the summary.
    let overflow = names.len() > max_rows;
    let shown = if overflow {
        max_rows.saturating_sub(1)
    } else {
        names.len()
    };
    let mut out = String::new();
    for name in names.iter().take(shown) {
        let desc = COLON_COMMANDS
            .iter()
            .find(|(n, _)| n == name)
            .map_or("", |(_, d)| *d);
        // The leading newline drops each row onto its own line below the input;
        // the whole row is dim so it reads as a hint, not as typed text.
        out.push_str(&format!("\n\x1b[2m:{name:<14} {desc}\x1b[0m"));
    }
    if overflow {
        let more = names.len() - shown;
        out.push_str(&format!(
            "\n\x1b[2m… and {more} more (keep typing to filter)\x1b[0m"
        ));
    }
    Some(out)
}

/// An inline hint painted after the prompt. One type carries both shapes the
/// single `Hinter` emits: the display-only `:`-command palette (`completion`
/// `None`, so →/Ctrl-F never insert its menu text), and the fish-style
/// history ghost text (`completion` `Some(suffix)`, so →/Ctrl-F accept it).
/// See S6.2 / TASK-136.
///
/// `display` carries its own ANSI (the palette's dim rows; the ghost text's
/// gray) — rustyline zero-widths SGR escapes when it computes layout, so the
/// embedded color never disturbs cursor math.
pub(crate) struct AishHint {
    display: String,
    completion: Option<String>,
}

impl rustyline::hint::Hint for AishHint {
    fn display(&self) -> &str {
        &self.display
    }
    fn completion(&self) -> Option<&str> {
        self.completion.as_deref()
    }
}

impl Completer for AishHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Colon command palette (explicit TAB). When the buffer is a
        // `:`-prefixed token we complete against the command catalog, not
        // PATH/paths. The live, as-you-type menu is rendered separately by the
        // Hinter (`palette_hint`); this branch only fires on an explicit TAB.
        let before = &line[..pos];
        if let Some(after) = before.strip_prefix(':') {
            if !after.contains(char::is_whitespace) {
                return Ok(complete_colon(after));
            }
        }
        let start = line[..pos]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        // Word one (the command position): everything before the cursor's word is
        // blank. Complete against PATH binaries + builtins + aliases. Later words
        // get flag/subcommand completion for known commands, falling back to
        // filenames.
        if line[..start].trim().is_empty() {
            return Ok(self.complete_command(line, pos, start));
        }
        Ok(complete_line(line, pos, &self.cwd))
    }
}

impl AishHelper {
    /// Command-name completion for the first word: union of cached PATH
    /// executables, builtins, and alias names matching the typed prefix. A
    /// prefix containing `/` is a path (`./script`, `/usr/bin/l`), so defer to
    /// filename completion.
    fn complete_command(&self, line: &str, pos: usize, start: usize) -> (usize, Vec<Pair>) {
        let prefix = &line[start..pos];
        if prefix.contains('/') {
            return complete_path(line, pos, &self.cwd);
        }
        let mut names = self.cached_path_commands();
        names.extend(BUILTINS.iter().map(|s| s.to_string()));
        names.extend(self.aliases.keys().cloned());
        names.sort();
        names.dedup();
        let pairs: Vec<Pair> = names
            .into_iter()
            .filter(|n| n.starts_with(prefix))
            .map(|n| Pair {
                display: n.clone(),
                replacement: n,
            })
            .collect();
        (start, pairs)
    }

    /// PATH executable names, re-scanning only when the PATH changed or the
    /// cached scan aged past `CMD_CACHE_TTL`.
    fn cached_path_commands(&self) -> Vec<String> {
        let mut cache = self.cmd_cache.lock().unwrap();
        let fresh = cache
            .as_ref()
            .is_some_and(|c| c.path == self.path && c.scanned_at.elapsed() < CMD_CACHE_TTL);
        if !fresh {
            *cache = Some(CmdCache {
                path: self.path.clone(),
                scanned_at: Instant::now(),
                names: scan_path_commands(&self.path),
            });
        }
        cache.as_ref().unwrap().names.clone()
    }
}

/// Sorted, de-duplicated executable basenames found across every directory on
/// `path_var`. Symlinks are followed (matching `resolve_program`); the exec bit
/// must be set.
fn scan_path_commands(path_var: &str) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut names: Vec<String> = Vec::new();
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let is_exec = std::fs::metadata(e.path())
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            if is_exec {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The PATH aish dispatches against: the rc-exported PATH if any, else the
/// process PATH.
fn session_path(session: &Session) -> String {
    session
        .env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default()
}

/// The gray (bright-black) SGR the history ghost text is painted with — the
/// fish convention for an unaccepted autosuggestion. rustyline treats SGR
/// escapes as zero-width, so embedding it in the hint `display` keeps the
/// cursor math correct.
const GHOST_COLOR: &str = "\x1b[90m";

/// The trailing remainder of `entry` after the prefix `line` — the ghost text —
/// when `line` is a strict, non-empty, case-sensitive prefix of `entry`. `None`
/// for an empty line, an exact match (nothing left to suggest), or a non-prefix.
/// Pure + unit-tested.
fn ghost_suffix(line: &str, entry: &str) -> Option<String> {
    if line.is_empty() || entry.len() <= line.len() || !entry.starts_with(line) {
        return None;
    }
    Some(entry[line.len()..].to_string())
}

/// Fish-style history autosuggestion: scan `history` newest→oldest for the
/// first entry that strictly extends `line`, and return its ghost-text
/// remainder. Fires only with the cursor at end-of-line (`pos == line.len()`) —
/// fish never suggests mid-edit. The scan walks rustyline's indexed
/// `History::get` (dyn-safe, unlike the RPITIT `iter`) from the most recent
/// entry down, so the freshest match wins.
fn history_suggestion(
    line: &str,
    pos: usize,
    history: &dyn rustyline::history::History,
) -> Option<String> {
    use rustyline::history::SearchDirection;
    if line.is_empty() || pos != line.len() {
        return None;
    }
    let mut idx = history.len();
    while idx > 0 {
        idx -= 1;
        if let Ok(Some(res)) = history.get(idx, SearchDirection::Reverse) {
            if let Some(suffix) = ghost_suffix(line, res.entry.as_ref()) {
                return Some(suffix);
            }
        }
    }
    None
}

impl Hinter for AishHelper {
    type Hint = AishHint;

    /// Emit the inline hint. The live `:`-command palette takes precedence (a
    /// display-only menu); otherwise the fish-style history autosuggestion paints
    /// the most recent matching command as gray ghost text after the cursor,
    /// acceptable with →/Ctrl-F (S6.2 / TASK-136). Returns `None` when neither
    /// applies, leaving ordinary input untouched. rustyline recomputes this each
    /// keystroke and redraws the hint in place.
    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<AishHint> {
        if let Some(menu) = palette_hint(line, pos, palette_max_rows()) {
            return Some(AishHint {
                display: menu,
                completion: None,
            });
        }
        let suffix = history_suggestion(line, pos, ctx.history())?;
        Some(AishHint {
            display: format!("{GHOST_COLOR}{suffix}\x1b[0m"),
            completion: Some(suffix),
        })
    }
}
/// What the dispatch routing WOULD do with the current line — surfaced live as
/// the user types (TASK-132) so a mis-route is visible and correctable BEFORE
/// Enter, instead of silently running the wrong thing. Reuses the exact dispatch
/// predicates so the preview can't disagree with the real decision.
#[derive(PartialEq, Clone, Copy, Debug)]
enum Preview {
    /// Runs directly as a shell command (green).
    Direct,
    /// Goes to the model (cyan).
    Model,
    /// Would dispatch directly, but the lead word is a real binary that's also an
    /// everyday English word (`clear`, `watch`, `pr`, …) — a judgment call worth
    /// a glance (dim). Add `?` to force the model or `!` to force direct.
    Ambiguous,
    /// A `:` REPL command, or nothing to classify — left uncolored.
    Plain,
}

impl Preview {
    /// The ANSI SGR prefix the route-preview Highlighter paints the whole line
    /// with — the single source of truth for the green=direct / cyan=model /
    /// dim=ambiguous contract (TASK-132). `Plain` returns `None` so a `:`-command
    /// or an empty line keeps the terminal's default color.
    fn ansi(self) -> Option<&'static str> {
        match self {
            Preview::Direct => Some("\x1b[32m"), // green — runs as a shell command
            Preview::Model => Some("\x1b[36m"),  // cyan  — handed to the model
            Preview::Ambiguous => Some("\x1b[2m"), // dim   — real binary that's also English
            Preview::Plain => None,              // uncolored — `:`-command / empty
        }
    }
}

/// Classify a line the way `dispatch` will route it, using only what the
/// completer already holds (cwd, PATH, aliases). `$VAR` isn't expanded here — a
/// preview approximation — but every other decision mirrors `dispatch`.
fn route_preview(
    line: &str,
    cwd: &Path,
    path: &str,
    aliases: &HashMap<String, Vec<String>>,
) -> Preview {
    let l = line.trim();
    if l.is_empty() || l.starts_with(':') {
        return Preview::Plain;
    }
    if l.starts_with('!') {
        return Preview::Direct; // forced direct
    }
    if l.starts_with('?') {
        return Preview::Model; // forced model
    }
    // A pipeline runs directly only when every stage resolves to a program.
    if let Some(stages) = pipeline::parse(l) {
        let all = stages.iter().all(|s| {
            s.first()
                .is_some_and(|p| resolve_program(p, cwd, path).is_some())
        });
        return if all { Preview::Direct } else { Preview::Model };
    }
    // Shell machinery / apostrophes don't tokenize → model.
    let Some(words) = rc::tokenize(l) else {
        return Preview::Model;
    };
    let Some(first) = words.first() else {
        return Preview::Plain;
    };
    let aliased = aliases.contains_key(first);
    if !aliased
        && (looks_like_prose(l, &words)
            || is_stray_confirmation(&words)
            || looks_like_command_arg_intent(&words, cwd, path))
    {
        return Preview::Model;
    }
    let resolves = aliased
        || BUILTINS.contains(&first.as_str())
        || resolve_program(first, cwd, path).is_some();
    if !resolves {
        return Preview::Model;
    }
    let lead = first.to_ascii_lowercase();
    if AMBIGUOUS_COMMANDS.contains(&lead.as_str()) || COMMAND_ARG_COMMANDS.contains(&lead.as_str())
    {
        Preview::Ambiguous
    } else {
        Preview::Direct
    }
}

impl AishHelper {
    /// The route-preview decision for `line`, memoized on `(line, cwd)` (S5.4 /
    /// TASK-133). The Highlighter hits this on every repaint; when the buffer is
    /// unchanged from the last paint (a cursor move, a forced refresh, a hint
    /// toggle, or rustyline re-highlighting to reset its highlight flag) it
    /// returns the cached decision without re-walking PATH or re-tokenizing — the
    /// flicker-free contract. The first paint of a new buffer (each keystroke)
    /// computes once and re-seeds the memo.
    fn cached_preview(&self, line: &str) -> Preview {
        let mut cache = self.route_cache.lock().unwrap();
        if let Some(memo) = cache.as_ref() {
            if memo.line == line && memo.cwd == self.cwd {
                return memo.preview;
            }
        }
        let preview = route_preview(line, &self.cwd, &self.path, &self.aliases);
        *cache = Some(RouteMemo {
            line: line.to_string(),
            cwd: self.cwd.clone(),
            preview,
        });
        preview
    }
}

impl Highlighter for AishHelper {
    /// Paint the whole input line by the route `dispatch` would take, so a
    /// mis-route is visible BEFORE Enter (TASK-132). The color is sourced from
    /// [`Preview::ansi`] — the one green/cyan/dim mapping — and a `Plain` line
    /// (a `:`-command or empty buffer) is returned untouched. The route is read
    /// through [`AishHelper::cached_preview`] so a repeated paint of the same
    /// buffer is an O(1) memo hit — the flicker-free guarantee at typing speed
    /// (S5.4 / TASK-133); only a genuine edit pays the recompute, exactly once.
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> std::borrow::Cow<'l, str> {
        match self.cached_preview(line).ansi() {
            Some(color) => std::borrow::Cow::Owned(format!("{color}{line}\x1b[0m")),
            None => std::borrow::Cow::Borrowed(line),
        }
    }

    /// Tell rustyline when a repaint must re-run the highlighter. A bare cursor
    /// move can't change the route (the buffer is identical), so it returns
    /// `false` and rustyline keeps the current colors. Any content edit returns
    /// `true` to re-color the whole line; that repaint's `highlight` call is a
    /// cheap memo lookup that recomputes only because the buffer actually changed
    /// — so the route is computed at most once per distinct line (S5.4 / TASK-133).
    fn highlight_char(&self, _line: &str, _pos: usize, kind: CmdKind) -> bool {
        kind != CmdKind::MoveCursor
    }
}
impl Validator for AishHelper {}
impl Helper for AishHelper {}

// ---------------------------------------------------------------------------
// Command-aware completion (word 2+) — static tables
// ---------------------------------------------------------------------------
//
// Scope decision (TASK-11): static tables, not a carapace-style spec source.
// A spec-driven source would mean vendoring + parsing an external corpus of
// hundreds of tool definitions and carrying a parser/dependency for it. The
// task only asks for four high-traffic tools (git, cargo, docker, kubectl),
// so a handful of `&'static [&str]` tables is the minimum that satisfies it,
// stays dependency-free, and is trivially unit-testable. Any command without a
// table — or any word past the subcommand slot — degrades to path completion.

/// Static completion table for one command: its subcommands (offered in the
/// first-argument slot) and its common flags (offered when the word starts
/// with `-`).
struct CmdSpec {
    subcommands: &'static [&'static str],
    flags: &'static [&'static str],
}

/// Look up the static spec for a command name. Returns `None` for any command
/// we don't have a table for, which is the signal to fall back to path
/// completion (graceful degradation).
fn command_spec(cmd: &str) -> Option<CmdSpec> {
    let spec = match cmd {
        "git" => CmdSpec {
            subcommands: &[
                "add",
                "branch",
                "checkout",
                "cherry",
                "cherry-pick",
                "clone",
                "commit",
                "config",
                "diff",
                "fetch",
                "init",
                "log",
                "merge",
                "mv",
                "pull",
                "push",
                "rebase",
                "remote",
                "reset",
                "restore",
                "revert",
                "rm",
                "show",
                "stash",
                "status",
                "switch",
                "tag",
            ],
            flags: &[
                "--help",
                "--version",
                "--bare",
                "--git-dir",
                "--work-tree",
                "--paginate",
                "--no-pager",
            ],
        },
        "cargo" => CmdSpec {
            subcommands: &[
                "add", "bench", "build", "check", "clean", "clippy", "doc", "fetch", "fix", "fmt",
                "init", "install", "new", "publish", "remove", "run", "search", "test", "tree",
                "update", "vendor",
            ],
            flags: &[
                "--help",
                "--version",
                "--release",
                "--verbose",
                "--quiet",
                "--offline",
                "--locked",
                "--frozen",
                "--all-features",
                "--no-default-features",
                "--features",
                "--target",
                "--manifest-path",
            ],
        },
        "docker" => CmdSpec {
            subcommands: &[
                "attach",
                "build",
                "commit",
                "container",
                "cp",
                "create",
                "exec",
                "image",
                "images",
                "info",
                "inspect",
                "kill",
                "logs",
                "network",
                "pause",
                "ps",
                "pull",
                "push",
                "rename",
                "restart",
                "rm",
                "rmi",
                "run",
                "start",
                "stop",
                "system",
                "tag",
                "top",
                "unpause",
                "version",
                "volume",
            ],
            flags: &["--help", "--version", "--config", "--log-level"],
        },
        "kubectl" => CmdSpec {
            subcommands: &[
                "apply",
                "attach",
                "config",
                "cordon",
                "cp",
                "create",
                "delete",
                "describe",
                "drain",
                "edit",
                "exec",
                "explain",
                "expose",
                "get",
                "logs",
                "patch",
                "port-forward",
                "proxy",
                "rollout",
                "run",
                "scale",
                "set",
                "top",
                "uncordon",
                "version",
                "wait",
            ],
            flags: &[
                "--help",
                "--namespace",
                "--context",
                "--kubeconfig",
                "--output",
                "--all-namespaces",
            ],
        },
        _ => return None,
    };
    Some(spec)
}

/// Build completion `Pair`s for the candidates that start with `prefix`.
fn matches(candidates: &[&str], prefix: &str) -> Vec<Pair> {
    candidates
        .iter()
        .filter(|c| c.starts_with(prefix))
        .map(|c| Pair {
            display: (*c).to_string(),
            replacement: (*c).to_string(),
        })
        .collect()
}

/// Top-level completion dispatcher. Completes flags and subcommands for known
/// commands at word 2+, and falls back to path completion everywhere else
/// (word 1, unknown commands, and any known-command slot with no table match).
fn complete_line(line: &str, pos: usize, cwd: &Path) -> (usize, Vec<Pair>) {
    let start = line[..pos]
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &line[start..pos];

    // First whitespace-delimited token on the line is the command name.
    let cmd = line.split_whitespace().next().unwrap_or("");

    // How many complete tokens precede the word under the cursor. 0 means we're
    // still typing the command itself → path completion (the Completer impl
    // routes that case to command-name completion before calling here).
    let words_before = line[..start].split_whitespace().count();
    if words_before == 0 {
        return complete_path(line, pos, cwd);
    }

    if let Some(spec) = command_spec(cmd) {
        if word.starts_with('-') {
            let pairs = matches(spec.flags, word);
            if !pairs.is_empty() {
                return (start, pairs);
            }
        } else if words_before == 1 {
            // First-argument slot → subcommand completion.
            let pairs = matches(spec.subcommands, word);
            if !pairs.is_empty() {
                return (start, pairs);
            }
        }
        // No table match → fall through to path completion (graceful).
    }

    complete_path(line, pos, cwd)
}

/// Complete the path under the cursor. Splits the current word into a
/// directory part (kept verbatim in the replacement, `~` included) and a name
/// prefix matched against that directory's entries. Directories complete with
/// a trailing `/`; names containing whitespace come back double-quoted so the
/// tokenizer can re-read them.
fn complete_path(line: &str, pos: usize, cwd: &Path) -> (usize, Vec<Pair>) {
    let start = line[..pos]
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &line[start..pos];

    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let base = if dir_part.starts_with('/') {
        PathBuf::from(dir_part)
    } else if let Some(rest) = dir_part.strip_prefix("~/") {
        Path::new(&home).join(rest)
    } else {
        cwd.join(dir_part)
    };

    let Ok(entries) = std::fs::read_dir(&base) else {
        return (start, Vec::new());
    };
    let mut pairs: Vec<Pair> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // hidden entries only when explicitly asked for
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let display = if is_dir {
                format!("{name}/")
            } else {
                name.clone()
            };
            let mut replacement = format!("{dir_part}{name}");
            if replacement.contains(char::is_whitespace) {
                replacement = format!("\"{replacement}\"");
            }
            if is_dir {
                replacement.push('/');
            }
            Some(Pair {
                display,
                replacement,
            })
        })
        .collect();
    pairs.sort_by(|a, b| a.display.cmp(&b.display));
    (start, pairs)
}

// ---------------------------------------------------------------------------
// Direct dispatch — the "real shell" path
// ---------------------------------------------------------------------------

enum Dispatch {
    /// Not an executable/builtin — route the line to the model.
    NotACommand,
    /// Ran (or errored) right here; prompt again.
    Handled,
    /// `exit` / `logout`.
    Quit,
}

#[derive(PartialEq, Clone, Copy)]
enum Route {
    Auto,
    Direct, // `!` prefix
    Model,  // `?` prefix
}

fn split_route(line: String) -> (String, Route) {
    if let Some(rest) = line.strip_prefix('!') {
        (rest.trim().to_string(), Route::Direct)
    } else if let Some(rest) = line.strip_prefix('?') {
        (rest.trim().to_string(), Route::Model)
    } else {
        (line, Route::Auto)
    }
}

/// Real commands that double as English function words. For these, a line of
/// nothing but bare words is almost certainly a question ("who is …",
/// "find me big files"), not an invocation — flags, paths, digits, or quotes
/// flip it back to a command.
///
/// HEURISTIC STOPGAP: this hand-curated word list is a blunt instrument — it
/// can only ever catch commands we thought to enumerate (cf. ISS-1480, the
/// `clear`/`open` class below). The durable fix is model-based route preview
/// (S5/S6: TASK-132/137), which decides routing by understanding the line
/// rather than matching its lead word; this list goes away once that lands.
///
/// Additions in the `clear … green` class are deliberately conservative: a word
/// only belongs here if a 3+word all-alphabetic line starting with it is far
/// more likely prose than a real invocation. Real invocations of these carry a
/// flag, path, dot, or digit (handled by the guards in `looks_like_prose`) — so
/// commands whose plain `cmd a b c` form is a legitimate multi-arg call
/// (`say`, `touch`, `file`, `link`, `paste`, `join`, `mail`, `banner`, …) are
/// intentionally excluded to avoid swallowing real usage.
const AMBIGUOUS_COMMANDS: &[&str] = &[
    "who", "w", "find", "time", "test", "yes", "look", "last", "watch", "date", "which", "what",
    "whatis", "finger", "write", "wall", "users", "top", "more", "head", "tail", "make", "cat",
    "kill", "pr", "wait",
    // clear/open: real use is bare (`clear`) or carries a path/dot/flag
    // (`open file.txt`, `open .`, `open -a App`); a bare-word sentence
    // starting with either ("clear the pointer so it reflects green",
    // "open the door slowly") is prose, not an invocation.
    "clear", "open",
    // cut (ISS-2044): real use carries a `-b/-c/-f` flag and a file/delimiter
    // (`cut -f1 file.txt`, `cut -d, -f2 data`); a bare-word phrase starting
    // with it ("cut new release", "cut release notes") is a release-management
    // intent, not the textutils binary. The membership lookup below already
    // lowercases, so `Cut`/`CUT` match too.
    "cut",
];

/// Subset of `AMBIGUOUS_COMMANDS` whose *idiomatic* first argument is a number:
/// `kill 1234`, `head 50`, `tail 100`, `top 1`, `w 1`, `nice 10`. For these, a
/// digit right after the command is a real operand, so a numeric token must NOT
/// be read as part of an English sentence (see the digit relaxation below).
/// `pr` is deliberately absent: `pr` takes filenames, never a bare PID/line
/// count, so "PR 22 merged" has no command reading.
const NUMERIC_ARG_COMMANDS: &[&str] = &[
    "kill", "head", "tail", "top", "w", "nice", "renice", "fold", "split",
];

fn looks_like_prose(line: &str, words: &[String]) -> bool {
    // The membership check is case-insensitive: on a case-insensitive
    // filesystem (macOS) `PR`/`What`/`FIND` resolve to the lowercase binary, so
    // they're just as ambiguous. We only lowercase for the *lookup* — the argv
    // handed to exec is never mutated.
    if words.len() < 3 {
        return false;
    }
    let lead = words[0].to_ascii_lowercase();
    if !AMBIGUOUS_COMMANDS.contains(&lead.as_str()) || line.contains(['"', '\'']) {
        return false;
    }
    // A comma reads as a sentence, not an argv — "watch for events from atum,
    // let me know…" must go to the model even though `watch` is real.
    if line.contains(", ") {
        return true;
    }

    // Classify each token (trailing sentence punctuation forgiven): is it a
    // plain alphabetic English word, a bare run of digits, or "other" (a flag
    // `-n`, a path `a/b`, a filename `file.txt`, a version `1.2`, …)?
    let trim = |w: &str| {
        w.trim_end_matches([',', '.', '!', '?', ';', ':'])
            .to_string()
    };
    let is_alpha = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphabetic());
    let is_digits = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit());

    let trimmed: Vec<String> = words.iter().map(|w| trim(w)).collect();

    // Fast path / strict rule: a line of nothing but plain words is prose.
    // (Keeps "what is the capital of texas" routing to the model.)
    if trimmed.iter().all(|w| is_alpha(w)) {
        return true;
    }

    // Targeted digit relaxation for "PR 22 merged" / "PR 22 was merged":
    // a developer narrating an event ("PR 22 merged", "issue 7 closed",
    // "find 3 failed") embeds exactly one number in an otherwise-English line.
    // We accept that as prose ONLY when every guard holds, so real invocations
    // can't slip through:
    //   * the lead word does NOT take a numeric operand (so `kill 1234 now`,
    //     `head 50 lines`, `tail 100 fast`, `top 1 now` stay commands — for
    //     those the digit is a genuine argument, not a sentence number);
    //   * the LAST token is a plain alphabetic predicate ("merged"/"closed"/
    //     "is") — a sentence ends in a word, an invocation often ends in a
    //     value/path;
    //   * exactly one token is purely numeric (two numbers reads like argv:
    //     `head 50 100`); and
    //   * every remaining token is plain alphabetic — no flag/path/filename/
    //     version token (those are unambiguous command syntax). This is why
    //     `pr file.txt` (a real pr invocation) is NOT prose: "file.txt" is an
    //     "other" token, and it's only two words anyway.
    if NUMERIC_ARG_COMMANDS.contains(&lead.as_str()) {
        return false;
    }
    let numeric = trimmed.iter().filter(|w| is_digits(w)).count();
    let last_is_word = trimmed.last().is_some_and(|w| is_alpha(w));
    let rest_alpha = trimmed.iter().all(|w| is_alpha(w) || is_digits(w));
    numeric == 1 && last_is_word && rest_alpha
}

/// Ambiguous commands whose argument must itself be a runnable program
/// (`watch ls`, `time make`, `nice cargo`). These never trip `looks_like_prose`
/// — they're typically used as two-word lines, below its `>= 3 words` bar — yet
/// `watch orch_c1b45797b841` (an atum id the user wants the model to look up via
/// MCP) is intent, not an invocation: `orch_…` isn't a command, so a real
/// `watch` can't run it.
///
/// So for these verbs, a bare `<verb> <word>` line routes to the model when the
/// argument doesn't resolve to a program. Deliberately narrow — exactly two
/// words, no leading-dash flag — so genuine usage keeps dispatching directly:
/// `watch df`, `time ls`, `watch -n 5 free`, `time ls -l`. Surface-form
/// heuristic; the durable fix is model-based route preview (S5/S6:
/// TASK-132/137), which understands the line instead of matching its lead word.
const COMMAND_ARG_COMMANDS: &[&str] = &["watch", "time", "nice"];

/// See [`COMMAND_ARG_COMMANDS`]. `cwd`/`path_var` are the same PATH dispatch uses.
fn looks_like_command_arg_intent(words: &[String], cwd: &Path, path_var: &str) -> bool {
    let [verb, arg] = words else {
        return false;
    };
    if !COMMAND_ARG_COMMANDS.contains(&verb.to_ascii_lowercase().as_str()) {
        return false;
    }
    // A flag is unambiguous invocation syntax; only a bare word qualifies.
    if arg.starts_with('-') {
        return false;
    }
    // A resolvable argument means a genuine `watch <cmd>` call; an unresolvable
    // one (an id, a typo, an English word) reads as intent → model.
    resolve_program(arg, cwd, path_var).is_none()
}

/// A lone confirmation token typed at the prompt (`y`, `yes`, `n`, `no`) is
/// almost certainly a stray answer to a prompt that is no longer there — not a
/// request to run the `yes` flood. Route it to the model rather than dispatch
/// `yes`/`y` as a command. Case-insensitive; only a single bare word qualifies,
/// so `yes hello` and a forced `!yes` still run directly.
fn is_stray_confirmation(words: &[String]) -> bool {
    matches!(words, [w] if matches!(w.to_ascii_lowercase().as_str(), "y" | "yes" | "n" | "no"))
}

/// Variable resolver for direct-dispatch `$VAR` expansion. Resolution order:
/// session `export`s first (so a user override wins), then the TASK-13
/// last-output bindings `$LAST` / `$_` (the most recent recorded output,
/// truncated per the last-output policy), then the process environment. This
/// lets the next line reference the previous output without re-running it —
/// e.g. `echo $LAST`.
fn var_lookup(session: &Session) -> impl Fn(&str) -> Option<String> + '_ {
    move |name: &str| {
        if name == "?" {
            return Some(session.last_status.to_string());
        }
        if name == "$" {
            // `$$` — the shell's own pid (S4.6). Resolved live so it stays
            // correct without a stored env entry.
            return Some(std::process::id().to_string());
        }
        if let Some(v) = session
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
        {
            return Some(v);
        }
        if matches!(name, "LAST" | "_") {
            if let Some(out) = session.last_output() {
                return Some(out);
            }
        }
        std::env::var(name).ok()
    }
}

async fn dispatch(
    line: &str,
    force: bool,
    session: &mut Session,
    aliases: &HashMap<String, Vec<String>>,
    prev_dir: &mut Option<PathBuf>,
) -> Dispatch {
    // A pipeline (a | b | c) is the one bit of shell syntax aish runs itself:
    // connect each stage's stdout to the next stage's stdin. Run it directly
    // only when every stage is a real program; otherwise route to the model.
    if let Some(stages) = pipeline::parse(line) {
        // Same session-aware PATH the single-command path uses below.
        let path_var = session
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        if stages.iter().all(|s| {
            s.first()
                .is_some_and(|p| resolve_program(p, &session.cwd, &path_var).is_some())
        }) {
            match pipeline::run(&stages, session).await {
                Ok(status) => {
                    session.set_last_status(&status);
                    if let Some(code) = status.code() {
                        if code != 0 {
                            eprintln!("\x1b[2m[exit {code}]\x1b[0m");
                        }
                    }
                }
                Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e:#}"),
            }
            return Dispatch::Handled;
        }
        if force {
            eprintln!("aish: a pipeline stage isn't an executable — can't run it directly");
            return Dispatch::Handled;
        }
        return Dispatch::NotACommand;
    }

    // Lines with shell machinery (globs, redirection, …) or that don't
    // tokenize (apostrophes in English) go to the model. `$VAR` references are
    // expanded here against the session's exports first, then the process
    // environment — matching what the spawned program would see.
    let mut words = match rc::tokenize_diagnosed(line, var_lookup(session)) {
        Ok(w) => w,
        Err(diag) => {
            // Forced (`!`): surface WHY — caret + `aish::parse::` code + help.
            // Auto: stay silent and route to the model (zero behavior change).
            if force {
                crate::diag::eprint(&diag);
                return Dispatch::Handled;
            }
            return Dispatch::NotACommand;
        }
    };
    let Some(first) = words.first() else {
        return Dispatch::NotACommand;
    };

    // English that happens to start with a command word ("who is …"), a bare
    // stray confirmation ("yes"), or a command-taking verb whose argument isn't
    // a real program ("watch orch_…") goes to the model — unless the user forced
    // it with `!` or aliased the word.
    if !force
        && !aliases.contains_key(first)
        && (looks_like_prose(line, &words)
            || is_stray_confirmation(&words)
            || looks_like_command_arg_intent(&words, &session.cwd, &session_path(session)))
    {
        return Dispatch::NotACommand;
    }

    if let Some(expansion) = aliases.get(first) {
        let mut expanded = expansion.clone();
        expanded.extend(words.into_iter().skip(1));
        words = expanded;
    }

    // Tilde expansion for arguments — programs receive literal argv.
    let home = std::env::var("HOME").unwrap_or_default();
    for w in words.iter_mut().skip(1) {
        if *w == "~" {
            w.clone_from(&home);
        } else if let Some(rest) = w.strip_prefix("~/") {
            *w = format!("{home}/{rest}");
        }
    }

    match words[0].as_str() {
        "exit" | "logout" => Dispatch::Quit,
        "login" => {
            builtin_login(words.get(1).map(String::as_str));
            Dispatch::Handled
        }
        "cd" => {
            builtin_cd(words.get(1).map(String::as_str), session, prev_dir);
            Dispatch::Handled
        }
        "pwd" => {
            builtin_pwd(session);
            Dispatch::Handled
        }
        "unset" => {
            builtin_unset(&words[1..], session);
            Dispatch::Handled
        }
        "set" => {
            builtin_set(&words[1..], session);
            Dispatch::Handled
        }
        "umask" => {
            builtin_umask(words.get(1).map(String::as_str), session);
            Dispatch::Handled
        }
        "source" | "." => {
            builtin_source(&words[1..], session);
            Dispatch::Handled
        }
        "exec" => {
            builtin_exec(&words[1..], session);
            Dispatch::Handled
        }
        "jobs" => {
            builtin_jobs(session);
            Dispatch::Handled
        }
        "bg" => {
            builtin_bg(words.get(1).map(String::as_str), session);
            Dispatch::Handled
        }
        "fg" => {
            builtin_fg(words.get(1).map(String::as_str), session).await;
            Dispatch::Handled
        }
        "wait" => {
            builtin_wait(words.get(1).map(String::as_str), session).await;
            Dispatch::Handled
        }
        cmd => {
            // Resolve against the session's PATH — which includes any
            // `export PATH="$PATH:…"` from ~/.aishrc — falling back to the
            // process PATH when the rc file sets none.
            let path_var = session
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var("PATH").ok())
                .unwrap_or_default();
            let Some(path) = resolve_program(cmd, &session.cwd, &path_var) else {
                if force {
                    // Forced (`!`) miss: render a coded `aish::exec::not_found`
                    // with a cheap bounded did-you-mean drawn from $PATH.
                    let candidates = scan_path_commands(&path_var);
                    let hint =
                        crate::diag::nearest_command(cmd, candidates.iter().map(String::as_str))
                            .map(|near| format!("did you mean `{near}`?"));
                    crate::diag::eprint(&crate::diag::AishDiagnostic::exec_not_found(cmd, hint));
                    return Dispatch::Handled;
                }
                return Dispatch::NotACommand;
            };
            match tools::run_on_tty(&path.to_string_lossy(), &words[1..], &[], session).await {
                Ok(status) => {
                    session.set_last_status(&status);
                    if let Some(code) = status.code() {
                        if code != 0 {
                            eprintln!("\x1b[2m[exit {code}]\x1b[0m");
                        }
                    }
                }
                Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e:#}"),
            }
            Dispatch::Handled
        }
    }
}

fn builtin_cd(arg: Option<&str>, session: &mut Session, prev: &mut Option<PathBuf>) {
    let home = std::env::var("HOME").unwrap_or_default();
    let target = match arg {
        None | Some("~") => PathBuf::from(&home),
        Some("-") => match prev.clone() {
            Some(p) => p,
            None => {
                eprintln!("cd: no previous directory");
                return;
            }
        },
        Some(p) => session.cwd.join(p),
    };
    match target.canonicalize() {
        Ok(c) if c.is_dir() => {
            // Keep $OLDPWD/$PWD in step with the move so spawned children see the
            // shell's real working directory (S4.3). OLDPWD is where we were; PWD
            // is where we land. Both go through set_var so each stays a single,
            // last-wins entry in the per-spawn env.
            let old = std::mem::replace(&mut session.cwd, c.clone());
            session.set_var("OLDPWD", old.to_string_lossy().into_owned());
            session.set_var("PWD", c.to_string_lossy().into_owned());
            *prev = Some(old);
        }
        Ok(c) => eprintln!("cd: {}: not a directory", c.display()),
        Err(e) => eprintln!("cd: {}: {e}", target.display()),
    }
}

// ---------------------------------------------------------------------------
// Env-mutating builtins — pwd / unset / set / umask (S4.1)
// ---------------------------------------------------------------------------
//
// A subprocess can’t change its parent, so these run in-process and mutate the
// session state every spawn inherits (`session.env` → Command::envs, the process
// umask → inherited by children). `pwd` reports the session’s own cwd — which
// `cd` maintains — rather than shelling out.

/// `pwd` — print the shell’s working directory (the session’s cwd, which `cd`
/// keeps current). A builtin so it always agrees with `session.cwd` and needs no
/// `/bin/pwd`.
fn builtin_pwd(session: &Session) {
    println!("{}", session.cwd.display());
    // pwd of an existing cwd cannot fail.
    // (last_status left to the caller’s default success.)
}

/// `login <plugin-id>` — route to the plugin that declares
/// `provides.login == <plugin-id>`, run its `login.sh` auth handler, and persist
/// the returned credentials to `~/.aish/credentials` under `[profile:<id>]`
/// (Phase 0.5.5). Secret values are never echoed — only the profile name and the
/// field names that were stored. Tenant id is best-effort from `AISH_TENANT_ID`.
fn builtin_login(name: Option<&str>) {
    let Some(name) = name else {
        eprintln!("login: usage: login <plugin-id>");
        eprintln!("  routes to the plugin declaring \"provides\": {{ \"login\": \"<plugin-id>\" }}");
        return;
    };
    let plugins_dir = crate::plugins::default_plugins_dir();
    let tenant = std::env::var("AISH_TENANT_ID").ok();
    match crate::plugin_auth::login(name, &plugins_dir, tenant.as_deref()) {
        Ok(out) => {
            println!(
                "login: {} authenticated — stored [{}] in {} ({} field(s): {})",
                out.plugin_id,
                out.profile,
                out.path.display(),
                out.field_names.len(),
                out.field_names.join(", "),
            );
            println!(
                "  reference it as ${{profile:{name}}} in this plugin's .mcp.json, \
                 or read $AISH_PROFILE_{} in its lifecycle hooks",
                name.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
                    .collect::<String>()
            );
        }
        Err(e) => eprintln!("login: {e:#}"),
    }
}

/// `unset NAME...` — drop each variable from the session env so later spawns no
/// `unset NAME...` — drop each variable from the session env so later spawns no
/// longer carry it. Unknown names are a no-op, as in POSIX.
fn builtin_unset(names: &[String], session: &mut Session) {
    if names.is_empty() {
        eprintln!("unset: usage: unset NAME...");
        session.last_status = 2;
        return;
    }
    for name in names {
        session.unset_var(name);
    }
    session.last_status = 0;
}

// source / . / exec builtins (S4.2)
// ---------------------------------------------------------------------------
//
// A subprocess can't change its parent, so `source`/`.` run in-process: they
// re-read a file through the same parser ~/.aishrc uses (`rc::parse`) and fold
// its `export NAME=value` lines into `session.env` (last-wins, so every later
// spawn sees the new value). `exec` is the opposite — it replaces the aish
// process image with the named program, mirroring a POSIX shell's `exec`.

/// `source FILE` / `. FILE` — apply FILE's `export`/`alias` lines to the live
/// session. There is no shell underneath aish, so "sourcing" reuses
/// `rc::parse`: `export` lines update the session env; functions/conditionals
/// are ignored just as they are in ~/.aishrc. Aliases are parsed but cannot be
/// installed at runtime (the alias table is built once at startup), so only the
/// env exports take effect this session — reported, not silently dropped.
fn builtin_source(args: &[String], session: &mut Session) {
    let Some(path) = args.first() else {
        eprintln!("source: usage: source FILE");
        session.last_status = 2;
        return;
    };
    let resolved = {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            session.cwd.join(p)
        }
    };
    let text = match std::fs::read_to_string(&resolved) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("source: {path}: {e}");
            session.last_status = 1;
            return;
        }
    };
    let parsed = rc::parse(&text);
    for (k, v) in parsed.env {
        // last-wins so later spawns carry the new value (var_lookup reads in rev).
        session.env.retain(|(k2, _)| k2 != &k);
        session.env.push((k, v));
    }
    if !parsed.aliases.is_empty() {
        eprintln!(
            "\x1b[2msource: {path}: applied env exports; {} alias(es) ignored (aliases load at startup)\x1b[0m",
            parsed.aliases.len()
        );
    }
    session.last_status = 0;
}

/// `set` — with no args, list the session env as sorted `NAME=value` lines (the
/// POSIX “list shell variables” shape). With `NAME=value` operands, assign each
/// into the session env (last-wins) so the new value reaches every later spawn.
/// A bare word that isn’t an assignment is reported and skipped.
fn builtin_set(args: &[String], session: &mut Session) {
    if args.is_empty() {
        // Collapse to last-wins, then sort by name for a stable listing.
        let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (k, v) in &session.env {
            seen.insert(k.as_str(), v.as_str());
        }
        let mut pairs: Vec<(&str, &str)> = seen.into_iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in pairs {
            println!("{k}={v}");
        }
        session.last_status = 0;
        return;
    }
    let mut ok = true;
    for a in args {
        match a.split_once('=') {
            Some((name, value)) if !name.is_empty() => session.set_var(name, value),
            _ => {
                eprintln!("set: {a}: not a NAME=value assignment");
                ok = false;
            }
        }
    }
    session.last_status = if ok { 0 } else { 1 };
}

/// `umask [mode]` — with no arg, print the current file-creation mask in octal;
/// with an octal `mode`, set it. The mask is process state inherited by every
/// child, so this is reflected in spawned programs. Reading is the standard
/// set-to-zero-then-restore dance (there is no getter in libc).
fn builtin_umask(arg: Option<&str>, session: &mut Session) {
    match arg {
        None => {
            // SAFETY: querying the umask by setting it to 0 and restoring it.
            let cur = unsafe {
                let m = libc::umask(0);
                libc::umask(m);
                m
            };
            println!("{:04o}", cur as u32);
            session.last_status = 0;
        }
        Some(spec) => match u32::from_str_radix(spec, 8) {
            Ok(mode) if mode <= 0o777 => {
                // SAFETY: setting the process file-creation mask.
                unsafe { libc::umask(mode as libc::mode_t) };
                session.last_status = 0;
            }
            _ => {
                eprintln!("umask: {spec}: invalid octal mode");
                session.last_status = 1;
            }
        },
    }
}

/// `exec CMD [args...]` — replace the aish process image with CMD, the way a
/// POSIX shell's `exec` does. The session's `export`s and cwd are applied first
/// (same `.current_dir().envs(session.env)` shape every normal spawn uses), so
/// the replacement inherits exactly what a spawned child would. With no
/// argument `exec` is a successful no-op (bash would apply redirections and
/// return; aish has none). On failure we print the error and set a non-zero
/// status WITHOUT exiting — `exec` only returns when it could not run the
/// program, and the shell must survive that.
fn builtin_exec(args: &[String], session: &mut Session) {
    use std::os::unix::process::CommandExt;
    let Some(program) = args.first() else {
        session.last_status = 0;
        return;
    };
    let path_var = session
        .env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let target = resolve_program(program, &session.cwd, &path_var)
        .unwrap_or_else(|| std::path::PathBuf::from(program));
    let err = std::process::Command::new(&target)
        .args(&args[1..])
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .exec();
    eprintln!("exec: {program}: {err}");
    session.last_status = 127;
}

// ---------------------------------------------------------------------------
// Job-control builtins — jobs / fg / bg / wait (TASK-122 / S3.5)
// ---------------------------------------------------------------------------
//
// These operate on the unified job table (src/jobs.rs): `jobs` lists, `bg`/`fg`
// resume a stopped job (SIGCONT to its process group), and `fg`/`wait` block on
// a job until it terminates. Job selection and listing live in `jobs.rs` (pure,
// unit-tested); the SIGCONT signalling lives in `tools::resume_job`; this layer
// is the dispatch glue plus the foreground/`wait` blocking.

/// Parse an optional `%n`/`n`/`%%` operand and resolve it to a job. `None`
/// selects the current job. The error string is surfaced as `<cmd>: <error>`.
fn select_job(arg: Option<&str>, session: &Session) -> Result<Arc<tools::Job>, String> {
    let spec = match arg {
        None => None,
        Some(tok) => {
            Some(crate::jobs::JobSpec::parse(tok).ok_or_else(|| format!("{tok}: no such job"))?)
        }
    };
    let table = session.jobs.lock().unwrap();
    crate::jobs::resolve_job(&table, spec)
}

/// `jobs` — list every tracked job and its state. Shares its formatting with the
/// `:jobs` meta-command via [`crate::jobs::format_listing`].
fn builtin_jobs(session: &mut Session) {
    let lines = {
        let table = session.jobs.lock().unwrap();
        crate::jobs::format_listing(&table)
    };
    if lines.is_empty() {
        println!("no jobs");
    }
    for line in &lines {
        println!("{line}");
    }
    session.last_status = 0;
}

/// `bg [job]` — resume a stopped job in the background (POSIX `bg`): SIGCONT it
/// and let it keep running detached. A job that is already running or has
/// finished is reported without re-signalling, matching bash.
fn builtin_bg(arg: Option<&str>, session: &mut Session) {
    match select_job(arg, session) {
        Err(msg) => {
            eprintln!("bg: {msg}");
            session.last_status = 1;
        }
        Ok(job) if job.is_done() => {
            eprintln!("bg: job {} has terminated ({})", job.id, job.status());
            session.last_status = 1;
        }
        Ok(job) if job.is_stopped() => {
            tools::resume_job(&job);
            println!("[{}] {} &", job.id, job.desc);
            session.last_status = 0;
        }
        Ok(job) => {
            // Already running in the background — bash treats this as a no-op (exit 0).
            eprintln!("bg: job {} already in background", job.id);
            session.last_status = 0;
        }
    }
}

/// `fg [job]` — bring a job to the foreground (POSIX `fg`): SIGCONT it if it was
/// stopped, then block until it finishes. The job's output already streams to
/// the terminal and its stdin is `/dev/null`, so "foreground" here means the
/// prompt waits on the job — there is no terminal hand-off to perform.
async fn builtin_fg(arg: Option<&str>, session: &mut Session) {
    let job = match select_job(arg, session) {
        Err(msg) => {
            eprintln!("fg: {msg}");
            session.last_status = 1;
            return;
        }
        Ok(job) => job,
    };
    if job.is_done() {
        eprintln!("fg: job {} has terminated ({})", job.id, job.status());
        session.last_status = 1;
        return;
    }
    println!("{}", job.desc);
    if job.is_stopped() {
        tools::resume_job(&job);
    }
    let summary = await_job(&job).await;
    session.last_status = job_exit_code(&summary);
}

/// `wait [job]` — wait for jobs to terminate (POSIX `wait`). With an operand,
/// waits for that job; with none, waits for every active job. A stopped job will
/// never terminate on its own, so — like bash with job control — `wait` returns
/// immediately for one, reporting `128 + SIGTSTP`.
async fn builtin_wait(arg: Option<&str>, session: &mut Session) {
    match arg {
        Some(tok) => {
            let job = match select_job(Some(tok), session) {
                Err(msg) => {
                    eprintln!("wait: {msg}");
                    session.last_status = 1;
                    return;
                }
                Ok(job) => job,
            };
            if job.is_stopped() {
                session.last_status = 128 + libc::SIGTSTP;
                return;
            }
            let summary = await_job(&job).await;
            session.last_status = job_exit_code(&summary);
        }
        None => {
            // Snapshot the active (not done, not stopped) jobs, then block on each.
            let active: Vec<Arc<tools::Job>> = {
                let table = session.jobs.lock().unwrap();
                table
                    .iter()
                    .filter(|j| !j.is_done() && !j.is_stopped())
                    .cloned()
                    .collect()
            };
            for job in &active {
                await_job(job).await;
            }
            session.last_status = 0;
        }
    }
}

/// Block until `job` reaches its terminal state, returning its exit summary.
/// Polls the shared job state — the background waiter (see
/// `tools::spawn_background`) flips it to `Done`; the job's output streams to the
/// terminal independently, so there is nothing to forward here.
async fn await_job(job: &Arc<tools::Job>) -> String {
    loop {
        if job.is_done() {
            return job.status();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Map a job's terminal summary (`"exited N"`, `"killed"`, …; produced by
/// `tools::spawn_background`'s waiter) to a `$?` value the way a POSIX shell
/// would: the child's exit code when it exited cleanly, else non-zero.
fn job_exit_code(summary: &str) -> i32 {
    summary
        .strip_prefix("exited ")
        .and_then(|n| n.trim().parse::<i32>().ok())
        .unwrap_or(1)
}

/// PATH lookup with the executable bit checked — `which`, basically.
pub(crate) fn resolve_program(cmd: &str, cwd: &Path, path_var: &str) -> Option<PathBuf> {
    fn is_exec(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        p.is_file()
            && p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    if cmd.contains('/') {
        let p = if Path::new(cmd).is_absolute() {
            PathBuf::from(cmd)
        } else {
            cwd.join(cmd)
        };
        return is_exec(&p).then_some(p);
    }
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(dir).join(cmd);
        if is_exec(&p) {
            return Some(p);
        }
    }
    None
}

/// Flip raw tool output and print a one-line status. Ctrl-O is a symmetric
/// toggle: switching ON reveals the most recent turn's raw results verbatim,
/// switching OFF re-collapses them to the `N lines of output — Ctrl-O to expand`
/// summary — so repeated Ctrl-O flips the last turn between expanded and
/// collapsed views.
fn toggle_raw_output(session: &mut Session) {
    session.raw_tool_output = !session.raw_tool_output;
    // Flip the last turn's tool output in place (expand ⇆ collapse), erasing the
    // previously-rendered block so a second Ctrl-O truly re-collapses instead of
    // appending a summary below the still-visible expanded text.
    engine::render_raw_toggle(session, session.raw_tool_output);
}

/// True when a prompt line mentions "troubleshoot" (case-insensitive). Such a
/// line is auto-offloaded to a background coordinator — open-ended diagnostic
/// work that shouldn't block the interactive prompt.
fn mentions_troubleshoot(line: &str) -> bool {
    line.to_ascii_lowercase().contains("troubleshoot")
}

/// Coding/work signal stems that — like a "troubleshoot" line — mark a prompt as
/// heavy, open-ended engineering work to auto-offload to a background
/// coordinator. Matched as a token PREFIX (case-folded) so inflected forms are
/// caught too: `build` → building/builds, `refactor` → refactoring/refactored,
/// `migrate` → migrating/migration, `implement` → implementing/implementation,
/// `fix` → fixing/fixes, `write` → writing/writes, `setup` → setups.
///
/// HEURISTIC STOPGAP, like AMBIGUOUS_COMMANDS: a hand-curated stem list, blunt by
/// nature. It runs only on lines the shell-first dispatch already declined to run
/// as a real command, so a command argument (`cargo build`, `git fixup`) can't
/// trip it — only genuine prose intent reaches it. The durable fix is model-based
/// route preview.
const WORK_SIGNALS: &[&str] = &[
    "write",
    "refactor",
    "implement",
    "fix",
    "migrate",
    "setup",
    "build",
];

/// True when a model-bound line carries a troubleshooting or coding/work signal,
/// marking it for auto-offload to a background coordinator (see the REPL call
/// site). `troubleshoot` keeps its original substring match; the WORK_SIGNALS
/// stems are matched as token prefixes so inflected forms count without
/// false-matching a signal buried mid-word (`prefix`/`suffix` never START a
/// token with a signal stem, so they don't match).
/// Clip a message to a single tidy line of at most `max` chars for a context
/// digest: internal whitespace/newlines collapse to single spaces, and an
/// over-long line is truncated on a char boundary with a trailing ellipsis.
fn clip_for_digest(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

/// Enrich a raw auto-offload line with a bounded digest of the most recent
/// conversation turns.
///
/// The deterministic auto-offload path (a work-signal line typed at the prompt)
/// spawns a background coordinator whose ONLY task-specific content is the raw
/// typed line — it never inherits the interactive transcript. A line like
/// "build a fix for this and open a PR" then loses its antecedent: whatever
/// "this" referred to lived in turns the worker can't see. This prepends a short
/// digest of recent turns so pronouns/deixis resolve, then appends the operator's
/// actual instruction verbatim as the trailing (and therefore load-bearing) line.
///
/// Bounded on purpose: at most `MAX_CTX_MSGS` recent messages that carry visible
/// text, each clipped to `MAX_CTX_CHARS`, so a huge session can't bloat the task
/// string — which also feeds the coordinator's same-task dedup key and the
/// `:workers` display. Returns `line` unchanged when there is no prior visible
/// context to attach (a cold prompt), so a fresh session offloads exactly as
/// before.
fn enrich_task_with_context(line: &str, history: &[crate::backend::Msg]) -> String {
    use crate::backend::Role;
    const MAX_CTX_MSGS: usize = 6;
    const MAX_CTX_CHARS: usize = 600;

    // Walk newest→oldest, keeping messages with visible text (tool-result and
    // prefill carriers have empty `text`) until the cap.
    let mut picked: Vec<&crate::backend::Msg> = Vec::new();
    for msg in history.iter().rev() {
        if msg.text.trim().is_empty() {
            continue;
        }
        picked.push(msg);
        if picked.len() >= MAX_CTX_MSGS {
            break;
        }
    }
    if picked.is_empty() {
        return line.to_string();
    }
    picked.reverse(); // restore chronological order for readability

    let mut digest = String::from(
        "=== Recent conversation context (for reference — the operator's request at the end may refer to it) ===\n",
    );
    for msg in picked {
        let who = match msg.role {
            Role::User => "Operator",
            Role::Assistant => "Assistant",
        };
        digest.push_str(who);
        digest.push_str(": ");
        digest.push_str(&clip_for_digest(msg.text.trim(), MAX_CTX_CHARS));
        digest.push('\n');
    }
    digest.push_str("=== End context ===\n\n");
    digest.push_str(line);
    digest
}

fn mentions_work_signal(line: &str) -> bool {
    if mentions_troubleshoot(line) {
        return true;
    }
    let lower = line.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .any(|tok| WORK_SIGNALS.iter().any(|sig| tok.starts_with(sig)))
}

/// Launch a background coordinator for `task`, returning the status line to
/// print. Shared by the `:dispatch` command and the automatic "troubleshoot"
/// auto-offload so both spawn workers identically (full toolset, shared cwd,
/// result auto-delivers). Guards: refuses an empty task, refuses to nest inside
/// a coordinator, and refuses when the active backend has no credential.
/// The outcome of attempting to launch a background coordinator: the human
/// message to print, plus the new coordinator id to auto-attach to when a child
/// was actually spawned. A guard failure (empty task, nested, no credential,
/// missing binary) carries `id: None` so the caller prints the message and does
/// NOT attach.
struct Dispatched {
    /// `Some(run_id)` when a coordinator was spawned (the id to auto-attach to);
    /// `None` on any guard / spawn failure.
    id: Option<String>,
    /// The line to print to the operator.
    message: String,
}

impl Dispatched {
    /// A guard failure / no-spawn outcome: just a message, nothing to attach to.
    fn message_only(message: impl Into<String>) -> Self {
        Self {
            id: None,
            message: message.into(),
        }
    }
}

fn dispatch_coordinator(task: &str, session: &mut Session) -> Dispatched {
    let task = task.trim();
    if task.is_empty() {
        return Dispatched::message_only(
            "usage: :dispatch <task>   — launch a background coordinator for <task>",
        );
    }
    if session.nested {
        return Dispatched::message_only(
            "can't dispatch from inside a coordinator (no nested coordinators)",
        );
    }
    let no_credential = match session.backend_kind.as_str() {
        "grok" => !crate::backend::grok::credential_available(&session.env),
        _ => crate::backend::claude::Credential::resolve(&session.env).is_err(),
    };
    if no_credential {
        return Dispatched::message_only(
            "no credential for the active backend — Claude: CLAUDE_CODE_OAUTH_TOKEN/ANTHROPIC_API_KEY · Grok: ~/.grok/auth.json or XAI_API_KEY",
        );
    }
    match std::env::current_exe() {
        Ok(exe) => {
            // Isolate a writing/building coordinator in its own git worktree +
            // fresh branch when we're inside a repo, mirroring the
            // `run_in_background` tool's smart-default (tools.rs). Without this an
            // interactively-dispatched coordinator ran in the SHARED cwd and
            // committed onto whatever branch the interactive session happened to
            // be on — the headline worktree bug (e.g. TASK-200's commit landing on
            // `fix/shift-tab-replay-history` instead of its own branch). Isolation
            // is free for a no-change run: the worktree auto-removes on completion,
            // and a job that does commit leaves its branch reported for review.
            let isolate = crate::worker::is_git_repo(&session.cwd);
            let spec = crate::worker::WorkerSpec {
                exe,
                cwd: session.cwd.clone(),
                backend: session.backend_kind.clone(),
                model: crate::worker::coordinator_model(
                    &session.backend_kind,
                    &session.batch_model,
                ),
                env: session.env.clone(),
                isolate,
                base: "main".to_string(),
                launch_session_id: session.session_id.clone(),
                launch_session_name: session.name.clone(),
                show_output: session.show_worker_output.clone(),
                attached: session.attached.clone(),
                coordinator_store: session.coordinator_store.clone(),
            };
            let id = crate::worker::spawn(&session.worker_jobs, task.to_string(), spec);
            let message = format!(
                "\x1b[2mdispatched background coordinator \x1b[0m\x1b[1;36m{id}\x1b[0m\x1b[2m \
— runs here with the full toolset; result auto-delivers. \x1b[0m\x1b[36m:workers\x1b[0m\x1b[2m to check.\x1b[0m"
            );
            Dispatched {
                id: Some(id),
                message,
            }
        }
        Err(e) => Dispatched::message_only(format!(
            "can't locate the aish binary to launch the coordinator: {e}"
        )),
    }
}

/// Launch a background coordinator for `task` and report it — WITHOUT
/// auto-attaching. The coordinator runs quietly in the background (its result
/// auto-delivers); the interactive session keeps its plain prompt and the
/// `:output` stream is NOT turned on. This mirrors the `run_in_background`
/// tool's fire-and-forget contract: dispatching does not commandeer the prompt
/// or start streaming the worker's activity. Use `:attach <id>` afterwards to
/// watch + steer it, or `:output on` to stream every coordinator's activity.
///
/// `escalation` distinguishes the two ways we reach here. An explicit
/// `:dispatch <task>` (`escalation == false`) is a deliberate launch and prints
/// the neutral "dispatched … :attach/:output" guidance. An ESCALATION
/// (`escalation == true`) is the agent offloading heavy work on its own — either
/// because it decided to, or because the typed line carried a work signal
/// (outside `:dispatch`). An escalation prints the dedicated escalating banner
/// (emoji + "Shift-Tab to see work") and leaves the operator free to keep
/// working in the interactive session, the newly-spawned worker (a `w_########`
/// id) waiting one Shift-Tab away.
/// Print the escalation banner, animating the leading arrow so it "lifts off"
/// the way the work does — a right→up-right→curve-up glyph sweep that settles on
/// the ⤴️ emoji, then the static banner text. The animation runs ONLY on an
/// interactive terminal (a piped/redirected stdout gets the final frame in one
/// clean write, no escape/`\r` noise — same TTY-gating contract as
/// `clear_screen`). Each frame rewrites the line in place with `\r\x1b[2K`; the
/// final frame is left on screen followed by a newline.
fn print_escalation_banner(short: &str) {
    // Body of the banner, minus the leading arrow glyph (which is what animates).
    let body = format!(
        "  escalated to background worker {short}\x1b[0m \x1b[2m— keep working here; Shift-Tab to see work.\x1b[0m"
    );
    // SAFETY: plain isatty query. Off a TTY, skip the motion entirely.
    let is_tty = unsafe { libc::isatty(1) } == 1;
    if !is_tty {
        println!("\x1b[1;33m⤴\u{fe0f}{body}");
        return;
    }
    // Liftoff frames: a flat arrow tilts upward, then curls into the final
    // curve-up emoji — reads as the escalated work taking off.
    const FRAMES: &[&str] = &["\u{2192}", "\u{2197}", "\u{2b06}\u{fe0f}", "\u{2934}\u{fe0f}"];
    let mut out = std::io::stdout();
    for frame in FRAMES {
        let _ = write!(out, "\r\x1b[2K\x1b[1;33m{frame}{body}");
        let _ = out.flush();
        std::thread::sleep(Duration::from_millis(90));
    }
    // Leave the final frame on screen and terminate the line.
    let _ = writeln!(out);
    let _ = out.flush();
}


fn dispatch_background(task: &str, session: &mut Session, escalation: bool) {
    let Dispatched { id, message } = dispatch_coordinator(task, session);
    // A guard failure (empty/nested/no-credential/no-binary) carries no id —
    // print its explanation regardless of how we got here and bail.
    let Some(id) = id else {
        println!("{message}");
        return;
    };
    let short = crate::batch::short_id(&id);
    if escalation {
        // The agent escalated this on its own (or via a work-signal line). Run it
        // as a background worker, tell the operator they can keep working, and
        // point them at Shift-Tab to watch it — newest worker is one press away.
        // The leading ⤴️ "lifts off" with a brief liftoff animation on a TTY.
        print_escalation_banner(&short);
        return;
    }
    println!("{message}");
    // Fire-and-forget: unlike the old behaviour we do NOT auto-attach or
    // turn `:output` on — surface how to opt into watching/steering instead.
    println!(
        "\x1b[1;36m:attach {short}\x1b[0m\x1b[2m to watch + steer it · \x1b[0m\x1b[36m:output on\x1b[0m\x1b[2m to stream coordinator activity\x1b[0m"
    );
}

/// Sentinel id `:attach goal` resolves to. Equal to the goal loop's live
/// stderr-stream label so attaching makes each goal turn stream (see
/// `worker::GOAL_STREAM_LABEL` / `should_forward`).
const GOAL_ATTACH_ID: &str = crate::worker::GOAL_STREAM_LABEL;

/// `:attach <id>` — attach the interactive session to a LIVE coordinator this
/// session launched, so its activity streams live (per-worker, independent of the
/// session-wide `:worker-output` toggle) and what you type is steered to it via
/// the `:tell` mailbox. Only this session's in-memory, non-terminal workers
/// qualify. An ambiguous prefix lists the matches; a finished or unknown id is
/// reported. `:detach` (or the worker finishing) ends it.
fn attach_worker(id: Option<&str>, session: &mut Session) {
    let Some(id) = id else {
        println!(
            "usage: :attach <worker-id>|goal   — watch + steer a running coordinator, or watch the goal (:detach to stop)"
        );
        return;
    };
    // `:attach goal` — watch the active background `:goal` loop. The goal
    // worker streams its stderr under the `GOAL_ATTACH_ID` label, so pointing
    // the shared `attached` handle at that sentinel forwards each goal turn's
    // activity live, just like attaching to a coordinator by id. A goal has no
    // operator mailbox, so it's watch-only (see `send_to_attached`).
    if id == GOAL_ATTACH_ID {
        match &session.goal {
            Some(g) if g.is_active() => {
                crate::terminal::open_attach_view();
                *session.attached.lock().unwrap() = Some(GOAL_ATTACH_ID.to_string());
                println!(
                    "\x1b[1;33m⇄ attached to the goal\x1b[0m — streaming its turns live. \x1b[2mgoals are watch-only; :goal clear to stop it, :detach to stop watching.\x1b[0m"
                );
            }
            Some(_) => println!("the goal has already finished — `:goal` for its final status"),
            None => println!("no goal set — `:goal <condition>` to start one (requires :batch on)"),
        }
        return;
    }
    let hit = |wid: &str| wid == id || wid.starts_with(id);
    // Collect every match in this session's workers, LIVE or terminal — a done/
    // failed coordinator is now attachable too (review + resume): a typed line
    // resumes it from its prior result. `(run_id, terminal)`.
    let mut matches: Vec<(String, bool)> = Vec::new();
    for w in session.worker_jobs.lock().unwrap().iter() {
        if hit(&w.id) {
            matches.push((
                w.id.clone(),
                matches!(w.status().as_str(), "done" | "failed"),
            ));
        }
    }
    match matches.as_slice() {
        [] => println!("no coordinator in this session matching '{id}' (see :workers)"),
        [(run_id, terminal)] => {
            // Open the worker view inline on the primary buffer (mirrors
            // Shift-Tab). Non-destructive: the interactive output scrolls up into
            // scrollback rather than being wiped, and the worker's output stays
            // scrollable via the terminal's native scrollback.
            crate::terminal::open_attach_view();
            *session.attached.lock().unwrap() = Some(run_id.clone());
            let short = crate::batch::short_id(run_id);
            if *terminal {
                // Pre-seed the review marker so `announce_attach_review` doesn't
                // re-announce the same finish on the next loop tick.
                *session.attach_review_announced.lock().unwrap() = Some(run_id.clone());
                println!(
                    "\x1b[1;33m⇄ attached to {short} (finished)\x1b[0m — review mode: replaying its work below. \x1b[2mType a message to resume it, or :detach to stop.\x1b[0m"
                );
                backfill_attached(run_id, session);
                print_attached_result(run_id, session);
            } else {
                *session.attach_review_announced.lock().unwrap() = None;
                println!(
                    "\x1b[1;33m⇄ attached to {short}\x1b[0m — streaming its activity live; what you type is steered to it. \x1b[2m:detach to stop.\x1b[0m"
                );
                // Replay the input + activity captured so far, THEN the live
                // stream continues (the worker's forwarder now sees this session
                // as attached and forwards every subsequent line).
                backfill_attached(run_id, session);
            }
        }
        many => {
            println!(
                "'{id}' matches {} coordinators — be more specific:",
                many.len()
            );
            for (rid, _) in many {
                println!("  {rid}");
            }
        }
    }
}

/// On `:attach`, replay the attached coordinator's input + activity captured
/// so far (its bounded transcript) so the operator sees the full context
/// BEFORE the live stream continues. A no-op when the worker can't be found.
fn backfill_attached(run_id: &str, session: &Session) {
    let job = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .find(|w| w.id == *run_id)
        .cloned();
    let Some(job) = job else { return };
    let short = crate::batch::short_id(run_id);
    println!("{}", crate::worker::pane_replay_header(&short));
    // The task is the coordinator's "input" — the START of the conversation. It's
    // rendered set-apart (a 💬 glyph + bold) by `pane_input_row` so it's obvious
    // this row is the prompt the coordinator was given, not one of its own
    // activity lines that follow.
    println!("{}", crate::worker::pane_input_row(run_id, &job.task));
    let mut rows = job.transcript_rows();
    if rows.is_empty() {
        // No activity captured yet. For a LIVE worker, show an ANIMATED
        // "thinking…" row (matching the live-stream / interactive spinner)
        // instead of a static placeholder — the live stream stops + replaces it
        // the instant its first forwarded line lands (worker::stop_backfill_thinking).
        // Off a TTY (no animation), or for an already-finished worker (nothing
        // left to stop the spinner), fall back to the one-shot static notice.
        let running = job.status() == "running";
        let animated = running
            && job.start_backfill_thinking(
                session.show_worker_output.clone(),
                session.attached.clone(),
            );
        if !animated {
            println!(
                "{}",
                crate::worker::pane_row(
                    run_id,
                    "\u{b7}thinking (no activity captured yet \u{2014} live output follows)",
                )
            );
        }
    } else {
        // Tail to the last 40 lines of transcript. When attaching/cycling,
        // show only the latest ~1 screen of output to avoid scrolling chat
        // history offscreen. The live stream continues below this tail.
        const TAIL_LINES: usize = 40;
        let total_rows = rows.len();
        if total_rows > TAIL_LINES {
            rows = rows.into_iter().skip(total_rows - TAIL_LINES).collect();
        }
        // The transcript suffix is empty under the single-glyph convention\u{2014}
        // the source glyph is already stamped in `text`. Fold any legacy suffix
        // into the row text so the gutter stays a bare `[label]`.
        for (suffix, text) in rows {
            let row = if suffix.is_empty() {
                text
            } else {
                format!("{suffix} {text}")
            };
            println!("{}", crate::worker::pane_row(run_id, &row));
        }
    }
}

/// Print a finished coordinator's final result inside the attach pane, so an
/// operator who `:attach`es a done/failed worker sees the work they're about to
/// continue from. A no-op when the worker isn't in this session.
/// Atomically CLAIM the "attach review already announced" marker for `run_id`.
/// Returns `true` exactly once per (marker, run_id) transition — the caller that
/// gets `true` is the one responsible for printing the finished coordinator's
/// result + review notice. Shared by the blocking main-loop path
/// (`announce_attach_review`) and the non-blocking presenter tick task so the
/// final result is printed EXACTLY ONCE no matter which path observes the finish
/// first (the presenter surfaces it live while the main loop is blocked in
/// `read_line`; whichever wins the claim prints, the other no-ops).
fn claim_attach_announce(marker: &Arc<Mutex<Option<String>>>, run_id: &str) -> bool {
    let mut g = marker.lock().unwrap();
    if g.as_deref() == Some(run_id) {
        return false;
    }
    *g = Some(run_id.to_string());
    true
}

/// Build the attach-pane rows for a finished coordinator's FINAL answer (stdout)
/// — the same content `print_attached_result` prints, returned as lines so both
/// the blocking main-loop path and the presenter task can emit it (the latter
/// via the ExternalPrinter, without waiting for the operator to press Enter).
fn attached_result_lines(run_id: &str, jobs: &crate::worker::WorkerJobs) -> Vec<String> {
    let job = jobs
        .lock()
        .unwrap()
        .iter()
        .find(|w| w.id == *run_id)
        .cloned();
    let Some(job) = job else {
        return Vec::new();
    };
    let rendered =
        crate::md::render_stdout_within(job.fetch().trim(), crate::worker::pane_content_cols(run_id));
    let mut out = Vec::new();
    let mut lines = rendered.split('\n');
    match lines.next() {
        Some(first) => {
            out.push(crate::worker::pane_row(
                run_id,
                &format!("·result {first}"),
            ));
            for line in lines {
                out.push(crate::worker::pane_row(run_id, line));
            }
        }
        None => out.push(crate::worker::pane_row(run_id, "·result (empty result)")),
    }
    out
}

fn print_attached_result(run_id: &str, session: &Session) {
    let job = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .find(|w| w.id == *run_id)
        .cloned();
    if let Some(job) = job {
        // The coordinator's FINAL answer is multi-line markdown. Render it for
        // the terminal (the parent REPL is interactive → ANSI) and emit it as
        // ONE pane row PER LINE so every line carries the `┃ [label]` border and
        // stays INSIDE the contained `:output` pane. Previously the whole blob
        // was handed to a single `pane_row`, so only the first physical line got
        // the border + `·result` gutter and every continuation line fell outside
        // the pane — the operator saw the result "cut off" after the first line
        // (e.g. only `·result Diagnosis complete. …` with the rest of the
        // markdown missing). Splitting per line is the same shape
        // `backfill_attached` uses for the streamed activity rows.
        let rendered = crate::md::render_stdout_within(
            job.fetch().trim(),
            crate::worker::pane_content_cols(run_id),
        );
        let mut lines = rendered.split('\n');
        match lines.next() {
            Some(first) => {
                println!(
                    "{}",
                    crate::worker::pane_row(run_id, &format!("·result {first}"))
                );
                for line in lines {
                    println!("{}", crate::worker::pane_row(run_id, line));
                }
            }
            None => println!(
                "{}",
                crate::worker::pane_row(run_id, "·result (empty result)")
            ),
        }
    }
}

/// `:detach` — stop watching the attached coordinator. It keeps running in the
/// background; its result still auto-delivers and shows in `:workers`.
fn detach_worker(session: &mut Session) {
    // Stop any live "thinking…" spinner now so it can't erase the next prompt
    // (and restore the cursor it hid) — same race guarded in `cycle_worker`.
    crate::worker::quiesce_thinking_spinners();
    match session.attached.lock().unwrap().take() {
        Some(run_id) => {
            // Close the worker view. It rendered inline on the primary buffer, so
            // the interactive output is already in scrollback — just re-anchor and
            // print the detach line trailing the worker output.
            crate::terminal::close_attach_view();
            let short = crate::batch::short_id(&run_id);
            println!(
                "\x1b[2m⇄ detached from {short} — it keeps running; :workers to check, :result {short} when done\x1b[0m"
            );
        }
        None => println!("not attached to any coordinator"),
    }
}

/// What a `:forget` invocation resolved to. Pure shaping (parsed from the
/// command tail) so the handler's IO stays thin and the parse is unit-testable.
#[derive(Debug, PartialEq)]
enum ForgetCmd {
    /// No argument (or an unrecognized one) -> print usage.
    Usage,
    /// `--all-exited` / `--all` -> bulk-forget every exited, owned worker (AC6).
    AllExited,
    /// `<id>` (full id or prefix) -> forget that one worker (AC5).
    One(String),
}

/// Parse the whitespace-split tail of `:forget` into a [`ForgetCmd`]. Pure ->
/// unit-tested. Exactly one token is accepted (an id, or the bulk flag);
/// anything else is usage.
fn parse_forget_cmd(parts: &[&str]) -> ForgetCmd {
    match parts {
        [] => ForgetCmd::Usage,
        [flag] if *flag == "--all-exited" || *flag == "--all" => ForgetCmd::AllExited,
        [id] if !id.is_empty() && !id.starts_with("--") => ForgetCmd::One((*id).to_string()),
        _ => ForgetCmd::Usage,
    }
}

/// Whether a worker/coordinator `status` (an in-memory `WorkerJob::status` or a
/// durable `coordinator_runs.phase`) means it is STILL LIVE. `:forget` refuses a
/// live worker (AC5a) so it can never reap work in flight. Unknown/terminal
/// states (`done`/`failed`, or a stale label whose container is gone) are
/// forgettable. Pure -> unit-tested.
fn forget_status_is_live(status: &str) -> bool {
    matches!(status.trim(), "running" | "coordinating" | "awaiting_batch")
}

/// Remove every durable trace of one (already-confirmed-exited) worker: its
/// container (if a runtime is engaged, AC5b), its on-disk state dir
/// (`~/.aish/workers/<id>/`, S9.3, AC5c), its durable `coordinator_runs` row, and
/// its in-memory job entry (AC5d). Best-effort per component; returns a one-line
/// human summary of what was reclaimed.
fn do_forget(full_id: &str, session: &mut Session) -> String {
    let short = crate::batch::short_id(full_id);
    let dir_path = crate::worker_store::worker_dir(full_id);
    let had_dir = dir_path.exists();
    let container_removed = crate::container::forget_container(full_id);
    let dir_removed = match crate::worker_store::forget(full_id) {
        Ok(()) => had_dir && !dir_path.exists(),
        Err(_) => false,
    };
    let row_removed = session
        .coordinator_store
        .as_ref()
        .and_then(|s| s.delete_runs(&[full_id.to_string()]).ok())
        .unwrap_or(0)
        > 0;
    // Drop from the in-memory list so it leaves :workers and the wheel-N badge.
    let mem_removed = {
        let mut jobs = session.worker_jobs.lock().unwrap();
        let before = jobs.len();
        jobs.retain(|w| w.id != full_id);
        jobs.len() != before
    };
    let mut parts: Vec<&str> = Vec::new();
    if container_removed {
        parts.push("container");
    }
    if dir_removed {
        parts.push("state dir");
    }
    if row_removed {
        parts.push("run record");
    }
    if mem_removed {
        parts.push("job entry");
    }
    if parts.is_empty() {
        format!("\x1b[2m\u{2713} forgot {short} (nothing left to remove)\x1b[0m")
    } else {
        format!(
            "\x1b[32m\u{2713}\x1b[0m forgot {short} \u{2014} removed {}",
            parts.join(", ")
        )
    }
}

/// `:forget <id>` (S9.5 AC5) — remove an EXITED worker's footprint. Resolves
/// `id` as a full id or prefix across this session's in-memory workers (live
/// status) and the durable coordinator store, refuses a still-running worker with
/// guidance (AC5a), reports an unknown or ambiguous id, and otherwise reclaims
/// the container + state dir + run record + job entry via [`do_forget`].
fn forget_worker(id: &str, session: &mut Session) {
    let id = id.trim();
    if id.is_empty() {
        println!("usage: :forget <id>   (or :forget --all-exited)");
        return;
    }
    let hit = |jid: &str| jid == id || jid.starts_with(id);
    // (full_id, status) candidates: in-memory first (live status), then the
    // durable store (skipping ids already collected in-memory).
    let mut found: Vec<(String, String)> = Vec::new();
    for w in session.worker_jobs.lock().unwrap().iter() {
        if hit(&w.id) {
            found.push((w.id.clone(), w.status()));
        }
    }
    if let Some(store) = &session.coordinator_store {
        if let Ok(rows) = store.load_all() {
            for r in rows.iter().filter(|r| hit(&r.run_id)) {
                if !found.iter().any(|(mid, _)| mid == &r.run_id) {
                    found.push((r.run_id.clone(), r.phase.clone()));
                }
            }
        }
    }
    match found.as_slice() {
        [] => println!("no background worker matching '{id}' (see :workers)"),
        [(full_id, status)] => {
            if forget_status_is_live(status) {
                let short = crate::batch::short_id(full_id);
                println!(
                    "\x1b[33m\u{2717}\x1b[0m {short} is still running \u{2014} :attach {short} and stop it (or wait) before :forget"
                );
                return;
            }
            println!("{}", do_forget(full_id, session));
        }
        many => {
            println!(
                "'{id}' is ambiguous \u{2014} matches {} workers:",
                many.len()
            );
            for (mid, status) in many {
                println!("  {} ({status})", crate::batch::short_id(mid));
            }
            println!("disambiguate with a longer id prefix");
        }
    }
}

/// `:forget --all-exited` (S9.5 AC6) — bulk-remove every exited worker OWNED by
/// this session: terminal in-memory workers (always owned) plus terminal durable
/// rows whose `session_id` matches. Running workers are left untouched.
fn forget_all_exited(session: &mut Session) {
    let mut victims: Vec<String> = Vec::new();
    for w in session.worker_jobs.lock().unwrap().iter() {
        if !forget_status_is_live(&w.status()) {
            victims.push(w.id.clone());
        }
    }
    if let Some(store) = &session.coordinator_store {
        if let Ok(rows) = store.load_all() {
            for r in rows.iter() {
                let mine = r.session_id.as_deref() == Some(session.session_id.as_str());
                if mine && !forget_status_is_live(&r.phase) && !victims.contains(&r.run_id) {
                    victims.push(r.run_id.clone());
                }
            }
        }
    }
    if victims.is_empty() {
        println!("no exited workers to forget");
        return;
    }
    let n = victims.len();
    for id in &victims {
        println!("{}", do_forget(id, session));
    }
    println!("\x1b[2mforgot {n} exited worker(s)\x1b[0m");
}

/// Ctrl-C at the prompt while `:attach`ed to a LIVE coordinator: forward SIGINT
/// to the worker's process group so its in-flight model turn aborts. The child
/// called `setsid()`, so its pgid == pid and `kill(-pid, SIGINT)` reaches the
/// whole worker group (the coordinator plus any tool children); the coordinator's
/// own `tokio::signal::ctrl_c` select arm then rolls back the half-finished turn,
/// exactly as an interactive Ctrl-C does locally. Returns true when a signal was
/// delivered — the caller then skips the default "clear the line" behaviour.
/// A no-op (false) when not attached, attached to the goal (watch-only, no
/// process), attached to a TERMINAL worker (nothing running to interrupt), or the
/// child pid isn't known yet (spawn hasn't recorded it).
fn interrupt_attached_worker(session: &Session) -> bool {
    let Some(run_id) = session.attached.lock().unwrap().clone() else {
        return false;
    };
    if run_id == GOAL_ATTACH_ID {
        return false;
    }
    let target = {
        let jobs = session.worker_jobs.lock().unwrap();
        jobs.iter()
            .find(|w| w.id == run_id)
            // Only a live (non-terminal) worker with a known child pid can be
            // interrupted; capture the pid while holding the lock, then drop it
            // before signalling.
            .filter(|w| !matches!(w.status().as_str(), "done" | "failed"))
            .and_then(|w| w.pid())
    };
    let Some(pid) = target else {
        return false;
    };
    // Negated pid targets the whole process group (pgid == pid via setsid()).
    unsafe {
        libc::kill(-(pid as i32), libc::SIGINT);
    }
    let short = crate::batch::short_id(&run_id);
    println!("\x1b[33m^C\x1b[0m interrupting {short}'s turn");
    true
}

/// Handle a typed operator line while attached. For a LIVE coordinator this
/// queues the line into its durable mailbox (the `:tell` channel) to be folded
/// into its next round. For a TERMINAL (done/failed) coordinator — nothing would
/// read a mailbox message — it RESUMES the run instead (relaunch seeded with the
/// prior context + this message; see `resume_coordinator`). Called for every
/// plain line typed while attached.
fn send_to_attached(run_id: &str, message: &str, session: &mut Session) {
    let message = message.trim();
    if message.is_empty() {
        return;
    }
    // A goal is verifier-driven and has no operator mailbox — it can't be
    // steered mid-flight. Say so rather than silently dropping the line into a
    // coordinator-tell with no recipient.
    if run_id == GOAL_ATTACH_ID {
        println!(
            "\x1b[2mthe goal is watch-only — it can't be steered. `:goal clear` to stop it, `:detach` to stop watching.\x1b[0m"
        );
        return;
    }
    // A TERMINAL (done/failed) coordinator has nothing to read a mailbox
    // message, so a typed line RESUMES it instead (relaunch seeded with the
    // prior context + this message; see `resume_coordinator`). A live one is
    // steered via the `:tell` mailbox.
    let terminal = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .find(|w| w.id == *run_id)
        .map(|w| matches!(w.status().as_str(), "done" | "failed"))
        .unwrap_or(false);
    if terminal {
        resume_coordinator(run_id, message, session);
    } else {
        tell_coordinator(Some(run_id), message, false, session);
    }
}

/// When the attached coordinator reaches a terminal state, announce it ONCE and
/// KEEP the attachment (review mode): the operator stays bound to the run so a
/// typed line RESUMES it (see `send_to_attached` -> `resume_coordinator`) rather
/// than being dropped. The one-shot notice is de-duped via
/// `attach_review_announced`, which `:attach` pre-seeds when binding an
/// already-finished worker. A live (or resumed) attachment clears the marker so
/// a later finish re-announces. A no-op when not attached, or when the worker is
/// no longer tracked in this session (left as-is rather than force-detached).
fn announce_attach_review(session: &mut Session) {
    let attached = session.attached.lock().unwrap().clone();
    let Some(run_id) = attached else {
        *session.attach_review_announced.lock().unwrap() = None;
        return;
    };
    let status = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .find(|w| w.id == run_id)
        .map(|w| w.status());
    match status.as_deref() {
        Some("done") | Some("failed") => {
            if claim_attach_announce(&session.attach_review_announced, &run_id) {
                // The coordinator's FINAL answer is written to stdout — NOT the
                // streamed stderr we forward live — so a live `:attach` that runs
                // through to completion would otherwise never show the last
                // (result) turn: it gets "cut off" at the last tool/narration
                // line. Replay the captured result into the attach pane before
                // the review notice, exactly as attaching to an already-finished
                // worker does (`print_attached_result`). The atomic claim above
                // means an `:attach` of an already-terminal worker (which already
                // printed the result) — or the presenter task racing us — does
                // not print it a second time here.
                print_attached_result(&run_id, session);
                let short = crate::batch::short_id(&run_id);
                println!(
                    "\x1b[2m⇄ {short} finished — review mode: type a message to resume it, or :detach to return to your shell.\x1b[0m"
                );
            }
        }
        // Still live (or resumed into a new live state) — clear so a later finish
        // re-announces.
        Some(_) => *session.attach_review_announced.lock().unwrap() = None,
        // Worker no longer tracked here — leave the attachment untouched.
        None => {}
    }
}

/// Resume a FINISHED coordinator interactively. Relaunch a fresh background
/// coordinator seeded with the prior run's task + final result as context plus
/// the operator's `message`, then attach to the new run so the conversation
/// continues live and further typed lines steer/resume it in turn. This is what
/// makes `:attach`-ing a done/failed worker a real "keep working with that
/// agent" session instead of a dead end. Shared-cwd (like `:dispatch`) so the
/// resumed agent iterates in the operator's working tree with full context.
fn resume_coordinator(prev_run_id: &str, message: &str, session: &mut Session) {
    if session.nested {
        println!("can't resume a coordinator from inside a coordinator");
        return;
    }
    // Pull the prior run's task + rendered result for context before relaunching.
    let prior = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .find(|w| w.id == *prev_run_id)
        .map(|w| (w.task.clone(), w.fetch()));
    let Some((task, result)) = prior else {
        println!(
            "can't resume '{prev_run_id}' — its record is no longer in this session (see :workers)"
        );
        return;
    };
    let no_credential = match session.backend_kind.as_str() {
        "grok" => !crate::backend::grok::credential_available(&session.env),
        _ => crate::backend::claude::Credential::resolve(&session.env).is_err(),
    };
    if no_credential {
        println!(
            "no credential for the active backend — can't resume (Claude: CLAUDE_CODE_OAUTH_TOKEN/ANTHROPIC_API_KEY · Grok: ~/.grok/auth.json or XAI_API_KEY)"
        );
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            println!("can't locate the aish binary to resume the coordinator: {e}");
            return;
        }
    };
    let prev_short = crate::batch::short_id(prev_run_id);
    let resume_task = build_resume_task(&prev_short, &task, &result, message);
    let spec = crate::worker::WorkerSpec {
        exe,
        cwd: session.cwd.clone(),
        backend: session.backend_kind.clone(),
        model: crate::worker::coordinator_model(&session.backend_kind, &session.batch_model),
        env: session.env.clone(),
        // Shared-cwd (no worktree), matching `:dispatch` — interactive iteration
        // happens in the operator's live tree.
        isolate: false,
        base: "main".to_string(),
        launch_session_id: session.session_id.clone(),
        launch_session_name: session.name.clone(),
        show_output: session.show_worker_output.clone(),
        attached: session.attached.clone(),
        coordinator_store: session.coordinator_store.clone(),
    };
    // Resume IN PLACE: reuse the SAME `WorkerJob` — same visible id, same slot in
    // `:workers`, same attachment — instead of spawning a fresh worker. The job
    // flips back to `running` and a NEW coordinator run id (a new thread) is minted
    // under the covers, so the operator keeps working with the SAME agent rather
    // than accruing a brand-new `w_…` on every follow-up. This is the fix for "we
    // keep getting a new worker when a worker finishes — I thought we continue the
    // session and track it as a new thread under the covers".
    let job = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .find(|w| w.id == *prev_run_id)
        .cloned();
    let Some(job) = job else {
        println!(
            "can't resume '{prev_run_id}' — its record is no longer in this session (see :workers)"
        );
        return;
    };
    // Keep a handle to the (same) job so we can flash an immediate thinking row
    // the instant the operator resumes — before the fresh child has spawned and
    // emitted its first `💭 thinking…` line.
    let job_for_spinner = job.clone();
    let (_id, thread) =
        crate::worker::resume_in_place(&session.worker_jobs, job, resume_task, spec);
    // The attachment already points at THIS worker (we were in its review mode),
    // so there's nothing to re-bind — just clear the review marker so the next
    // finish re-announces. Activity for the new thread streams into this same pane.
    *session.attach_review_announced.lock().unwrap() = None;
    // Instant feedback: as soon as the operator types a follow-up to a finished
    // worker, show the animated "thinking…" row in the attach pane rather than a
    // blank gap until the relaunched coordinator produces its first forwardable
    // line. We're still attached to this worker (review mode), so the forward
    // gate is open and the spinner animates immediately; the live stream stops +
    // replaces it the instant its first line lands (`stop_backfill_thinking`),
    // exactly like the attach-time backfill spinner.
    job_for_spinner
        .start_backfill_thinking(session.show_worker_output.clone(), session.attached.clone());
    // Silently continue per the operator's input — no resume banner. The new
    // thread's activity streams into this same pane; there's no need to notify
    // the operator that the worker is resuming.
    let _ = (prev_short, thread);
}

/// Build the seed task for a resumed coordinator: the prior run's task + final
/// result as context, then the operator's follow-up. Pure (string-only) so the
/// framing is unit-tested without spawning a subprocess.
fn build_resume_task(prev_short: &str, task: &str, result: &str, message: &str) -> String {
    format!(
        "You are resuming an earlier aish coordinator run ({prev_short}) that has already finished. \
Continue working WITH the operator from where it left off — treat the prior result below as your \
OWN prior work, not a fresh task, and build on it.\n\n\
=== ORIGINAL TASK ===\n{task}\n\n\
=== YOUR PRIOR RESULT ===\n{result}\n\n\
=== OPERATOR'S FOLLOW-UP ===\n{message}"
    )
}

/// Pure index math for the Shift-Tab worker cycle (extracted from
/// [`cycle_worker`] so it's testable without spawning coordinators). `running`
/// is the ordered list of live coordinator ids; `current` is the
/// currently-attached id (`None` = the interactive prompt). The cycle is
/// `interactive(0) → running[0](1) → … → running[n-1](n) → interactive(0)`;
/// the return is the NEXT slot, where 0 means "interactive" and any `k` in
/// `1..=running.len()` means `running[k - 1]`. An unknown `current` (e.g. it was
/// `:close`d and removed from `running`) restarts the cycle from interactive.
/// Returns 0 when there is nothing to cycle through.
fn next_attach_index(running: &[String], current: Option<&str>) -> usize {
    if running.is_empty() {
        return 0;
    }
    let cur_idx = match current {
        None => 0,
        Some(id) => running.iter().position(|w| w == id).map_or(0, |i| i + 1),
    };
    (cur_idx + 1) % (running.len() + 1)
}

/// `Shift-Tab` — cycle the interactive session's attach cursor across the
/// coordinators it launched, NEWEST first. The cycle is `interactive → newest
/// → … → oldest → interactive` (reverse spawn order): the first press from the
/// interactive prompt lands on the most recently spawned coordinator — the one
/// a just-escalated worker — and each further press walks back toward the
/// oldest. Each Shift-Tab advances to the next coordinator (streaming its
/// activity + steering input to it,
/// exactly like `:attach`), and one more press past the last detaches back to
/// the interactive prompt. With a single worker it toggles attach/detach of that
/// one; with none it's a silent no-op (no screen redraw, no hint).
///
/// Returns `true` when the cursor actually moved (screen was redrawn and a new
/// view was printed), `false` when it was a no-op because this session has no
/// coordinators to cycle through — the caller uses this to decide whether the
/// prompt needs a gap/redraw afterward.
///
/// FINISHED (done/failed) coordinators are part of the rotation too — landing on
/// one drops into review mode (replay its work + result; a typed line resumes
/// it), mirroring `attach_worker`'s terminal branch. This makes Shift-Tab a way
/// to flip back through completed/failed agents, not just the still-running ones.
fn cycle_worker(session: &mut Session) -> bool {
    // No coordinators launched by this session → nothing to attach to. Take
    // NO action: don't clear/redraw the screen, don't quiesce spinners, don't
    // print a hint. A Shift-Tab with no workers is a silent no-op that leaves
    // the operator's prompt exactly as it was. This check MUST come before the
    // screen clear below (otherwise an empty cycle still wipes the screen).
    if session.worker_jobs.lock().unwrap().is_empty() {
        return false;
    }
    // Synchronously tear down any live worker "thinking…" spinner BEFORE we
    // print this view and the REPL redraws the prompt. The spinner polls its
    // forward gate only every ~80 ms, so a spinner for the worker we're leaving
    // would otherwise self-erase (`\r\x1b[2K`) a beat later — right on top of the
    // freshly-drawn interactive prompt (the "prompt doesn't always show after
    // Shift-Tab" bug). Aborting the tasks here also un-hides the cursor.
    crate::worker::quiesce_thinking_spinners();
    // All coordinators this session launched, in listing order — LIVE and
    // TERMINAL — paired with a `terminal` flag so the attach branch can choose
    // review-mode vs live-stream. Shift-Tab rotates through finished/failed
    // agents too, so a completed run is reachable by cycling, not only `:attach`.
    // Newest worker FIRST: `worker_jobs` is in spawn order (oldest pushed first),
    // so `.rev()` puts the most recently spawned coordinator at slot 1 — the
    // first Shift-Tab from the interactive prompt lands on the newest worker and
    // each further press walks back toward the oldest, then wraps to interactive.
    let workers: Vec<(String, bool, String)> = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .rev()
        .map(|w| {
            (
                w.id.clone(),
                matches!(w.status().as_str(), "done" | "failed"),
                w.task.clone(),
            )
        })
        .collect();
    // `worker_jobs` was non-empty at the top guard, so `workers` is non-empty
    // here too — there is always at least one slot to cycle to.

    // The cycle is [interactive, workers[0], workers[1], …]; index 0 is the
    // detached interactive prompt. The index math is factored into the pure,
    // unit-tested `next_attach_index` so it stays verifiable without spawning
    // real coordinators.
    let ids: Vec<String> = workers.iter().map(|(id, _, _)| id.clone()).collect();
    let current = session.attached.lock().unwrap().clone();
    let next_idx = next_attach_index(&ids, current.as_deref());

    if next_idx == 0 {
        // Wrapped past the last worker — back to the interactive prompt. Close
        // the worker view; it rendered inline on the primary buffer, so the
        // interactive output is already in scrollback and the detached line
        // trails it.
        crate::terminal::close_attach_view();
        *session.attached.lock().unwrap() = None;
        *session.attach_review_announced.lock().unwrap() = None;
        // No output-field hint here: the detached "back to interactive" state is
        // already reflected on the 2nd statusline (see `coordinator_status_line`),
        // so printing it again would just be redundant noise in the scrollback.
        //
        // Return false so the caller does NOT arm `needs_gap`: this branch prints
        // no output, and `close_attach_view` already re-anchored the cursor to
        // the bottom body row, so the prompt should redraw right there. Arming the
        // gap would inject blank lines that scroll the output up two rows on every
        // return trip.
        return false;
    }

    // Open the worker view inline on the primary buffer WITHOUT destroying
    // interactive scrollback: the interactive output scrolls up into scrollback
    // (not wiped), and the worker's output stays scrollable via the terminal's
    // native scrollback. On the first hop off the prompt, erase the ephemeral
    // interactive prompt row rustyline left behind so the attach header opens on
    // a clean row instead of trailing a stray blank prompt.
    if !crate::terminal::attach_view_active() {
        crate::terminal::erase_current_line();
    }
    crate::terminal::open_attach_view();
    let (run_id, terminal, task) = workers[next_idx - 1].clone();
    *session.attached.lock().unwrap() = Some(run_id.clone());
    let short = crate::batch::short_id(&run_id);
    let task_suffix = attach_task_suffix(&task);
    if terminal {
        // Finished agent: review mode (mirrors `attach_worker`'s terminal
        // branch). Pre-seed the review marker so `announce_attach_review`
        // doesn't re-announce the finish, replay its work, then show its result.
        // A typed line resumes it (see `send_to_attached` → `resume_coordinator`).
        *session.attach_review_announced.lock().unwrap() = Some(run_id.clone());
        println!(
            "\x1b[1;33m⇄ attached to {short} (finished)\x1b[0m \x1b[2m({}/{} · review mode: type a message to resume it, Shift-Tab to cycle, :detach to stop){}\x1b[0m",
            next_idx,
            ids.len(),
            task_suffix
        );
        backfill_attached(&run_id, session);
        print_attached_result(&run_id, session);
    } else {
        // Live worker: clear any stale review marker so a later finish is
        // announced exactly once (mirrors `attach_worker`'s live branch).
        *session.attach_review_announced.lock().unwrap() = None;
        println!(
            "\x1b[1;33m⇄ attached to {short}\x1b[0m \x1b[2m({}/{} · Shift-Tab to cycle, :detach to stop){}\x1b[0m",
            next_idx,
            ids.len(),
            task_suffix
        );
        // Replay the input + activity captured so far, THEN the live stream
        // continues — the same backfill `:attach` performs. The worker's
        // forwarder now sees this session as attached and forwards every
        // subsequent line, so the operator gets the full session transcript
        // first and live output after, instead of only future output.
        backfill_attached(&run_id, session);
    }
    true
}

/// Mid-turn sibling of [`cycle_worker`]: cycle the attach cursor while a model
/// turn is actively streaming (driven by [`crate::keywatch`] on a Shift-Tab
/// press). It CANNOT take `&mut Session` — the in-flight turn borrows it — so it
/// works off cloned attach-cursor handles and does the minimal, safe subset:
/// advance the cursor and flip the `attached` / review markers so the worker
/// forwarder begins routing that coordinator's output. Deliberately omits
/// `clear_screen` (which would wipe the turn's in-flight output) and the
/// backfill / result replay (those need `&Session` for the db); a post-turn
/// Shift-Tab runs the full [`cycle_worker`] with backfill. Index math is the same
/// pure, unit-tested [`next_attach_index`].
fn cycle_worker_live(
    workers: &crate::worker::WorkerJobs,
    attached: &Arc<Mutex<Option<String>>>,
    review: &Arc<Mutex<Option<String>>>,
) {
    let workers_v: Vec<(String, bool, String)> = workers
        .lock()
        .unwrap()
        .iter()
        .rev()
        .map(|w| {
            (
                w.id.clone(),
                matches!(w.status().as_str(), "done" | "failed"),
                w.task.clone(),
            )
        })
        .collect();
    if workers_v.is_empty() {
        // No coordinators to attach to — take no action (no hint, no redraw),
        // matching the idle-prompt `cycle_worker` no-op.
        return;
    }
    let ids: Vec<String> = workers_v.iter().map(|(id, _, _)| id.clone()).collect();
    let current = attached.lock().unwrap().clone();
    let next_idx = next_attach_index(&ids, current.as_deref());
    if next_idx == 0 {
        *attached.lock().unwrap() = None;
        *review.lock().unwrap() = None;
        // No output-field hint here: the detached "back to interactive" state is
        // already reflected on the 2nd statusline (see `coordinator_status_line`),
        // so printing it again would just be redundant noise in the scrollback.
        return;
    }
    let (run_id, terminal, task) = workers_v[next_idx - 1].clone();
    *attached.lock().unwrap() = Some(run_id.clone());
    let short = crate::batch::short_id(&run_id);
    let task_suffix = attach_task_suffix(&task);
    if terminal {
        *review.lock().unwrap() = Some(run_id.clone());
        println!(
            "\x1b[1;33m⇄ attached to {short} (finished)\x1b[0m \x1b[2m({}/{} · review mode: type a message to resume it, Shift-Tab to cycle, :detach to stop){}\x1b[0m",
            next_idx,
            ids.len(),
            task_suffix
        );
    } else {
        *review.lock().unwrap() = None;
        println!(
            "\x1b[1;33m⇄ attached to {short}\x1b[0m \x1b[2m({}/{} · Shift-Tab to cycle, :detach to stop){}\x1b[0m",
            next_idx,
            ids.len(),
            task_suffix
        );
    }
}

/// `:close [worker-id]` — remove a coordinator from THIS session: drop it from
/// the worker list (so it no longer shows in `:workers`) AND from the Shift-Tab
/// rotation, and detach if it was the attached one. With no id it closes the
/// coordinator the session is currently attached to — "the one you're in". An
/// exact id or a unique prefix selects the target; an ambiguous prefix lists the
/// matches.
///
/// Removing the in-memory record is what drops the worker from both `:workers`
/// and the Shift-Tab cycle (both read `session.worker_jobs`). Closing a FINISHED
/// agent simply discards its record. A still-RUNNING coordinator keeps running
/// in the background — aish only stops tracking it here — so we say so plainly.
fn close_worker(id: Option<&str>, session: &mut Session) {
    let attached = session.attached.lock().unwrap().clone();
    // Default target: the worker we're attached to ("the one you're in"). The
    // goal sentinel isn't a worker job, so it can't be closed this way.
    let target = id
        .map(str::to_string)
        .or_else(|| attached.clone().filter(|r| r != GOAL_ATTACH_ID));
    let Some(target) = target else {
        if attached.as_deref() == Some(GOAL_ATTACH_ID) {
            println!(
                "\x1b[2mthe goal isn't a worker — `:goal clear` to stop it, `:detach` to stop watching.\x1b[0m"
            );
        } else {
            println!(
                "usage: :close [worker-id]   — not attached to a coordinator; pass an id (see :workers)"
            );
        }
        return;
    };

    // Resolve to a single worker by exact id or unique prefix (like `:attach`).
    let hit = |wid: &str| wid == target || wid.starts_with(&target);
    let matches: Vec<(String, String)> = session
        .worker_jobs
        .lock()
        .unwrap()
        .iter()
        .filter(|w| hit(&w.id))
        .map(|w| (w.id.clone(), w.status()))
        .collect();
    let (run_id, status) = match matches.as_slice() {
        [] => {
            println!("no coordinator in this session matching '{target}' (see :workers)");
            return;
        }
        [(rid, st)] => (rid.clone(), st.clone()),
        many => {
            println!(
                "'{target}' matches {} coordinators — be more specific:",
                many.len()
            );
            for (rid, _) in many {
                println!("  {rid}");
            }
            return;
        }
    };

    // Drop the record — removes it from `:workers` AND the Shift-Tab rotation.
    session
        .worker_jobs
        .lock()
        .unwrap()
        .retain(|w| w.id != run_id);

    // Detach if we just closed the worker we were attached to.
    if attached.as_deref() == Some(run_id.as_str()) {
        *session.attached.lock().unwrap() = None;
        *session.attach_review_announced.lock().unwrap() = None;
    }

    let short = crate::batch::short_id(&run_id);
    if matches!(status.as_str(), "done" | "failed") {
        println!(
            "\x1b[2m⇄ closed {short} — removed from :workers and the Shift-Tab rotation.\x1b[0m"
        );
    } else {
        println!(
            "\x1b[2m⇄ closed {short} — removed from :workers and the Shift-Tab rotation; it keeps running in the background.\x1b[0m"
        );
    }
}
/// AC7 (TASK-201 / S9.2) ownership decision for steering a coordinator that may
/// belong to ANOTHER session. A coordinator is "owned" by the session that
/// launched it — its durable `coordinator_runs.session_id`, or, for an in-memory
/// worker, the live session by construction. By default only the owning session
/// may steer it; `--any` is the explicit, audited cross-session override
/// (multi-user safety). A run with no recorded owner (legacy pre-AC7 rows) is
/// treated as own-session so the gate never strands an old run. Pure → unit-tested.
#[derive(Debug, PartialEq, Eq)]
enum OwnerGate {
    Allow,
    Forbidden,
}

/// Decide whether `my_session` may steer a coordinator owned by
/// `target_session`, honoring the `--any` override. See [`OwnerGate`].
fn owner_gate(target_session: Option<&str>, my_session: &str, any: bool) -> OwnerGate {
    if any {
        return OwnerGate::Allow;
    }
    match target_session {
        Some(sid) if sid != my_session => OwnerGate::Forbidden,
        _ => OwnerGate::Allow,
    }
}

/// Pull a leading `--any` / `-a` ownership-override flag out of a `:tell`
/// argument token list, returning `(any, remaining_tokens)`. The flag is only
/// recognized in the FIRST position (`:tell --any <id> <msg>`); anywhere else it
/// is left untouched as message text, so a literal `--any` can still be sent as
/// part of a message after the id. Pure → unit-tested.
fn take_any_flag<'a>(toks: &[&'a str]) -> (bool, Vec<&'a str>) {
    if let Some((first, rest)) = toks.split_first() {
        if *first == "--any" || *first == "-a" {
            return (true, rest.to_vec());
        }
    }
    (false, toks.to_vec())
}

/// Queue an operator message for an in-flight background coordinator — the
// ───────────────────────── `:goal` subcommands (TASK-278) ─────────────────────────
//
// CRUD over the durable `crate::goal::Goal` records that TASK-277 persists in
// `aish.db`. `goal_command` returns the text to print (so the dispatcher stays a
// one-liner and the whole surface is unit-testable), mutating + persisting
// `session.goals` as a side effect. Every path returns a helpful one-line
// message on bad input — none panic (TASK-278 AC4). Unrecognized heads never
// reach here: the dispatcher routes them to the back-compat batch pursuit loop.

/// Short, stable handle for a goal in listings — the first 8 chars of its uuid.
fn goal_short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Coarse "N<unit> ago" for a unix-seconds stamp. Never panics on skew.
fn goal_ago(secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(secs);
    let d = (now - secs).max(0);
    if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// Heuristic date sniff for the optional trailing `[date]` of `:goal milestone`.
/// Accepts ISO-ish `2025-01-31` or `1/31` / `01/31/2025` — a token made only of
/// digits and `-`/`/` separators with at least one separator and one digit. Kept
/// deliberately loose (no chrono dependency); a non-date trailing word simply
/// stays part of the milestone name.
fn looks_like_date(tok: &str) -> bool {
    let ok_chars = tok.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '/');
    let has_sep = tok.contains('-') || tok.contains('/');
    let has_digit = tok.chars().any(|c| c.is_ascii_digit());
    ok_chars && has_sep && has_digit && tok.len() >= 3
}

/// Render a goal's full detail block (used by `:goal show`).
fn goal_detail(g: &crate::goal::Goal) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let (done, total) = g.milestone_progress();
    let _ = writeln!(
        s,
        "\x1b[1mgoal [{}]\x1b[0m {} \x1b[2m{}\x1b[0m",
        g.status,
        g.title,
        goal_short(&g.id)
    );
    if !g.description.is_empty() {
        let _ = writeln!(s, "  {}", g.description);
    }
    let _ = writeln!(
        s,
        "  \x1b[2mupdated {} · created {}\x1b[0m",
        goal_ago(g.updated_at),
        goal_ago(g.created_at)
    );
    let _ = write!(s, "  milestones ({done}/{total})");
    if g.milestones.is_empty() {
        let _ = write!(s, " —");
    } else {
        s.push('\n');
        for m in &g.milestones {
            let mark = if m.done { "[x]" } else { "[ ]" };
            let _ = writeln!(s, "    {mark} {}", m.title);
        }
        // trim the trailing newline from the last writeln for tidy joining below
        while s.ends_with('\n') {
            s.pop();
        }
    }
    let open = g.open_blockers();
    let _ = write!(s, "\n  blockers ({open} open)");
    if g.blockers.is_empty() {
        let _ = write!(s, " —");
    } else {
        s.push('\n');
        for b in &g.blockers {
            let mark = if b.resolved { "✓" } else { "✗" };
            let _ = writeln!(s, "    {mark} {}", b.description);
        }
        while s.ends_with('\n') {
            s.pop();
        }
    }
    let _ = write!(s, "\n  linked");
    if g.linked_tasks.is_empty() {
        let _ = write!(s, " —");
    } else {
        let refs: Vec<String> = g
            .linked_tasks
            .iter()
            .map(|t| match &t.title {
                Some(title) => format!("{} ({title})", t.key),
                None => t.key.clone(),
            })
            .collect();
        let _ = write!(s, " {}", refs.join(", "));
    }
    s
}

/// Handle a persisted-goal subcommand, returning the line(s) to print. Mutating
/// subcommands persist via `session.persist_goal` and update `current_goal_id`.
fn goal_command(session: &mut Session, sub: &str, args: &str) -> String {
    use crate::goal::{Goal, GoalStatus, TaskRef};
    let args = args.trim();
    match sub {
        "new" => {
            if args.is_empty() {
                return "usage: :goal new <title>".into();
            }
            let g = Goal::new(args);
            let (id, short, title) = (g.id.clone(), goal_short(&g.id).to_string(), g.title.clone());
            session.persist_goal(g);
            session.current_goal_id = Some(id);
            format!("goal created \x1b[2m{short}\x1b[0m {title} — now current")
        }
        "show" => {
            let g = if args.is_empty() {
                session.current_goal().cloned()
            } else {
                session.find_goal_by_prefix(args).cloned()
            };
            match g {
                Some(g) => {
                    session.current_goal_id = Some(g.id.clone());
                    goal_detail(&g)
                }
                None if args.is_empty() => {
                    "no goals yet — `:goal new <title>` to create one".into()
                }
                None => format!("no goal matching \x1b[2m{args}\x1b[0m (ambiguous or unknown id)"),
            }
        }
        "status" => {
            let mut out = String::new();
            if let Some(loop_) = &session.goal {
                out.push_str(&loop_.status_line());
                out.push('\n');
            }
            if session.goals.is_empty() {
                out.push_str(
                    "no goals yet — `:goal new <title>` to track one, or `:goal <condition>` to pursue one in the background",
                );
                return out;
            }
            let current = session.current_goal().map(|g| g.id.clone());
            let mut goals: Vec<&Goal> = session.goals.iter().collect();
            goals.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            use std::fmt::Write;
            let _ = write!(out, "{} goal(s):", goals.len());
            for g in goals {
                let (done, total) = g.milestone_progress();
                let marker = if Some(&g.id) == current.as_ref() { "*" } else { " " };
                let open = g.open_blockers();
                let blk = if open > 0 { format!(", {open} blocker(s)") } else { String::new() };
                let _ = write!(
                    out,
                    "\n {marker} [{}] \x1b[2m{}\x1b[0m {} — {done}/{total} milestones{blk}",
                    g.status,
                    goal_short(&g.id),
                    g.title
                );
            }
            out
        }
        "link" => {
            if args.is_empty() {
                return "usage: :goal link <task-id> [title]".into();
            }
            // First token is the key; any remainder is a cached human title.
            let mut it = args.splitn(2, char::is_whitespace);
            let key = it.next().unwrap_or("").to_string();
            let title = it.next().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            let Some(mut g) = session.current_goal().cloned() else {
                return "no current goal — `:goal new <title>` or `:goal show <id>` first".into();
            };
            let tref = match title {
                Some(t) => TaskRef::with_title(key.clone(), t),
                None => TaskRef::new(key.clone()),
            };
            g.link_task(tref);
            let (short, gt) = (goal_short(&g.id).to_string(), g.title.clone());
            session.persist_goal(g);
            format!("linked {key} → \x1b[2m{short}\x1b[0m {gt}")
        }
        "block" => {
            if args.is_empty() {
                return "usage: :goal block <reason> [@waiting-on]".into();
            }
            // Optional trailing `@who` marks what the goal is waiting on.
            let (reason, waiting) = match args.rsplit_once(char::is_whitespace) {
                Some((head, last)) if last.starts_with('@') && last.len() > 1 => {
                    (head.trim().to_string(), Some(last[1..].to_string()))
                }
                _ => (args.to_string(), None),
            };
            if reason.is_empty() {
                return "usage: :goal block <reason> [@waiting-on]".into();
            }
            let desc = match waiting {
                Some(who) => format!("{reason} (waiting on {who})"),
                None => reason,
            };
            let Some(mut g) = session.current_goal().cloned() else {
                return "no current goal — `:goal new <title>` or `:goal show <id>` first".into();
            };
            g.add_blocker(desc.clone());
            let short = goal_short(&g.id).to_string();
            session.persist_goal(g);
            format!("blocked \x1b[2m{short}\x1b[0m: {desc}")
        }
        "unblock" => {
            if args.is_empty() {
                return "usage: :goal unblock <reason>   (matches an open blocker)".into();
            }
            let Some(mut g) = session.current_goal().cloned() else {
                return "no current goal — nothing to unblock".into();
            };
            let needle = args.to_ascii_lowercase();
            let hit = g
                .blockers
                .iter_mut()
                .find(|b| !b.resolved && b.description.to_ascii_lowercase().contains(&needle));
            match hit {
                Some(b) => {
                    b.resolved = true;
                    let desc = b.description.clone();
                    g.touch();
                    session.persist_goal(g);
                    format!("unblocked: {desc}")
                }
                None => format!("no open blocker matching \x1b[2m{args}\x1b[0m"),
            }
        }
        "milestone" => {
            if args.is_empty() {
                return "usage: :goal milestone <name> [date]".into();
            }
            // Fold an optional trailing date token into the milestone title.
            let title = match args.rsplit_once(char::is_whitespace) {
                Some((head, last)) if looks_like_date(last) && !head.trim().is_empty() => {
                    format!("{} (due {last})", head.trim())
                }
                _ => args.to_string(),
            };
            let Some(mut g) = session.current_goal().cloned() else {
                return "no current goal — `:goal new <title>` or `:goal show <id>` first".into();
            };
            g.add_milestone(title.clone());
            let short = goal_short(&g.id).to_string();
            session.persist_goal(g);
            format!("milestone added \x1b[2m{short}\x1b[0m: {title}")
        }
        "complete" => {
            // `:goal complete [<id>]` — the named goal, else the current one.
            let g = if args.is_empty() {
                session.current_goal().cloned()
            } else {
                session.find_goal_by_prefix(args).cloned()
            };
            let Some(mut g) = g else {
                return if args.is_empty() {
                    "no current goal to complete — `:goal new <title>` first".into()
                } else {
                    format!("no goal matching \x1b[2m{args}\x1b[0m")
                };
            };
            if g.status == GoalStatus::Completed {
                return format!("already complete: \x1b[2m{}\x1b[0m {}", goal_short(&g.id), g.title);
            }
            g.set_status(GoalStatus::Completed);
            let (short, title) = (goal_short(&g.id).to_string(), g.title.clone());
            session.persist_goal(g);
            format!("goal completed \x1b[2m{short}\x1b[0m {title} ✓")
        }
        // The dispatcher only routes known subcommands here.
        other => format!("unknown :goal subcommand `{other}`"),
    }
}


/// `:tell` / SendMessage channel. The message lands in the durable
/// `coordinator_messages` mailbox keyed by the run's id and is folded into the
/// coordinator's NEXT round (see `coordinator::drive`), so it's how you steer a
/// running job — add a clarification, correct a wrong assumption, narrow scope —
/// without killing and re-launching it. `id` is matched exactly or by prefix
/// against this session's live workers first, then every durable run, so the
/// short ids shown by `:workers` work. A terminal (finished) run is refused —
/// nothing would read the message — and an ambiguous prefix lists the matches.
fn tell_coordinator(id: Option<&str>, message: &str, any: bool, session: &mut Session) {
    let Some(id) = id else {
        println!(
            "usage: :tell [--any] <worker-id> <message>   — steer an in-flight coordinator (--any: across sessions)"
        );
        return;
    };
    if message.is_empty() {
        println!(
            "usage: :tell [--any] <worker-id> <message>   — steer an in-flight coordinator (--any: across sessions)"
        );
        return;
    }
    let Some(store) = session.coordinator_store.clone() else {
        println!("coordinator store unavailable — can't queue messages");
        return;
    };
    let hit = |rid: &str| rid == id || rid.starts_with(id);

    // Candidates: (run_id, terminal?). This session's in-memory workers first —
    // their run_id is known even before the child has written its store row, so a
    // message sent immediately after launch still lands — then durable runs from
    // any session (deduped on run_id).
    let mut candidates: Vec<(String, bool, Option<String>)> = Vec::new();
    for w in session.worker_jobs.lock().unwrap().iter() {
        if hit(&w.id) {
            candidates.push((
                w.id.clone(),
                matches!(w.status().as_str(), "done" | "failed"),
                Some(session.session_id.clone()),
            ));
        }
    }
    if let Ok(rows) = store.load_all() {
        for r in rows {
            if hit(&r.run_id) && !candidates.iter().any(|(rid, _, _)| rid == &r.run_id) {
                candidates.push((
                    r.run_id.clone(),
                    matches!(r.phase.as_str(), "done" | "failed"),
                    r.session_id.clone(),
                ));
            }
        }
    }

    // AC7 ownership gate: by default you may only steer coordinators your OWN
    // session launched; `--any` opts into another session's coordinator. A
    // foreign-only match without `--any` is refused with guidance rather than
    // silently treated as "not found", so the run isn't invisibly reachable.
    let owned: Vec<(String, bool, Option<String>)> = candidates
        .iter()
        .filter(|(_, _, owner)| {
            matches!(
                owner_gate(owner.as_deref(), &session.session_id, any),
                OwnerGate::Allow
            )
        })
        .cloned()
        .collect();
    if owned.is_empty() {
        if candidates.is_empty() {
            println!("no background coordinator matching '{id}' (see :workers)");
        } else {
            println!(
                "'{id}' matches a coordinator launched by another session — re-run as `:tell --any {id} <message>` to steer it"
            );
        }
        return;
    }

    match owned.as_slice() {
        [(run_id, terminal, _)] => {
            let short = crate::batch::short_id(run_id);
            if *terminal {
                println!(
                    "coordinator {short} has already finished — message not queued (`:result {short}` to view its result)"
                );
                return;
            }
            match store.enqueue_message(run_id, message, Some(&session.session_id)) {
                Ok(_) => {
                    let pending = store.pending_message_count(run_id).unwrap_or(0);
                    println!(
                        "\x1b[2m✉ queued for {short} ({pending} pending) — folded in at the start of its next round\x1b[0m"
                    );
                }
                Err(e) => println!("couldn't queue message: {e}"),
            }
        }
        many => {
            println!(
                "'{id}' matches {} coordinators — be more specific:",
                many.len()
            );
            for (rid, _, _) in many {
                println!("  {rid}");
            }
        }
    }
}

/// `:stop` — stand down an in-flight coordinator: the harsh sibling of `:tell`.
/// Where `:tell` queues a message the coordinator folds in and keeps working,
/// `:stop` ENDS the run — it raises the durable stand-down flag (honored at the
/// coordinator's next round boundary; see `coordinator::drive`) and, for a worker
/// this session owns, additionally SIGINTs its process group so the in-flight
/// turn aborts and the boundary is reached promptly. Candidate resolution and the
/// AC7 `--any` ownership gate mirror `tell_coordinator`.
/// Dispatch a background coordinator to resolve a SEMANTIC `:alert` — a
/// condition no local probe can decide. The coordinator runs with the full
/// toolset and, the moment it judges the condition met, calls the `set_alert`
/// tool with this alert's id, which the presenter surfaces to the operator
/// console exactly like a native probe. Returns false when no coordinator could
/// be launched (missing credential / can't locate the aish binary), leaving the
/// alert armed for a human/agent to fire manually.
fn spawn_alert_coordinator(session: &mut Session, id: i64, description: &str) -> bool {
    // A coordinator needs a credential for the active backend and our own exe.
    let no_credential = match session.backend_kind.as_str() {
        "grok" => !crate::backend::grok::credential_available(&session.env),
        _ => crate::backend::claude::Credential::resolve(&session.env).is_err(),
    };
    if no_credential {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let task = format!(
        "You are resolving an operator ALERT (the aish `:alert` feature). Watch for this \
condition and, the INSTANT it is satisfied, call the `set_alert` tool with alert_id={id} and a \
concise `message` describing what happened — that is the whole job. Use your tools to check \
periodically (poll external state like PRs, files, services, logs as appropriate); do not spin in \
a tight loop — space out your checks. If the condition is already true right now, fire immediately. \
If after reasonable effort it is clearly unsatisfiable or you cannot determine it, stop and say so \
instead of firing.\n\nCondition: {description}"
    );
    // Alert monitors observe live state; run in the shared cwd (no worktree
    // isolation — they don't write code) so `gh`/file checks see the real world.
    let spec = crate::worker::WorkerSpec {
        exe,
        cwd: session.cwd.clone(),
        backend: session.backend_kind.clone(),
        model: crate::worker::coordinator_model(&session.backend_kind, &session.batch_model),
        env: session.env.clone(),
        isolate: false,
        base: "main".to_string(),
        launch_session_id: session.session_id.clone(),
        launch_session_name: session.name.clone(),
        show_output: session.show_worker_output.clone(),
        attached: session.attached.clone(),
        coordinator_store: session.coordinator_store.clone(),
    };
    let _ = crate::worker::spawn(&session.worker_jobs, task, spec);
    true
}


fn stop_coordinator(id: Option<&str>, any: bool, session: &mut Session) {
    let Some(id) = id else {
        println!(
            "usage: :stop [--any] <worker-id>   — stand down an in-flight coordinator (--any: across sessions)"
        );
        return;
    };
    let Some(store) = session.coordinator_store.clone() else {
        println!("coordinator store unavailable — can't stand down a coordinator");
        return;
    };
    let hit = |rid: &str| rid == id || rid.starts_with(id);

    // Candidates: (run_id, terminal?, owner, pid). This session's in-memory
    // workers first — their pid is known so we can interrupt the current turn —
    // then durable runs from any session (deduped on run_id; flag-only, no pid).
    let mut candidates: Vec<(String, bool, Option<String>, Option<u32>)> = Vec::new();
    for w in session.worker_jobs.lock().unwrap().iter() {
        if hit(&w.id) {
            candidates.push((
                w.id.clone(),
                matches!(w.status().as_str(), "done" | "failed"),
                Some(session.session_id.clone()),
                w.pid(),
            ));
        }
    }
    if let Ok(rows) = store.load_all() {
        for r in rows {
            if hit(&r.run_id) && !candidates.iter().any(|(rid, _, _, _)| rid == &r.run_id) {
                candidates.push((
                    r.run_id.clone(),
                    matches!(r.phase.as_str(), "done" | "failed"),
                    r.session_id.clone(),
                    None,
                ));
            }
        }
    }

    // AC7 ownership gate: by default you may only stand down coordinators your OWN
    // session launched; `--any` opts into another session's coordinator.
    let owned: Vec<(String, bool, Option<String>, Option<u32>)> = candidates
        .iter()
        .filter(|(_, _, owner, _)| {
            matches!(
                owner_gate(owner.as_deref(), &session.session_id, any),
                OwnerGate::Allow
            )
        })
        .cloned()
        .collect();
    if owned.is_empty() {
        if candidates.is_empty() {
            println!("no background coordinator matching '{id}' (see :workers)");
        } else {
            println!(
                "'{id}' matches a coordinator launched by another session — re-run as `:stop --any {id}` to stand it down"
            );
        }
        return;
    }

    match owned.as_slice() {
        [(run_id, terminal, _, pid)] => {
            let short = crate::batch::short_id(run_id);
            if *terminal {
                println!(
                    "coordinator {short} has already finished — nothing to stand down (`:result {short}` to view its result)"
                );
                return;
            }
            if let Err(e) = store.request_stand_down(run_id) {
                println!("couldn't raise stand-down flag: {e}");
                return;
            }
            // Best-effort interrupt of the in-flight turn for a worker we own, so
            // the round boundary (where the flag is honored) is reached now rather
            // than after the current turn. Negated pid targets the whole process
            // group (pgid == pid via the child's setsid()).
            if let Some(pid) = pid {
                unsafe { libc::kill(-(*pid as i32), libc::SIGINT) };
                println!(
                    "\x1b[33m🛑 stand-down ordered for {short}\x1b[0m — turn interrupted; one final wrap-up turn, then it exits"
                );
            } else {
                println!(
                    "\x1b[33m🛑 stand-down flag raised for {short}\x1b[0m — it will wrap up and exit at its next round boundary"
                );
            }
        }
        many => {
            println!(
                "'{id}' matches {} coordinators — be more specific:",
                many.len()
            );
            for (rid, _, _, _) in many {
                println!("  {rid}");
            }
        }
    }
}

/// `:context` — show how full the model's context window is, plus the history
/// and stored-memory counts. The usage figure is the running estimate the engine
/// maintains from each turn's reported token usage (see `crate::context`).
fn handle_context(backend: &Backend, session: &Session) {
    let window = backend.context_window();
    let used = session.context_used;
    let pct = if window > 0 { used * 100 / window } else { 0 };
    let mem = session
        .db
        .as_ref()
        .and_then(|d| d.memory_count().ok())
        .unwrap_or(0);
    println!(
        "context: {used} / {window} tokens ({pct}%) · {} message(s) in history · {mem} stored memories",
        session.history.len()
    );
    if used == 0 {
        println!("  (usage updates after the first model turn)");
    } else if crate::context::should_compact(used, window, crate::context::COMPACT_THRESHOLD_PCT) {
        println!(
            "  over the {}% compaction threshold — the next turn will offload older history to memory (or run :compact now)",
            crate::context::COMPACT_THRESHOLD_PCT
        );
    }
}

/// `:reasoning` — render the reasoning-quality telemetry summary: the escalate
/// rate, the guess→wrong-turn rate, and both broken down by complexity and risk.
/// This is the ground-truth model of *when aish's own reasoning is good enough*
/// vs. *when to reach for the stronger model* (see `crate::reasoning_telemetry`).
fn handle_reasoning() {
    let summary = crate::reasoning_telemetry::summarize();
    println!("{}", crate::reasoning_telemetry::render_report(&summary));
}

/// `:compact` — force a context compaction now: offload the oldest slice of the
/// conversation to the SQLite memories table (recoverable via the recall tool,
/// tagged `context-offload`) and replace it with a short in-context summary.
fn handle_compact(backend: &Backend, session: &mut Session) {
    match crate::context::plan_compaction(&session.history, crate::context::KEEP_RECENT_MSGS) {
        Some(plan) => {
            if let Some(db) = &session.db {
                let _ = db.remember(&plan.offload, Some("context-offload"));
            }
            let dropped = plan.dropped;
            crate::context::apply_compaction(&mut session.history, &plan);
            session.context_used = crate::context::estimate_history_tokens(&session.history);
            let window = backend.context_window();
            let pct = if window > 0 {
                session.context_used * 100 / window
            } else {
                0
            };
            println!(
                "compacted — {dropped} message(s) offloaded to memory (recall \"context-offload\"); context now ~{pct}%"
            );
        }
        None => println!("nothing to compact yet — the conversation is short"),
    }
}

/// `:memories [organize]` — show the stored-memory count, or run an organization
/// (dedup) pass that prunes duplicate memories from the SQLite store.
fn handle_memories(sub: Option<&str>, session: &Session) {
    let Some(db) = &session.db else {
        println!("memory store unavailable");
        return;
    };
    match sub {
        Some("organize" | "organise" | "gc" | "dedup") => match db.organize_memories() {
            Ok(n) => println!(
                "organized — {n} duplicate memory(ies) pruned ({} remain)",
                db.memory_count().unwrap_or(0)
            ),
            Err(e) => println!("organize failed: {e:#}"),
        },
        _ => {
            let n = db.memory_count().unwrap_or(0);
            println!("{n} stored memories · :memories organize to dedup/consolidate");
        }
    }
}

/// `:hooks [list|reload]` — show the merged lifecycle-hook set with per-hook
/// provenance (`local` / `plugin:<id>`), or reload the user + project + plugin
/// hook layers mid-session (Phase 0.5.6). Bare `:hooks` is an alias for `list`.
fn handle_hooks(sub: Option<&str>, session: &mut Session) {
    match sub {
        Some("reload") => {
            session.load_hooks();
            println!("hooks reloaded");
            println!("{}", session.hooks.format_list());
        }
        Some("list") | None => println!("{}", session.hooks.format_list()),
        Some(other) => println!("unknown :hooks subcommand `{other}` — try :hooks list"),
    }
}

/// `:plugin [list|info <id>]` — plugin provenance introspection (Phase 0.5.6).
/// `list` enumerates discovered plugins; `info <id>` renders one plugin's full
/// capability report (metadata, login, lifecycle + event hooks, MCP servers,
/// schemas, skills). `info <id> --schema` (Phase 3.5) renders the plugin's
/// JSON-Schema shapes instead. Bare `:plugin` is an alias for `list`.
fn handle_plugin(args: Vec<&str>) {
    let dir = crate::plugins::default_plugins_dir();
    let sub = args.first().copied();
    let id = args.get(1).copied();
    let flag = args.get(2).copied();
    match sub {
        Some("info") => {
            let Some(id) = id else {
                println!("usage: :plugin info <id> [--schema]");
                return;
            };
            // Phase 3.5: `--schema` drills into the plugin's shipped schemas.
            if matches!(flag, Some("--schema") | Some("--schemas")) {
                match crate::plugins::format_plugin_schemas(&dir, id) {
                    Some(report) => println!("{report}"),
                    None => println!("no such plugin `{id}` — try :plugin list"),
                }
                return;
            }
            match crate::plugins::format_plugin_info(&dir, id) {
                Some(report) => println!("{report}"),
                None => println!("no such plugin `{id}` — try :plugin list"),
            }
        }
        Some("memory" | "mem") => handle_plugin_memory(&args[1..]),
        Some("list") | None => {
            let plugins = crate::plugins::discover(&dir);
            if plugins.is_empty() {
                println!("no plugins installed ({})", dir.display());
                return;
            }
            for p in &plugins {
                let m = &p.manifest;
                let name = if m.name.is_empty() { &m.id } else { &m.name };
                let ver = if m.version.is_empty() { "-" } else { &m.version };
                let state = if m.is_enabled() { "" } else { " (disabled)" };
                println!("  {:<20} {name} v{ver}{state}", m.id);
            }
            println!("\n:plugin info <id> for full provenance");
        }
        Some(other) => println!("unknown :plugin subcommand `{other}` — try :plugin list"),
    }
}

/// `:plugin memory …` — inspect and manage a plugin's file-based memory
/// (`src/plugin_memory.rs`). Secret `auth` namespace values are ALWAYS redacted
/// on display. `args` is the token stream *after* `memory`.
///
/// Forms:
///   :plugin memory list                                  # namespaces + key counts, per plugin
///   :plugin memory <id> <namespace>                      # dump a namespace (auth redacted)
///   :plugin memory <id> get <namespace> <key>            # one key
///   :plugin memory <id> set <namespace> <key> <value…>   # value parsed as JSON, else string
///   :plugin memory <id> delete <namespace> <key>         # remove a key
///   :plugin memory <id> clear <namespace> [yes]          # empty a namespace (needs `yes`)
fn handle_plugin_memory(args: &[&str]) {
    let mem = crate::plugin_memory::global();
    let verbs = ["get", "set", "delete", "del", "clear"];

    match args {
        // ---- list ----------------------------------------------------------
        [] | ["list" | "ls"] => {
            let dir = crate::plugins::default_plugins_dir();
            let plugins = crate::plugins::discover(&dir);
            if plugins.is_empty() {
                println!("no plugins installed ({})", dir.display());
                return;
            }
            for p in &plugins {
                let id = &p.manifest.id;
                let counts: Vec<String> = crate::plugin_memory::MemoryNamespace::ALL
                    .iter()
                    .map(|ns| {
                        let n = mem.key_count(id, *ns).unwrap_or(0);
                        format!("{ns}={n}")
                    })
                    .collect();
                println!("  {:<20} {}", id, counts.join("  "));
            }
            println!("\n:plugin memory <id> <namespace> to inspect (auth is redacted)");
        }

        // ---- get -----------------------------------------------------------
        [id, "get", ns, key] => match mem.get(id, ns, key) {
            Ok(v) => println!("{}", pretty_json(&v)),
            Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e}"),
        },

        // ---- set -----------------------------------------------------------
        [id, "set", ns, key, value_toks @ ..] if !value_toks.is_empty() => {
            let raw = value_toks.join(" ");
            // Try to parse as JSON; fall back to a plain string literal.
            let value = serde_json::from_str(&raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
            match mem.set(id, ns, key, value) {
                Ok(()) => println!("set {id}/{ns}.{key}"),
                Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e}"),
            }
        }

        // ---- delete --------------------------------------------------------
        [id, "delete" | "del", ns, key] => match mem.delete(id, ns, key) {
            Ok(()) => println!("deleted {id}/{ns}.{key}"),
            Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e}"),
        },

        // ---- clear (needs explicit confirmation) ---------------------------
        [id, "clear", ns] => {
            let n = crate::plugin_memory::MemoryNamespace::parse(ns)
                .and_then(|namespace| mem.key_count(id, namespace))
                .unwrap_or(0);
            println!(
                "this will clear {n} key(s) from {id}/{ns}. Re-run with `yes` to confirm:\n  :plugin memory {id} clear {ns} yes"
            );
        }
        [id, "clear", ns, "yes" | "--yes" | "-y"] => match mem.clear(id, ns) {
            Ok(()) => println!("cleared {id}/{ns}"),
            Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e}"),
        },

        // ---- display a whole namespace: `<id> <namespace>` -----------------
        [id, ns] if !verbs.contains(ns) => {
            match mem.display_namespace(id, match crate::plugin_memory::MemoryNamespace::parse(ns) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("\x1b[31maish:\x1b[0m {e}");
                    return;
                }
            }) {
                Ok(v) => {
                    let redacted = crate::plugin_memory::MemoryNamespace::parse(ns)
                        .map(|n| n.is_secret())
                        .unwrap_or(false);
                    if redacted {
                        println!("# {id}/{ns} (values redacted)");
                    }
                    println!("{}", pretty_json(&v));
                }
                Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e}"),
            }
        }

        _ => {
            println!(
                "usage:\n  \
                 :plugin memory list\n  \
                 :plugin memory <id> <namespace>\n  \
                 :plugin memory <id> get <namespace> <key>\n  \
                 :plugin memory <id> set <namespace> <key> <value>\n  \
                 :plugin memory <id> delete <namespace> <key>\n  \
                 :plugin memory <id> clear <namespace> [yes]\n\
                 namespaces: auth (redacted), cache, webhooks, prefs"
            );
        }
    }
}

/// Pretty-print a JSON value for REPL display, falling back to `Debug` if
/// serialization somehow fails.
fn pretty_json(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| format!("{v:?}"))
}

/// `:telemetry [clear]` — show aggregated tool-call failure & retry-recovery
/// stats (which tools fail most, with what error class, and whether retries
/// recover), or wipe the telemetry table. Feeds the "is this error worth
/// retrying vs. escalating?" repair heuristic (see crate::tool_telemetry).
fn handle_telemetry(sub: Option<&str>, session: &mut Session) {
    if session.db.is_none() {
        println!("telemetry store unavailable");
        return;
    }
    match sub {
        Some("clear" | "reset" | "wipe") => {
            let db = session.db.as_ref().unwrap();
            match db.clear_tool_telemetry() {
                Ok(n) => println!("telemetry cleared — {n} row(s) removed"),
                Err(e) => println!("clear failed: {e:#}"),
            }
            // Wiping the table invalidates any pre-aggregated snapshot (TASK-252).
            session.tool_telemetry_cache = None;
        }
        _ => {
            // Pre-aggregated read: served from the 60s Session cache on a hit,
            // re-queried and cached on a miss (TASK-252 / FR-305).
            if let Some(snap) = crate::tool_telemetry::aggregate_cached(session) {
                print!(
                    "{}",
                    crate::tool_telemetry::render_report(
                        snap.total,
                        &snap.totals,
                        &snap.class_failures,
                        &snap.retries,
                    )
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// :skill — install, search, list, remove skill.fish skills (Phase 2)
// ---------------------------------------------------------------------------

/// The local skills directory `~/.aish/skills` the `:skill` commands act on —
/// the same path startup scans and `Session::reload_skills` refreshes.
fn skills_dir_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("skills")
}

/// A parsed `:skill` subcommand. Pure routing, so the dispatch decision is
/// unit-testable without performing any network or filesystem work.
#[derive(Debug, PartialEq)]
enum SkillCmd {
    /// `:skill add <owner/name[@version]>`
    Add(String),
    /// `:skill search <query…>`
    Search(String),
    /// `:skill list`
    List,
    /// `:skill remove <name>`
    Remove(String),
    /// Anything else → print usage.
    Usage,
}

/// Route the whitespace-split args after `:skill` to a `SkillCmd`. `add`/`remove`
/// require an operand; `search` requires a non-empty query; bare/unknown forms
/// fall through to `Usage`.
fn parse_skill_command(args: &[&str]) -> SkillCmd {
    match args {
        ["add", reference, ..] => SkillCmd::Add((*reference).to_string()),
        ["search", rest @ ..] if !rest.is_empty() => SkillCmd::Search(rest.join(" ")),
        ["list" | "ls"] => SkillCmd::List,
        ["remove" | "rm" | "delete", name, ..] => SkillCmd::Remove((*name).to_string()),
        _ => SkillCmd::Usage,
    }
}

/// True when `name` is safe to use as a skill directory under `~/.aish/skills`:
/// non-empty, not `.`/`..`, and free of path separators. The hard guard against
/// `:skill remove ../something` escaping the skills dir.
fn valid_skill_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

/// Write a fetched SKILL.md into `skills_dir` and reload the session's catalog
/// from there, returning the imported skill's frontmatter name. The testable
/// core of `:skill add`'s reload step — no network, so the reload behavior is
/// unit-tested directly. (Test-only: production `:skill add` goes through
/// `skill_provider::add`, which handles both skill.fish and GitHub sources.)
#[cfg(test)]
fn install_skill_text(text: &str, skills_dir: &Path, session: &mut Session) -> Result<String> {
    crate::skill_provider::import(text, skills_dir)?;
    session.reload_skills_from(skills_dir);
    let name = crate::skills::parse_frontmatter(text)
        .map(|(n, _)| n)
        .unwrap_or_default();
    Ok(name)
}

/// `:skill <add|search|list|remove …>` — manage skill.fish skills interactively.
/// Errors are returned for the REPL to print; nothing here exits the REPL.
async fn handle_skill_command(args: &[&str], session: &mut Session) -> Result<()> {
    match parse_skill_command(args) {
        SkillCmd::Add(reference) => skill_add(&reference, session).await,
        SkillCmd::Search(query) => skill_search(&query).await,
        SkillCmd::List => {
            skill_list();
            Ok(())
        }
        SkillCmd::Remove(name) => skill_remove(&name, session),
        SkillCmd::Usage => {
            println!(
                "usage: :skill add <owner/name[@version] | github:owner/repo[/path][@ref]> | search <query> | list | remove <name>"
            );
            Ok(())
        }
    }
}

/// `:skill add <ref>` — fetch a SKILL.md (from skill.fish or GitHub), import it,
/// and reload the catalog so the model can use it from the next turn. A GitHub
/// repo spec (`github:owner/repo`) may import several skills at once.
async fn skill_add(reference: &str, session: &mut Session) -> Result<()> {
    let skills_dir = skills_dir_path();
    let _ = std::fs::create_dir_all(&skills_dir);
    println!("\x1b[2mfetching {reference} …\x1b[0m");
    let imported = crate::skill_provider::add(reference, &skills_dir).await?;
    session.reload_skills_from(&skills_dir);
    for sk in &imported {
        if sk.description.is_empty() {
            println!("\x1b[32m✓\x1b[0m added \x1b[1m{}\x1b[0m", sk.name);
        } else {
            println!(
                "\x1b[32m✓\x1b[0m added \x1b[1m{}\x1b[0m — \x1b[2m{}\x1b[0m",
                sk.name, sk.description
            );
        }
    }
    let n = imported.len();
    println!(
        "  catalog reloaded ({n} skill{}).",
        if n == 1 { "" } else { "s" }
    );
    Ok(())
}

/// `:skill search <query>` — query the skill.fish catalog and print matches.
async fn skill_search(query: &str) -> Result<()> {
    let results = crate::skill_provider::search(query).await?;
    println!(
        "{}",
        crate::skill_provider::print_results_table(query, &results)
    );
    Ok(())
}

/// `:skill list` — list the locally installed skills (name + description).
fn skill_list() {
    let skills = crate::skills::load(&skills_dir_path());
    if skills.is_empty() {
        println!("No skills installed. `:skill search <query>` to find some.");
        return;
    }
    for s in &skills {
        if s.description.is_empty() {
            println!("\x1b[1m{}\x1b[0m", s.name);
        } else {
            println!("\x1b[1m{}\x1b[0m — {}", s.name, s.description);
        }
    }
}

/// `:skill remove <name>` — delete an installed skill and reload the catalog.
/// Validates `name` first (no traversal/separators), then removes the directory
/// (handles both a file and a dir). A missing skill is reported, not an error.
fn skill_remove(name: &str, session: &mut Session) -> Result<()> {
    if !valid_skill_name(name) {
        anyhow::bail!("invalid skill name: {name:?}");
    }
    let dir = skills_dir_path().join(name);
    if !dir.exists() {
        println!("no skill named \x1b[1m{name}\x1b[0m installed (see :skill list)");
        return Ok(());
    }
    std::fs::remove_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("removing {}: {e}", dir.display()))?;
    session.reload_skills()?;
    println!("\x1b[32m✓\x1b[0m removed \x1b[1m{name}\x1b[0m; catalog reloaded.");
    Ok(())
}

/// Markdown header for the `:workers` table. Kept as a const so the column set
/// — Worker | Session | Status | Started | Runtime | Doing | Result — is a
/// single source of truth and unit-testable. The Started column is a relative
/// "ago" label; Runtime is the elapsed-so-far (running) or total (terminal) span
/// (see `crate::style::time_cells`).
const WORKERS_TABLE_HEADER: &str =
    "| Worker | Session | Status | Started | Runtime | Doing | Result |\n|---|---|---|---|---|---|---|\n";

/// Returns true when the REPL should exit.
/// Is the active backend the local llama.cpp backend? (Always false without the
/// `local` feature, where `Backend::Local` doesn't exist.)
#[cfg(feature = "local")]
fn is_local_backend(backend: &Backend) -> bool {
    matches!(backend, Backend::Local(_))
}
#[cfg(not(feature = "local"))]
fn is_local_backend(_backend: &Backend) -> bool {
    false
}

/// Rebuild the local backend in place so a freshly-detected model selection
/// takes effect. No-op stub without the `local` feature.
#[cfg(feature = "local")]
fn rebuild_local_backend(backend: &mut Backend, session: &mut Session) -> anyhow::Result<()> {
    *backend = Backend::new_local()?;
    session.backend_kind = backend.kind().to_string();
    Ok(())
}
#[cfg(not(feature = "local"))]
fn rebuild_local_backend(_backend: &mut Backend, _session: &mut Session) -> anyhow::Result<()> {
    anyhow::bail!("built without the 'local' feature")
}

async fn handle_colon(
    cmd: &str,
    backend: &mut Backend,
    session: &mut Session,
    pending_update: &mut Option<crate::update::UpdateInfo>,
) -> bool {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("q" | "quit" | "exit") => return true,
        Some("loop") => match parts.next() {
            // Bare `:loop` or `:loop status` — report the active loop, if any.
            None | Some("status") => println!("\x1b[2m{}\x1b[0m", session.loop_status()),
            // `:loop stop` / `:loop clear` — abandon an in-flight loop.
            Some("stop") | Some("clear") => {
                if session.clear_loop() {
                    println!("\x1b[33m↻\x1b[0m loop stopped");
                } else {
                    println!("\x1b[2mno loop running\x1b[0m");
                }
            }
            // `:loop <count> <prompt>` — arm a new loop.
            Some(tok) => match tok.parse::<usize>() {
                Ok(count) => {
                    let prompt = parts.collect::<Vec<_>>().join(" ");
                    if prompt.trim().is_empty() {
                        println!("\x1b[2musage: :loop <count> <prompt>\x1b[0m");
                    } else if session.loop_state.is_some() {
                        println!(
                            "\x1b[33m↻\x1b[0m a loop is already active — `:loop stop` it first"
                        );
                    } else {
                        if count > crate::session::MAX_LOOP {
                            println!(
                                "\x1b[2mloop count capped at {}\x1b[0m",
                                crate::session::MAX_LOOP
                            );
                        }
                        let total = count.clamp(1, crate::session::MAX_LOOP);
                        session.start_loop(count, prompt);
                        println!(
                            "\x1b[2m↻ loop armed — {total} iteration{}\x1b[0m",
                            if total == 1 { "" } else { "s" }
                        );
                    }
                }
                Err(_) => println!(
                    "\x1b[2musage: :loop <count> <prompt>  (count must be a number)\x1b[0m"
                ),
            },
        },
        Some("help") => {
            println!(
                "type a command (first word in PATH) to run it directly — anything else goes to the model\n\
                 prefix ! to force direct execution, ? to force the model\n\
                 type : (then a letter, or TAB) to pop up the :command palette\n\
                 login <plugin-id>                   run a plugin's auth handler; stores its credential in\n\
                                                     ~/.aish/credentials as [profile:<plugin-id>], reusable via\n\
                                                     ${{profile:<plugin-id>}} in the plugin's .mcp.json\n\
                 :mode <paranoid|careful|normal|yolo> confirmation level (paranoid asks for everything,\n\
                                                     normal only for write/create/delete, yolo never)\n\
                 :model <opus|sonnet|haiku|full-id>  switch model\n\
                 :model-detect                       re-detect + select the best local model for this machine\n\
                 :backend <claude|grok|local>        switch backend\n\
                 :mcp [list|status]                  list connected MCP servers\n\
                 :mcp reconnect [name|all]           restart MCP server(s)\n\
                 :mcp reload                          connect servers newly added to .mcp.json (no restart)\n\
                 :mcp add <name> <command|url> [args] connect + save an MCP server (~/.aish/.mcp.json)\n\
                 :mcp remove <name>                  disconnect + unsave an MCP server\n\
                 :mcp tools [name]                   list MCP tools\n\
                 :yolo                               toggle yolo mode\n\
                 :new                                clear conversation history\n\
                 :context                            show context-window usage (tokens, %, memories)\n\
                 :reasoning                          reasoning-quality telemetry: escalate-vs-guess rate + outcomes by complexity/risk\n\
                 :compact                            offload older history to long-term memory now\n\
                 :memories [organize]                list stored memories, or dedup them\n\
                 :telemetry [clear]                  tool-call failure/retry-recovery stats (or wipe them)\n\
                 :version                            show aish version + backend (also `aish --version`)\n\
                 :update [prod|dev|ci]               check GitHub for a newer release and upgrade;\n\
                                                     optional channel is a one-off override of AISH_UPDATE_CHANNEL\n\
                 :batch <on|off|status|clear>        interactive batch mode: agent offloads deferrable\n\
                                                     work to background Anthropic batches (Opus, ~50%\n\
                                                     cheaper); jobs persist + reattach across restarts\n\
                 :batch model <opus|sonnet|haiku|id> model background batches run on (default opus)\n\
                 :jobs                               list background jobs\n\
                 :kill <id>                          kill a background job\n\
                 :workers [all]                      list this session's background coordinators (all = every session)\n\
                 :runtime [host|podman|docker|status] show/select the worker execution runtime (host vs container); sets AISH_WORKER_RUNTIME for the next coordinator\n\
                 :dispatch-stats [all]               background-job dispatch efficiency: outcomes, latency\n\
                                                     distribution + missed-inline heuristic (alias :dstats)\n\
                 :output [on|off]             stream background coordinators' activity (💭 thinking + 🛠️/🔧 tool + 🚀 standard/🐌 batch lines) in a contained bordered pane; off (default) keeps them quiet\n\
                 :startup-digest [on|off]            show/suppress the boot-time coordinator digest (completed-result walls + salvage/reattach lines); off (default). env: AISH_STARTUP_DIGEST\n\
                 \n\
                 :result <job>                       view a finished job's full result (id or prefix)\n\
                 :dispatch <task>                    launch a background coordinator for <task> (runs quietly; :attach <id> to watch/steer)\n\
                 :tell [--any] <id> <message>        send instructions to an in-flight coordinator (folded in next round; --any steers another session's)\n\
                 :stop [--any] <id>                  stand down an in-flight coordinator — harsher than :tell (ends it; --any: cross-session)\n\
                 :attach <id>|goal                   watch a running coordinator live + steer it (typed lines go to it); `goal` watches the active :goal\n\
                 :detach                             stop watching the attached coordinator (it keeps running)\n\
                 :forget <id>|--all-exited           remove an exited worker's container + state dir + run record (refuses a running one)\n\
                 :close [id]                         remove the attached (or named) coordinator from :workers + the Shift-Tab rotation\n\
                 :skill <add|search|list|remove>     install, search, list, or remove skill.fish skills\n\
                 :rename <name>                      name the session (prefixes the prompt); auto-set to the repo name at startup, bare :rename clears\n\
                 :rewrite <intent>                   AI-rewrite intent into ONE command, prefilled to edit/accept before it runs (alias :rw)\n\
                 :suggest [hint]                     AI-suggest the next command from session context, prefilled to edit/accept before it runs (alias :sg)\n\
                 :alert <condition>                  monitor for a condition and surface it in the console when met (native file/PR/command probe,\n\
                                                     or a delegated agent for free-form conditions); distinct audible cue. :alert list, :alert clear <id|all>\n\
                 :goal <condition>                   pursue a goal in the background until met (requires :batch);\n\
                                                     a verifier judges each turn. :goal status, :goal clear\n\
                 :goal new <title>                   track a durable goal (persisted); then link/block/milestone/show/complete\n\
                 :goal link <task> [title]|block <reason>|milestone <name> [date]\n\
                                                     annotate the current goal; :goal show [id], :goal complete [id]\n\
                 :loop <count> <prompt>              re-run <prompt> as an inline agentic turn <count> times;\n\
                                                     context accumulates across iterations. :loop status, :loop stop\n\
                 :allow                              list always-allowed tools/commands + dir grants\n\
                 :allow remove <tool>|<perm>:<dir>   revoke a tool or a directory grant\n\
                 a at a prompt                       always-allow this tool (see :allow)\n\
                 d at a read/write/delete prompt     allow that permission for the whole dir, recursively\n\
                 Ctrl-O                              expand/collapse tool output (verbatim results ⇄ line-count summary)\n\
                 Shift-Tab                           cycle attach across all coordinators incl. finished/failed, newest first (interactive → newest → … → oldest → back)\n\
                 :quit                               exit (also Ctrl-D or `exit`)"
            );
        }
        Some("version" | "ver") => {
            println!(
                "\x1b[1maish\x1b[0m \x1b[2mv{}\x1b[0m — {}",
                crate::update::current_version(),
                backend.describe()
            );
        }
        Some("jobs") => {
            let jobs = session.jobs.lock().unwrap();
            if jobs.is_empty() {
                println!("no background jobs");
            }
            for line in &crate::jobs::format_listing(&jobs) {
                println!("{line}");
            }
        }
        Some("kill") => match parts.next().and_then(|s| s.parse::<usize>().ok()) {
            Some(id) => {
                let jobs = session.jobs.lock().unwrap();
                match jobs.iter().find(|j| j.id == id) {
                    Some(j) if j.kill() => println!("job {id} killed"),
                    Some(j) => println!("job {id} already finished ({})", j.status()),
                    None => println!("no such job: {id}"),
                }
            }
            None => println!("usage: :kill <job-id>"),
        },
        Some("output" | "wo") => {
            let target = match parts.next() {
                Some("on") => Some(true),
                Some("off") => Some(false),
                None => Some(!session.show_worker_output.load(Ordering::SeqCst)),
                Some(_) => None,
            };
            match target {
                Some(true) => {
                    session.show_worker_output.store(true, Ordering::SeqCst);
                    println!(
                        "worker output ON — background coordinators now stream their 💭 thinking, tool activity (🛠️ local · 🔧 MCP) and 🚀 standard/🐌 batch turn output, framed in a contained pane:"
                    );
                    // Open the contained pane: every streamed coordinator line
                    // is now rendered as a bordered row under this top frame
                    // (see worker::pane_row), so the activity reads as a
                    // bordered side-column distinct from the user's own shell
                    // scroll, not loose interleaved lines (w_sn1fHhd5).
                    println!("{}", crate::worker::pane_open());
                }
                Some(false) => {
                    session.show_worker_output.store(false, Ordering::SeqCst);
                    // Close the contained pane, then report the quiet default.
                    println!("{}", crate::worker::pane_close());
                    println!(
                        "worker output OFF — background coordinators run quietly (only the ⟳N pulse + completion notice show)"
                    );
                }
                None => println!("usage: :output [on|off]"),
            }
        }
        Some("runtime") => {
            // Report or select the WORKER execution runtime: the host subprocess
            // (default) vs a podman/docker container. Read-only display + a
            // selector that sets AISH_WORKER_RUNTIME for the NEXT coordinator
            // launched from this session — it does NOT retroactively move
            // already-running workers (they keep the vehicle they started with).
            let podman_avail =
                crate::container::runtime_on_path(crate::container::Runtime::Podman);
            let docker_avail =
                crate::container::runtime_on_path(crate::container::Runtime::Docker);
            let yn = |b: bool| {
                if b {
                    "\x1b[32m✓ on PATH\x1b[0m"
                } else {
                    "\x1b[2m✗ not found\x1b[0m"
                }
            };
            let show_status = || {
                let raw = std::env::var("AISH_WORKER_RUNTIME").ok();
                let current = match crate::container::Runtime::parse_selector(raw.as_deref()) {
                    crate::container::SelectorPref::Force(crate::container::Runtime::Podman) => {
                        "Podman (container)"
                    }
                    crate::container::SelectorPref::Force(crate::container::Runtime::Docker) => {
                        "Docker (container)"
                    }
                    crate::container::SelectorPref::Host => "Host (no container)",
                    crate::container::SelectorPref::Auto => {
                        "Host (auto — report-only until cutover)"
                    }
                };
                let raw_disp = raw.as_deref().unwrap_or("unset → auto");
                println!("\x1b[1mworker runtime\x1b[0m");
                println!("  current:   {current}   \x1b[2m(AISH_WORKER_RUNTIME={raw_disp})\x1b[0m");
                println!(
                    "  available: podman {}   docker {}",
                    yn(podman_avail),
                    yn(docker_avail)
                );
                println!(
                    "  \x1b[2mset for the next coordinator:  :runtime host | :runtime podman | :runtime docker\x1b[0m"
                );
                println!(
                    "  \x1b[2mpersist across sessions:       export AISH_WORKER_RUNTIME=<host|podman|docker>\x1b[0m"
                );
            };
            match parts.next() {
                None | Some("status") => show_status(),
                Some("host") | Some("none") => {
                    unsafe { std::env::set_var("AISH_WORKER_RUNTIME", "host") };
                    println!(
                        "\x1b[36m⇄\x1b[0m worker runtime forced to \x1b[1mHost\x1b[0m for the next coordinator."
                    );
                    show_status();
                }
                Some("podman") => {
                    unsafe { std::env::set_var("AISH_WORKER_RUNTIME", "podman") };
                    if !podman_avail {
                        println!(
                            "\x1b[33m!\x1b[0m podman not on PATH — workers fall back to the host path until it's installed."
                        );
                    }
                    println!(
                        "\x1b[36m⇄\x1b[0m worker runtime set to \x1b[1mPodman\x1b[0m for the next coordinator."
                    );
                    show_status();
                }
                Some("docker") => {
                    unsafe { std::env::set_var("AISH_WORKER_RUNTIME", "docker") };
                    if !docker_avail {
                        println!(
                            "\x1b[33m!\x1b[0m docker not on PATH — workers fall back to the host path until it's installed."
                        );
                    }
                    println!(
                        "\x1b[36m⇄\x1b[0m worker runtime set to \x1b[1mDocker\x1b[0m for the next coordinator."
                    );
                    show_status();
                }
                Some(other) => {
                    println!("usage: :runtime [host|podman|docker|status]  (got '{other}')")
                }
            }
        }

        Some("workers") => {
            // `:workers` lists THIS session's coordinators by default — the ones
            // launched from this terminal — so a busy multi-terminal host never
            // shows another terminal's background workers here. `:workers all`
            // widens to every session's durable runs (the old behavior).
            let show_all = matches!(parts.next(), Some("all"));
            // Collapse a (possibly multi-line) task to one clipped line.
            let one_line = |t: &str| {
                let s = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let s = if s.chars().count() > 70 {
                    format!("{}…", s.chars().take(70).collect::<String>())
                } else {
                    s
                };
                s.replace('|', "\\|")
            };
            // This session's label; in-memory workers are always "yours".
            let me_label = session
                .name
                .clone()
                .unwrap_or_else(|| crate::batch::short_id(&session.session_id).to_string());

            let mut table = String::from(WORKERS_TABLE_HEADER);
            // Accumulate rendered rows keyed by their start epoch so the whole
            // listing — in-memory + durable, both sources interleaved — is
            // sorted newest-first before it's emitted. Without this the two
            // sources each appear in their own insertion order (oldest-first),
            // which reads as an arbitrary jumble to the operator.
            let mut rows_out: Vec<(i64, String)> = Vec::new();
            // Epoch-seconds "now", computed once so every row's Started/Runtime
            // cell is reconciled against the same instant.
            let now_epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut any = false;
            let mut seen = std::collections::HashSet::new();
            // In-memory coordinators launched by THIS session (live status).
            // Sort newest-first by start time.
            let mut in_mem: Vec<_> = session.worker_jobs.lock().unwrap().iter().cloned().collect();
            in_mem.sort_by(|a, b| b.started_epoch().cmp(&a.started_epoch()));
            for w in in_mem.iter() {
                any = true;
                seen.insert(w.id.clone());
                let (started_cell, runtime_cell) = crate::style::time_cells(
                    w.started_epoch(),
                    w.finished_epoch(),
                    now_epoch,
                );
                // A resumed worker (≥1 in-place resume) carries a `↻N` thread
                // marker on its id so the operator can see it's the SAME worker
                // continued as a new thread, not a fresh spawn.
                let id_cell = if w.thread_count() > 1 {
                    format!("{} ↻{}", w.id, w.thread_count())
                } else {
                    w.id.clone()
                };
                rows_out.push((
                    w.started_epoch().unwrap_or(0),
                    format!(
                        "| {} {} | {} * | {} | {} | {} | {} | {} |\n",
                        crate::style::job_type_emoji("worker"),
                        id_cell,
                        me_label,
                        crate::style::styled_status(&w.status()),
                        started_cell,
                        runtime_cell,
                        one_line(&w.task),
                        crate::style::styled_result(&w.result_cell())
                    ),
                ));
            }
            // Durable runs from the shared store — every session's, so workers
            // started elsewhere (or in a prior process) show up too. Skip ids
            // already listed in-memory to avoid double-counting this session's.
            if let Some(store) = &session.coordinator_store {
                // Live orphan reap before listing: flip stale, unowned,
                // non-terminal rows to `failed` so zombie "coordinating" workers
                // (owner process gone before its detached sub-coordinator tasks
                // reconciled) self-heal in a long-lived session, not just at
                // startup. Shares the startup reaper's predicate.
                crate::coordinator::reap_orphaned_runs(store, session.session_id.as_str());
                if let Ok(rows) = store.load_all() {
                    // Sort newest-first by creation time.
                    let mut durable: Vec<&_> = rows.iter().filter(|r| {
                        !seen.contains(&r.run_id)
                            && (show_all
                                || r.session_id.as_deref() == Some(session.session_id.as_str()))
                    }).collect();
                    durable.sort_by(|a, b| {
                        let a_ts = a.created_at.as_deref().and_then(crate::style::parse_sqlite_utc).unwrap_or(0);
                        let b_ts = b.created_at.as_deref().and_then(crate::style::parse_sqlite_utc).unwrap_or(0);
                        b_ts.cmp(&a_ts)
                    });
                    for r in durable {
                        any = true;
                        let is_me = r.session_id.as_deref() == Some(session.session_id.as_str());
                        let label = r
                            .session_name
                            .clone()
                            .or_else(|| {
                                r.session_id
                                    .as_deref()
                                    .map(|s| crate::batch::short_id(s).to_string())
                            })
                            .unwrap_or_else(|| "—".into());
                        let session_cell = if is_me { format!("{label} *") } else { label };
                        let result_cell = match (r.result.as_deref(), r.error.as_deref()) {
                            (Some(res), None) => {
                                if let Some(pr) =
                                    res.split_whitespace().find(|s| s.starts_with('#'))
                                {
                                    format!("✓ {pr}")
                                } else {
                                    "✓ success".to_string()
                                }
                            }
                            (None, Some(e)) => {
                                let t = if e.len() > 40 {
                                    format!("{}…", &e[..40])
                                } else {
                                    e.to_string()
                                };
                                format!("✗ {t}")
                            }
                            _ => "—".to_string(),
                        };
                        // Start from created_at; freeze the runtime at heartbeat_at
                        // once terminal (set_done/set_failed bump the heartbeat),
                        // else let it tick against now.
                        let started = r
                            .created_at
                            .as_deref()
                            .and_then(crate::style::parse_sqlite_utc);
                        let terminal = matches!(r.phase.as_str(), "done" | "failed");
                        let finished = if terminal {
                            r.heartbeat_at
                                .as_deref()
                                .and_then(crate::style::parse_sqlite_utc)
                        } else {
                            None
                        };
                        let (started_cell, runtime_cell) =
                            crate::style::time_cells(started, finished, now_epoch);
                        rows_out.push((
                            started.unwrap_or(0),
                            format!(
                                "| {} {} | {} | {} | {} | {} | {} | {} |\n",
                                crate::style::job_type_emoji("coordinator"),
                                crate::batch::short_id(&r.run_id),
                                session_cell,
                                crate::style::styled_status(&r.phase),
                                started_cell,
                                runtime_cell,
                                one_line(&r.task),
                                crate::style::styled_result(&result_cell)
                            ),
                        ));
                    }
                }
            }
            // Sort the merged listing oldest-first (smallest start epoch first),
            // then emit. A stable sort keeps same-epoch rows in insertion order.
            rows_out.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, row) in &rows_out {
                table.push_str(row);
            }
            if any {
                println!("{}", crate::md::render_stdout(table.trim()));
                if show_all {
                    println!("{}", crate::style::dim("* = launched from this session"));
                } else {
                    println!(
                        "{}",
                        crate::style::dim(
                            "this session's coordinators — :workers all for every session"
                        )
                    );
                }
            } else if show_all {
                println!("no background workers");
            } else {
                println!(
                    "no background workers in this session — :workers all to see every session"
                );
            }
        }
        Some("dispatch-stats" | "dstats") => {
            // Dispatch-efficiency report: reads the durable coordinator store and
            // shows how offloading is paying off — outcomes (done/failed/running),
            // latency distribution, and a missed-inline heuristic. Scoped to THIS
            // session by default; `all` widens to every session's runs.
            let show_all = matches!(parts.next(), Some("all"));
            let scope = if show_all { "all sessions" } else { "this session" };
            match &session.coordinator_store {
                Some(store) => match store.load_all() {
                    Ok(rows) => {
                        let refs: Vec<&crate::db::CoordinatorRow> = rows
                            .iter()
                            .filter(|r| {
                                show_all
                                    || r.session_id.as_deref()
                                        == Some(session.session_id.as_str())
                            })
                            .collect();
                        let st = crate::dispatch_stats::summarize(&refs);
                        for line in crate::dispatch_stats::render(&st, scope) {
                            println!("{line}");
                        }
                        if !show_all {
                            println!(
                                "{}",
                                crate::style::dim(
                                    "this session only — :dispatch-stats all for every session"
                                )
                            );
                        }
                    }
                    Err(e) => println!("dispatch stats unavailable: {e}"),
                },
                None => println!("dispatch stats unavailable: no coordinator store"),
            }
        }
        Some("result") => {
            // View a finished background job's full result on demand.
            let Some(&id) = parts.next().as_ref() else {
                println!(
                    "usage: :result <job>   (id or prefix from a completion notice / :results)"
                );
                return false;
            };
            let hit = |jid: &str| jid == id || jid.starts_with(id);
            let found = session
                .worker_jobs
                .lock()
                .unwrap()
                .iter()
                .find(|j| hit(&j.id))
                .map(|j| j.fetch())
                .or_else(|| {
                    session
                        .batch_jobs
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|j| hit(&j.id))
                        .map(|j| j.fetch())
                });
            match found {
                Some(r) => println!("{}", crate::md::render_stdout(r.trim())),
                None => {
                    // Store fallback: a completed background coordinator from a
                    // prior session (retained in the durable store because the
                    // startup digest is suppressed) has no in-memory job here, so
                    // read its full result/error body straight from the store.
                    let store_hit = session.coordinator_store.as_ref().and_then(|s| {
                        s.load_all().ok().and_then(|rows| {
                            rows.into_iter().find(|r| hit(&r.run_id)).map(|r| {
                                r.result
                                    .clone()
                                    .filter(|s| !s.trim().is_empty())
                                    .or_else(|| r.error.clone())
                                    .unwrap_or_else(|| {
                                        format!(
                                            "coordinator {} — phase {}, no result yet",
                                            crate::batch::short_id(&r.run_id),
                                            r.phase
                                        )
                                    })
                            })
                        })
                    });
                    match store_hit {
                        Some(body) => println!("{}", crate::md::render_stdout(body.trim())),
                        None => println!("no background job matching '{id}' (see :workers)"),
                    }
                }
            }
        }
        Some("results") => {
            // List background jobs (workers + batches) so the user can :result one.
            let mut table = String::from("| Job | Status | Task |\n|---|---|---|\n");
            let mut any = false;
            for j in session.worker_jobs.lock().unwrap().iter() {
                any = true;
                table.push_str(&format!(
                    "| {} | {} | {} |\n",
                    j.id,
                    j.status(),
                    crate::batch::one_line(&j.task).replace('|', "\\|")
                ));
            }
            for j in session.batch_jobs.lock().unwrap().iter() {
                any = true;
                table.push_str(&format!(
                    "| {} | {} | {} |\n",
                    crate::batch::short_id(&j.id),
                    j.status(),
                    crate::batch::one_line(&j.task).replace('|', "\\|")
                ));
            }
            if any {
                println!("{}", crate::md::render_stdout(table.trim()));
                println!("\x1b[2m:result <job> to view a result\x1b[0m");
            } else {
                println!("no background jobs");
            }
        }
        Some("dispatch") => {
            // Launch a background coordinator directly, without a model turn —
            // the deterministic equivalent of the model calling run_in_background.
            // Shares dispatch_coordinator with the "troubleshoot" auto-offload.
            let task = parts.collect::<Vec<_>>().join(" ");
            // Explicit operator launch — not an agent escalation.
            dispatch_background(&task, session, false);
        }
        Some("attach") => attach_worker(parts.next(), session),
        Some("detach") => detach_worker(session),
        Some("forget") => {
            let toks: Vec<&str> = parts.collect();
            match parse_forget_cmd(&toks) {
                ForgetCmd::Usage => println!(
                    "usage: :forget <id>            remove an EXITED worker's container + state dir + record\n       :forget --all-exited    remove every exited worker in this session"
                ),
                ForgetCmd::AllExited => forget_all_exited(session),
                ForgetCmd::One(id) => forget_worker(&id, session),
            }
        }
        Some("close") => close_worker(parts.next(), session),
        Some("tell" | "msg" | "send") => {
            // Steer an in-flight coordinator: queue an operator message that the
            // running coordinator folds into its next round (see coordinator::drive).
            // A leading `--any`/`-a` opts into steering another session's
            // coordinator (AC7 cross-session ownership override).
            let toks: Vec<&str> = parts.collect();
            let (any, rest) = take_any_flag(&toks);
            let target = rest.first().copied();
            let message = rest.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
            tell_coordinator(target, message.trim(), any, session);
        }
        Some("stop" | "standdown" | "stand-down") => {
            // Stand down an in-flight coordinator — the harsh sibling of :tell.
            // Raises the durable stand-down flag AND interrupts the running turn,
            // so the coordinator takes one final graceful wrap-up turn and exits.
            // A leading `--any`/`-a` opts into standing down another session's
            // coordinator (same cross-session ownership override as :tell).
            let toks: Vec<&str> = parts.collect();
            let (any, rest) = take_any_flag(&toks);
            stop_coordinator(rest.first().copied(), any, session);
        }
        Some("rename") => {
            let rest = parts.collect::<Vec<_>>().join(" ");
            let rest = rest.trim();
            if rest.is_empty() {
                if session.name.take().is_some() {
                    println!("session name cleared");
                } else {
                    println!("usage: :rename <name>   (bare :rename clears it)");
                }
            } else {
                session.name = Some(rest.to_string());
                println!("session named \x1b[1;35m[{rest}]\x1b[0m");
            }
        }
        Some("alert") => {
            let rest = parts.collect::<Vec<_>>().join(" ");
            let rest = rest.trim().to_string();
            let lower = rest.to_lowercase();
            match session.alert_store.clone() {
                None => println!(
                    "alert store unavailable — `:alert` needs the shared aish.db (check startup warnings)"
                ),
                // Bare `:alert` / `:alert list [all]` — show armed (and, with
                // `all`, done/cancelled) monitors.
                Some(store) if rest.is_empty() || lower == "list" || lower == "list all" || lower == "ls" => {
                    let include_all = lower == "list all";
                    match store.list_alerts(include_all) {
                        Ok(rows) if rows.is_empty() => println!(
                            "no alerts — `:alert <condition>` to arm one (e.g. `:alert let me know when PR 333 is merged`)"
                        ),
                        Ok(rows) => {
                            println!("\x1b[1m ID  STATE      KIND      CONDITION\x1b[0m");
                            for (id, state, kind, condition, _short) in rows {
                                println!("{id:>3}  {state:<9} {kind:<8}  {condition}");
                            }
                            println!("\x1b[2m:alert clear <id|all> to cancel\x1b[0m");
                        }
                        Err(e) => println!("alert list failed: {e:#}"),
                    }
                }
                // `:alert clear|cancel [<id>|all]`.
                Some(store)
                    if lower.starts_with("clear") || lower.starts_with("cancel") =>
                {
                    let arg = lower
                        .strip_prefix("clear")
                        .or_else(|| lower.strip_prefix("cancel"))
                        .unwrap_or("")
                        .trim();
                    if arg.is_empty() || arg == "all" {
                        match store.cancel_all_alerts() {
                            Ok(0) => println!("no armed alerts to cancel"),
                            Ok(n) => println!("cancelled {n} alert(s)"),
                            Err(e) => println!("cancel failed: {e:#}"),
                        }
                    } else if let Ok(id) = arg.parse::<i64>() {
                        match store.cancel_alert(id) {
                            Ok(true) => println!("alert {id} cancelled"),
                            Ok(false) => println!("no armed alert #{id}"),
                            Err(e) => println!("cancel failed: {e:#}"),
                        }
                    } else {
                        println!("usage: :alert clear [<id>|all]");
                    }
                }
                // `:alert <condition>` — arm a monitor. A trailing `--silent`
                // (or `--no-bell`) opts out of the audible cue.
                Some(store) => {
                    let (audible, cond) = if let Some(c) = rest.strip_suffix("--silent") {
                        (false, c.trim())
                    } else if let Some(c) = rest.strip_suffix("--no-bell") {
                        (false, c.trim())
                    } else {
                        (true, rest.as_str())
                    };
                    if cond.is_empty() {
                        println!("usage: :alert <condition>   e.g. :alert notify me when /tmp/build.done appears");
                    } else {
                        let kind = crate::alert::parse_condition(cond);
                        match store.insert_alert(cond, &kind, audible, Some(&session.session_id)) {
                            Ok(id) => match &kind {
                                crate::alert::AlertKind::Semantic { description } => {
                                    let dispatched =
                                        spawn_alert_coordinator(session, id, description);
                                    if dispatched {
                                        println!(
                                            "\x1b[1;33m⚠ alert #{id} armed\x1b[0m (semantic) — a background agent is watching; I'll surface it here the moment it's met."
                                        );
                                    } else {
                                        println!(
                                            "\x1b[1;33m⚠ alert #{id} armed\x1b[0m (semantic) — but no coordinator could be dispatched (credential/exe?); ask an agent to `set_alert {id}` when met, or `:alert clear {id}`."
                                        );
                                    }
                                }
                                _ => println!(
                                    "\x1b[1;33m⚠ alert #{id} armed\x1b[0m ({}) — monitoring token-free; I'll surface it here when it fires. `:alert list` / `:alert clear {id}`.",
                                    kind.tag()
                                ),
                            },
                            Err(e) => println!("couldn't arm alert: {e:#}"),
                        }
                    }
                }
            }
        }

        Some("goal") => {
            let rest = parts.collect::<Vec<_>>().join(" ");
            let rest = rest.trim();
            let mut goal_it = rest.splitn(2, char::is_whitespace);
            let goal_head = goal_it.next().unwrap_or("");
            let goal_tail = goal_it.next().unwrap_or("").trim();
            match goal_head {
                // Persisted-goal subcommands (TASK-278) — CRUD on the durable
                // `Goal` records. Unrecognized heads fall through to the
                // back-compat batch pursuit loop below.
                "new" | "show" | "status" | "link" | "block" | "unblock" | "milestone"
                | "complete" => println!("{}", goal_command(session, goal_head, goal_tail)),
                // Bare `:goal`: the live loop's status if one is running, else a
                // persisted-goal overview (which prints its own empty-state hint).
                "" => match &session.goal {
                    Some(g) => println!("{}", g.status_line()),
                    None => println!("{}", goal_command(session, "status", "")),
                },
                "clear" | "stop" | "off" | "reset" | "none" | "cancel" => match session.goal.take()
                {
                    Some(g) => {
                        g.clear();
                        println!("goal loop cleared");
                    }
                    None => println!("no goal loop running"),
                },
                _ => {
                    let cond = rest;
                    if !session.batch_mode {
                        println!(
                            "`:goal` runs as background batch work — enable it first with `:batch on`"
                        );
                    } else if session.goal.as_ref().is_some_and(|g| g.is_active()) {
                        println!(
                            "a goal is already active (one per session) — `:goal clear` it first"
                        );
                    } else {
                        match (
                            crate::backend::claude::Credential::resolve(&session.env),
                            std::env::current_exe(),
                        ) {
                            (Ok(cred), Ok(exe)) => {
                                let spec = crate::worker::WorkerSpec {
                                    exe,
                                    cwd: session.cwd.clone(),
                                    // The generator runs on the active backend (parity).
                                    backend: session.backend_kind.clone(),
                                    model: crate::worker::coordinator_model(
                                        &session.backend_kind,
                                        &session.batch_model,
                                    ),
                                    env: session.env.clone(),
                                    // The goal loop iterates in the live cwd (no worktree).
                                    isolate: false,
                                    base: "main".to_string(),
                                    launch_session_id: session.session_id.clone(),
                                    launch_session_name: session.name.clone(),
                                    show_output: session.show_worker_output.clone(),
                                    attached: session.attached.clone(),
                                    coordinator_store: session.coordinator_store.clone(),
                                };
                                // KNOWN LIMITATION: the verifier still judges on
                                // Claude (batch_model + the Claude credential
                                // resolved above) — xAI has no judge here, so a
                                // Grok `:goal` runs its GENERATOR on Grok but
                                // requires a Claude credential to judge each turn.
                                let g = crate::goal::spawn(
                                    cond.to_string(),
                                    spec,
                                    session.batch_model.clone(),
                                    cred,
                                );
                                session.goal = Some(g);
                                println!(
                                    "\x1b[2mgoal set — pursuing it in the background; progress and the result appear here. `:goal` for status, `:goal clear` to stop.\x1b[0m"
                                );
                            }
                            (Err(_), _) => {
                                println!(
                                    "no Claude credential — set CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY"
                                )
                            }
                            (_, Err(e)) => {
                                println!("can't locate the aish binary to run goal workers: {e}")
                            }
                        }
                    }
                }
            }
        }
        Some("model") => match parts.next() {
            Some(m) => {
                let id = match m {
                    "opus" => "claude-opus-4-8",
                    "sonnet" => "claude-sonnet-4-6",
                    "haiku" => "claude-haiku-4-5",
                    other => other,
                };
                backend.set_model(id.to_string());
                println!("model → {}", backend.describe());
            }
            None => println!("{}", backend.describe()),
        },
        Some("model-detect") => {
            // Re-run hardware detection, pick + persist the best-fitting local
            // model, and export the AISH_LOCAL_* hints. An operator pin (env /
            // --model) wins and is simply reported. If we're already on the
            // local backend, rebuild it so the new selection takes effect.
            match crate::hwdetect::ensure_selected(true, None) {
                Ok(sel) => {
                    crate::hwdetect::apply_env(&sel);
                    println!("{}", crate::hwdetect::report(&sel));
                    if is_local_backend(backend) {
                        match rebuild_local_backend(backend, session) {
                            Ok(()) => println!("backend → {}", backend.describe()),
                            Err(e) => println!("can't reload local backend: {e:#}"),
                        }
                    }
                }
                Err(e) => eprintln!("\x1b[31maish:\x1b[0m local model detection failed: {e:#}"),
            }
        }
        Some("backend") => match parts.next() {
            Some("claude") => {
                if matches!(backend, Backend::Claude(_)) {
                    println!("already on {}", backend.describe());
                } else {
                    match crate::backend::claude::Credential::resolve(&session.env)
                        .and_then(|cred| Backend::new_claude("claude-opus-4-8".into(), cred))
                    {
                        Ok(b) => {
                            *backend = b;
                            session.backend_kind = backend.kind().to_string();
                            println!("backend → {}", backend.describe());
                        }
                        Err(e) => println!("can't switch: {e:#}"),
                    }
                }
            }
            Some("grok") => {
                if matches!(backend, Backend::Grok(_)) {
                    println!("already on {}", backend.describe());
                } else {
                    match Backend::new_grok(
                        crate::backend::grok::DEFAULT_MODEL.into(),
                        &session.env,
                    ) {
                        Ok(b) => {
                            *backend = b;
                            session.backend_kind = backend.kind().to_string();
                            println!("backend → {}", backend.describe());
                        }
                        Err(e) => println!("can't switch: {e:#}"),
                    }
                }
            }
            #[cfg(feature = "local")]
            Some("local") => {
                if matches!(backend, Backend::Local(_)) {
                    println!("already on {}", backend.describe());
                } else {
                    // First switch to local on a machine that's never been
                    // profiled? Detect hardware and pick the best-fitting local
                    // model (operator env pins win; never overridden).
                    match crate::hwdetect::ensure_selected(false, None) {
                        Ok(sel) => {
                            crate::hwdetect::apply_env(&sel);
                            println!("{}", crate::hwdetect::short_line(&sel));
                        }
                        Err(e) => eprintln!(
                            "\x1b[33maish:\x1b[0m local model detection failed: {e:#}"
                        ),
                    }
                    match Backend::new_local() {
                        Ok(b) => {
                            *backend = b;
                            session.backend_kind = backend.kind().to_string();
                            println!("backend → {}", backend.describe());
                        }
                        Err(e) => println!("can't switch: {e:#}"),
                    }
                }
            }
            #[cfg(not(feature = "local"))]
            Some("local") => {
                println!("built without the local feature (cargo build --features local)")
            }
            _ => println!("usage: :backend <claude|grok|local>"),
        },
        Some("mode") => match parts.next().and_then(crate::session::Mode::parse) {
            Some(m) => {
                session.mode = m;
                println!("mode → {}", describe_mode(m));
            }
            None => println!(
                "mode: {}\nusage: :mode <paranoid|careful|normal|yolo>",
                describe_mode(session.mode)
            ),
        },
        Some("yolo") => {
            // legacy toggle: yolo ⇄ normal
            session.mode = if session.mode == crate::session::Mode::Yolo {
                crate::session::Mode::Normal
            } else {
                crate::session::Mode::Yolo
            };
            println!("mode → {}", describe_mode(session.mode));
        }
        Some("new") => {
            session.reset_conversation();
            println!("history cleared");
        }
        Some("allow") => handle_allow(parts.next(), parts.next(), session),
        Some("batch") => handle_batch(parts.next(), parts.next(), session),
        Some("startup-digest") => handle_startup_digest(parts.next(), session),
        Some("update") => {
            // handle_update returns true when the user opted to auto-restart onto
            // the freshly-installed binary — propagate it as the REPL break signal.
            if handle_update(pending_update, session, parts.next()).await {
                return true;
            }
        }
        Some("restart") => {
            // Re-exec the current aish with the same argv it was launched with.
            // Sets the flag the REPL loop consumes after exit cleanup, then breaks.
            session.restart_requested = true;
            return true;
        }
        Some("mcp") => handle_mcp(parts.collect(), session).await,
        Some("skill" | "skills") => {
            let rest: Vec<&str> = parts.collect();
            if let Err(e) = handle_skill_command(&rest, session).await {
                eprintln!("\x1b[31maish:\x1b[0m {e:#}");
            }
        }
        Some("context") => handle_context(backend, session),
        Some("reasoning") => handle_reasoning(),
        Some("compact") => handle_compact(backend, session),
        Some("memories" | "memory") => handle_memories(parts.next(), session),
        Some("hooks") => handle_hooks(parts.next(), session),
        Some("plugin" | "plugins") => handle_plugin(parts.collect()),
        Some("telemetry" | "tool-stats") => handle_telemetry(parts.next(), session),
        Some(other) => println!("unknown command :{other} — try :help"),
        None => {}
    }
    false
}

/// `:update` checks GitHub for a newer release and, with confirmation, installs
/// it via the `gh` CLI. A pending update discovered at startup is used directly;
/// otherwise a fresh check runs on demand. Yolo mode skips the confirm.
/// Combined count of background work still tied to THIS aish binary: in-memory
/// Anthropic batches + re-exec'd workers (`run_in_background` / `:dispatch`),
/// plus durable coordinator runs the in-memory tallies miss (goal-loop turns,
/// runs launched from another session, runs reattached after a restart). This
/// is the exact tally the prompt's ⟳N badge shows; `:update` reuses it to gate
/// the version-skew warning (ISS-2045). Batches are counted too — they're
/// durable + reattach, so they aren't a *data-loss* risk, but their local
/// polling task still runs in this (about-to-be-stale) parent.
fn background_running_count(session: &Session) -> usize {
    let batches = crate::batch::running_count(&session.batch_jobs);
    let workers = crate::worker::running_count(&session.worker_jobs);
    let coordinators = session.coordinator_store.as_ref().map_or(0, |store| {
        let in_memory: std::collections::HashSet<String> = session
            .worker_jobs
            .lock()
            .unwrap()
            .iter()
            .map(|w| w.id.clone())
            .collect();
        crate::coordinator::active_store_count(store, &in_memory)
    });
    combined_running_count(batches, workers, coordinators)
}

/// Sum the three background-job tallies (batches, workers, durable coordinators)
/// into the combined "still on the current binary" count the `:update` skew gate
/// checks. Pure over the tallies so it's unit-testable (ISS-2045 AC6).
fn combined_running_count(batches: usize, workers: usize, coordinators: usize) -> usize {
    batches + workers + coordinators
}

/// Build the `:update` confirmation prompt. With background jobs still on the
/// current binary (`running > 0`) it spells out the version-skew consequence and
/// names the count so the user makes an informed choice before the atomic binary
/// swap (ISS-2045); with none it's the plain install confirm (behavior
/// unchanged). Pure over the count + version for unit-testing.
fn update_confirm_prompt(running: usize, version: &str) -> String {
    if running == 0 {
        format!("download and install aish {version}?")
    } else {
        let plural = if running == 1 { "" } else { "s" };
        format!(
            "\u{26a0} {running} background job{plural} (workers/batches/coordinators) still on the current binary. Updating now leaves them on the OLD version while new work spawns on the NEW one (version skew). Download and install aish {version} anyway?"
        )
    }
}

/// Describe a channel in one short phrase for `:update` status lines.
fn channel_blurb(ch: crate::update::Channel) -> &'static str {
    match ch {
        crate::update::Channel::Prod => "stable v{semver} releases marked latest",
        crate::update::Channel::Dev => "nightly dev-v{next}-dev.{n} pre-releases",
        crate::update::Channel::Ci => "per-main-push ci-{run}-{sha} pre-releases",
    }
}

/// `:update [prod|dev|ci]` — check GitHub for a newer release and, with
/// confirmation, install it via the `gh` CLI.
///
/// With no argument it tracks the channel from `AISH_UPDATE_CHANNEL` (default
/// `prod`). An optional channel argument is a ONE-OFF override for this single
/// invocation — e.g. `:update dev` pulls the latest nightly without exporting
/// the env var — so it doubles as the channel selector (there's no separate
/// `:channel` command). A pending update discovered at startup is reused only
/// when it matches the requested channel; otherwise a fresh check runs. Yolo
/// mode skips the confirm.
async fn handle_update(
    pending_update: &mut Option<crate::update::UpdateInfo>,
    session: &mut Session,
    chan_arg: Option<&str>,
) -> bool {
    if !crate::update::gh_available() {
        println!("update needs the GitHub CLI (`gh`) on PATH — see https://cli.github.com");
        return false;
    }
    // Resolve the requested channel: an explicit arg overrides the env default
    // for this invocation only. An unrecognised arg aborts loudly rather than
    // silently falling back to prod.
    let requested = match chan_arg {
        None => None,
        Some(s) => match crate::update::parse_channel(s) {
            Some(c) => Some(c),
            None => {
                println!(
                    "unknown channel '{s}' — valid channels: \x1b[36mprod\x1b[0m, \x1b[36mdev\x1b[0m, \x1b[36mci\x1b[0m"
                );
                return false;
            }
        },
    };
    let active = requested.unwrap_or_else(crate::update::channel);
    // The startup-cached pending update was checked against the env channel, so
    // it only applies when no explicit channel was asked for (or it matches).
    let cache_ok = requested.is_none() || requested == Some(crate::update::channel());
    let cached = if cache_ok { pending_update.take() } else { None };
    let info = match cached {
        Some(info) => info,
        None => {
            println!(
                "\x1b[2mchecking \x1b[0m\x1b[36m{}\x1b[0m\x1b[2m channel ({}) for updates…\x1b[0m",
                active.as_str(),
                channel_blurb(active)
            );
            match crate::update::check_channel(active).await {
                Ok(Some(info)) => info,
                Ok(None) => {
                    println!(
                        "aish is up to date ({}) on the {} channel.",
                        crate::update::current_version(),
                        active.as_str()
                    );
                    return false;
                }
                Err(e) => {
                    println!("update check failed: {e:#}");
                    return false;
                }
            }
        }
    };
    println!(
        "aish {} is available (you have {}).",
        info.version,
        crate::update::current_version()
    );
    // Count background work still on the current binary BEFORE the swap. When any
    // is in flight, an atomic `perform` would leave a mixed-version fleet — the
    // parent + already-spawned children on the old inode, anything spawned after
    // on the new one — so we surface the skew and require an explicit confirm
    // (ISS-2045). With nothing running, the prompt is the unchanged single
    // confirm. Yolo mode proceeds without asking either way.
    let running = background_running_count(session);
    let go = session.mode == crate::session::Mode::Yolo
        || matches!(
            confirm_tty(&update_confirm_prompt(running, &info.version)),
            tools::Decision::AllowOnce | tools::Decision::AlwaysAllow | tools::Decision::AllowDir
        );
    if !go {
        println!("update cancelled — run :update when you're ready.");
        *pending_update = Some(info); // keep it so the next :update needn't re-check
        return false;
    }
    if let Err(e) = crate::update::perform(&info).await {
        println!("\x1b[31mupdate failed:\x1b[0m {e:#}");
        *pending_update = Some(info);
        return false;
    }
    // The swap took effect on disk but in-flight jobs keep running the OLD inode;
    // the new binary applies on next launch. Remind the user when skew is live.
    if running > 0 {
        let plural = if running == 1 { "" } else { "s" };
        println!(
            "\x1b[33mnote:\x1b[0m {running} background job{plural} still on the old binary — the new version applies to work started after the next restart."
        );
    }
    // Auto-restart onto the freshly-installed binary. Only offered when it's safe:
    // no background workers on the old inode (a restart re-execs the parent and
    // would strand them) — colon commands are handled between turns, so we're
    // never mid-turn here. Ask first (yolo skips the prompt), then signal the
    // REPL loop to re-exec via `session.restart_requested`.
    if running == 0 {
        let restart = session.mode == crate::session::Mode::Yolo
            || matches!(
                confirm_tty("restart aish now to load the new version?"),
                tools::Decision::AllowOnce
                    | tools::Decision::AlwaysAllow
                    | tools::Decision::AllowDir
            );
        if restart {
            session.restart_requested = true;
            return true;
        }
        println!(
            "\x1b[2mupdate installed — run \x1b[0m\x1b[36m:restart\x1b[0m\x1b[2m (or relaunch) to load it.\x1b[0m"
        );
    }
    false
}

/// `:mcp` manages MCP servers (like Claude Code's `/mcp`): list, reconnect, add,
/// remove, and inspect tools. Adds/removes persist to ~/.aish/.mcp.json.
async fn handle_mcp(args: Vec<&str>, session: &mut Session) {
    match args.split_first() {
        None | Some((&"list", _)) | Some((&"status", _)) => {
            let servers = session.mcp.status();
            if servers.is_empty() {
                println!("no MCP servers connected");
                return;
            }
            for s in servers {
                println!(
                    "  {} [{}] {} — {} tool{}, {} skill{}",
                    s.name,
                    s.kind,
                    s.detail,
                    s.tools,
                    if s.tools == 1 { "" } else { "s" },
                    s.prompts,
                    if s.prompts == 1 { "" } else { "s" },
                );
            }
        }
        Some((&"reconnect", rest)) => {
            if rest.is_empty() || (rest.len() == 1 && rest[0] == "all") {
                let results = session.mcp.reconnect_all().await;
                if results.is_empty() {
                    println!("no MCP servers to reconnect");
                }
                for (name, res) in results {
                    match res {
                        Ok(()) => println!("reconnected {name}"),
                        Err(e) => println!("reconnect {name}: {e:#}"),
                    }
                }
            } else {
                for name in rest {
                    match session.mcp.reconnect(name).await {
                        Ok(()) => println!("reconnected {name}"),
                        Err(e) => println!("reconnect {name}: {e:#}"),
                    }
                }
            }
        }
        Some((&"reload", _)) => {
            // Re-scan .mcp.json and connect anything newly added there, without a
            // restart. New servers' tools join the model's tool set on the next
            // turn; their MCP-published skills appear in the system prompt only
            // after a restart.
            let added = session.mcp.reload().await;
            if added.is_empty() {
                println!("no new MCP servers in .mcp.json (all already connected)");
            } else {
                println!(
                    "connected {} new server{}: {}",
                    added.len(),
                    if added.len() == 1 { "" } else { "s" },
                    added.join(", ")
                );
            }
        }
        Some((&"add", rest)) => {
            let Some((&name, tail)) = rest.split_first() else {
                println!("usage: :mcp add <name> <command|url> [args…]");
                return;
            };
            let Some((&first, extra)) = tail.split_first() else {
                println!("usage: :mcp add <name> <command|url> [args…]");
                return;
            };
            let spec = if first.starts_with("http://") || first.starts_with("https://") {
                serde_json::json!({"type": "http", "url": first})
            } else {
                serde_json::json!({"command": first, "args": extra})
            };
            match session.mcp.add(name, spec, true).await {
                Ok(()) => println!("connected and saved {name}"),
                Err(e) => println!("mcp add {name}: {e:#}"),
            }
        }
        Some((&"remove" | &"rm", rest)) => {
            let Some(&name) = rest.first() else {
                println!("usage: :mcp remove <name>");
                return;
            };
            match session.mcp.remove(name) {
                Ok((false, _)) => println!("no connected server {name}"),
                Ok((true, true)) => println!("disconnected and unsaved {name}"),
                Ok((true, false)) => {
                    println!(
                        "disconnected {name} (defined in project .mcp.json — it returns on restart)"
                    )
                }
                Err(e) => println!("mcp remove {name}: {e:#}"),
            }
        }
        Some((&"tools", rest)) => {
            let names: Vec<String> = if rest.is_empty() {
                session.mcp.server_names()
            } else {
                rest.iter().map(|s| s.to_string()).collect()
            };
            if names.is_empty() {
                println!("no MCP servers connected");
            }
            for n in names {
                match session.mcp.tools_of(&n) {
                    Some(tools) if tools.is_empty() => println!("{n}: (no tools)"),
                    Some(tools) => {
                        println!("{n}:");
                        for (t, ro) in tools {
                            println!("  mcp__{n}__{t}{}", if ro { "  (read-only)" } else { "" });
                        }
                    }
                    None => println!("no such server {n}"),
                }
            }
        }
        Some((other, _)) => {
            println!(
                "unknown :mcp subcommand '{other}' — usage: :mcp [list|reconnect|add|remove|tools]"
            )
        }
    }
}

/// `:allow` lists the always-allowed tools and directory grants;
/// `:allow remove <tool>` revokes a tool, `:allow remove <perm>:<dir>` a dir grant.
fn handle_allow(sub: Option<&str>, arg: Option<&str>, session: &Session) {
    let Some(db) = session.db.as_ref() else {
        println!("allow: persistent store unavailable");
        return;
    };
    match sub {
        None => {
            match db.allowed_tools() {
                Ok(tools) if tools.is_empty() => {
                    println!(
                        "no always-allowed tools — press 'a' at a confirmation prompt to add one"
                    )
                }
                Ok(tools) => {
                    println!("always-allowed tools:");
                    for (t, ts) in tools {
                        println!("  {t}  ({ts})");
                    }
                }
                Err(e) => println!("allow: {e:#}"),
            }
            // Directory-scoped grants (the 'd' answer) — listed alongside the
            // tool allow-list so every standing permission is visible in one place.
            if let Ok(dirs) = db.allowed_dirs() {
                if !dirs.is_empty() {
                    println!("directory grants (recursive):");
                    for (perm, dir, ts) in dirs {
                        println!("  {perm:<6} {dir}  ({ts})");
                    }
                }
            }
        }
        Some("remove") => match arg {
            // `<perm>:<dir>` (e.g. `read:/tmp/proj`) revokes a directory grant;
            // a bare name revokes a tool/command from the always-allow list.
            Some(spec) => {
                if let Some((perm, dir)) = spec.split_once(':') {
                    if matches!(perm, "read" | "write" | "delete") {
                        match db.revoke_dir(perm, dir) {
                            Ok(true) => println!("removed {perm} grant on {dir}"),
                            Ok(false) => println!("no {perm} grant on {dir}"),
                            Err(e) => println!("allow: {e:#}"),
                        }
                        return;
                    }
                }
                match db.revoke(spec) {
                    Ok(true) => println!("removed {spec} from the always-allow list"),
                    Ok(false) => println!("{spec} wasn't on the always-allow list"),
                    Err(e) => println!("allow: {e:#}"),
                }
            }
            None => println!("usage: :allow remove <tool> | <perm>:<dir>"),
        },
        Some(other) => println!(
            "unknown :allow subcommand '{other}' — usage: :allow [remove <tool> | <perm>:<dir>]"
        ),
    }
}

/// `:batch` toggles/inspects interactive batch mode. `:batch` or `:batch status`
/// reports the mode and lists this session's batch jobs; `:batch on|off` flips
/// the (persisted) flag; `:batch model <id>` sets the model batches run on.
fn handle_batch(sub: Option<&str>, arg: Option<&str>, session: &mut Session) {
    let persist = |session: &Session| {
        if let Some(db) = session.db.as_ref() {
            let _ = db.set_setting(
                "batch_mode",
                if session.batch_mode { "true" } else { "false" },
            );
        }
    };
    match sub {
        Some("on") => {
            session.batch_mode = true;
            persist(session);
            println!(
                "batch mode on — the agent can offload deferrable work to background batches on {} (takes effect next turn)",
                session.batch_model
            );
        }
        Some("off") => {
            session.batch_mode = false;
            persist(session);
            println!("batch mode off");
        }
        Some("model") => match arg {
            Some(m) => {
                let id = match m {
                    "opus" => "claude-opus-4-8",
                    "sonnet" => "claude-sonnet-4-6",
                    "haiku" => "claude-haiku-4-5",
                    other => other,
                };
                session.batch_model = id.to_string();
                println!("batch model → {}", session.batch_model);
            }
            None => println!(
                "batch model: {}\nusage: :batch model <opus|sonnet|haiku|full-id>",
                session.batch_model
            ),
        },
        Some("clear") => {
            // Drop finished (done/failed) jobs from both the store and memory.
            if let Some(store) = session.batch_store.as_ref() {
                match store.clear_finished() {
                    Ok(n) => println!("cleared {n} finished batch job(s)"),
                    Err(e) => println!("batch clear: {e:#}"),
                }
            } else {
                println!("cleared (session-only — no persistent store)");
            }
            session
                .batch_jobs
                .lock()
                .unwrap()
                .retain(|j| !matches!(j.status().as_str(), "done" | "failed"));
        }
        None | Some("status") => {
            println!(
                "batch mode: {} · model: {}",
                if session.batch_mode { "on" } else { "off" },
                session.batch_model
            );
            let jobs = session.batch_jobs.lock().unwrap();
            if jobs.is_empty() {
                println!("no batch jobs");
            } else {
                for j in jobs.iter() {
                    println!("  {}", j.summary_line());
                }
            }
        }
        Some(other) => {
            println!(
                "unknown :batch subcommand '{other}' — usage: :batch [on|off|status|clear|model <id>]"
            )
        }
    }
}

/// `:startup-digest [on|off|status]` controls whether aish surfaces the verbose
/// coordinator digest at boot — the completed-result walls, per-salvage lines,
/// and the `reattached coordinator runs (…)` summary. Suppressed by default so a
/// fresh terminal doesn't open onto a wall of prior workers' output; completed
/// results stay retrievable via `:workers all` / `background_status` /
/// `:result <id>`. The persisted `startup_digest` setting is read at the next
/// startup; the `AISH_STARTUP_DIGEST` env var overrides it for a single run.
fn handle_startup_digest(sub: Option<&str>, session: &mut Session) {
    let persisted = session
        .db
        .as_ref()
        .and_then(|db| db.get_setting("startup_digest").ok().flatten())
        .and_then(|v| crate::coordinator::parse_flag(&v));
    let env_override = std::env::var("AISH_STARTUP_DIGEST")
        .ok()
        .and_then(|v| crate::coordinator::parse_flag(&v));
    let effective = env_override.or(persisted).unwrap_or(false);
    match sub {
        Some("on") | Some("off") => {
            let on = sub == Some("on");
            match session.db.as_ref() {
                Some(db) => {
                    let _ = db.set_setting("startup_digest", if on { "on" } else { "off" });
                    println!(
                        "startup digest {} — takes effect next startup{}",
                        if on { "on" } else { "off" },
                        if env_override.is_some() {
                            " (AISH_STARTUP_DIGEST env is set and overrides this)"
                        } else {
                            ""
                        }
                    );
                }
                None => println!("startup digest: no persistent store — can't save the setting"),
            }
        }
        None | Some("status") => {
            println!(
                "startup digest: {} (default off; suppresses the boot-time coordinator wall)",
                if effective { "on" } else { "off" }
            );
            if env_override.is_some() {
                println!(
                    "  source: AISH_STARTUP_DIGEST env override{}",
                    persisted
                        .map(|p| format!(" (persisted setting: {})", if p { "on" } else { "off" }))
                        .unwrap_or_default()
                );
            } else if persisted.is_some() {
                println!("  source: persisted :startup-digest setting");
            }
        }
        Some(other) => {
            println!("unknown :startup-digest subcommand '{other}' — usage: :startup-digest [on|off|status]")
        }
    }
}

fn describe_mode(m: crate::session::Mode) -> String {
    use crate::session::Mode;
    match m {
        Mode::Paranoid => "paranoid — confirm every tool call".into(),
        Mode::Careful => "careful — confirm anything not provably read-only".into(),
        Mode::Normal => "normal — confirm write/create/delete".into(),
        Mode::Yolo => "\x1b[31myolo — nothing is confirmed\x1b[0m".into(),
    }
}

fn short_cwd(session: &Session) -> String {
    let cwd = session.cwd.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if cwd.starts_with(&home) => cwd.replacen(&home, "~", 1),
        _ => cwd,
    }
}

fn dirs_history_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish_history")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::DefaultHistory;

    // The attach-review claim is once-only per (marker, run_id): the first caller
    // to observe a finished attached worker prints its final result; the racing
    // path (presenter task vs. blocked main loop) no-ops. Re-attaching a
    // different run re-arms the claim.
    #[test]
    fn claim_attach_announce_is_once_only_per_run() {
        let marker = Arc::new(Mutex::new(None));
        assert!(claim_attach_announce(&marker, "run-a"), "first claim wins");
        assert!(
            !claim_attach_announce(&marker, "run-a"),
            "second claim for same run no-ops"
        );
        assert!(
            claim_attach_announce(&marker, "run-b"),
            "a different run re-arms the claim"
        );
        assert!(!claim_attach_announce(&marker, "run-b"));
    }

    // S4.2 — `source` folds a file's exports into the session env.
    #[test]
    fn source_applies_exports_to_session_env() {
        let mut session = Session::new().unwrap();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("aish_source_{}.rc", std::process::id()));
        std::fs::write(&path, "export FOO=bar\nexport BAZ=qux\nalias ll='ls -l'\n").unwrap();

        builtin_source(&[path.to_string_lossy().into_owned()], &mut session);
        let get = |s: &Session, k: &str| {
            s.env
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get(&session, "FOO"), Some("bar".into()));
        assert_eq!(get(&session, "BAZ"), Some("qux".into()));
        assert_eq!(session.last_status, 0);

        // Re-sourcing an updated value replaces in place (last-wins, no dupes).
        std::fs::write(&path, "export FOO=baz\n").unwrap();
        builtin_source(&[path.to_string_lossy().into_owned()], &mut session);
        assert_eq!(get(&session, "FOO"), Some("baz".into()));
        assert_eq!(session.env.iter().filter(|(k, _)| k == "FOO").count(), 1);

        // No argument is a usage error.
        builtin_source(&[], &mut session);
        assert_eq!(session.last_status, 2);

        // A missing file is a non-zero status, not a panic.
        builtin_source(&["/no/such/aish/rc/file".into()], &mut session);
        assert_ne!(session.last_status, 0);

        let _ = std::fs::remove_file(&path);
    }

    // S4.2 — `exec` of a nonexistent program reports and sets a non-zero status
    // WITHOUT replacing the process (the failure path; a successful exec can't
    // be unit-tested since it never returns).
    #[test]
    fn exec_missing_program_sets_nonzero_status() {
        let mut session = Session::new().unwrap();
        builtin_exec(&["this-command-does-not-exist-aish".into()], &mut session);
        assert_ne!(session.last_status, 0);
        // No args is a successful no-op.
        builtin_exec(&[], &mut session);
        assert_eq!(session.last_status, 0);
    }

    #[test]
    fn path_completion() {
        let dir = std::env::temp_dir().join(format!("aish_complete_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("sub.txt"), "").unwrap();
        std::fs::write(dir.join("other.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        std::fs::write(dir.join("with space.txt"), "").unwrap();

        // mid-line word, relative to cwd
        let (start, pairs) = complete_path("cat su", 6, &dir);
        assert_eq!(start, 4);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["sub.txt", "subdir/"], "got: {reps:?}");

        // hidden entries only with an explicit dot prefix
        let (_, pairs) = complete_path("ls .h", 5, &dir);
        assert_eq!(pairs[0].replacement, ".hidden");
        let (_, pairs) = complete_path("ls ", 3, &dir);
        assert!(pairs.iter().all(|p| !p.replacement.starts_with('.')));

        // whitespace in a name → double-quoted replacement
        let (_, pairs) = complete_path("cat wi", 6, &dir);
        assert_eq!(pairs[0].replacement, "\"with space.txt\"");

        // explicit directory part is kept verbatim in the replacement
        let line = "ls subdir/";
        let (start, _) = complete_path(line, line.len(), &dir);
        assert_eq!(start, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn make_exec(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).unwrap();
    }

    fn helper_for(dir: &Path, aliases: HashMap<String, Vec<String>>) -> AishHelper {
        AishHelper {
            cwd: dir.to_path_buf(),
            path: dir.to_string_lossy().into_owned(),
            aliases: Arc::new(aliases),
            cmd_cache: Arc::new(Mutex::new(None)),
            route_cache: Mutex::new(None),
        }
    }

    #[test]
    fn command_completion_word_one() {
        let dir = std::env::temp_dir().join(format!("aish_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("cargo"));
        make_exec(&dir.join("cat"));
        std::fs::write(dir.join("README"), "").unwrap(); // non-exec: excluded

        let mut aliases = HashMap::new();
        aliases.insert(
            "gs".to_string(),
            vec!["git".to_string(), "status".to_string()],
        );
        let helper = helper_for(&dir, aliases);

        // AC1: car<TAB> at the command position offers cargo
        let (start, pairs) = helper.complete_command("car", 3, 0);
        assert_eq!(start, 0);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["cargo"], "got: {reps:?}");

        // builtins are offered as command names
        let (_, pairs) = helper.complete_command("ex", 2, 0);
        assert!(
            pairs.iter().any(|p| p.replacement == "exit"),
            "exit missing"
        );

        // aliases are offered as command names
        let (_, pairs) = helper.complete_command("g", 1, 0);
        assert!(
            pairs.iter().any(|p| p.replacement == "gs"),
            "alias gs missing"
        );

        // non-executable files are not offered. (rustyline's Pair has no Debug;
        // report the replacement strings — `{pairs:?}` broke main's test build.)
        let (_, pairs) = helper.complete_command("READ", 4, 0);
        let names: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert!(names.is_empty(), "non-exec README offered: {names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_completion_defers_to_path_for_later_words_and_slashes() {
        let dir = std::env::temp_dir().join(format!("aish_cmd2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("cargo"));
        std::fs::write(dir.join("sub.txt"), "").unwrap();
        let helper = helper_for(&dir, HashMap::new());

        // word two routes to filename completion, not command names
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, pairs) = helper.complete("cat su", 6, &ctx).unwrap();
        assert_eq!(start, 4);
        assert!(pairs.iter().any(|p| p.replacement == "sub.txt"));
        assert!(!pairs.iter().any(|p| p.replacement == "cargo"));

        // a slash in word one means it's a path (e.g. ./script) → filename
        // completion (the dir part is kept verbatim in the replacement).
        let (_, pairs) = helper.complete_command("./su", 4, 0);
        assert!(pairs.iter().any(|p| p.display == "sub.txt"));
        assert!(pairs.iter().any(|p| p.replacement == "./sub.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_scan_is_cached() {
        let dir = std::env::temp_dir().join(format!("aish_cmd3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("foo"));
        let helper = helper_for(&dir, HashMap::new());

        let first = helper.cached_path_commands();
        assert!(first.contains(&"foo".to_string()));

        // A binary added after the first scan must NOT appear within the TTL —
        // proving the scan is served from cache (AC2: no re-scan on every TAB).
        make_exec(&dir.join("bar"));
        let second = helper.cached_path_commands();
        assert!(!second.contains(&"bar".to_string()), "scan was not cached");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_subcommand_and_flag_completion() {
        let cwd = std::env::temp_dir();
        let reps = |line: &str| -> Vec<String> {
            let (_, pairs) = complete_line(line, line.len(), &cwd);
            pairs.into_iter().map(|p| p.replacement).collect()
        };

        // AC1: `git che<TAB>` offers checkout and cherry-pick.
        let r = reps("git che");
        assert!(r.contains(&"checkout".to_string()), "got: {r:?}");
        assert!(r.contains(&"cherry-pick".to_string()), "got: {r:?}");

        // start index points at the word under the cursor, not the command.
        let (start, _) = complete_line("git che", 7, &cwd);
        assert_eq!(start, 4);

        // Flags complete when the word begins with `-`.
        let r = reps("cargo --no-d");
        assert_eq!(r, vec!["--no-default-features".to_string()]);
        let r = reps("docker --ver");
        assert_eq!(r, vec!["--version".to_string()]);

        // Subcommands for the other supported tools.
        assert!(reps("kubectl get").contains(&"get".to_string()));
        assert!(reps("cargo bu").contains(&"build".to_string()));
    }

    #[test]
    fn unknown_command_degrades_to_path() {
        // AC2: unknown commands must not error and must fall back to path
        // completion (here: no matching paths → empty, but never a panic).
        let dir = std::env::temp_dir().join(format!("aish_unknown_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.txt"), "").unwrap();

        // A made-up command at the argument slot completes filenames, not a crash.
        let (_, pairs) = complete_line("frobnicate rep", 14, &dir);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["report.txt"], "got: {reps:?}");

        // A known command past the subcommand slot also falls to path completion.
        let (_, pairs) = complete_line("git add rep", 11, &dir);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["report.txt"], "got: {reps:?}");

        // An unrecognised git subcommand prefix degrades rather than erroring.
        let (_, pairs) = complete_line("git zzz", 7, &dir);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert!(reps.is_empty(), "got: {reps:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prose_detection() {
        let prose = |l: &str| looks_like_prose(l, &rc::tokenize(l).unwrap());
        // English questions that start with a command word → model
        assert!(prose("who is zachary hohertz"));
        assert!(prose("find me big files"));
        assert!(prose("make this file executable"));
        assert!(prose("what is the capital of texas")); // `what` is a real binary on macOS

        // `clear`/`open` class (same class as ISS-1480): `clear`/`open` are real
        // commands that also begin everyday sentences. A bare-word sentence is
        // prose; bare `clear` and path/flag-bearing `open` invocations stay direct.
        assert!(prose("clear the pointer so it reflects green"));
        assert!(prose("clear the screen for me"));
        assert!(prose("open the door slowly"));
        // ISS-2044: bare-word phrases led by `cut` are release intent, not textutils.
        assert!(prose("cut new release"));
        assert!(prose("cut release notes"));
        assert!(prose("Cut the release for me")); // case-insensitive lead word

        // real invocations stay direct
        assert!(!prose("who"));
        assert!(!prose("who -a"));
        assert!(!prose("which ls"));
        assert!(!prose("what /usr/bin/ls")); // real `what` use takes a path
        assert!(!prose("find . -name foo"));
        assert!(!prose("tail logfile"));
        assert!(!prose("clear")); // bare clear-screen stays direct (only 1 word)
        assert!(!prose("open file.txt")); // dot in the path → command
        assert!(!prose("sort -n data")); // flag → command (sort not ambiguous either)
        assert!(!prose("touch newfile.txt")); // dot in the path → command
        assert!(!prose("cut -f1 file.txt")); // real cut: flag + filename stays a command
        assert!(!prose("cut -c 1-5 data")); // real cut: flag + range stays a command
        // ISS-1480 negatives must keep routing direct
        assert!(!prose("kill 1234 now")); // digits → command
        assert!(!prose("head 50")); // 2 words → not prose
        assert!(!prose("pr file.txt")); // dot in the path → command
        assert!(!prose("echo hello there world")); // echo isn't ambiguous
        assert!(!prose("cat \"my file\" backup")); // quotes signal shell intent

        // sentence punctuation is prose evidence, not command evidence
        assert!(prose(
            "watch for events from atum, let me know about activity in this sprint"
        ));
        assert!(prose("find big files, oldest first"));
        // `?` is a glob metachar: tokenize already rejects it → model
        assert!(rc::tokenize("who is on this machine right now?").is_none());
        // …but flags/paths still win even with a trailing comma elsewhere
        assert!(!prose("watch -n 5 free"));
        assert!(!prose("tail logs/app.log"));

        // ISS-1480: "PR 22 merged" (dev shorthand) must route to the model, not
        // dispatch to /usr/bin/pr. Covers all three former failure modes:
        // `pr` is now ambiguous, the lookup is case-insensitive, and a single
        // embedded number no longer flips an English sentence to a command.
        assert!(prose("pr 22 merged"));
        assert!(prose("PR 22 merged")); // case-insensitive lead word
        assert!(prose("PR is merged")); // no digit at all
        assert!(prose("pr 22 was merged"));
        // …without breaking real invocations. `pr` with a filename, and the
        // numeric-operand commands, stay direct even though a number is present.
        assert!(!prose("pr file.txt")); // genuine pr usage takes a filename
        assert!(!prose("kill 1234")); // PID operand
        assert!(!prose("kill 1234 now")); // number after kill is still a PID
        assert!(!prose("head 50")); // line count
        assert!(!prose("tail 100")); // line count
        assert!(!prose("w 1")); // w's numeric arg
        assert!(!prose("top 1")); // top's numeric arg
    }

    #[test]
    fn command_arg_intent_detection() {
        use std::os::unix::fs::PermissionsExt;
        let tok = |s: &str| rc::tokenize(s).unwrap();
        let cwd = std::env::temp_dir();

        // A PATH dir holding one real executable, `tool`, so resolution is
        // deterministic without depending on what's installed.
        let dir = std::env::temp_dir().join(format!("aish_cmdarg_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let exe = dir.join("tool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.to_string_lossy().to_string();
        let intent = |s: &str| looks_like_command_arg_intent(&tok(s), &cwd, &path);

        // The reported bug: `watch <atum-id>` (and peers) → model — the argument
        // isn't a runnable command, so it can't be a real invocation.
        assert!(intent("watch orch_c1b45797b841"));
        assert!(intent("time some_made_up_target"));
        assert!(intent("nice frobnicate"));
        // Genuine command-taking invocations stay direct.
        assert!(!intent("watch tool")); // `tool` resolves on our PATH
        assert!(!intent("watch -n")); // a flag is real invocation syntax
        // Not a command-taking verb → not intercepted here (handled, or not, elsewhere).
        assert!(!intent("git status"));
        assert!(!intent("make build")); // make targets aren't programs — deliberately excluded
        // Only the exact two-word shape qualifies.
        assert!(!intent("watch")); // 1 word
        assert!(!intent("watch tool extra")); // 3 words

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn route_preview_classifies() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = std::env::temp_dir();
        let dir = std::env::temp_dir().join(format!("aish_preview_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        for name in ["tool", "clear"] {
            let p = dir.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = dir.to_string_lossy().to_string();
        let aliases: HashMap<String, Vec<String>> = HashMap::new();
        let pv = |s: &str| route_preview(s, &cwd, &path, &aliases);

        assert_eq!(pv(""), Preview::Plain);
        assert_eq!(pv(":help"), Preview::Plain);
        assert_eq!(pv("!anything goes here"), Preview::Direct); // forced direct
        assert_eq!(pv("?run this for me"), Preview::Model); // forced model
        assert_eq!(pv("tool --flag"), Preview::Direct); // resolves, not ambiguous
        assert_eq!(pv("clear"), Preview::Ambiguous); // resolves but real-binary-also-English
        assert_eq!(pv("clear the screen for me"), Preview::Model); // prose
        assert_eq!(pv("watch orch_c1b45797b841"), Preview::Model); // command-arg intent
        assert_eq!(pv("what is the capital of texas"), Preview::Model); // prose
        assert_eq!(pv("definitelynotacommand"), Preview::Model); // unresolvable lead word

        // TASK-122 job-control builtins route directly (they're in BUILTINS), so
        // they never leak to the model. `wait` is also an everyday English word
        // (in AMBIGUOUS_COMMANDS), so it previews dim — still dispatched direct.
        assert_eq!(pv("jobs"), Preview::Direct);
        assert_eq!(pv("fg"), Preview::Direct);
        assert_eq!(pv("fg %1"), Preview::Direct);
        assert_eq!(pv("bg %2"), Preview::Direct);
        assert_eq!(pv("wait"), Preview::Ambiguous);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highlight_paints_line_by_route() {
        use rustyline::highlight::Highlighter as _;
        let dir = std::env::temp_dir().join(format!("aish_hl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("tool")); // a plain resolvable binary
        make_exec(&dir.join("clear")); // resolvable AND an everyday English word
        let helper = helper_for(&dir, HashMap::new());
        let paint = |line: &str| helper.highlight(line, 0).to_string();

        let (green, cyan, dim, reset) = ("\x1b[32m", "\x1b[36m", "\x1b[2m", "\x1b[0m");

        // green = direct: a resolvable, non-ambiguous command.
        assert_eq!(paint("tool --flag"), format!("{green}tool --flag{reset}"));
        // cyan = model: prose, and an unresolvable lead word.
        assert_eq!(
            paint("what is the capital of texas"),
            format!("{cyan}what is the capital of texas{reset}")
        );
        assert_eq!(paint("toolnope"), format!("{cyan}toolnope{reset}"));
        // dim = ambiguous: resolves but is also an English word.
        assert_eq!(paint("clear"), format!("{dim}clear{reset}"));
        // Forced `!`/`?` prefixes win over the heuristics.
        assert_eq!(
            paint("!clear the screen for me"),
            format!("{green}!clear the screen for me{reset}")
        );
        assert_eq!(paint("?tool --flag"), format!("{cyan}?tool --flag{reset}"));
        // Plain: a `:`-command and an empty buffer stay the terminal default (untouched).
        assert_eq!(paint(":help"), ":help");
        assert_eq!(paint(""), "");

        // The AC — the line recolors live as routing flips (binary resolves vs
        // prose): the same lead word paints green once it resolves to a real
        // binary and cyan when it does not.
        assert_ne!(paint("tool"), paint("toolnope"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_preview_memoizes_and_tracks_cwd() {
        // S5.4 / TASK-133: the Highlighter reads the route through
        // `cached_preview`, which must agree with the uncached `route_preview`
        // on every buffer and stay stable across the repeated paints rustyline
        // fires for one unchanged line (the flicker-free contract).
        let dir = std::env::temp_dir().join(format!("aish_memo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("tool")); // a plain resolvable binary
        let mut helper = helper_for(&dir, HashMap::new());

        // Cached answer equals the direct classifier for each route class.
        let oracle = |l: &str| route_preview(l, &helper.cwd, &helper.path, &helper.aliases);
        for line in ["tool --flag", "what is the capital of texas", ":help", ""] {
            assert_eq!(
                helper.cached_preview(line),
                oracle(line),
                "mismatch for {line:?}"
            );
        }
        assert_eq!(helper.cached_preview("tool --flag"), Preview::Direct);
        assert_eq!(
            helper.cached_preview("what is the capital of texas"),
            Preview::Model
        );

        // Repeated paints of the SAME buffer return the same decision (memo hit),
        // and interleaving a different line never corrupts the prior answer.
        let direct = helper.cached_preview("tool --flag");
        assert_eq!(helper.cached_preview("tool --flag"), direct);
        assert_eq!(
            helper.cached_preview("what is the capital of texas"),
            Preview::Model
        );
        assert_eq!(helper.cached_preview("tool --flag"), direct);

        // A `cd` (new cwd) invalidates the memo: the same lead word now resolves
        // nowhere, so the route flips from Direct to Model on the next paint.
        let empty = std::env::temp_dir().join(format!("aish_memo_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        helper.set_cwd(&empty);
        // helper_for sets `path` to the original dir; point it at the empty dir
        // too so "tool" truly resolves nowhere after the move.
        helper.path = empty.to_string_lossy().into_owned();
        assert_eq!(helper.cached_preview("tool --flag"), Preview::Model);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    // TASK-122 — `$?` mapping from a job's terminal summary (POSIX fg/wait).
    #[test]
    fn job_exit_code_maps_summary_to_status() {
        assert_eq!(job_exit_code("exited 0"), 0);
        assert_eq!(job_exit_code("exited 1"), 1);
        assert_eq!(job_exit_code("exited 137"), 137);
        // A signalled / unknown summary is non-zero, as a shell reports it.
        assert_eq!(job_exit_code("killed"), 1);
        assert_eq!(job_exit_code("wait failed: boom"), 1);
    }

    #[test]
    fn last_output_binding_expands_in_dispatch() {
        let mut session = Session::new().unwrap();
        let path = std::env::temp_dir().join(format!("aish_repl_last_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = crate::db::Db::open(&path).unwrap();
        db.record("output", "/tmp", "hello from last");
        session.db = Some(db);

        // Both $LAST and $_ resolve to the most recent recorded output.
        assert_eq!(
            rc::tokenize_with("echo $LAST", var_lookup(&session)).unwrap(),
            vec!["echo", "hello from last"]
        );
        assert_eq!(
            rc::tokenize_with("echo $_", var_lookup(&session)).unwrap(),
            vec!["echo", "hello from last"]
        );
        // An explicit session export shadows the binding.
        session.env.push(("LAST".into(), "override".into()));
        assert_eq!(
            rc::tokenize_with("echo $LAST", var_lookup(&session)).unwrap(),
            vec!["echo", "override"]
        );
        let _ = std::fs::remove_file(&path);
    }

    // S4.1 — env-mutating builtins change session state spawned children inherit.
    #[test]
    fn set_and_unset_mutate_session_env() {
        let mut session = Session::new().unwrap();
        let get = |s: &Session, k: &str| {
            s.env
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };

        builtin_set(&["FOO=bar".into(), "BAZ=qux".into()], &mut session);
        assert_eq!(get(&session, "FOO"), Some("bar".into()));
        assert_eq!(get(&session, "BAZ"), Some("qux".into()));
        assert_eq!(session.last_status, 0);

        // Re-assigning replaces in place — no duplicate entries pile up.
        builtin_set(&["FOO=baz".into()], &mut session);
        assert_eq!(get(&session, "FOO"), Some("baz".into()));
        assert_eq!(session.env.iter().filter(|(k, _)| k == "FOO").count(), 1);

        // A non-assignment operand is reported and sets a non-zero status.
        builtin_set(&["notanassignment".into()], &mut session);
        assert_eq!(session.last_status, 1);

        // unset drops the var so later spawns no longer carry it.
        builtin_unset(&["FOO".into()], &mut session);
        assert_eq!(get(&session, "FOO"), None);
        assert_eq!(get(&session, "BAZ"), Some("qux".into()));
        assert_eq!(session.last_status, 0);

        // unset with no names is a usage error.
        builtin_unset(&[], &mut session);
        assert_eq!(session.last_status, 2);
    }

    // S4.1 — umask sets the process mask (inherited by children); read-back works.
    #[test]
    fn umask_sets_and_reads_back() {
        let mut session = Session::new().unwrap();
        // Save the current mask so the test leaves the process untouched.
        let saved = unsafe {
            let m = libc::umask(0);
            libc::umask(m);
            m
        };
        builtin_umask(Some("077"), &mut session);
        assert_eq!(session.last_status, 0);
        let now = unsafe {
            let m = libc::umask(0);
            libc::umask(m);
            m
        };
        assert_eq!(now, 0o077);
        // An invalid mode is rejected without changing the mask.
        builtin_umask(Some("nope"), &mut session);
        assert_eq!(session.last_status, 1);
        // SAFETY: restore the saved mask.
        unsafe { libc::umask(saved) };
    }
    // S4.3 — cd keeps $PWD/$OLDPWD in step so spawned children see the real cwd.
    #[test]
    fn cd_updates_pwd_and_oldpwd() {
        let mut session = Session::new().unwrap();
        let base = std::env::temp_dir();
        let a = base.join(format!("aish_cd_a_{}", std::process::id()));
        let b = base.join(format!("aish_cd_b_{}", std::process::id()));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (a, b) = (a.canonicalize().unwrap(), b.canonicalize().unwrap());
        session.cwd = a.clone();
        let mut prev = None;
        let var = |s: &Session, k: &str| {
            s.env
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };

        builtin_cd(b.to_str(), &mut session, &mut prev);
        assert_eq!(session.cwd, b);
        assert_eq!(var(&session, "PWD"), Some(b.to_string_lossy().into_owned()));
        assert_eq!(
            var(&session, "OLDPWD"),
            Some(a.to_string_lossy().into_owned())
        );

        // `cd -` swaps back, and PWD/OLDPWD swap with it.
        builtin_cd(Some("-"), &mut session, &mut prev);
        assert_eq!(session.cwd, a);
        assert_eq!(var(&session, "PWD"), Some(a.to_string_lossy().into_owned()));
        assert_eq!(
            var(&session, "OLDPWD"),
            Some(b.to_string_lossy().into_owned())
        );

        // set_var keeps exactly one entry per key — no stale duplicates accrue.
        assert_eq!(session.env.iter().filter(|(k, _)| k == "PWD").count(), 1);
        assert_eq!(session.env.iter().filter(|(k, _)| k == "OLDPWD").count(), 1);

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn route_prefixes() {
        assert!(matches!(split_route("!who is x".into()), (l, Route::Direct) if l == "who is x"));
        assert!(matches!(split_route("?ls".into()), (l, Route::Model) if l == "ls"));
        assert!(matches!(split_route("ls".into()), (l, Route::Auto) if l == "ls"));
    }

    // ---- colon command palette ------------------------------------------
    #[test]
    fn colon_catalog_is_sorted_and_unique() {
        let names: Vec<&str> = COLON_COMMANDS.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "COLON_COMMANDS must stay alphabetical");
        let mut dedup = sorted.clone();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            names.len(),
            "COLON_COMMANDS has a duplicate name"
        );
        assert!(!COLON_COMMANDS.is_empty());
    }

    #[test]
    fn colon_command_matches_filters_by_prefix() {
        // Empty prefix → the whole catalog (the bare-`:` popup).
        assert_eq!(colon_command_matches("").len(), COLON_COMMANDS.len());
        // A discriminating prefix narrows to its family.
        assert_eq!(colon_command_matches("o"), vec!["output"]);
        // A unique prefix yields exactly one.
        assert_eq!(colon_command_matches("ba"), vec!["backend", "batch"]);
        assert_eq!(colon_command_matches("y"), vec!["yolo"]);
        // `result` is now a unique prefix (the `:results` alias was removed in #117).
        assert_eq!(colon_command_matches("result"), vec!["result"]);
        // No match → empty (and definitely no panic).
        assert!(colon_command_matches("zzz").is_empty());
    }

    #[test]
    fn complete_colon_narrows_as_prefix_deepens() {
        // Live-filter invariant: each extra char narrows the candidate set
        // monotonically, ending at a single match. `palette_hint` and the TAB
        // completer share `colon_command_matches`, so this pins both.
        let names = |after: &str| {
            complete_colon(after)
                .1
                .into_iter()
                .map(|p| p.replacement.trim_start_matches(':').trim_end().to_string())
                .collect::<Vec<_>>()
        };
        // commands starting with 'm': mcp, mode, model, model-detect.
        assert_eq!(
            names("m"),
            vec!["mcp", "memories", "mode", "model", "model-detect"]
        );
        // typing m -> o -> d -> e -> l narrows down to model/model-detect
        assert_eq!(names("mo"), vec!["mode", "model", "model-detect"]);
        assert_eq!(names("mod"), vec!["mode", "model", "model-detect"]);
        assert_eq!(names("mode"), vec!["mode", "model", "model-detect"]);
        assert_eq!(names("model"), vec!["model", "model-detect"]);
        // a char that matches nothing yields an empty set (menu closes / beeps).
        assert!(names("modez").is_empty());
    }

    #[test]
    fn complete_colon_replacements_and_start() {
        // start is always 0 so the whole `:<prefix>` token is replaced.
        let (start, pairs) = complete_colon("mo");
        assert_eq!(start, 0);
        // `mode` and `model` both match `mo`.
        let repls: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(repls, vec![":mode ", ":model ", ":model-detect "]);
        // The display carries the description; the replacement does not.
        assert!(pairs[0].display.starts_with(":mode"));
        assert!(pairs[0].display.contains("confirmation"));

        // The empty-prefix popup offers every command, each replaceable as `:name `.
        let (start, all) = complete_colon("");
        assert_eq!(start, 0);
        assert_eq!(all.len(), COLON_COMMANDS.len());
        assert!(
            all.iter()
                .all(|p| p.replacement.starts_with(':') && p.replacement.ends_with(' '))
        );

        // Every catalog command's replacement, stripped of `:` and trailing space,
        // is a name `handle_colon` can route (guards against catalog/replacement drift).
        for p in &all {
            let name = p.replacement.trim_start_matches(':').trim_end();
            assert!(COLON_COMMANDS.iter().any(|(n, _)| *n == name));
        }
    }

    #[test]
    fn palette_hint_filters_in_place() {
        // A bare `:` shows the whole catalog as a multi-line hint (one row per
        // command), each row carrying its name and description.
        let full = palette_hint(":", 1, usize::MAX).expect("bare `:` yields the palette");
        assert_eq!(full.matches('\n').count(), COLON_COMMANDS.len());
        assert!(
            full.starts_with('\n'),
            "hint drops onto its own line below the input"
        );
        assert!(full.contains(":mode"));
        assert!(full.contains("confirmation"));

        // Typing narrows it in place: `:re` -> reasoning + rename + restart + result + rewrite.
        let re = palette_hint(":re", 3, usize::MAX)
            .expect("`:re` matches reasoning + rename + restart + result + rewrite");
        assert_eq!(re.matches('\n').count(), 5);
        assert!(re.contains(":reasoning"));
        assert!(re.contains(":rename"));
        assert!(re.contains(":restart"));
        assert!(re.contains(":result"));
        assert!(re.contains(":rewrite"));

        // No hint once an argument starts (a space ends the command name)...
        assert!(palette_hint(":mode dev", 9, usize::MAX).is_none());
        // ...when the cursor isn't at the end of the buffer...
        assert!(palette_hint(":mode", 2, usize::MAX).is_none());
        // ...for a non-`:` line...
        assert!(palette_hint("ls -la", 6, usize::MAX).is_none());
        // ...or when nothing matches.
        assert!(palette_hint(":zzz", 4, usize::MAX).is_none());
    }

    #[test]
    fn palette_hint_caps_rows_so_prompt_never_scrolls_off() {
        // Regression (prompt-disappears): a bare `:` offers the whole catalog,
        // which on any real terminal is taller than the body. Capping keeps the
        // hint short enough that the bottom-anchored prompt stays on-screen and
        // rustyline's relative cursor math stays in sync.
        let total = COLON_COMMANDS.len();
        assert!(total > 5, "catalog large enough to exercise the cap");

        // Cap at 5 rows => 4 command rows + 1 "… and N more" summary row.
        let capped = palette_hint(":", 1, 5).expect("bare `:` yields the palette");
        assert_eq!(
            capped.matches('\n').count(),
            5,
            "hint occupies exactly the cap, never more"
        );
        let more = total - 4;
        assert!(
            capped.contains(&format!("… and {more} more")),
            "overflow summary names the hidden remainder"
        );

        // A cap at least as large as the match set prints every row, no summary.
        let uncapped = palette_hint(":", 1, total).expect("palette");
        assert_eq!(uncapped.matches('\n').count(), total);
        assert!(!uncapped.contains("more (keep typing"));

        // Degenerate cap of 1 still yields a single, safe summary row.
        let one = palette_hint(":", 1, 1).expect("palette");
        assert_eq!(one.matches('\n').count(), 1);
        assert!(one.contains("more (keep typing"));
    }

    // ---- S6.2 / TASK-136: fish-style history ghost text -----------------
    #[test]
    fn ghost_suffix_only_for_strict_prefix() {
        // A strict prefix yields just the trailing remainder.
        assert_eq!(
            ghost_suffix("git", "git status").as_deref(),
            Some(" status")
        );
        assert_eq!(
            ghost_suffix("git ", "git status").as_deref(),
            Some("status")
        );
        assert_eq!(
            ghost_suffix("cargo b", "cargo build").as_deref(),
            Some("uild")
        );
        // An exact match leaves nothing to suggest.
        assert_eq!(ghost_suffix("git status", "git status"), None);
        // Empty line never suggests (fish shows nothing on a blank prompt).
        assert_eq!(ghost_suffix("", "git status"), None);
        // Non-prefix and case mismatch do not match (case-sensitive).
        assert_eq!(ghost_suffix("sl", "git status"), None);
        assert_eq!(ghost_suffix("GIT", "git status"), None);
    }

    /// Build a `DefaultHistory` with `entries` added oldest-first (so the last is
    /// the most recent), matching how the REPL appends submitted lines.
    fn history_with(entries: &[&str]) -> DefaultHistory {
        use rustyline::history::History as _;
        let mut h = DefaultHistory::new();
        for e in entries {
            let _ = h.add(e);
        }
        h
    }

    #[test]
    fn history_suggestion_picks_most_recent_match() {
        let h = history_with(&["git status", "git stash", "cargo test"]);
        // Newest matching entry wins: `git st` -> the most recent `git stash`.
        assert_eq!(history_suggestion("git st", 6, &h).as_deref(), Some("ash"));
        // A prefix matching only the older entry still resolves.
        assert_eq!(history_suggestion("git stat", 8, &h).as_deref(), Some("us"));
        // A unique prefix.
        assert_eq!(history_suggestion("car", 3, &h).as_deref(), Some("go test"));
    }

    #[test]
    fn history_suggestion_requires_cursor_at_end() {
        let h = history_with(&["git status"]);
        // Cursor mid-line (pos < len) suppresses the suggestion — fish only
        // autosuggests at end-of-buffer.
        assert_eq!(history_suggestion("git", 1, &h), None);
        // At end-of-line it resolves.
        assert_eq!(history_suggestion("git", 3, &h).as_deref(), Some(" status"));
    }

    #[test]
    fn history_suggestion_empty_and_no_match() {
        let empty = history_with(&[]);
        assert_eq!(history_suggestion("git", 3, &empty), None);
        let h = history_with(&["git status"]);
        // No history entry extends the line.
        assert_eq!(history_suggestion("docker ps", 9, &h), None);
        // Empty line never suggests.
        assert_eq!(history_suggestion("", 0, &h), None);
    }

    #[test]
    fn hint_prefers_palette_then_ghost_text() {
        use rustyline::hint::Hint as _;
        let dir = std::env::temp_dir();
        let helper = helper_for(&dir, HashMap::new());
        let h = history_with(&["git status"]);
        let ctx = Context::new(&h);

        // A `:`-prefix yields the display-only palette: a multi-line menu with no
        // completion (→/Ctrl-F must not insert it).
        let palette = helper.hint(":m", 2, &ctx).expect("palette hint");
        assert!(palette.display().contains(":mode"));
        assert!(
            palette.completion().is_none(),
            "palette must not be acceptable"
        );

        // A history-extending line yields gray ghost text whose completion is the
        // plain trailing remainder (what →/Ctrl-F insert).
        let ghost = helper.hint("git", 3, &ctx).expect("ghost hint");
        assert_eq!(ghost.completion(), Some(" status"));
        assert!(ghost.display().contains(" status"));
        assert!(
            ghost.display().starts_with(GHOST_COLOR),
            "ghost text is gray"
        );

        // No palette, no history match — no hint at all.
        assert!(helper.hint("nomatch-xyz", 11, &ctx).is_none());
    }

    #[test]
    fn attach_detach_in_catalog() {
        assert_eq!(colon_command_matches("at"), vec!["attach"]);
        assert_eq!(colon_command_matches("det"), vec!["detach"]);
    }

    // ---- Shift-Tab worker cycle (next_attach_index) ---------------------
    /// Build an ordered id list the way `cycle_worker` collects live workers.
    fn ids(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cycle_advances_interactive_through_workers_and_back() {
        let running = ids(&["w_a", "w_b", "w_c"]);
        // interactive (None) -> first worker (slot 1)
        assert_eq!(next_attach_index(&running, None), 1);
        // w_a (slot 1) -> w_b (slot 2)
        assert_eq!(next_attach_index(&running, Some("w_a")), 2);
        // w_b (slot 2) -> w_c (slot 3)
        assert_eq!(next_attach_index(&running, Some("w_b")), 3);
        // last worker (slot 3) wraps back to interactive (slot 0)
        assert_eq!(next_attach_index(&running, Some("w_c")), 0);
    }

    #[test]
    fn cycle_single_worker_toggles_attach_detach() {
        let running = ids(&["w_only"]);
        // interactive -> the one worker
        assert_eq!(next_attach_index(&running, None), 1);
        // the one worker -> back to interactive
        assert_eq!(next_attach_index(&running, Some("w_only")), 0);
    }

    #[test]
    fn cycle_unknown_attachment_restarts_from_interactive() {
        let running = ids(&["w_a", "w_b"]);
        // An attachment whose id is no longer in the cycle list (e.g. it was
        // `:close`d) is treated as interactive, so the next press lands on the
        // first worker rather than panicking or skipping.
        assert_eq!(next_attach_index(&running, Some("w_gone")), 1);
    }

    // ---- 2nd-statusline coordinator status message ----------------------
    fn workers(xs: &[(&str, bool)]) -> Vec<(String, bool, String)> {
        xs.iter()
            .map(|(id, done)| (id.to_string(), *done, String::new()))
            .collect()
    }

    // Same as `workers` but with an explicit task per row, for the task-hint tests.
    fn workers_with_task(xs: &[(&str, bool, &str)]) -> Vec<(String, bool, String)> {
        xs.iter()
            .map(|(id, done, task)| (id.to_string(), *done, task.to_string()))
            .collect()
    }

    #[test]
    fn status_line_detached_shows_colorized_cycle_hint() {
        // Detached with coordinators present → the cyan-colorized detached hint
        // lands on the 2nd statusline instead of a blank row.
        let ws = workers(&[("w_a", false)]);
        let s = coordinator_status_line(None, &ws, true);
        assert_eq!(
            s,
            "\x1b[36m⇄ detached — back to interactive (Shift-Tab to cycle into a coordinator)\x1b[0m"
        );
        // color_on=false → same text, no ANSI (respects NO_COLOR / non-TTY).
        let plain = coordinator_status_line(None, &ws, false);
        assert_eq!(
            plain,
            "⇄ detached — back to interactive (Shift-Tab to cycle into a coordinator)"
        );
        assert!(!plain.contains('\x1b'));
    }

    #[test]
    fn status_line_detached_no_workers_is_blank() {
        // No coordinators to cycle into → nothing on the 2nd statusline.
        assert_eq!(coordinator_status_line(None, &[], true), String::new());
    }

    #[test]
    fn status_line_attached_is_yellow_and_indexed() {
        let ws = workers(&[("w_a", false), ("w_b", false)]);
        let s = coordinator_status_line(Some("w_a"), &ws, true);
        assert!(s.starts_with("\x1b[33m"));
        assert!(s.ends_with("\x1b[0m"));
        assert!(s.contains("attached to"));
        assert!(s.contains("(1/2 · Shift-Tab to cycle, :detach to stop)"));
    }

    #[test]
    fn status_line_attached_shows_task_hint() {
        let ws = workers_with_task(&[("w_a", false, "fix the release workflow")]);
        let s = coordinator_status_line(Some("w_a"), &ws, false);
        assert!(
            s.contains("(1/1 · Shift-Tab to cycle, :detach to stop) - fix the release workflow"),
            "unexpected status line: {s}"
        );
    }

    #[test]
    fn status_line_attached_no_task_has_no_suffix() {
        let ws = workers(&[("w_a", false)]);
        let s = coordinator_status_line(Some("w_a"), &ws, false);
        assert!(s.ends_with("(1/1 · Shift-Tab to cycle, :detach to stop)"));
        assert!(!s.contains(" - "));
    }

    #[test]
    fn short_task_hint_collapses_and_truncates() {
        assert_eq!(
            short_task_hint("fix   the\n release\tworkflow", 48),
            "fix the release workflow"
        );
        let long = "a".repeat(100);
        let hint = short_task_hint(&long, 10);
        assert_eq!(hint.chars().count(), 10);
        assert!(hint.ends_with('…'));
        assert_eq!(short_task_hint("   \n\t ", 48), "");
    }

    #[test]
    fn attach_task_suffix_formats_or_empties() {
        assert_eq!(attach_task_suffix("do a thing"), " - do a thing");
        assert_eq!(attach_task_suffix("   "), "");
    }

    #[test]
    fn status_line_attached_finished_shows_review_mode() {
        let ws = workers(&[("w_done", true)]);
        let s = coordinator_status_line(Some("w_done"), &ws, false);
        assert!(s.contains("(finished)"));
        assert!(s.contains("review mode"));
        assert!(!s.contains('\x1b')); // color_on=false → plain
    }

    #[test]
    fn cycle_with_no_running_workers_is_interactive() {
        let running: Vec<String> = Vec::new();
        // Nothing to cycle into — stays at interactive regardless of state.
        assert_eq!(next_attach_index(&running, None), 0);
        assert_eq!(next_attach_index(&running, Some("w_a")), 0);
    }

    #[test]
    fn cycle_includes_active_goal_as_last_target() {
        // `cycle_worker` appends the GOAL_ATTACH_ID sentinel after the live
        // workers when a `:goal` loop is active, so Shift-Tab lands on the goal
        // just like `:attach goal`. The index math treats it as any other
        // target: interactive → worker → goal → interactive.
        let with_goal = ids(&["w_a", GOAL_ATTACH_ID]);
        assert_eq!(next_attach_index(&with_goal, None), 1); // -> w_a
        assert_eq!(next_attach_index(&with_goal, Some("w_a")), 2); // -> goal
        assert_eq!(next_attach_index(&with_goal, Some(GOAL_ATTACH_ID)), 0); // -> interactive

        // Goal-only (no workers): interactive toggles to/from the goal.
        let goal_only = ids(&[GOAL_ATTACH_ID]);
        assert_eq!(next_attach_index(&goal_only, None), 1); // -> goal
        assert_eq!(next_attach_index(&goal_only, Some(GOAL_ATTACH_ID)), 0); // -> interactive
    }

    #[test]
    fn resume_task_embeds_prior_context_and_followup() {
        let t = build_resume_task(
            "w_abc123",
            "Original task text",
            "Prior result body",
            "now also handle X",
        );
        // The prior run id, original task, prior result, and the operator's
        // follow-up must all survive into the seed so the resumed agent has
        // continuity.
        assert!(t.contains("w_abc123"), "{t}");
        assert!(t.contains("Original task text"), "{t}");
        assert!(t.contains("Prior result body"), "{t}");
        assert!(t.contains("now also handle X"), "{t}");
        // The follow-up comes last (after the prior context), so the model reads
        // the new ask against the established background.
        let ftask = t.find("Original task text").unwrap();
        let fresult = t.find("Prior result body").unwrap();
        let ffollow = t.find("now also handle X").unwrap();
        assert!(ftask < fresult && fresult < ffollow, "ordering wrong: {t}");
    }

    #[test]
    fn owner_gate_enforces_session_ownership_with_any_override() {
        // Own-session run -> always allowed.
        assert_eq!(
            owner_gate(Some("sess-a"), "sess-a", false),
            OwnerGate::Allow
        );
        // Foreign run without --any -> forbidden (multi-user safety, AC7).
        assert_eq!(
            owner_gate(Some("sess-b"), "sess-a", false),
            OwnerGate::Forbidden
        );
        // Foreign run WITH --any -> allowed (explicit cross-session override).
        assert_eq!(owner_gate(Some("sess-b"), "sess-a", true), OwnerGate::Allow);
        // Legacy row with no recorded owner -> treated as own-session (allow).
        assert_eq!(owner_gate(None, "sess-a", false), OwnerGate::Allow);
        // --any is a harmless no-op when the run is already yours.
        assert_eq!(owner_gate(Some("sess-a"), "sess-a", true), OwnerGate::Allow);
    }

    #[test]
    fn take_any_flag_only_strips_a_leading_flag() {
        // Leading --any / -a is consumed; the rest is the (id, message...) tail.
        assert_eq!(
            take_any_flag(&["--any", "w_x", "hello"]),
            (true, vec!["w_x", "hello"])
        );
        assert_eq!(
            take_any_flag(&["-a", "w_x", "hello"]),
            (true, vec!["w_x", "hello"])
        );
        // No flag -> unchanged.
        assert_eq!(
            take_any_flag(&["w_x", "hello"]),
            (false, vec!["w_x", "hello"])
        );
        // A --any AFTER the id is message text, not the flag.
        assert_eq!(
            take_any_flag(&["w_x", "--any", "hello"]),
            (false, vec!["w_x", "--any", "hello"])
        );
        // Empty input -> no flag, empty tail.
        let empty: Vec<&str> = Vec::new();
        assert_eq!(take_any_flag(&[]), (false, empty));
    }

    #[test]
    fn goal_attach_id_matches_worker_stream_label() {
        // `:attach goal` works only because the sentinel equals the goal
        // worker's live stderr-stream label — if they ever drift, attaching to
        // the goal would silently stream nothing. Pin them together.
        assert_eq!(GOAL_ATTACH_ID, crate::worker::GOAL_STREAM_LABEL);
        assert_eq!(GOAL_ATTACH_ID, "goal");
        // short_id leaves the bare sentinel intact for the ⇄ prompt badge.
        assert_eq!(crate::batch::short_id(GOAL_ATTACH_ID), "goal");
    }

    #[test]
    fn mentions_troubleshoot_is_case_insensitive_substring() {
        assert!(mentions_troubleshoot("troubleshoot the build"));
        assert!(mentions_troubleshoot("please TROUBLESHOOT this"));
        assert!(mentions_troubleshoot("can you Troubleshoot the flaky test"));
        assert!(mentions_troubleshoot("auto-troubleshooting the worker")); // substring
        assert!(!mentions_troubleshoot("trouble with the shooter"));
        assert!(!mentions_troubleshoot("ls -la"));
    }

    #[test]
    fn mentions_work_signal_matches_coding_intents() {
        // The new coding/work signals, including inflected forms (token-prefix).
        assert!(mentions_work_signal("write a parser for this grammar"));
        assert!(mentions_work_signal("refactor the engine loop"));
        assert!(mentions_work_signal("refactoring the engine loop"));
        assert!(mentions_work_signal("implement the new routing"));
        assert!(mentions_work_signal("fix the failing test"));
        assert!(mentions_work_signal("fixing the flaky worker"));
        assert!(mentions_work_signal("migrate the schema to v2"));
        assert!(mentions_work_signal("setup the CI pipeline"));
        assert!(mentions_work_signal("build me a login page"));
        assert!(mentions_work_signal("building a small CLI"));
        // troubleshoot is folded in, and matching is case-insensitive.
        assert!(mentions_work_signal("troubleshoot the build"));
        assert!(mentions_work_signal("FIX the bug"));
    }

    #[test]
    fn enrich_task_with_context_prepends_recent_turns() {
        use crate::backend::{Msg, Role};
        let history = vec![
            Msg::user("the build fails with a borrow error in engine.rs"),
            Msg {
                role: Role::Assistant,
                text: "It's a double mutable borrow at line 40.".into(),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            },
        ];
        let out = enrich_task_with_context("build a fix for this and open a PR", &history);
        // The digest carries the antecedent the raw line's "this" referred to…
        assert!(out.contains("Recent conversation context"), "{out}");
        assert!(out.contains("borrow error"), "{out}");
        assert!(out.contains("double mutable borrow"), "{out}");
        assert!(out.contains("Operator:") && out.contains("Assistant:"), "{out}");
        // …and the operator's actual instruction is the trailing, load-bearing line.
        assert!(
            out.trim_end().ends_with("build a fix for this and open a PR"),
            "{out}"
        );
    }

    #[test]
    fn enrich_task_with_context_noop_when_no_history() {
        // A cold prompt (or history with no visible text) offloads exactly as
        // before — the raw line is returned unchanged, no digest wrapper.
        assert_eq!(enrich_task_with_context("fix the thing", &[]), "fix the thing");
        let carriers = vec![crate::backend::Msg::tool_results(vec![])];
        assert_eq!(
            enrich_task_with_context("refactor it", &carriers),
            "refactor it"
        );
    }

    #[test]
    fn enrich_task_with_context_caps_recent_messages() {
        use crate::backend::Msg;
        // One empty-text carrier (skipped) followed by 10 visible turns.
        let mut history = vec![Msg::tool_results(vec![])];
        for i in 0..10 {
            history.push(Msg::user(format!("line {i} with some words")));
        }
        let out = enrich_task_with_context("refactor it", &history);
        // At most 6 newest visible messages are included: line 9..=4 survive,
        // the older ones (incl. "line 0") are dropped.
        assert!(out.contains("line 9"), "{out}");
        assert!(!out.contains("line 0"), "{out}");
    }

    #[test]
    fn clip_for_digest_collapses_and_truncates() {
        // Whitespace/newlines collapse to single spaces.
        assert_eq!(clip_for_digest("a\n  b\t c", 100), "a b c");
        // Over-long input is clipped on a char boundary with an ellipsis.
        let clipped = clip_for_digest("abcdefghij", 4);
        assert_eq!(clipped, "abcd…");
    }

    #[test]
    fn dispatch_guard_failures_carry_no_attach_id() {
        // A guard failure short-circuits BEFORE any child is spawned, so there
        // is nothing to auto-attach to — `id` must be None and the message must
        // explain why. (The happy path spawns a real coordinator process and so
        // isn't unit-testable here; the guards are.)
        let mut session = Session::new().unwrap();

        // Empty task → usage message, no attach id.
        let d = dispatch_coordinator("   ", &mut session);
        assert!(d.id.is_none(), "empty task must not spawn");
        assert!(d.message.contains("usage"), "{}", d.message);

        // Inside a coordinator → refused (no nested coordinators), no attach id.
        session.nested = true;
        let d = dispatch_coordinator("fix the failing test", &mut session);
        assert!(d.id.is_none(), "nested dispatch must not spawn");
        assert!(d.message.contains("nested"), "{}", d.message);
    }

    #[test]
    fn mentions_work_signal_ignores_substrings_and_plain_prose() {
        // A signal stem buried mid-word (not starting a token) does not match.
        assert!(!mentions_work_signal("show me the prefix and suffix"));
        assert!(!mentions_work_signal("what is the capital of texas"));
        assert!(!mentions_work_signal("who is on this machine"));
        assert!(!mentions_work_signal("ls -la"));
    }

    // ---- Phase 2: :skill commands ---------------------------------------
    #[test]
    fn dispatch_routes_subcommands() {
        assert_eq!(
            parse_skill_command(&["add", "acme/git"]),
            SkillCmd::Add("acme/git".into())
        );
        assert_eq!(
            parse_skill_command(&["search", "git", "helper"]),
            SkillCmd::Search("git helper".into())
        );
        assert_eq!(parse_skill_command(&["list"]), SkillCmd::List);
        assert_eq!(parse_skill_command(&["ls"]), SkillCmd::List);
        assert_eq!(
            parse_skill_command(&["remove", "demo"]),
            SkillCmd::Remove("demo".into())
        );
        assert_eq!(
            parse_skill_command(&["rm", "demo"]),
            SkillCmd::Remove("demo".into())
        );
    }

    #[test]
    fn dispatch_unknown_subcommand_is_usage() {
        assert_eq!(parse_skill_command(&[]), SkillCmd::Usage);
        assert_eq!(parse_skill_command(&["bogus"]), SkillCmd::Usage);
        assert_eq!(parse_skill_command(&["add"]), SkillCmd::Usage); // missing ref
        assert_eq!(parse_skill_command(&["search"]), SkillCmd::Usage); // empty query
        assert_eq!(parse_skill_command(&["remove"]), SkillCmd::Usage); // missing name
    }

    #[test]
    fn skill_remove_rejects_path_traversal() {
        // SECURITY: traversal / separators / empty are rejected outright.
        assert!(!valid_skill_name(""));
        assert!(!valid_skill_name("."));
        assert!(!valid_skill_name(".."));
        assert!(!valid_skill_name("../etc"));
        assert!(!valid_skill_name("a/b"));
        assert!(!valid_skill_name("a\\b"));
        assert!(valid_skill_name("git-helper"));
        // The handler bails (Err) on a traversal name BEFORE touching disk.
        let mut session = Session::new().unwrap();
        assert!(skill_remove("../evil", &mut session).is_err());
        assert!(skill_remove("a/b", &mut session).is_err());
    }

    #[test]
    fn skill_add_reloads_catalog() {
        // Installing a SKILL.md + reloading surfaces it in the prompt section
        // (the testable core of `:skill add`, no network).
        let mut session = Session::new().unwrap();
        let tmp = std::env::temp_dir().join(format!("aish-skilladd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(session.skills_prompt, "");
        let md = "---\nname: installed\ndescription: Freshly added.\n---\nbody\n";
        let name = install_skill_text(md, &tmp, &mut session).unwrap();
        assert_eq!(name, "installed");
        assert!(
            session.skills_prompt.contains("installed"),
            "{}",
            session.skills_prompt
        );
        assert!(session.skills_prompt.contains("Freshly added."));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn skill_in_colon_catalog() {
        // The catalog carries :skill so the palette + completion offer it.
        assert!(colon_command_matches("sk").contains(&"skill"));
    }

    #[test]
    fn forget_cmd_parses_id_flag_and_usage() {
        assert_eq!(parse_forget_cmd(&[]), ForgetCmd::Usage);
        assert_eq!(parse_forget_cmd(&["--all-exited"]), ForgetCmd::AllExited);
        assert_eq!(parse_forget_cmd(&["--all"]), ForgetCmd::AllExited);
        assert_eq!(
            parse_forget_cmd(&["w_abc"]),
            ForgetCmd::One("w_abc".to_string())
        );
        // A card-id / prefix is just an id.
        assert_eq!(
            parse_forget_cmd(&["card_9"]),
            ForgetCmd::One("card_9".to_string())
        );
        // Extra tokens, or an unknown flag, are usage (no silent guess).
        assert_eq!(parse_forget_cmd(&["w_abc", "extra"]), ForgetCmd::Usage);
        assert_eq!(parse_forget_cmd(&["--bogus"]), ForgetCmd::Usage);
    }

    #[test]
    fn forget_status_live_vs_terminal() {
        // Live phases/statuses are refused by :forget (AC5a).
        assert!(forget_status_is_live("running"));
        assert!(forget_status_is_live("coordinating"));
        assert!(forget_status_is_live("awaiting_batch"));
        assert!(forget_status_is_live("  running  ")); // trimmed
        // Terminal states are forgettable.
        assert!(!forget_status_is_live("done"));
        assert!(!forget_status_is_live("failed"));
        // An unknown/stale status (e.g. container gone) is treated as terminal
        // so an orphan dir can still be reclaimed (AC edge).
        assert!(!forget_status_is_live(""));
        assert!(!forget_status_is_live("zombie"));
    }

    #[test]
    fn forget_in_colon_catalog() {
        // The catalog carries :forget so the palette + completion offer it.
        assert!(colon_command_matches("for").contains(&"forget"));
    }

    #[test]
    fn workers_table_header_carries_started_and_runtime_columns() {
        // The `:workers` table must list Started + Runtime between Status and
        // Doing, in that order, so each row shows when a worker started and how
        // long it has run (or ran).
        let header = WORKERS_TABLE_HEADER.lines().next().unwrap();
        let cols: Vec<&str> = header
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim())
            .collect();
        assert_eq!(
            cols,
            vec![
                "Worker", "Session", "Status", "Started", "Runtime", "Doing", "Result"
            ]
        );
        // The separator row has one delimiter cell per column (7).
        let sep = WORKERS_TABLE_HEADER.lines().nth(1).unwrap();
        assert_eq!(sep.matches("---").count(), 7);
    }

    #[test]
    fn close_in_colon_catalog() {
        // The catalog carries :close so the palette + completion offer it.
        assert!(colon_command_matches("cl").contains(&"close"));
        // It does not collide with :compact / :context under the `c` prefix.
        let c = colon_command_matches("c");
        assert!(c.contains(&"close"));
        assert!(c.contains(&"compact"));
        assert!(c.contains(&"context"));
    }

    // ---- ISS-2045: :update running-job skew gate ------------------------
    #[test]
    fn combined_running_count_sums_the_three_tallies() {
        // Pure sum over (batches, workers, coordinators) — the count the skew
        // gate thresholds on.
        assert_eq!(combined_running_count(0, 0, 0), 0);
        assert_eq!(combined_running_count(1, 0, 0), 1); // batch alone counts
        assert_eq!(combined_running_count(0, 2, 0), 2); // workers alone
        assert_eq!(combined_running_count(0, 0, 3), 3); // durable coordinators alone
        assert_eq!(combined_running_count(2, 3, 4), 9); // all three
    }

    #[test]
    fn update_confirm_prompt_warns_only_when_jobs_running() {
        // Count 0 → plain confirm, behavior unchanged (AC3).
        let calm = update_confirm_prompt(0, "0.5.0");
        assert_eq!(calm, "download and install aish 0.5.0?");
        assert!(!calm.contains("skew"));

        // Count > 0 → explicit skew warning that NAMES the count and the
        // consequence (AC2/AC4), and still carries the version.
        let warn = update_confirm_prompt(3, "0.5.0");
        assert!(warn.contains('3'), "warning must name the count: {warn}");
        assert!(
            warn.contains("version skew"),
            "warning must name the consequence: {warn}"
        );
        assert!(warn.to_lowercase().contains("old"));
        assert!(warn.contains("0.5.0"));
        // Mentions every job class so batches are counted for skew (AC5).
        assert!(warn.contains("workers/batches/coordinators"));

        // Singular vs plural agreement on the job noun.
        assert!(update_confirm_prompt(1, "1.0.0").contains("job "));
        assert!(update_confirm_prompt(2, "1.0.0").contains("jobs "));
    }

    // Snapshot of the three pure routing heuristics — `!`/`?` force routing,
    // looks_like_prose, and the tokenizer gate — so a tweak to any of them
    // surfaces as a golden diff instead of silently reshaping UX. The
    // executable-resolution step in `dispatch` is intentionally excluded to
    // keep the snapshot hermetic (no PATH/filesystem dependence); "direct"
    // here means "the heuristics did not divert the line to the model".
    fn heuristic_route(input: &str) -> &'static str {
        let (rest, route) = split_route(input.to_string());
        match route {
            Route::Direct => "direct  [! force]",
            Route::Model => "model   [? force]",
            Route::Auto => match rc::tokenize(&rest) {
                None => "model   [shell-syntax / non-tokenizable]",
                Some(words) if words.is_empty() => "model   [empty]",
                Some(words) => {
                    if is_stray_confirmation(&words) {
                        "model   [bare-yes guard]"
                    } else if looks_like_prose(&rest, &words) {
                        "model   [looks_like_prose]"
                    } else {
                        "direct  [auto]"
                    }
                }
            },
        }
    }

    #[test]
    fn routing_decision_snapshot() {
        // Golden corpus grouped by the heuristic each case exercises. Keep the
        // inputs and their order stable: a heuristic change should show up as a
        // value diff, not a reshuffle.
        let groups: &[(&str, &[&str])] = &[
            (
                "looks_like_prose — English starting with a real command word routes to the model, real invocations stay direct",
                &[
                    "who is zachary hohertz",
                    "find me big files",
                    "make this file executable",
                    "watch for events from atum, let me know about activity",
                    "find big files, oldest first",
                    "who",
                    "who -a",
                    "which ls",
                    "find . -name foo",
                    "tail logfile",
                    "echo hello there world",
                    "cat \"my file\" backup",
                    "watch -n 5 free",
                    "tail logs/app.log",
                ],
            ),
            (
                "bare-yes guard — a lone confirmation token (y/yes/n/no) routes to the model; multi-word invocations stay direct",
                &[
                    "yes",
                    "yes please",
                    "yes please thanks",
                    "test -f file",
                    "test the connection right now",
                ],
            ),
            (
                "!/? force routing — explicit escape hatches override every other heuristic",
                &[
                    "!who is zachary hohertz",
                    "!ls -la",
                    "?ls",
                    "?make this file executable",
                    "?who",
                ],
            ),
            (
                "tokenizer gate — shell syntax and prose punctuation the tokenizer rejects route to the model",
                &[
                    "who is on this machine right now?",
                    "echo *.rs",
                    "cat a > b",
                ],
            ),
        ];

        let mut snap = String::new();
        snap.push_str("# routing decision snapshot (TASK-110)\n");
        snap.push_str("# direct = run on the terminal · model = hand the line to the model\n");
        snap.push_str("# regenerate after an intended heuristic change: UPDATE_GOLDEN=1 cargo test routing_decision_snapshot\n");
        for (heading, cases) in groups {
            snap.push('\n');
            snap.push_str(&format!("## {heading}\n"));
            for &input in *cases {
                snap.push_str(&format!("{:<52} -> {}\n", input, heuristic_route(input)));
            }
        }

        let golden_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/routing_decisions.snap"
        );
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(golden_path, &snap).expect("write golden snapshot");
            return;
        }

        let golden = std::fs::read_to_string(golden_path)
            .expect("missing tests/golden/routing_decisions.snap — run UPDATE_GOLDEN=1 cargo test to create it");
        if golden != snap {
            let mut diff = String::new();
            for (i, (g, a)) in golden.lines().zip(snap.lines()).enumerate() {
                if g != a {
                    diff.push_str(&format!("  L{}\n    golden: {g}\n    actual: {a}\n", i + 1));
                }
            }
            let (gn, an) = (golden.lines().count(), snap.lines().count());
            if gn != an {
                diff.push_str(&format!("  line count differs: golden={gn} actual={an}\n"));
            }
            panic!(
                "routing decision snapshot drift — a routing heuristic changed.\n\
                 Review the diff below; if the change is intended, regenerate the \n\
                 golden file with `UPDATE_GOLDEN=1 cargo test routing_decision_snapshot`.\n\n{diff}"
            );
        }
    }

    // ---- TASK-109: oracle harness — direct-dispatch path ------------------
    //
    // Sibling to pipeline.rs's pipeline-path oracle. A line with no `|` takes
    // the *direct-dispatch* path: `dispatch` resolves one program via
    // `resolve_program` and runs it through `tools::run_on_tty`. bash is the
    // ground truth aish's single-command path must reproduce on stdout bytes
    // and exit status.
    //
    // `run_on_tty` inherits the terminal's stdout, so — exactly as the pipeline
    // oracle appends a `dd` sink rather than read the terminal — the stdout
    // cases mirror `run_on_tty`'s spawn configuration (same cwd, same session
    // env, kill_on_drop) but pipe stdout into a capture buffer. The exit-status
    // cases call the genuine `run_on_tty`, so the production path is exercised
    // directly. The corpus is integer/ASCII coreutils with no stdin reads, so a
    // mismatch is an aish regression, not a locale or echo-builtin quirk.

    /// Resolve `cmd`'s program the way `dispatch` does (real `resolve_program`
    /// against the process PATH), returning the resolved path and argv tail.
    fn resolve_for_oracle(cmd: &str, session: &Session) -> (PathBuf, Vec<String>) {
        let words = rc::tokenize(cmd).expect("oracle command must tokenize");
        let path_var = std::env::var("PATH").unwrap_or_default();
        let program = resolve_program(&words[0], &session.cwd, &path_var)
            .expect("oracle command's program must resolve on PATH");
        (program, words[1..].to_vec())
    }

    /// Run `cmd` through aish's direct-dispatch resolution and a faithful mirror
    /// of `run_on_tty`'s spawn (stdout piped for capture). Returns stdout bytes.
    async fn aish_direct_stdout(cmd: &str, session: &Session) -> Vec<u8> {
        let (program, args) = resolve_for_oracle(cmd, session);
        tokio::process::Command::new(&program)
            .args(&args)
            .current_dir(&session.cwd)
            .envs(session.env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn direct-dispatch program")
            .stdout
    }

    /// Exit code of `cmd` through the genuine `tools::run_on_tty`.
    async fn aish_direct_code(cmd: &str, session: &Session) -> Option<i32> {
        let (program, args) = resolve_for_oracle(cmd, session);
        tools::run_on_tty(&program.to_string_lossy(), &args, &[], session)
            .await
            .expect("run_on_tty")
            .code()
    }

    /// Run `cmd` through the oracle (`bash -c`) in `cwd`, returning its stdout.
    fn bash_stdout(cmd: &str, cwd: &Path) -> Vec<u8> {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn bash oracle (is bash on PATH?)")
            .stdout
    }

    /// Exit code of `cmd` under the oracle (`bash -c`) in `cwd`.
    fn bash_code(cmd: &str, cwd: &Path) -> Option<i32> {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .status()
            .expect("spawn bash oracle")
            .code()
    }

    #[tokio::test]
    async fn oracle_direct_stdout_matches_bash() {
        let session = Session::new().unwrap();
        // Each entry is a single command aish runs natively (no pipe), reading
        // no stdin; bash is the ground truth.
        let corpus = [
            "echo hello world",
            "seq 1 20",
            "seq 5 1",   // descending with the default step → empty output
            "seq 1 2 9", // strided: 1 3 5 7 9
            "printf %s+%s 3 4",
            "expr 6 + 7",
            "basename /usr/local/bin/aish",
            "dirname /usr/local/bin/aish",
            "head -c 8 /dev/zero",
            "wc -c /dev/null",
        ];
        for cmd in corpus {
            let got = aish_direct_stdout(cmd, &session).await;
            let want = bash_stdout(cmd, &session.cwd);
            assert_eq!(
                got,
                want,
                "aish direct-dispatch stdout diverged from bash for `{cmd}`\n  \
                 aish: {:?}\n  bash: {:?}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want),
            );
        }
    }

    #[tokio::test]
    async fn oracle_direct_exit_status_matches_bash() {
        let session = Session::new().unwrap();
        // A single command's status is the program's status in both shells.
        let corpus = [
            "true",
            "false",
            "test -d /",
            "test -f /no/such/path/aish-oracle",
            "grep -q anything /dev/null", // empty file → no match → exit 1
            "expr 1 = 1",                 // result 1 (true) → exit 0
            "expr 1 = 2",                 // result 0 (false) → exit 1
        ];
        for cmd in corpus {
            let got = aish_direct_code(cmd, &session).await;
            let want = bash_code(cmd, &session.cwd);
            assert_eq!(
                got, want,
                "aish direct-dispatch exit status diverged from bash for `{cmd}`"
            );
        }
    }

    #[tokio::test]
    async fn oracle_direct_detects_deliberate_divergence() {
        // Teeth: feed aish and the oracle *different* commands and confirm the
        // comparator sees them as unequal. If this ever matches, the direct
        // oracle is blind and the agreement tests above are worthless.
        let session = Session::new().unwrap();
        let aish = aish_direct_stdout("echo same", &session).await;
        let bash = bash_stdout("echo different", &session.cwd);
        assert_ne!(
            aish, bash,
            "direct oracle failed to detect an intentional divergence — the harness is blind"
        );
    }

    // ───────── `:goal` persisted-subcommand surface (TASK-278) ─────────

    #[test]
    fn goal_new_creates_and_pins_current() {
        let mut s = Session::new().unwrap();
        let out = goal_command(&mut s, "new", "Ship the release");
        assert!(out.contains("goal created"), "got: {out}");
        assert!(out.contains("now current"), "got: {out}");
        assert_eq!(s.goals.len(), 1);
        assert_eq!(s.goals[0].title, "Ship the release");
        // current_goal_id points at the just-created goal.
        assert_eq!(s.current_goal_id.as_deref(), Some(s.goals[0].id.as_str()));
    }

    #[test]
    fn goal_new_requires_a_title() {
        let mut s = Session::new().unwrap();
        let out = goal_command(&mut s, "new", "   ");
        assert!(out.starts_with("usage:"), "got: {out}");
        assert!(s.goals.is_empty());
    }

    #[test]
    fn goal_link_block_milestone_apply_to_current() {
        let mut s = Session::new().unwrap();
        goal_command(&mut s, "new", "Land TASK-278");
        let link = goal_command(&mut s, "link", "TASK-278 goal CRUD");
        assert!(link.contains("linked TASK-278"), "got: {link}");
        let ms = goal_command(&mut s, "milestone", "wire dispatcher 2025-01-31");
        assert!(ms.contains("milestone added"), "got: {ms}");
        assert!(ms.contains("(due 2025-01-31)"), "date folded in: {ms}");
        let blk = goal_command(&mut s, "block", "waiting on review @grhohertz");
        assert!(blk.contains("waiting on grhohertz"), "got: {blk}");

        let g = s.current_goal().expect("current goal");
        assert_eq!(g.linked_tasks.len(), 1);
        assert_eq!(g.linked_tasks[0].key, "TASK-278");
        assert_eq!(g.linked_tasks[0].title.as_deref(), Some("goal CRUD"));
        assert_eq!(g.milestones.len(), 1);
        assert_eq!(g.open_blockers(), 1);
    }

    #[test]
    fn goal_unblock_matches_open_blocker() {
        let mut s = Session::new().unwrap();
        goal_command(&mut s, "new", "G");
        goal_command(&mut s, "block", "waiting on CI");
        let miss = goal_command(&mut s, "unblock", "nonsense");
        assert!(miss.contains("no open blocker"), "got: {miss}");
        assert_eq!(s.current_goal().unwrap().open_blockers(), 1);
        let hit = goal_command(&mut s, "unblock", "ci");
        assert!(hit.contains("unblocked"), "got: {hit}");
        assert_eq!(s.current_goal().unwrap().open_blockers(), 0);
    }

    #[test]
    fn goal_complete_is_terminal_and_idempotent() {
        let mut s = Session::new().unwrap();
        goal_command(&mut s, "new", "Finish");
        let done = goal_command(&mut s, "complete", "");
        assert!(done.contains("goal completed"), "got: {done}");
        assert_eq!(s.current_goal().unwrap().status, crate::goal::GoalStatus::Completed);
        let again = goal_command(&mut s, "complete", "");
        assert!(again.contains("already complete"), "got: {again}");
    }

    #[test]
    fn goal_show_and_status_render_without_panicking() {
        let mut s = Session::new().unwrap();
        // Empty-state messages first.
        assert!(goal_command(&mut s, "status", "").contains("no goals yet"));
        assert!(goal_command(&mut s, "show", "").contains("no goals yet"));
        goal_command(&mut s, "new", "Observe everything");
        goal_command(&mut s, "milestone", "add tracing");
        let show = goal_command(&mut s, "show", "");
        assert!(show.contains("Observe everything"), "got: {show}");
        assert!(show.contains("milestones (0/1)"), "got: {show}");
        let status = goal_command(&mut s, "status", "");
        assert!(status.contains("1 goal(s):"), "got: {status}");
        assert!(status.contains("Observe everything"), "got: {status}");
    }

    #[test]
    fn goal_subcommands_need_a_current_goal() {
        let mut s = Session::new().unwrap();
        // With no goals, mutators explain themselves instead of panicking.
        for sub in ["link", "block", "milestone"] {
            let out = goal_command(&mut s, sub, "whatever");
            assert!(out.contains("no current goal"), "{sub}: {out}");
        }
    }

    #[test]
    fn goal_show_unknown_prefix_reports_miss() {
        let mut s = Session::new().unwrap();
        goal_command(&mut s, "new", "A goal");
        let out = goal_command(&mut s, "show", "deadbeefcafe");
        assert!(out.contains("no goal matching"), "got: {out}");
    }
}
