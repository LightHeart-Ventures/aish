mod backend;
mod db;
mod engine;
mod md;
mod mcp;
mod pipeline;
mod rc;
mod repl;
mod session;
mod skills;
mod tools;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "aish", about = "An AI-native shell — no bash, just intent")]
struct Args {
    /// Run a single prompt non-interactively and exit (login-shell style)
    #[arg(short = 'c', long = "command")]
    command: Option<String>,

    /// Backend to use: claude | local
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let backend = match args.backend.as_str() {
        "claude" => backend::Backend::new_claude(
            args.model.unwrap_or_else(|| "claude-opus-4-8".into()),
        )?,
        #[cfg(feature = "local")]
        "local" => backend::Backend::new_local(),
        other => anyhow::bail!("unknown backend: {other} (available: claude, local)"),
    };
    let mut session = session::Session::new()?;
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

    if let Some(prompt) = args.command {
        let out = engine::run_turn(&backend, &mut session, prompt, &mut repl::confirm_tty).await?;
        println!("{}", md::render_stdout(&out));
        return Ok(());
    }

    repl::run(backend, session).await
}

fn aish_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish")
}
