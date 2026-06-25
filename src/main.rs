mod autosuggest;
mod backend;
mod batch;
mod container;
mod context;
mod coordinator;
mod db;
mod editor;
mod engine;
mod goal;
mod jobs;
mod loopguard;
mod md;
mod mcp;
#[cfg(test)]
mod oracle;
mod pipeline;
mod present;
mod rc;
mod repl;
mod rewrite;
mod scope;
mod script;
mod session;
mod skill_provider;
mod skills;
mod style;
mod suggest;
mod tools;
mod turn_audit;
mod update;
mod worker;
mod worker_store;

use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};

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

    /// Check for a newer published release, upgrade if one exists, and exit.
    #[arg(long)]
    update: bool,

    /// Fetch and import a skill (opt-in), then exit. Accepts a URL or the
    /// owner/name shorthand. Supports any skill provider (default: skill.fish).
    #[arg(long = "skill-fetch", value_name = "REF")]
    skill_fetch: Option<String>,

    /// Search the skill registry catalog and print matches, then exit. Each
    /// printed `owner/name` ref can be passed straight to --skill-fetch.
    #[arg(long = "skill-search", value_name = "QUERY")]
    skill_search: Option<String>,

    /// Disable ANSI color/emoji output. Also auto-disabled when stdout is
    /// not a TTY (piped/redirected) or when the NO_COLOR env var is set.
    #[arg(long = "no-color")]
    no_color: bool,

    /// Login shell: source profiles and become a session leader. Also implied
    /// by an argv[0] beginning with `-` (e.g. `-aish`), the classic convention.
    #[arg(short = 'l', long = "login")]
    login: bool,

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

    /// Script to run non-interactively, then exit, plus its arguments
    /// (TASK-17/18). The FIRST value is the script path; the rest are passed
    /// through as the script's positional parameters `$1`/`$2`/… (with `$0` the
    /// script path). Each script line runs as if typed at the prompt; blank
    /// lines and `#` comments are skipped, so a `#!/usr/bin/env aish` shebang
    /// line works and the kernel's `aish <script> <args…>` invocation is handled
    /// as-is. The process exits with the status of the last line. Ignored when
    /// `-c` is given. `trailing_var_arg` + `allow_hyphen_values` collect the
    /// operands verbatim so a script's own `-flag` args aren't parsed as aish
    /// flags.
    #[arg(value_name = "SCRIPT [ARGS…]", trailing_var_arg = true, allow_hyphen_values = true)]
    script_argv: Vec<String>,
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
    // Honor --no-color process-wide before anything styles output.
    style::set_no_color(args.no_color);

    // `aish --update`: non-interactive self-upgrade. Check the latest release and,
    // if it's newer for this platform, download + swap the binary, then exit.
    if args.update {
        match update::check().await {
            Ok(Some(info)) => {
                println!(
                    "aish {} is available (you have {}).",
                    info.version,
                    update::current_version()
                );
                update::perform(&info).await?;
            }
            Ok(None) => println!("aish is up to date ({}).", update::current_version()),
            Err(e) => {
                eprintln!("\x1b[31maish:\x1b[0m update check failed: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // `aish --skill-fetch <ref>`: opt-in skill import. Runs WITHOUT a backend
    // or credentials — it only fetches a SKILL.md over HTTPS and writes it into
    // ~/.aish/skills/, then exits. See src/skill_provider.rs.
    if let Some(reference) = args.skill_fetch.as_deref() {
        let skills_dir = aish_dir().join("skills");
        let _ = std::fs::create_dir_all(&skills_dir);
        if let Err(e) = skill_provider::run_fetch(reference, &skills_dir).await {
            eprintln!("\x1b[31maish:\x1b[0m skill fetch failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // `aish --skill-search <query>`: opt-in skill registry search. Like
    // --skill-fetch it needs no backend or credentials. By default it reads the
    // curated, binary-embedded catalog from the local `file://` index at
    // ~/.aish/registry/index.json (set up by skill_provider::initialize_registry
    // on a normal launch); override AISH_SKILL_REGISTRY to query skill.fish or a
    // self-hosted mirror over HTTPS instead. Prints the matches as a table, then
    // exits. See src/skill_provider.rs.
    if let Some(query) = args.skill_search.as_deref() {
        // Ensure the embedded index exists so the default file:// registry has a
        // catalog to search even on the very first run (before a full startup).
        let _ = skill_provider::initialize_registry(&aish_dir());
        if let Err(e) = skill_provider::run_search(query).await {
            eprintln!("\x1b[31maish:\x1b[0m skill search failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Load ~/.aishrc up front, BEFORE building the backend: its `export` lines
    // (credentials included) populate session.env, and the Claude backend
    // resolves its credential from those rc exports as well as the process env —
    // so `export CLAUDE_CODE_OAUTH_TOKEN=…` / `export ANTHROPIC_API_KEY=…` in
    // ~/.aishrc works for aish itself, not just the programs it spawns.
    let rc = rc::load();
    timer.mark("rc::load");
    let mut session = session::Session::new()?;
    timer.mark("Session::new");
    // Login-shell semantics (S4.4 / TASK-127): honor `-l`/`--login` and the
    // classic dash-argv0 convention. A login shell becomes a session leader
    // owning a fresh controlling tty; profile sourcing (S4.5) keys off this.
    let argv0 = std::env::args().next().unwrap_or_default();
    session.login = session::is_login_invocation(args.login, &argv0);
    // The aliases + per-spawn env the session runs with. For a non-login shell
    // these are just ~/.aishrc's; a login shell layers ~/.aishrc OVER the profile
    // files below. The rc fields are consumed into these owners either way.
    let (mut aliases, mut env) = (rc.aliases, rc.env);
    if session.login {
        // Best-effort: setsid() returns EPERM when we are already a process-
        // group leader (the common case under a normal exec) — ignore that and
        // never abort startup on failure.
        // SAFETY: setsid() takes no arguments and only affects this process.
        unsafe { libc::setsid(); }
        // Profile sourcing (S4.5 / TASK-128): source /etc/profile then ~/.profile
        // BENEATH ~/.aishrc. Profiles are the base layer; ~/.aishrc is overlaid on
        // top, so a name set in both resolves to the ~/.aishrc value — the
        // "/etc/profile → ~/.profile → ~/.aishrc" precedence the card specifies.
        // env is a last-wins list (read in reverse), so appending the rc env after
        // the profile env makes rc win; aliases overlay by name for the same effect.
        let profiles = rc::load_login_profiles();
        let mut merged_env = profiles.env;
        merged_env.extend(env);
        env = merged_env;
        let mut merged_aliases = profiles.aliases;
        merged_aliases.extend(aliases);
        aliases = merged_aliases;
        timer.mark("profiles sourced");
    }
    session.env = env;
    // Shell identity (S4.6 / TASK-129): expose the running shell + its pids so
    // spawned children and `$VAR` dispatch see them, and login tooling (`chsh`)
    // finds the right interpreter.
    //   * SHELL — absolute path to THIS aish binary (exported to children).
    //   * PPID  — parent process id (exported to children).
    //   * $$    — this shell's own pid, resolved dynamically by the dispatch
    //             tokenizer (see rc::expand_dollar + repl::var_lookup); not a
    //             stored env entry, matching how POSIX shells treat the special
    //             parameter.
    let shell_path = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "aish".to_string());
    session.set_var("SHELL", shell_path);
    // SAFETY: getppid() always succeeds and is reentrant.
    session.set_var("PPID", (unsafe { libc::getppid() }).to_string());
    // Session identity (session-scoped jobs — docs/session-scoped-jobs.md):
    // export the stable per-session id so every spawned child can tag the work
    // it does back to THIS shell. A background coordinator re-adopts the
    // launching session's id below (from AISH_LAUNCH_SESSION_ID) and re-exports
    // it, so the coordinator and the jobs it spawns all attribute to the human's
    // original session, not the coordinator's throwaway uuid.
    session.set_var("AISH_SESSION_ID", session.session_id.clone());

    // ~/.aish/ — config home. Create it and the skill-registry directory, then
    // write the binary-embedded curated skill index to ~/.aish/registry/index.json
    // (idempotent; refreshed every launch so it tracks this binary). This runs
    // early — after aish_dir exists, before the backend is built — so the default
    // `file://` skill registry always has a catalog to search, even fully offline.
    let aish_dir = aish_dir();
    let _ = std::fs::create_dir_all(&aish_dir);
    let _ = std::fs::create_dir_all(aish_dir.join("registry"));
    if let Err(e) = skill_provider::initialize_registry(&aish_dir) {
        eprintln!("\x1b[33maish:\x1b[0m skill registry init failed: {e:#}");
    }
    timer.mark("registry init");

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

    // ~/.aish/ — config home (created above): .mcp.json (MCP servers) and skills/.
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
    //   * One-shot (-c / --coordinator / a script) — connect SYNCHRONOUSLY: the
    //     run needs the MCP tools immediately, with no interactive loop to
    //     install them into later.
    let interactive = args.command.is_none() && args.script_argv.is_empty();
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
                    // Re-export so any children THIS coordinator spawns inherit
                    // the launching session's id too (the env still carries the
                    // child's throwaway uuid from the startup export above).
                    session.set_var("AISH_SESSION_ID", session.session_id.clone());
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

    // Script mode (TASK-17/18): run the file's lines non-interactively and exit
    // with the status of the last line, like `sh script`. The first operand is
    // the script path; the remaining ones become the script's positional
    // parameters ($1/$2/…, with $0 the script path) so an executable
    // `#!/usr/bin/env aish` script receives the argv the kernel appended (TASK-18).
    // `normalize_script_argv` smooths the Linux/macOS shebang-argv difference.
    if let Some((path, script_args)) = normalize_script_argv(&args.script_argv).split_first() {
        let code = script::run(&backend, &mut session, Path::new(path), script_args).await?;
        std::process::exit(code);
    }

    repl::run(backend, session, aliases, mcp_paths, skills_dir).await
}

fn aish_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish")
}

/// Normalize the script operand vector across the Linux/macOS shebang split
/// (TASK-18). When the kernel runs a `#!/usr/bin/env aish` script it appends the
/// script path before the user's args — but the two platforms disagree on the
/// shape: Linux hands the interpreter `[script, args…]` (path once), while macOS
/// (XNU) hands it `[script, script, args…]` — the path is duplicated. Collapse a
/// single leading exact-duplicate so `$0`/`$1`/`$#` line up identically on both.
/// A normal `aish foo.aish bar` invocation has no duplicate, so this is a no-op
/// there; the only false-positive is the pathological `aish foo foo`, where the
/// repeated operand is intentionally treated as the macOS doubling.
fn normalize_script_argv(argv: &[String]) -> &[String] {
    match argv {
        [first, second, ..] if first == second => &argv[1..],
        _ => argv,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_script_argv;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn normalize_collapses_macos_shebang_duplicate() {
        // macOS (XNU): the kernel duplicates the script path → collapse to one.
        let argv = v(&["/tmp/s.aish", "/tmp/s.aish", "alpha", "beta"]);
        assert_eq!(
            normalize_script_argv(&argv),
            &v(&["/tmp/s.aish", "alpha", "beta"])[..]
        );
    }

    #[test]
    fn normalize_leaves_linux_shebang_untouched() {
        // Linux: the path appears once already → no change.
        let argv = v(&["/tmp/s.aish", "alpha", "beta"]);
        assert_eq!(normalize_script_argv(&argv), &argv[..]);
        // A direct `aish foo.aish` with no args is also untouched.
        let one = v(&["/tmp/s.aish"]);
        assert_eq!(normalize_script_argv(&one), &one[..]);
        // Empty stays empty (no script supplied).
        let none: Vec<String> = Vec::new();
        assert_eq!(normalize_script_argv(&none), &none[..]);
    }

    #[test]
    fn normalize_only_collapses_a_leading_duplicate() {
        // A duplicate that is NOT the leading pair (e.g. a real repeated arg)
        // must survive — only the script-path doubling is collapsed.
        let argv = v(&["/tmp/s.aish", "alpha", "alpha"]);
        assert_eq!(normalize_script_argv(&argv), &argv[..]);
    }
}
