//! Oracle test harness (TASK-109 / S1.2) — "brush-style" oracle testing.
//!
//! Each declarative [`Case`] is a single command line. The harness runs it two
//! ways and diffs stdout + exit code:
//!
//! 1. through aish's **native** execution path — the *real* pipeline parser and
//!    executor ([`pipeline::parse`] + [`pipeline::run_captured`]) and the *real*
//!    argv tokenizer ([`rc::tokenize_with`]); and
//! 2. through real `bash -c`.
//!
//! Any divergence fails the test. Scope is deliberately narrow: only the subset
//! aish runs natively (single external commands and multi-stage pipelines of
//! external commands). Lines aish hands to the model instead — globs,
//! redirection, command substitution, `&&`/`||`, prose — are out of scope and
//! reported as [`Native::NotNative`], so a case can assert a line stays
//! model-routed rather than silently passing.
//!
//! The whole module is `#[cfg(test)]` (see `main.rs`): it is test-only
//! infrastructure and never ships in the binary.

use crate::pipeline;
use crate::rc;
use crate::repl::resolve_program;
use crate::session::Session;
use std::process::Stdio;

/// stdout + exit code captured from one execution (aish-native or bash).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    stdout: String,
    code: i32,
}

/// What aish's native router did with a line.
#[derive(Debug)]
enum Native {
    /// aish ran the line itself (pipeline or single command).
    Ran(Outcome),
    /// aish would route the line to the model — outside the oracle subset.
    NotNative,
}

/// A declarative oracle case: a label and the command line under test. `env`
/// entries are exported into BOTH executions so `$VAR` expansion is compared on
/// equal footing.
struct Case {
    name: &'static str,
    line: &'static str,
    env: &'static [(&'static str, &'static str)],
}

/// Exit code as a shell reports it: the process code, or 128+signal when
/// terminated by a signal — matching bash and `Session::set_last_status`.
fn code_of(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// The session's effective PATH: rc exports first, then the process PATH —
/// identical to what `repl::dispatch` resolves programs against.
fn path_var(session: &Session) -> String {
    session
        .env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default()
}

/// A fresh session whose `env` carries the case's exported variables.
fn session_with_env(env: &[(&str, &str)]) -> Session {
    let mut session = Session::new().expect("session");
    session.env = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    session
}

/// Run `line` through aish's native execution path, capturing stdout + exit
/// code. Mirrors `repl::dispatch`'s routing: a parsable pipeline whose every
/// stage resolves to a real program runs via the real pipeline executor; else a
/// line that tokenizes to a resolvable single command runs directly; anything
/// else is [`Native::NotNative`] (aish would route it to the model).
async fn run_native(line: &str, session: &Session) -> Native {
    let path = path_var(session);

    // 1. Native pipeline path — the real parser + the real executor.
    if let Some(stages) = pipeline::parse(line) {
        let all_real = stages.iter().all(|s| {
            s.argv
                .first()
                .is_some_and(|p| resolve_program(p, &session.cwd, &path).is_some())
        });
        if !all_real {
            return Native::NotNative;
        }
        let (status, stdout) = pipeline::run_captured(&stages, session)
            .await
            .expect("pipeline execution failed");
        return Native::Ran(Outcome {
            stdout,
            code: code_of(&status),
        });
    }

    // 2. Direct-dispatch path — the real tokenizer, then a captured spawn of the
    //    resolved program. (`tools::run_on_tty` inherits the terminal and can't
    //    be captured, so the oracle execs the identical program/args/cwd/env.)
    let lookup = |name: &str| {
        session
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(name).ok())
    };
    let Some(words) = rc::tokenize_with(line, lookup) else {
        return Native::NotNative;
    };
    let Some(program) = words.first() else {
        return Native::NotNative;
    };
    let Some(program_path) = resolve_program(program, &session.cwd, &path) else {
        return Native::NotNative;
    };
    let output = tokio::process::Command::new(program_path)
        .args(&words[1..])
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .await
        .expect("failed to run native command");
    Native::Ran(Outcome {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        code: code_of(&output.status),
    })
}

/// Run `line` through real bash (`bash -c`), with the same cwd and exported env
/// the native path saw, capturing stdout + exit code.
async fn run_bash(line: &str, session: &Session) -> Outcome {
    let output = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(line)
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .await
        .expect("failed to run bash");
    Outcome {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        code: code_of(&output.status),
    }
}

/// The declarative oracle suite. Every line is in aish's native subset and is
/// expected to match bash byte-for-byte on stdout + exit code. Both sides invoke
/// the same external binaries, so the only divergence source is aish's parsing
/// vs bash's — which is exactly what this suite pins down.
const CASES: &[Case] = &[
    Case {
        name: "echo_simple",
        line: "echo hello",
        env: &[],
    },
    Case {
        name: "echo_double_quoted_spaces",
        line: "echo \"a  b\"",
        env: &[],
    },
    Case {
        name: "echo_single_quoted_pipe_literal",
        line: "echo 'a|b'",
        env: &[],
    },
    Case {
        name: "printf_newlines",
        line: "printf 'a\\nb\\nc\\n'",
        env: &[],
    },
    Case {
        name: "true_exit_zero",
        line: "true",
        env: &[],
    },
    Case {
        name: "false_exit_one",
        line: "false",
        env: &[],
    },
    Case {
        name: "var_in_double_quotes",
        line: "printf '%s' \"$WORD\"",
        env: &[("WORD", "solid")],
    },
    Case {
        name: "seq_grep_wc_pipeline",
        line: "seq 1 100 | grep 5 | wc -l",
        env: &[],
    },
    Case {
        name: "printf_sort_pipeline",
        line: "printf 'c\\na\\nb\\n' | sort",
        env: &[],
    },
    Case {
        name: "echo_tr_pipeline",
        line: "echo hi | tr a-z A-Z",
        env: &[],
    },
    Case {
        name: "pipeline_exit_last_stage_ok",
        line: "false | true",
        env: &[],
    },
    Case {
        name: "pipeline_exit_last_stage_fail",
        line: "true | false",
        env: &[],
    },
    Case {
        name: "yes_head_wc_pipeline",
        line: "yes | head -n 3 | wc -l",
        env: &[],
    },
];

/// AC1: every declarative case runs through aish's native path and matches bash
/// on stdout + exit code. This is the body the CI job executes.
#[tokio::test]
async fn declarative_cases_match_bash() {
    let mut failures = Vec::new();
    for case in CASES {
        let session = session_with_env(case.env);
        let native = match run_native(case.line, &session).await {
            Native::Ran(outcome) => outcome,
            Native::NotNative => {
                failures.push(format!(
                    "[{}] `{}` was not run natively (routed to model) — not a valid in-subset case",
                    case.name, case.line
                ));
                continue;
            }
        };
        let bash = run_bash(case.line, &session).await;
        if native != bash {
            failures.push(format!(
                "[{}] `{}` diverged from bash:\n  native: {native:?}\n  bash:   {bash:?}",
                case.name, case.line
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "oracle divergences:\n{}",
        failures.join("\n")
    );
}

/// AC2: a deliberate divergence from bash must be *caught* by the harness.
///
/// `printf '[%s]\n' $V` with `V="a b"` is a genuine aish-vs-bash semantic gap:
/// aish never re-splits an expanded `$VAR`, so it runs printf with the single
/// argument `a b` → `[a b]`; bash word-splits the expansion into two args →
/// `[a]\n[b]`. The harness must report this as a divergence — proving it would
/// fail a run if aish ever diverged on a case that's supposed to match.
#[tokio::test]
async fn deliberate_divergence_is_detected() {
    let session = session_with_env(&[("V", "a b")]);
    let line = "printf '[%s]\\n' $V";

    let native = match run_native(line, &session).await {
        Native::Ran(outcome) => outcome,
        Native::NotNative => panic!("expected `{line}` to run natively"),
    };
    let bash = run_bash(line, &session).await;

    // Pin the exact divergence we engineered, so this stays a real test of the
    // word-splitting gap rather than an accidental mismatch.
    assert_eq!(native.stdout, "[a b]\n", "native must not word-split $V");
    assert_eq!(bash.stdout, "[a]\n[b]\n", "bash must word-split $V");

    assert!(
        native != bash,
        "harness FAILED to detect a deliberate divergence — the oracle would pass a broken aish"
    );
}

/// Scope guard: lines using shell machinery aish doesn't implement are reported
/// as [`Native::NotNative`], never silently executed — keeping the oracle honest
/// about covering "only the subset aish runs natively".
#[tokio::test]
async fn out_of_subset_lines_are_not_native() {
    let session = session_with_env(&[]);
    for line in ["echo *", "cat a > b", "echo $(date)", "true && false"] {
        assert!(
            matches!(run_native(line, &session).await, Native::NotNative),
            "`{line}` should route to the model, not run natively"
        );
    }
}
