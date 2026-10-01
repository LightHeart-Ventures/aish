//! `skill-mirror` — build-time generator for a self-hosted aish skill-registry
//! mirror (TASK-694).
//!
//! Takes a tree of `{owner}/{dir}/SKILL.md` files and emits the two artifacts
//! the aish client already knows how to consume:
//!
//!   index.json          a bare `SearchResult[]`, sorted by `reference`
//!   {owner}/{name}/raw  the verbatim SKILL.md bytes
//!
//! Point aish at the result with `AISH_SKILL_REGISTRY=file:///…/index.json`
//! (search) or an http(s) mirror serving the same tree (search + fetch).
//!
//! Design commitments, all load-bearing:
//!
//!   * **No reimplementation.** Validation calls the client's own
//!     `parse_frontmatter` / `validate_segment` / `sanitize_dir_segment` and
//!     emits the client's own `SearchResult` row type, via the `aish` lib. A
//!     rule can therefore never drift between publisher and consumer.
//!   * **Byte-stable output.** Directory walks are sorted and rows are sorted by
//!     `reference`, so an unchanged input tree regenerates identical bytes.
//!   * **Fail soft per file, fail hard on the catalog.** One bad SKILL.md is a
//!     WARN + skip; a catalog that ended up empty, or smaller than
//!     `--min-rows`, exits non-zero *before* writing anything, so a broken
//!     ingest can never silently publish a shrunken registry over a good one.

mod allowlist;
mod catalog;
mod emit;
mod github;
mod ingest;
mod state;
mod validate;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "skill-mirror",
    version,
    about = "Build an aish skill-registry mirror: ingest allowlisted GitHub repos, then generate index.json + per-skill raw objects"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a SKILL.md tree and emit index.json + the raw object tree.
    Generate(GenerateArgs),
    /// Crawl allowlisted GitHub repos into a SKILL.md tree (`generate`'s input).
    Ingest(ingest::IngestArgs),
}

#[derive(clap::Args)]
struct GenerateArgs {
    /// Input tree of `{owner}/{dir}/SKILL.md` files.
    #[arg(long)]
    input: PathBuf,
    /// Output directory for `index.json` and the `{owner}/{name}/raw` tree.
    #[arg(long)]
    out: PathBuf,
    /// Reject any SKILL.md larger than this many bytes (default 256 KiB).
    #[arg(long, default_value_t = 262_144)]
    max_size: u64,
    /// Fail the run when fewer than this many rows survive validation. The
    /// anti-shrink guard: set it to roughly the last known-good row count so a
    /// half-failed ingest can't publish a gutted catalog.
    #[arg(long, default_value_t = 0)]
    min_rows: usize,
    /// Pretty-print index.json (larger, but reviewable in a diff).
    #[arg(long)]
    pretty: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Generate(args) => report(generate(args)),
        // `generate` is pure filesystem work and stays sync; only the crawl
        // needs an async runtime, so it is built on demand rather than
        // wrapping main in #[tokio::main].
        Command::Ingest(args) => {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("skill-mirror: error: building the async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            report(rt.block_on(ingest::run(args)))
        }
    }
}

/// Collapse a subcommand result into a process exit code, with the error chain
/// on stderr so a nightly job's log says *why* it failed.
fn report(result: anyhow::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("skill-mirror: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn generate(args: GenerateArgs) -> anyhow::Result<ExitCode> {
    let cat = catalog::scan(&args.input, args.max_size)?;

    // Per-file failures: one WARN line each, then a single accounting summary.
    for s in &cat.skipped {
        eprintln!(
            "skill-mirror: WARN skipping {}: {}",
            s.path.display(),
            s.reason
        );
    }
    eprintln!(
        "skill-mirror: skipped {} of {}",
        cat.skipped_count(),
        cat.considered
    );

    // Catalog-level guards run BEFORE any write, so a failed run leaves the
    // previously published artifacts untouched.
    if cat.rows.is_empty() {
        eprintln!(
            "skill-mirror: error: no valid skills found in {}",
            args.input.display()
        );
        return Ok(ExitCode::FAILURE);
    }
    if cat.rows.len() < args.min_rows {
        eprintln!(
            "skill-mirror: error: {} valid rows is below --min-rows {}",
            cat.rows.len(),
            args.min_rows
        );
        return Ok(ExitCode::FAILURE);
    }

    let out = emit::emit(&args.out, &cat.rows, args.pretty)?;
    println!(
        "wrote {} ({} bytes) and {} raw object(s)",
        out.index.display(),
        out.index_bytes,
        out.raw_objects
    );
    Ok(ExitCode::SUCCESS)
}
