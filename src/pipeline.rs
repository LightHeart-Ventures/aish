//! Pipeline execution — `a | b | c` run as connected OS processes.
//!
//! Direct dispatch (`tools::run_on_tty`) runs one program at a time. A pipeline
//! is the single piece of shell syntax aish runs itself rather than handing to
//! the model: each stage's stdout is wired to the next stage's stdin through a
//! kernel pipe, the first stage inherits the terminal's stdin and the last its
//! stdout, and every stage shares aish's foreground process group. A terminal
//! Ctrl-C therefore reaches all stages at once — the existing SIGINT routing
//! (aish's handler swallows the signal for itself; the children, lacking that
//! handler, take the default terminate action) needs no extra job-control code.

use crate::rc;
use crate::session::Session;
use anyhow::Result;
use std::process::{ExitStatus, Stdio};

/// Split a command line into pipeline stages on top-level `|`, then tokenize
/// each stage. Returns `None` when the line is not a pipeline aish runs
/// directly: no unquoted `|` (a plain command — the single-command path owns
/// it), a `||` (logical-or, unimplemented — route to the model), an empty stage
/// (`a |`, `| b`, `a | | b`), or a stage using shell syntax `tokenize` rejects
/// (`$`, `>`, globs, …).
pub fn parse(line: &str) -> Option<Vec<Vec<String>>> {
    let raw = split_top_level(line)?;
    let mut stages = Vec::with_capacity(raw.len());
    for segment in raw {
        let words = rc::tokenize(segment)?;
        if words.is_empty() {
            return None; // empty stage — malformed pipeline
        }
        stages.push(words);
    }
    (stages.len() >= 2).then_some(stages)
}

/// Split on unquoted pipe characters. `None` if there is no top-level pipe, or a
/// `||` (logical-or) is found.
fn split_top_level(line: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let (mut in_single, mut in_double) = (false, false);
    let mut chars = line.char_indices().peekable();
    let mut found = false;
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '|' if !in_single && !in_double => {
                if matches!(chars.peek(), Some((_, '|'))) {
                    return None; // `||` is logical-or, not a pipe
                }
                segments.push(&line[start..i]);
                start = i + 1; // '|' is one ASCII byte
                found = true;
            }
            _ => {}
        }
    }
    if !found {
        return None;
    }
    segments.push(&line[start..]);
    Some(segments)
}

/// Spawn every stage with stdout→stdin wired between neighbours, wait for all,
/// and return the last stage's exit status (the pipeline's status, as in any
/// shell). Stages run concurrently; aish keeps no pipe ends of its own, so
/// reaping them in order cannot deadlock.
pub async fn run(stages: &[Vec<String>], session: &Session) -> Result<ExitStatus> {
    Ok(exec(stages, session, false).await?.0)
}

/// Like [`run`], but capture the final stage's stdout instead of inheriting the
/// terminal, returning it alongside the exit status. The oracle test harness
/// (TASK-109) uses this to diff aish's real pipeline execution against bash —
/// sharing [`exec`] with [`run`] so the harness exercises the production wiring
/// and exit-status logic, not a reimplementation.
#[cfg(test)]
pub(crate) async fn run_captured(
    stages: &[Vec<String>],
    session: &Session,
) -> Result<(ExitStatus, String)> {
    let (status, captured) = exec(stages, session, true).await?;
    Ok((status, captured.unwrap_or_default()))
}

/// Shared pipeline executor. When `capture` is false the final stage's stdout
/// stays inherited (the terminal), as an interactive shell needs; when true it
/// is piped and collected and returned in the second tuple slot.
async fn exec(
    stages: &[Vec<String>],
    session: &Session,
    capture: bool,
) -> Result<(ExitStatus, Option<String>)> {
    let n = stages.len();
    let mut children: Vec<tokio::process::Child> = Vec::with_capacity(n);
    let mut prev_stdout: Option<tokio::process::ChildStdout> = None;

    for (i, stage) in stages.iter().enumerate() {
        let (program, args) = stage.split_first().expect("parse() rejects empty stages");
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .current_dir(&session.cwd)
            .envs(session.env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true);
        // First stage inherits the terminal's stdin; every later stage reads the
        // previous stage's stdout. Earlier stages pipe theirs onward; the last
        // stage's stdout stays inherited (the terminal) unless we're capturing.
        // stderr is always the terminal, so each stage's diagnostics stay visible.
        if let Some(out) = prev_stdout.take() {
            cmd.stdin(TryInto::<Stdio>::try_into(out)?);
        }
        if i < n - 1 || capture {
            cmd.stdout(Stdio::piped());
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to exec {program}: {e}"))?;
        if i < n - 1 {
            prev_stdout = Some(child.stdout.take().expect("piped stdout"));
        }
        children.push(child);
    }

    // When capturing, drain the final stage's stdout concurrently with the reap
    // loop so a large output can't fill the pipe and deadlock the waits.
    let capture_task = capture.then(|| {
        let out = children
            .last_mut()
            .expect("pipeline has at least two stages")
            .stdout
            .take()
            .expect("piped stdout");
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut out = out;
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf).await;
            buf
        })
    });

    // Reap every stage; the last one's status is the pipeline's status.
    let mut last = None;
    for (i, mut child) in children.into_iter().enumerate() {
        let status = child
            .wait()
            .await
            .map_err(|e| anyhow::anyhow!("failed to wait on stage {}: {e}", i + 1))?;
        if i == n - 1 {
            last = Some(status);
        }
    }
    let captured = match capture_task {
        Some(task) => Some(String::from_utf8_lossy(&task.await.unwrap_or_default()).into_owned()),
        None => None,
    };
    Ok((last.expect("pipeline has at least two stages"), captured))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(stages: &[&[&str]]) -> Vec<Vec<String>> {
        stages
            .iter()
            .map(|s| s.iter().map(|w| w.to_string()).collect())
            .collect()
    }

    #[test]
    fn parse_splits_stages() {
        assert_eq!(
            parse("cat big.log | grep ERROR | wc -l").unwrap(),
            argv(&[&["cat", "big.log"], &["grep", "ERROR"], &["wc", "-l"]])
        );
        // tight spacing around the pipe
        assert_eq!(parse("ls|wc -l").unwrap(), argv(&[&["ls"], &["wc", "-l"]]));
    }

    #[test]
    fn parse_rejects_non_pipelines() {
        // a plain command is the single-command path's job
        assert!(parse("ls -la").is_none());
        // logical-or is not a pipe
        assert!(parse("a || b").is_none());
        // malformed: empty stages
        assert!(parse("a |").is_none());
        assert!(parse("| b").is_none());
        assert!(parse("a | | b").is_none());
        // a stage using other shell syntax routes the whole line to the model
        assert!(parse("a | b > c").is_none());
        // ($VAR in a stage is no longer machinery — the tokenizer expands it)
        assert_eq!(parse("cat x | grep $HOME").unwrap().len(), 2);
    }

    #[test]
    fn parse_does_not_split_quoted_pipe() {
        // the `|` is inside quotes — one stage — so this is not a pipeline
        assert!(parse("grep 'a|b' file").is_none());
        // a real pipe whose stage also contains a quoted pipe still splits once
        assert_eq!(
            parse("grep 'a|b' file | wc -l").unwrap(),
            argv(&[&["grep", "a|b", "file"], &["wc", "-l"]])
        );
    }

    #[tokio::test]
    async fn pipeline_wires_stdout_to_stdin() {
        // yes | head -n 3 | wc -l  →  3 ; grep -q checks it without capturing.
        let session = Session::new().unwrap();
        let stages = parse("yes | head -n 3 | wc -l | grep -q 3").unwrap();
        let status = run(&stages, &session).await.unwrap();
        assert_eq!(status.code(), Some(0), "stdout did not reach the next stage");
    }

    #[tokio::test]
    async fn exit_status_is_the_last_stage() {
        let session = Session::new().unwrap();
        // an earlier failure must NOT mask the last stage's success …
        let ok = run(&parse("false | false | true").unwrap(), &session).await.unwrap();
        assert_eq!(ok.code(), Some(0));
        // … and the last stage's failure must surface.
        let bad = run(&parse("true | true | false").unwrap(), &session).await.unwrap();
        assert_eq!(bad.code(), Some(1));
    }

    #[tokio::test]
    async fn cat_grep_wc_matches_expected_count() {
        // AC-1: cat big.log | grep ERROR | wc -l equals the real ERROR count.
        let dir = std::env::temp_dir().join(format!("aish_pipe_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for i in 0..1000 {
            body.push_str(if i % 3 == 0 { "ERROR boom\n" } else { "info fine\n" });
        }
        std::fs::write(dir.join("big.log"), &body).unwrap();
        let expected = body.lines().filter(|l| l.contains("ERROR")).count();

        let out = dir.join("count.txt");
        let mut session = Session::new().unwrap();
        session.cwd = dir.clone();
        // dd captures the final stdout to a file without a shell redirection.
        let cmd = format!("cat big.log | grep ERROR | wc -l | dd of={}", out.display());
        let status = run(&parse(&cmd).unwrap(), &session).await.unwrap();
        assert!(status.success());

        let got: usize = std::fs::read_to_string(&out).unwrap().trim().parse().unwrap();
        assert_eq!(got, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- TASK-109: oracle harness -----------------------------------------
    //
    // A differential test: run a corpus of pipelines through aish's *native*
    // executor (`pipeline::run`) and through a real `bash -c`, then assert the
    // two agree on stdout bytes and on exit status. bash is the oracle — the
    // ground truth aish's no-shell pipeline must reproduce byte-for-byte. The
    // corpus is deliberately env- and locale-stable (integer/ASCII coreutils,
    // numeric sorts) so a mismatch means an aish regression, not a flaky locale.
    //
    // aish's last stage writes to the inherited terminal, so to capture its
    // stdout we append a `dd of=<file>` sink (aish has no `>`) exactly as the
    // AC-1 test does. bash's stdout is captured directly via a pipe.

    use std::sync::atomic::{AtomicUsize, Ordering};
    static ORACLE_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Run `cmd` through aish's pipeline executor, capturing the final stdout
    /// into a temp file via an appended `dd` sink. Returns the captured bytes.
    async fn aish_stdout(cmd: &str, session: &Session) -> Vec<u8> {
        let n = ORACLE_SEQ.fetch_add(1, Ordering::Relaxed);
        let sink = std::env::temp_dir().join(format!("aish_oracle_{}_{n}.out", std::process::id()));
        let piped = format!("{cmd} | dd of={}", sink.display());
        let stages = parse(&piped).expect("oracle command must be a pipeline aish runs directly");
        let status = run(&stages, session).await.expect("aish pipeline run");
        assert!(status.success(), "dd capture sink failed for `{cmd}`");
        let bytes = std::fs::read(&sink).unwrap_or_default();
        let _ = std::fs::remove_file(&sink);
        bytes
    }

    /// Run `cmd` through the oracle (`bash -c`) in `cwd`, returning its stdout.
    fn bash_stdout(cmd: &str, cwd: &std::path::Path) -> Vec<u8> {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .output()
            .expect("spawn bash oracle (is bash on PATH?)")
            .stdout
    }

    /// Exit code of `cmd` under the oracle (`bash -c`) in `cwd`.
    fn bash_code(cmd: &str, cwd: &std::path::Path) -> Option<i32> {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .status()
            .expect("spawn bash oracle")
            .code()
    }

    #[tokio::test]
    async fn oracle_stdout_matches_bash() {
        let session = Session::new().unwrap();
        // Each entry is a pipeline aish runs natively; bash is the ground truth.
        let corpus = [
            "seq 1 20 | sort -rn | head -n 3",
            "seq 1 100 | grep 7 | wc -l",
            "seq 1 9 | tr 0-9 a-j",
            "yes hello | head -n 4 | wc -l",
            "echo one two three four | wc -w",
            "seq 1 3 | sort -r",
            "seq 1 50 | grep -c 2",
        ];
        for cmd in corpus {
            let got = aish_stdout(cmd, &session).await;
            let want = bash_stdout(cmd, &session.cwd);
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want),
                "aish stdout diverged from bash for `{cmd}`"
            );
        }
    }

    #[tokio::test]
    async fn oracle_exit_status_matches_bash() {
        let session = Session::new().unwrap();
        // Default (no pipefail) pipeline status == last stage status in both.
        let corpus = [
            "true | true | false",
            "false | false | true",
            "seq 1 3 | grep 2",
            "seq 1 3 | grep 9",
        ];
        for cmd in corpus {
            let got = run(&parse(cmd).unwrap(), &session).await.unwrap().code();
            let want = bash_code(cmd, &session.cwd);
            assert_eq!(got, want, "aish exit status diverged from bash for `{cmd}`");
        }
    }

    #[tokio::test]
    async fn oracle_detects_deliberate_divergence() {
        // The harness must have teeth: feed aish and the oracle *different*
        // commands and confirm the comparator sees them as unequal. If this
        // ever passes as equal, the oracle is blind and the matching tests
        // above are worthless.
        let session = Session::new().unwrap();
        let aish = aish_stdout("seq 1 5 | sort -r", &session).await;
        let bash = bash_stdout("seq 1 5 | sort", &session.cwd); // ascending, not reversed
        assert_ne!(
            String::from_utf8_lossy(&aish),
            String::from_utf8_lossy(&bash),
            "oracle failed to detect an intentional divergence — the harness is blind"
        );
    }
}
