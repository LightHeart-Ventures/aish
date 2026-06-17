mod backend;
mod batch;
mod coordinator;
mod db;
mod engine;
mod goal;
mod jobs;
mod md;
mod mcp;
#[cfg(test)]
mod oracle;
mod pipeline;
mod present;
mod rc;
mod repl;
mod session;
mod skills;
mod tools;
mod worker;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "aish", about = "An AI-native shell — no bash, just intent")]
struct Args {
    /// Run a single prompt non-interactively and exit (login-shell style)
    #[arg(short = 'c', long = "command")]
    command: Option<String>,

    /// Backend to use: claude | grok | local
    #[arg(long, default_value = "claude")]
    backend: String,

    /// Model override (e.g. claude-haiku-4-5)
    #[arg(long)]
    model: Option<String>,

    /// Confirmation mode: paranoid | careful | normal | yolo
    #[arg(long, default_value = "normal")]
    mode: String,

    /// Skip all confirmation prompts (alias for --mode yolo)
    #[arg(long)]
    yolo: bool,

    /// Run headless as a background coordinator: like -c, but AWAIT all
    /// background batch jobs before exiting (plain -c would orphan them). Runs
    /// unattended in yolo mode. Requires --run-id. This is how aish re-execs
    /// itself as a full-tool background worker.
    #[arg(long)]
    coordinator: bool,

    /// Durable id for a --coordinator run (used in logs and, later, the
    /// coordinator store for result read-back).
    #[arg(long = "run-id")]
    run_id: Option<String>,
}

/// Lightweight, env-gated startup phase timer. Enabled by `AISH_TIME_STARTUP=1`
/// (any non-empty value but `0`). Prints one stderr line per phase:
/// `[startup] <phase> +<delta>ms (total <total>ms)`. Off by default — zero cost.
/// Diagnostics aid for the startup-latency work: MCP connect was ~99% of the
/// total before it was moved off the interactive critical path (see repl::run).
struct StartupTimer {
    on: bool,
    t0: std::time::Instant,
    last: std::time::Instant,
}
impl StartupTimer {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            on: std::env::var("AISH_TIME_STARTUP").map(|v| v != "0" && !v.is_empty()).unwrap_or(false),
            t0: now,
            last: now,
        }
    }
    fn mark(&mut self, phase: &str) {
        if !self.on {
            return;
        }
        let now = std::time::Instant::now();
        eprintln!(
            "\x1b[36m[startup]\x1b[0m {phase:<22} +{:>6.1}ms  (total {:>6.1}ms)",
            now.duration_since(self.last).as_secs_f64() * 1000.0,
            now.duration_since(self.t0).as_secs_f64() * 1000.0,
        );
        self.last = now;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut timer = StartupTimer::new();
    let args = Args::parse();
    timer.mark("args parsed");
    // Load ~/.aishrc up front, BEFORE building the backend: its `export` lines
    // (credentials included) populate session.env, and the Claude backend
    // resolves its credential from those rc exports as well as the process env —
    // so `export CLAUDE_CODE_OAUTH_TOKEN=…` / `export ANTHROPIC_API_KEY=…` in
    // ~/.aishrc works for aish itself, not just the programs it spawns.
    let rc = rc::load();
    timer.mark("rc::load");
    let mut session = session::Session::new()?;
    timer.mark("Session::new");
    session.env = rc.env;
    let backend = match args.backend.as_str() {
        "claude" => {
            let cred = backend::claude::Credential::resolve(&session.env)?;
            backend::Backend::new_claude(
                args.model.unwrap_or_else(|| "claude-haiku-4-5".into()),
                cred,
            )?
        }
        "grok" => backend::Backend::new_grok(
            args.model.unwrap_or_else(|| backend::grok::DEFAULT_MODEL.into()),
            &session.env,
        )?,
        #[cfg(feature = "local")]
        "local" => backend::Backend::new_local(),
        other => anyhow::bail!("unknown backend: {other} (available: claude, grok, local)"),
    };
    timer.mark("backend built");
    // Record which provider the interactive session runs on, so background
    // coordinators are spawned on the SAME backend (full parity).
    session.backend_kind = backend.kind().to_string();
    session.mode = if args.yolo {
        session::Mode::Yolo
    } else {
        session::Mode::parse(&args.mode)
            .ok_or_else(|| anyhow::anyhow!("unknown mode: {} (paranoid|careful|normal|yolo)", args.mode))?
    };

    // ~/.aish/ — config home: .mcp.json (MCP servers) and skills/.
    let aish_dir = aish_dir();
    let _ = std::fs::create_dir_all(aish_dir.join("skills"));
    let skills_dir = aish_dir.join("skills");
    let mcp_config = aish_dir.join(".mcp.json");
    if !mcp_config.exists() {
        let _ = std::fs::write(
            &mcp_config,
            "{\n  \"mcpServers\": {\n  }\n}\n",
        );
    }
    // Project-scope .mcp.json (cwd) outranks the user-scope one on name clashes.
    let project_mcp = session.cwd.join(".mcp.json");
    let mcp_paths = vec![project_mcp.clone(), mcp_config.clone()];

    // MCP connect is the dominant startup cost (a remote HTTP server's
    // initialize → tools/list → prompts/list handshake can take many seconds —
    // ~18s for the atum server — and it formerly ran here, BEFORE the REPL
    // prompt, freezing the shell that whole time). Split the two paths:
    //
    //   * Interactive REPL — DEFER it. Start with a local-only skills catalog
    //     and hand the config paths to repl::run, which connects in the
    //     background and installs the tools/skills when the handshake completes.
    //     engine::run_turn reads session.mcp.tool_defs() fresh each turn, so a
    //     late arrival simply becomes available on the next turn. The prompt now
    //     appears in ~tens of ms instead of after the full handshake.
    //
    //   * One-shot (-c / --coordinator) — connect SYNCHRONOUSLY: that single
    //     turn needs the MCP tools immediately, with no interactive loop to
    //     install them into later.
    let interactive = args.command.is_none();
    if interactive {
        session.skills_prompt =
            skills::render_prompt_section(&skills::load(&skills_dir), &[]);
    } else {
        session.mcp =
            mcp::McpHost::start(&[project_mcp.as_path(), mcp_config.as_path()]).await;
        timer.mark("MCP connect");
        session.skills_prompt = skills::render_prompt_section(
            &skills::load(&skills_dir),
            &session.mcp.skills(),
        );
    }
    timer.mark("skills render");
    session.db = match db::Db::open(&aish_dir.join("aish.db")) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("\x1b[33maish:\x1b[0m persistent store unavailable: {e:#}");
            None
        }
    };
    timer.mark("db open");

    // Restore the persisted interactive-batch-mode flag. On by default; a prior
    // `:batch off` is honored across restarts (unset → stays on).
    if let Some(db) = &session.db {
        if let Ok(Some(v)) = db.get_setting("batch_mode") {
            session.batch_mode = v == "true";
        }
    }

    // Durable batch jobs: open the store and reattach any in-flight batches from
    // a previous session (the batch keeps running platform-side while aish is
    // down; this picks the handle back up so results land here when they finish).
    match db::BatchStore::open(&aish_dir.join("aish.db")) {
        Ok(store) => {
            session.batch_store = Some(store);
            batch::rehydrate(&mut session);
        }
        Err(e) => eprintln!("\x1b[33maish:\x1b[0m batch store unavailable: {e:#}"),
    }
    timer.mark("batch rehydrate");

    // Durable coordinator runs: open the store and reattach prior runs — surface
    // any that finished while we were down, reap orphaned (stale-heartbeat) ones.
    match db::CoordinatorStore::open(&aish_dir.join("aish.db")) {
        Ok(store) => {
            session.coordinator_store = Some(store);
            coordinator::rehydrate(&mut session);
        }
        Err(e) => eprintln!("\x1b[33maish:\x1b[0m coordinator store unavailable: {e:#}"),
    }
    timer.mark("coordinator rehydrate");

    if let Some(prompt) = args.command {
        if args.coordinator {
            let run_id = args
                .run_id
                .ok_or_else(|| anyhow::anyhow!("--coordinator requires --run-id"))?;
            // Unattended: no TTY to answer confirm prompts, so run without gates.
            session.mode = session::Mode::Yolo;
            // Adopt the LAUNCHING session's identity so every durable record this
            // coordinator writes is attributed to the session that asked for the
            // work, not to this child's throwaway uuid (set in Session::new).
            if let Ok(sid) = std::env::var("AISH_LAUNCH_SESSION_ID") {
                if !sid.is_empty() {
                    session.session_id = sid;
                }
            }
            if let Ok(name) = std::env::var("AISH_LAUNCH_SESSION_NAME") {
                if !name.is_empty() {
                    session.name = Some(name);
                }
            }
            return engine::run_coordinator(&backend, &mut session, prompt, &run_id).await;
        }
        let out = engine::run_turn(&backend, &mut session, prompt, &mut repl::confirm_tty).await?;
        println!("{}", md::render_stdout(&out));
        return Ok(());
    }

    repl::run(backend, session, rc.aliases, mcp_paths, skills_dir).await
}

fn aish_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish")
}
