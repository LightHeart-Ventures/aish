//! Broken-pipe shield for detached background coordinators.
//!
//! A background coordinator is spawned with `Stdio::piped()` stdout/stderr and
//! calls `setsid()` so it survives its parent's exit (no SIGHUP). That is only
//! half the story: the pipes themselves are owned by the parent. When the parent
//! process exits, the read ends close, and the *next* write the child makes to
//! stdout/stderr fails with `EPIPE`.
//!
//! Rust ignores `SIGPIPE` at startup, so the write doesn't kill the process with
//! a signal — but `println!` / `eprintln!` **panic** on a failed write. A
//! coordinator that is narrating its work (every one of them) therefore dies at
//! its next line of output, seconds after its parent exits, with its real work
//! (worktree, commits, branch push, PR) unfinished. `setsid()` saved it from the
//! signal and the pipe killed it anyway.
//!
//! The shield interposes a relay: we hand the process a *fresh* pipe as fd 1/2
//! and copy from it to the original fd on a helper thread. The child's writes
//! now always land in a pipe whose read end we own, so they never fail. If the
//! downstream (the parent) goes away, the relay notices once, flips to
//! discarding, and keeps draining — the coordinator runs to completion and its
//! durable side effects (git, PRs, the coordinator store) still land.
//!
//! `drain` is the ordered shutdown: flush, close the write end so the relay sees
//! EOF, and wait briefly for it to copy the tail. It must be the last thing the
//! process does with stdout/stderr — the final answer is written before it.

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// How long `drain` waits for a relay to flush its tail downstream before
/// giving up and letting the process exit anyway. Generous relative to a pipe
/// copy; bounded so a wedged reader can't hang a finished coordinator.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Live shield over the process's standard fds. Hold it for the duration of the
/// run and call [`FdShield::drain`] exactly once, last.
pub struct FdShield {
    relays: Vec<Relay>,
}

struct Relay {
    /// The standard fd we hijacked (1 or 2). Closing it is what signals EOF to
    /// the relay thread, because the dup'd write end was already closed.
    fd: libc::c_int,
    /// Fires once the relay thread has copied everything and exited.
    done: Receiver<()>,
}

/// Interpose a relay on stdout and stderr, if they are pipes.
///
/// A tty or a regular file can't break under us the way a parent-owned pipe
/// can, so those are left completely untouched — the shield costs nothing on an
/// interactive or file-redirected run.
pub fn engage() -> FdShield {
    let relays = [libc::STDOUT_FILENO, libc::STDERR_FILENO]
        .into_iter()
        .filter_map(hijack)
        .collect();
    FdShield { relays }
}

impl FdShield {
    /// Flush, signal EOF, and wait (briefly) for each relay to drain.
    ///
    /// After this returns, fd 1/2 are closed — nothing may write to them again,
    /// so call it as the final act of the run.
    pub fn drain(self) {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        for r in self.relays {
            // Sole remaining write end: closing it is the relay's EOF.
            unsafe { libc::close(r.fd) };
            let _ = r.done.recv_timeout(DRAIN_GRACE);
        }
    }
}

/// Replace `fd` with the write end of a fresh pipe and start the relay thread.
/// Returns `None` (leaving `fd` exactly as it was) if `fd` isn't a pipe or any
/// step fails — the shield is an enhancement, never a prerequisite.
fn hijack(fd: libc::c_int) -> Option<Relay> {
    if !is_pipe(fd) {
        return None;
    }
    let mut ends = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return None;
    }
    let (rd, wr) = (ends[0], ends[1]);

    // Keep the real destination alive under a private number before we clobber.
    let orig = unsafe { libc::dup(fd) };
    if orig < 0 {
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }
        return None;
    }
    if unsafe { libc::dup2(wr, fd) } < 0 {
        unsafe {
            libc::close(rd);
            libc::close(wr);
            libc::close(orig);
        }
        return None;
    }
    // fd is now the write end; drop the duplicate so `fd` is the only one and
    // closing it in `drain` actually reaches EOF.
    unsafe { libc::close(wr) };

    let (tx, done) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("fd-shield-{fd}"))
        .spawn(move || {
            relay(rd, orig);
            let _ = tx.send(());
        });
    if spawned.is_err() {
        // Put the original back so the process is no worse off than unshielded.
        unsafe {
            libc::dup2(orig, fd);
            libc::close(orig);
            libc::close(rd);
        }
        return None;
    }
    Some(Relay { fd, done })
}

/// Copy `rd` -> `orig` until EOF, tolerating a dead downstream.
///
/// The first failed write means the parent is gone. We do **not** stop: the
/// writer (this process) must keep finding a reader on the other end of its
/// pipe, or it would block once the buffer fills and then die on the same EPIPE
/// we exist to absorb. So we keep reading and throw the bytes away.
fn relay(rd: libc::c_int, orig: libc::c_int) {
    let mut src = unsafe { std::fs::File::from_raw_fd(rd) };
    let mut dst = unsafe { std::fs::File::from_raw_fd(orig) };
    let mut buf = [0u8; 16 * 1024];
    let mut downstream_alive = true;
    loop {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if downstream_alive && dst.write_all(&buf[..n]).is_err() {
                    downstream_alive = false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = dst.flush();
    // `src`/`dst` own their fds and close on drop.
}

/// True when `fd` is a pipe/FIFO — the only case that can break under us.
fn is_pipe(fd: libc::c_int) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return false;
    }
    (st.st_mode & libc::S_IFMT) == libc::S_IFIFO
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    fn pipe_pair() -> (libc::c_int, libc::c_int) {
        let mut ends = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        (ends[0], ends[1])
    }

    #[test]
    fn is_pipe_distinguishes_pipes_from_files() {
        let (rd, wr) = pipe_pair();
        assert!(is_pipe(rd), "pipe read end should be detected");
        assert!(is_pipe(wr), "pipe write end should be detected");
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }

        let path = std::env::temp_dir().join(format!("aish-fd-shield-{}", std::process::id()));
        let f = std::fs::File::create(&path).expect("temp file");
        assert!(
            !is_pipe(f.as_raw_fd()),
            "a regular file is not a pipe and must be left unshielded"
        );
        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    /// The whole point: once the downstream reader is gone, writes into the
    /// shielded pipe still succeed instead of raising EPIPE on the writer.
    #[test]
    fn writes_survive_a_dead_downstream() {
        // Upstream: what the shielded process writes into.
        let (up_rd, up_wr) = pipe_pair();
        // Downstream: stands in for the parent-owned pipe.
        let (down_rd, down_wr) = pipe_pair();

        let t = std::thread::spawn(move || relay(up_rd, down_wr));

        let mut writer = unsafe { std::fs::File::from_raw_fd(up_wr) };
        writer.write_all(b"before\n").expect("write while parent alive");

        // Parent exits: its read end disappears.
        unsafe { libc::close(down_rd) };

        // Enough traffic to be sure the relay tried (and failed) a write and
        // that we'd have filled a 64K pipe buffer if it had stopped draining.
        for _ in 0..64 {
            writer
                .write_all(&[b'x'; 4096])
                .expect("writes must keep succeeding after the parent is gone");
        }

        drop(writer); // EOF -> relay exits
        t.join().expect("relay thread should exit cleanly, not panic");
    }

    /// A relay with a live downstream must deliver the bytes verbatim.
    #[test]
    fn relay_forwards_bytes_to_a_live_downstream() {
        let (up_rd, up_wr) = pipe_pair();
        let (down_rd, down_wr) = pipe_pair();

        let t = std::thread::spawn(move || relay(up_rd, down_wr));

        let mut writer = unsafe { std::fs::File::from_raw_fd(up_wr) };
        writer.write_all(b"hello coordinator\n").expect("write");
        drop(writer);

        let mut reader = unsafe { std::fs::File::from_raw_fd(down_rd) };
        let mut got = String::new();
        reader.read_to_string(&mut got).expect("read downstream");
        assert_eq!(got, "hello coordinator\n");

        t.join().expect("relay thread");
    }
}
