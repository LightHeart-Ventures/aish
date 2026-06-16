mod backend;
mod batch;
mod coordinator;
mod db;
mod engine;
mod goal;
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Load ~/.aishrc up front, BEFORE building the backend: its `export` lines
    // (credentials included) populate session.env, and the Claude backend
    // resolves its credential from those rc exports as well as the process env —
    // so `export CLAUDE_CODE_OAUTH_TOKEN=…` / `export ANTHROPIC_API_KEY=…` in
    // ~/.aishrc works for aish itself, not just the programs it spawns.
    let rc = rc::load();
    let mut session = session::Session::new()?;
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
    let mcp_config = aish_dir.join(".mcp.json");
    if !mcp_config.exists() {
        let _ = std::fs::write(
            &mcp_config,
            "{\n  \"mcpServers\": {\n  }\n}\n",
        );
    }
    // Project-scope .mcp.json (cwd) outranks the user-scope one on name clashes.
    let project_mcp = session.cwd.join(".mcp.json");
    session.mcp = mcp::McpHost::start(&[project_mcp.as_path(), mcp_config.as_path()]).await;
    // After MCP connect: the skills catalog merges the local directory with
    // every server's published prompts.
    session.skills_prompt = skills::render_prompt_section(
        &skills::load(&aish_dir.join("skills")),
        &session.mcp.skills(),
    );
    session.db = match db::Db::open(&aish_dir.join("aish.db")) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("\x1b[33maish:\x1b[0m persistent store unavailable: {e:#}");
            None
        }
    };

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

    // Durable coordinator runs: open the store and reattach prior runs — surface
    // any that finished while we were down, reap orphaned (stale-heartbeat) ones.
    match db::CoordinatorStore::open(&aish_dir.join("aish.db")) {
        Ok(store) => {
            session.coordinator_store = Some(store);
            coordinator::rehydrate(&mut session);
        }
        Err(e) => eprintln!("\x1b[33maish:\x1b[0m coordinator store unavailable: {e:#}"),
    }

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

    repl::run(backend, session, rc.aliases).await
}

fn aish_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish")
}
