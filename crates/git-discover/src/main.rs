//! `git-discover` CLI — print the repository the given directory (default: the
//! current directory) currently belongs to.
//!
//! Usage:
//!   git-discover [PATH] [--json] [--key] [--quiet]
//!
//! Exit status is `0` when a repo is discovered, `1` when the path isn't inside
//! a git working tree. This makes it usable as a cheap shell/aish predicate:
//! `git-discover --quiet && echo "in a repo"`.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut path: Option<PathBuf> = None;
    let mut as_json = false;
    let mut key_only = false;
    let mut quiet = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--json" | "-j" => as_json = true,
            "--key" | "-k" => key_only = true,
            "--quiet" | "-q" => quiet = true,
            "-h" | "--help" => {
                println!(
                    "git-discover [PATH] [--json|-j] [--key|-k] [--quiet|-q]\n\n\
                     Detect the git repository PATH (default: cwd) belongs to.\n\
                     Exit 0 when inside a repo, 1 otherwise.\n\n\
                     --json   emit the full RepoInfo as JSON\n\
                     --key    print only the stable repo_key\n\
                     --quiet  print nothing; use the exit status as a predicate"
                );
                return ExitCode::SUCCESS;
            }
            other if !other.starts_with('-') && path.is_none() => {
                path = Some(PathBuf::from(other));
            }
            other => {
                eprintln!("git-discover: unrecognized argument `{other}` (try --help)");
                return ExitCode::from(2);
            }
        }
    }

    let info = match &path {
        Some(p) => git_discover::discover(p),
        None => git_discover::discover_here(),
    };

    match info {
        Some(info) => {
            if quiet {
                // predicate mode: exit status only
            } else if key_only {
                println!("{}", info.repo_key);
            } else if as_json {
                println!("{}", info.to_json());
            } else {
                println!("{}", info.summary());
            }
            ExitCode::SUCCESS
        }
        None => {
            if !quiet {
                eprintln!(
                    "git-discover: {} is not inside a git repository",
                    path.as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| ".".to_string())
                );
            }
            ExitCode::FAILURE
        }
    }
}
