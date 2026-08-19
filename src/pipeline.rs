//! Pipeline & redirection execution — `a | b | c`, `cmd > file`, `cmd < in`,
//! `cmd 2>&1`, and combinations, run as connected OS processes.
//!
//! Direct dispatch (`tools::run_on_tty`) runs one program at a time with a full
//! interactive TTY. A pipeline — or any command carrying an I/O redirection —
//! is the piece of shell syntax aish runs itself rather than handing to the
//! model: each stage's stdout is wired to the next stage's stdin through a
//! kernel pipe, redirections are applied per stage on top of that wiring, and
//! every stage shares aish's foreground process group so a terminal Ctrl-C
//! reaches them all at once (aish's SIGINT handler swallows the signal for
//! itself; the children take the default terminate action).
//!
//! Redirections are resolved with real file descriptors (open/dup/pipe) so
//! `2>&1`, `&>file`, and `< in` behave like a POSIX shell, including inside a
//! pipeline (`cmd 2>&1 | next`).

use crate::rc::{self, FileMode, Redir};
use crate::session::Session;
use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::{ExitStatus, Stdio};

/// One stage of a command line: the program + args, plus any explicit I/O
/// redirections attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub argv: Vec<String>,
    pub redirs: Vec<Redir>,
}

/// Parse a command line into stages aish runs directly. Returns `Some` when the
/// line is a **multi-stage pipeline** or **any stage carries a redirection**;
/// returns `None` for a bare single command with no redirection (so the
/// interactive `run_on_tty` path keeps owning plain foreground programs), a
/// `||` (logical-or), an empty stage (`a |`, `| b`), or a stage using shell
/// syntax the redirection-aware tokenizer rejects (`$(...)`, globs, `;`, …).
pub fn parse(line: &str) -> Option<Vec<Stage>> {
    let segments = split_top_level(line)?;
    let piped = segments.len() > 1;
    let mut stages = Vec::with_capacity(segments.len());
    let mut any_redir = false;
    for segment in segments {
        let (argv, redirs) = rc::tokenize_redir(segment, |name| std::env::var(name).ok()).ok()?;
        if argv.is_empty() {
            return None; // empty stage — malformed, or a redir with no command
        }
        any_redir |= !redirs.is_empty();
        stages.push(Stage { argv, redirs });
    }
    (piped || any_redir).then_some(stages)
}

/// Split on unquoted top-level `|`. Returns a single segment when there is no
/// pipe (so a lone command-with-redirection still parses), and `None` on a
/// `||` (logical-or) or an unbalanced quote.
fn split_top_level(line: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let (mut in_single, mut in_double) = (false, false);
    let mut chars = line.char_indices().peekable();
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
            }
            _ => {}
        }
    }
    if in_single || in_double {
        return None; // unbalanced quote
    }
    segments.push(&line[start..]);
    Some(segments)
}

/// Spawn every stage (stdout→stdin wired between neighbours, redirections
/// applied per stage), wait for all, and return the last stage's exit status
/// (the pipeline's status, as in any shell).
pub async fn run(stages: &[Stage], session: &Session) -> Result<ExitStatus> {
    Ok(exec(stages, session, false).await?.0)
}

/// Like [`run`], but capture the final stage's stdout instead of inheriting the
/// terminal. Used by the oracle test harness to diff aish's real execution
/// against bash — sharing [`exec`] so the harness exercises production wiring.
#[cfg(test)]
pub(crate) async fn run_captured(
    stages: &[Stage],
    session: &Session,
) -> Result<(ExitStatus, String)> {
    let (status, captured) = exec(stages, session, true).await?;
    Ok((status, captured.unwrap_or_default()))
}

/// Where an fd is connected for a stage while redirections are being resolved.
enum Sink {
    /// Inherit the parent's fd of this number (the terminal, a pipe end, …).
    Inherit(RawFd),
    /// A concrete owned descriptor (an opened file or a duplicated fd).
    Owned(OwnedFd),
    /// Closed (`n>&-`).
    Null,
}

/// `pipe()` → (read, write), both marked CLOEXEC. CLOEXEC is required: it keeps
/// a pipe end from leaking into a *sibling* stage's child (which would keep a
/// downstream reader from ever seeing EOF and hang a 3+ stage pipeline).
/// `Stdio::from` re-dups without CLOEXEC into the one child that should see it.
/// Portable `pipe` + `fcntl` is used instead of Linux-only `pipe2` so the crate
/// still builds on macOS.
fn make_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds is a valid 2-element array; pipe fills it or returns < 0.
    let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe");
    }
    // SAFETY: on success both fds are freshly-owned open descriptors.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(read.as_raw_fd())?;
    set_cloexec(write.as_raw_fd())?;
    Ok((read, write))
}

/// Set the close-on-exec flag on a raw fd.
fn set_cloexec(fd: RawFd) -> Result<()> {
    // SAFETY: F_GETFD/F_SETFD on a valid fd; return values are checked.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl F_GETFD");
    }
    let r = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if r < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl F_SETFD");
    }
    Ok(())
}

/// `dup(2)` a raw fd into a fresh owned descriptor.
fn dup_raw(fd: RawFd) -> Result<OwnedFd> {
    // SAFETY: dup of a (presumed valid) inherited std fd; checked below.
    let n = unsafe { libc::dup(fd) };
    if n < 0 {
        return Err(std::io::Error::last_os_error()).context("dup");
    }
    // SAFETY: n is a freshly-owned descriptor on success.
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

/// Clone a sink so two fds point at the *same* destination (the `2>&1` case):
/// a file/pipe is `dup`'d (shared open file description, correct offset), an
/// inherited fd stays inherited, a closed fd stays closed.
fn clone_sink(s: &Sink) -> Result<Sink> {
    Ok(match s {
        Sink::Inherit(fd) => Sink::Inherit(*fd),
        Sink::Owned(fd) => Sink::Owned(dup_raw(fd.as_raw_fd())?),
        Sink::Null => Sink::Null,
    })
}

/// Materialize a resolved sink into a `Stdio` for the child.
fn sink_to_stdio(s: Sink) -> Result<Stdio> {
    Ok(match s {
        // dup so ownership is clean and the same parent fd can back several
        // children/slots; the child sees an identical destination.
        Sink::Inherit(fd) => dup_raw(fd)?.into(),
        Sink::Owned(fd) => fd.into(),
        Sink::Null => Stdio::null(),
    })
}

/// Open a redirection target relative to the session cwd.
fn open_redir(session: &Session, path: &str, mode: &FileMode) -> Result<std::fs::File> {
    let full = session.cwd.join(path);
    let mut opts = std::fs::OpenOptions::new();
    match mode {
        FileMode::Read => {
            opts.read(true);
        }
        FileMode::Write => {
            opts.write(true).create(true).truncate(true);
        }
        FileMode::Append => {
            opts.write(true).create(true).append(true);
        }
    }
    opts.open(&full)
        .with_context(|| format!("{}: {}", path, redir_verb(mode)))
}

fn redir_verb(mode: &FileMode) -> &'static str {
    match mode {
        FileMode::Read => "cannot open for reading",
        FileMode::Write | FileMode::Append => "cannot open for writing",
    }
}

/// Assign `val` to the sink for fd 0/1/2; higher fds are ignored (rejected at
/// parse time, so unreachable in practice).
fn set_slot(fd: i32, val: Sink, sin: &mut Sink, sout: &mut Sink, serr: &mut Sink) {
    match fd {
        0 => *sin = val,
        1 => *sout = val,
        2 => *serr = val,
        _ => {}
    }
}

/// Apply one redirection to the current sink set, in shell left-to-right order.
fn apply_redir(
    r: &Redir,
    session: &Session,
    sin: &mut Sink,
    sout: &mut Sink,
    serr: &mut Sink,
) -> Result<()> {
    match r {
        Redir::File { fd, mode, path } => {
            let f = open_redir(session, path, mode)?;
            set_slot(*fd, Sink::Owned(f.into()), sin, sout, serr);
        }
        Redir::Both { append, path } => {
            let mode = if *append {
                FileMode::Append
            } else {
                FileMode::Write
            };
            let f = open_redir(session, path, &mode)?;
            let owned: OwnedFd = f.into();
            let dup = dup_raw(owned.as_raw_fd())?;
            *sout = Sink::Owned(owned);
            *serr = Sink::Owned(dup);
        }
        Redir::Dup { fd, from } => {
            let cloned = match *from {
                0 => clone_sink(sin)?,
                1 => clone_sink(sout)?,
                2 => clone_sink(serr)?,
                _ => return Ok(()),
            };
            set_slot(*fd, cloned, sin, sout, serr);
        }
        Redir::Close { fd } => set_slot(*fd, Sink::Null, sin, sout, serr),
    }
    Ok(())
}

/// Shared executor. `capture=false` leaves the final stage's stdout inherited
/// (the terminal); `capture=true` pipes and collects it.
async fn exec(
    stages: &[Stage],
    session: &Session,
    capture: bool,
) -> Result<(ExitStatus, Option<String>)> {
    let n = stages.len();

    // Inter-stage pipes: pipe[i] connects stage i's stdout to stage i+1's stdin.
    let mut pipes: Vec<(Option<OwnedFd>, Option<OwnedFd>)> = Vec::with_capacity(n.saturating_sub(1));
    for _ in 0..n.saturating_sub(1) {
        let (r, w) = make_pipe()?;
        pipes.push((Some(r), Some(w)));
    }
    // Capture pipe for the final stage's stdout.
    let (mut cap_read, mut cap_write) = (None, None);
    if capture {
        let (r, w) = make_pipe()?;
        cap_read = Some(r);
        cap_write = Some(w);
    }

    let mut children: Vec<tokio::process::Child> = Vec::with_capacity(n);
    for (i, stage) in stages.iter().enumerate() {
        let (program, args) = stage
            .argv
            .split_first()
            .expect("parse() rejects empty stages");

        // Default fd wiring before redirections.
        let mut sin = if i == 0 {
            Sink::Inherit(0)
        } else {
            Sink::Owned(pipes[i - 1].0.take().expect("read end"))
        };
        let mut sout = if i < n - 1 {
            Sink::Owned(pipes[i].1.take().expect("write end"))
        } else if capture {
            Sink::Owned(cap_write.take().expect("capture write end"))
        } else {
            Sink::Inherit(1)
        };
        let mut serr = Sink::Inherit(2);

        for r in &stage.redirs {
            apply_redir(r, session, &mut sin, &mut sout, &mut serr)?;
        }

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .current_dir(&session.cwd)
            .envs(session.env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true)
            .stdin(sink_to_stdio(sin)?)
            .stdout(sink_to_stdio(sout)?)
            .stderr(sink_to_stdio(serr)?);
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to exec {program}: {e}"))?;
        children.push(child);
        // `cmd` drops here, closing the parent's copy of this stage's pipe write
        // end so the next stage's reader sees EOF once this stage exits.
    }
    drop(pipes); // release any ends we never consumed

    // Drain the captured stdout concurrently with the reap loop so a large
    // output can't fill the pipe and deadlock the waits.
    let capture_task = cap_read.take().map(|r| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut f = tokio::fs::File::from_std(std::fs::File::from(r));
            let mut buf = Vec::new();
            let _ = f.read_to_end(&mut buf).await;
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
    Ok((last.expect("pipeline has at least one stage"), captured))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stages(spec: &[&[&str]]) -> Vec<Stage> {
        spec.iter()
            .map(|s| Stage {
                argv: s.iter().map(|w| w.to_string()).collect(),
                redirs: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn parse_splits_stages() {
        assert_eq!(
            parse("cat big.log | grep ERROR | wc -l").unwrap(),
            stages(&[&["cat", "big.log"], &["grep", "ERROR"], &["wc", "-l"]])
        );
        assert_eq!(parse("ls|wc -l").unwrap(), stages(&[&["ls"], &["wc", "-l"]]));
    }

    #[test]
    fn parse_rejects_non_pipelines_without_redir() {
        // a plain command with no redirection is the single-command path's job
        assert!(parse("ls -la").is_none());
        // logical-or is not a pipe
        assert!(parse("a || b").is_none());
        // malformed: empty stages
        assert!(parse("a |").is_none());
        assert!(parse("| b").is_none());
        assert!(parse("a | | b").is_none());
        // $VAR in a stage still expands (the tokenizer handles it)
        assert_eq!(parse("cat x | grep $HOME").unwrap().len(), 2);
    }

    #[test]
    fn parse_does_not_split_quoted_pipe() {
        // the `|` is inside quotes — one stage, no redir — so not a pipeline
        assert!(parse("grep 'a|b' file").is_none());
        assert_eq!(
            parse("grep 'a|b' file | wc -l").unwrap(),
            stages(&[&["grep", "a|b", "file"], &["wc", "-l"]])
        );
    }

    #[test]
    fn parse_accepts_redirections() {
        // a single command with a redirection now parses directly
        let s = parse("sort < in.txt").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].argv, vec!["sort"]);
        assert_eq!(
            s[0].redirs,
            vec![Redir::File {
                fd: 0,
                mode: FileMode::Read,
                path: "in.txt".into()
            }]
        );
        // stdout truncate + append
        assert_eq!(
            parse("echo hi > out").unwrap()[0].redirs,
            vec![Redir::File {
                fd: 1,
                mode: FileMode::Write,
                path: "out".into()
            }]
        );
        assert_eq!(
            parse("echo hi >> out").unwrap()[0].redirs,
            vec![Redir::File {
                fd: 1,
                mode: FileMode::Append,
                path: "out".into()
            }]
        );
        // explicit fd + dup
        assert_eq!(
            parse("cmd 2> err").unwrap()[0].redirs,
            vec![Redir::File {
                fd: 2,
                mode: FileMode::Write,
                path: "err".into()
            }]
        );
        assert_eq!(
            parse("cmd > out 2>&1").unwrap()[0].redirs,
            vec![
                Redir::File {
                    fd: 1,
                    mode: FileMode::Write,
                    path: "out".into()
                },
                Redir::Dup { fd: 2, from: 1 }
            ]
        );
        // &> both
        assert_eq!(
            parse("cmd &> all.log").unwrap()[0].redirs,
            vec![Redir::Both {
                append: false,
                path: "all.log".into()
            }]
        );
        // redirection inside a pipeline
        let p = parse("cat f | grep x > out").unwrap();
        assert_eq!(p.len(), 2);
        assert!(p[0].redirs.is_empty());
        assert_eq!(p[1].argv, vec!["grep", "x"]);
        assert_eq!(p[1].redirs.len(), 1);
    }

    #[test]
    fn parse_rejects_unsupported_syntax() {
        assert!(parse("echo a && echo b").is_none()); // and-list
        assert!(parse("echo a ; echo b").is_none()); // sequence
        assert!(parse("echo `date`").is_none()); // command substitution
        assert!(parse("echo >").is_none()); // redirection with no target
    }

    #[tokio::test]
    async fn pipeline_wires_stdout_to_stdin() {
        let session = Session::new().unwrap();
        let stages = parse("yes | head -n 3 | wc -l | grep -q 3").unwrap();
        let status = run(&stages, &session).await.unwrap();
        assert_eq!(status.code(), Some(0), "stdout did not reach the next stage");
    }

    #[tokio::test]
    async fn exit_status_is_the_last_stage() {
        let session = Session::new().unwrap();
        let ok = run(&parse("false | false | true").unwrap(), &session)
            .await
            .unwrap();
        assert_eq!(ok.code(), Some(0));
        let bad = run(&parse("true | true | false").unwrap(), &session)
            .await
            .unwrap();
        assert_eq!(bad.code(), Some(1));
    }

    #[tokio::test]
    async fn redirect_stdout_truncate_and_append() {
        let dir = std::env::temp_dir().join(format!("aish_redir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = Session::new().unwrap();
        session.cwd = dir.clone();

        run(&parse("printf one > f.txt").unwrap(), &session)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "one");
        // append adds without truncating
        run(&parse("printf two >> f.txt").unwrap(), &session)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "onetwo");
        // plain `>` truncates
        run(&parse("printf x > f.txt").unwrap(), &session)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn redirect_stdin_from_file() {
        let dir = std::env::temp_dir().join(format!("aish_redir_in_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in.txt"), "a\nb\nc\n").unwrap();
        let mut session = Session::new().unwrap();
        session.cwd = dir.clone();
        // wc -l < in.txt | grep -q 3
        let status = run(&parse("wc -l < in.txt | grep -q 3").unwrap(), &session)
            .await
            .unwrap();
        assert_eq!(status.code(), Some(0), "stdin redirection did not feed wc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn redirect_stderr_merge_2to1() {
        let dir = std::env::temp_dir().join(format!("aish_redir_err_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = Session::new().unwrap();
        session.cwd = dir.clone();
        // `ls` a missing path: its error goes to stderr, merged to the file via 2>&1
        let cmd = "ls definitely-missing-xyz > out.txt 2>&1";
        let _ = run(&parse(cmd).unwrap(), &session).await.unwrap();
        let body = std::fs::read_to_string(dir.join("out.txt")).unwrap();
        assert!(
            body.contains("missing-xyz") || !body.is_empty(),
            "stderr was not merged into the redirected file: {body:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- oracle harness: diff aish's native execution against bash ---------
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ORACLE_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Run `cmd` through aish, capturing the final stdout into a temp file via
    /// an appended `dd` sink (kept for pipelines with no explicit redirection).
    async fn aish_stdout(cmd: &str, session: &Session) -> Vec<u8> {
        let n = ORACLE_SEQ.fetch_add(1, Ordering::Relaxed);
        let sink = std::env::temp_dir().join(format!("aish_oracle_{}_{n}.out", std::process::id()));
        let piped = format!("{cmd} | dd of={}", sink.display());
        let stages = parse(&piped).expect("oracle command must run directly");
        let status = run(&stages, session).await.expect("aish pipeline run");
        assert!(status.success(), "dd capture sink failed for `{cmd}`");
        let bytes = std::fs::read(&sink).unwrap_or_default();
        let _ = std::fs::remove_file(&sink);
        bytes
    }

    fn bash_stdout(cmd: &str, cwd: &std::path::Path) -> Vec<u8> {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .output()
            .expect("spawn bash oracle (is bash on PATH?)")
            .stdout
    }

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
    async fn oracle_redirection_matches_bash() {
        // Redirection corpus: run through aish and bash, compare the file each
        // wrote plus exit status. Distinct cwds so the two can't collide.
        let base = std::env::temp_dir().join(format!("aish_oracle_redir_{}", std::process::id()));
        let (adir, bdir) = (base.join("aish"), base.join("bash"));
        std::fs::create_dir_all(&adir).unwrap();
        std::fs::create_dir_all(&bdir).unwrap();
        for d in [&adir, &bdir] {
            std::fs::write(d.join("src.txt"), "gamma\nalpha\nbeta\nalpha\n").unwrap();
        }
        let mut session = Session::new().unwrap();

        let corpus = [
            "sort src.txt > out",
            "sort < src.txt > out",
            "grep alpha src.txt | wc -l > out",
            "sort -u src.txt >> out",
            "printf 'x\\ny\\n' > out",
        ];
        for cmd in corpus {
            session.cwd = adir.clone();
            let acode = run(&parse(cmd).unwrap(), &session).await.unwrap().code();
            let bcode = bash_code(cmd, &bdir);
            assert_eq!(acode, bcode, "exit status diverged for `{cmd}`");
            let aout = std::fs::read_to_string(adir.join("out")).unwrap_or_default();
            let bout = std::fs::read_to_string(bdir.join("out")).unwrap_or_default();
            assert_eq!(aout, bout, "redirected file diverged for `{cmd}`");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn oracle_exit_status_matches_bash() {
        let session = Session::new().unwrap();
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
        let session = Session::new().unwrap();
        let aish = aish_stdout("seq 1 5 | sort -r", &session).await;
        let bash = bash_stdout("seq 1 5 | sort", &session.cwd);
        assert_ne!(
            String::from_utf8_lossy(&aish),
            String::from_utf8_lossy(&bash),
            "oracle failed to detect an intentional divergence — the harness is blind"
        );
    }
}
