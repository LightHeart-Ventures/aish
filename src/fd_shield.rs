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
//! Both fds the shield keeps for itself — the relay's read end and the dup of
//! the real destination — are `CLOEXEC`. They must be: every process this
//! coordinator spawns (sub-workers, `git`, `cargo`) would otherwise inherit a
//! copy of the *parent's* pipe write end. The parent's reader then cannot see
//! EOF while any descendant is alive, so a coordinator that finished minutes
//! ago still reads as `running` upstream and its result is withheld until the
//! last grandchild exits. `pipeline.rs` carries the same lesson for pipeline
//! stages; this is that hazard one level up.
//!
//! `drain` is the ordered shutdown: flush, close the write end so the relay sees
//! EOF, and wait briefly for it to copy the tail. It must be the last thing the
//! process does with stdout/stderr — the final answer is written before it.

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
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
    /// The real destination, shared with the relay thread.
    ///
    /// `drain` *takes* it rather than waiting for the thread to notice EOF.
    /// fd 1/2 are inheritable by design — every child must be able to write to
    /// them — so any still-live grandchild holds a copy of the relay's write
    /// end and the relay would not see EOF until the last of them exits.
    /// Handing ownership over makes the downstream's EOF depend only on *this*
    /// process finishing.
    dst: Arc<Mutex<Option<std::fs::File>>>,
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
            // Our last write end: closing it is the relay's EOF, unless a
            // grandchild inherited a copy.
            unsafe { libc::close(r.fd) };
            let _ = r.done.recv_timeout(DRAIN_GRACE);
            // Close the downstream whether or not the relay got there first.
            // Dropping the `File` is what the parent's reader sees as EOF, and
            // it must not be contingent on a descendant's lifetime.
            if let Ok(mut slot) = r.dst.lock() {
                drop(slot.take());
            }
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
    let (rd, wr) = pipe_cloexec()?;

    // Keep the real destination alive under a private number before we clobber.
    // CLOEXEC matters most here: this fd *is* the parent's pipe, and a copy
    // leaked into a grandchild keeps the parent from ever seeing EOF.
    let orig = match dup_cloexec(fd) {
        Some(n) => n,
        None => {
            unsafe {
                libc::close(rd);
                libc::close(wr);
            }
            return None;
        }
    };
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
    // SAFETY: `orig` is ours alone from here on; the `File` owns and closes it.
    let dst = Arc::new(Mutex::new(Some(unsafe {
        std::fs::File::from_raw_fd(orig)
    })));
    let thread_dst = Arc::clone(&dst);
    let spawned = std::thread::Builder::new()
        .name(format!("fd-shield-{fd}"))
        .spawn(move || {
            relay(rd, thread_dst);
            let _ = tx.send(());
        });
    if spawned.is_err() {
        // Put the original back so the process is no worse off than unshielded.
        // `orig` is still owned by the `File` in `dst`; restore through it, then
        // let the drop close it.
        unsafe {
            libc::dup2(orig, fd);
            libc::close(rd);
        }
        if let Ok(mut slot) = dst.lock() {
            drop(slot.take());
        }
        return None;
    }
    Some(Relay { fd, dst, done })
}

/// Copy `rd` -> the shared destination until EOF, tolerating a dead downstream.
///
/// The first failed write means the parent is gone. We do **not** stop: the
/// writer (this process) must keep finding a reader on the other end of its
/// pipe, or it would block once the buffer fills and then die on the same EPIPE
/// we exist to absorb. So we drop the destination and keep draining to nowhere.
///
/// `drain` may take the destination out from under us at shutdown; a `None`
/// slot means exactly the same thing as a dead downstream — discard and
/// continue until the fd closes.
fn relay(rd: libc::c_int, dst: Arc<Mutex<Option<std::fs::File>>>) {
    let mut src = unsafe { std::fs::File::from_raw_fd(rd) };
    let mut buf = [0u8; 16 * 1024];
    loop {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Ok(mut slot) = dst.lock() {
                    let dead = match slot.as_mut() {
                        Some(f) => f.write_all(&buf[..n]).is_err(),
                        None => false,
                    };
                    if dead {
                        // Release the parent's pipe the moment it breaks.
                        drop(slot.take());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if let Ok(mut slot) = dst.lock() {
        if let Some(f) = slot.as_mut() {
            let _ = f.flush();
        }
        drop(slot.take());
    }
    // `src` owns `rd` and closes on drop.
}

/// `pipe()` with both ends marked `CLOEXEC`.
///
/// Portable `pipe` + `fcntl` rather than Linux-only `pipe2(O_CLOEXEC)` so the
/// crate still builds on macOS. The window between the two calls is safe: the
/// shield is engaged once, at startup, before anything else can `fork`.
fn pipe_cloexec() -> Option<(libc::c_int, libc::c_int)> {
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: `ends` is a valid 2-element array; `pipe` fills it or returns < 0.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return None;
    }
    let (rd, wr) = (ends[0], ends[1]);
    if !set_cloexec(rd) || !set_cloexec(wr) {
        // SAFETY: both are freshly-owned descriptors we have not published.
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }
        return None;
    }
    Some((rd, wr))
}

/// `dup(2)` onto a fresh descriptor that is already `CLOEXEC`, atomically.
fn dup_cloexec(fd: libc::c_int) -> Option<libc::c_int> {
    // SAFETY: F_DUPFD_CLOEXEC on a presumed-valid fd; the result is checked.
    let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if n < 0 {
        return None;
    }
    Some(n)
}

/// Set `FD_CLOEXEC` on `fd`, preserving the other descriptor flags.
fn set_cloexec(fd: libc::c_int) -> bool {
    // SAFETY: F_GETFD/F_SETFD on a valid fd; both return values are checked.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return false;
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    rc >= 0
}

/// True when `fd` has `FD_CLOEXEC` set.
#[cfg(test)]
fn is_cloexec(fd: libc::c_int) -> bool {
    // SAFETY: F_GETFD on a valid fd; the return value is checked.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags >= 0 && (flags & libc::FD_CLOEXEC) != 0
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

    /// Wrap a raw write fd the way `hijack` does, so tests can call `relay`.
    fn dest(fd: libc::c_int) -> Arc<Mutex<Option<std::fs::File>>> {
        Arc::new(Mutex::new(Some(unsafe { std::fs::File::from_raw_fd(fd) })))
    }

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

        let t = std::thread::spawn(move || relay(up_rd, dest(down_wr)));

        let mut writer = unsafe { std::fs::File::from_raw_fd(up_wr) };
        writer
            .write_all(b"before\n")
            .expect("write while parent alive");

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
        t.join()
            .expect("relay thread should exit cleanly, not panic");
    }

    /// Every descriptor the shield keeps for itself must be `CLOEXEC`, or an
    /// `exec` in any descendant inherits a copy.
    #[test]
    fn shield_primitives_are_close_on_exec() {
        let (rd, wr) = pipe_cloexec().expect("pipe");
        assert!(is_cloexec(rd), "relay read end must not survive exec");
        assert!(is_cloexec(wr), "shield write end must not survive exec");

        let duped = dup_cloexec(wr).expect("dup");
        assert!(
            is_cloexec(duped),
            "the dup of the real destination must not survive exec"
        );

        unsafe {
            libc::close(rd);
            libc::close(wr);
            libc::close(duped);
        }
    }

    /// The regression this module exists to prevent at the *process* level: a
    /// grandchild spawned while the shield is up must not pin the parent's
    /// pipe open. If it does, the parent never sees EOF, keeps reporting the
    /// finished coordinator as `running`, and withholds its result until the
    /// last descendant exits.
    #[test]
    fn a_live_grandchild_does_not_hold_the_downstream_open() {
        // `down_rd` stands in for the parent coordinator's read end.
        let (down_rd, down_wr) = pipe_pair();

        // Shield a scratch fd that points at the parent's write end, so the
        // test harness's own stdout/stderr are left alone.
        let victim = unsafe { libc::dup(down_wr) };
        assert!(victim >= 0, "dup the downstream onto a scratch fd");
        unsafe { libc::close(down_wr) };

        let relay = hijack(victim).expect("a pipe fd must be shielded");

        // Something long-lived, spawned *after* the shield is up.
        let mut grandchild = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn grandchild");

        // The shielded process finishes: fd closed, relay joined, both of the
        // relay's descriptors dropped. Nothing in *this* process holds the
        // downstream write end any more.
        FdShield {
            relays: vec![relay],
        }
        .drain();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = unsafe { std::fs::File::from_raw_fd(down_rd) };
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
            let _ = tx.send(());
        });
        let saw_eof = rx.recv_timeout(Duration::from_secs(5));

        let _ = grandchild.kill();
        let _ = grandchild.wait();

        assert!(
            saw_eof.is_ok(),
            "downstream never reached EOF: a live grandchild inherited the \
             shielded destination, so the parent would keep this coordinator \
             marked `running` until that descendant exits"
        );
    }

    /// A relay with a live downstream must deliver the bytes verbatim.
    #[test]
    fn relay_forwards_bytes_to_a_live_downstream() {
        let (up_rd, up_wr) = pipe_pair();
        let (down_rd, down_wr) = pipe_pair();

        let t = std::thread::spawn(move || relay(up_rd, dest(down_wr)));

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
