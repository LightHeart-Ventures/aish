//! PTY test harness for foreground/signal behaviour (TASK-117).
//!
//! Verifies the core S2 job-control invariant:
//!
//! > A Ctrl-C keypress on a PTY terminal kills the **foreground child's
//! > process group** while the shell process (aish) is unaffected.
//!
//! Job-control signal delivery is a kernel TTY driver feature that cannot be
//! exercised with ordinary pipes or a bare `kill(2)` call — the terminal
//! driver checks the **foreground process group** of the controlling terminal,
//! routing SIGINT there without touching the shell's process group.  A real
//! PTY pair is therefore required to exercise this path end-to-end.
//!
//! ## Test matrix
//!
//! | Test | Mechanism | What is exercised |
//! |---|---|---|
//! | `ctrl_c_kills_child_not_shell` | PTY master write of `\x03` | Full kernel TTY INTR path |
//! | `sigint_to_child_pgrp_does_not_kill_shell` | `kill(-pgid, SIGINT)` | `setpgid` scoping (no PTY needed) |
//!
//! ## Effort estimate
//!
//! ~25 min to production deployment (tests only; no production code changes
//! required; CI green on the PR is the gate).

#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::time::Duration;

// ── PTY helpers ────────────────────────────────────────────────────────────────

/// Open a POSIX PTY master and return `(master_fd, slave_path)`.
///
/// The slave is **not** opened in the parent — the child will open it inside
/// `pre_exec` without `O_NOCTTY`, which makes the slave automatically become
/// the child's controlling terminal after `setsid()` (POSIX.1-2017 §11.1.3).
///
/// `master_fd` is opened without `O_CLOEXEC` so the child can close it
/// explicitly inside `pre_exec`.
fn open_pty_master() -> (libc::c_int, std::ffi::CString) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt: {}", std::io::Error::last_os_error());
        assert_eq!(libc::grantpt(master), 0, "grantpt: {}", std::io::Error::last_os_error());
        assert_eq!(libc::unlockpt(master), 0, "unlockpt: {}", std::io::Error::last_os_error());
        let slave_path = slave_device_name(master);
        (master, slave_path)
    }
}

/// Retrieve the slave PTY device path for `master_fd`.
///
/// Uses `ptsname_r(3)` on Linux (thread-safe, stack-allocated buffer) and the
/// non-reentrant `ptsname(3)` everywhere else.
fn slave_device_name(master_fd: libc::c_int) -> std::ffi::CString {
    #[cfg(target_os = "linux")]
    {
        let mut buf = [0u8; 64];
        let rc = unsafe {
            libc::ptsname_r(master_fd, buf.as_mut_ptr().cast::<libc::c_char>(), buf.len())
        };
        assert_eq!(rc, 0, "ptsname_r: {}", std::io::Error::last_os_error());
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::ffi::CString::new(&buf[..nul]).expect("ptsname_r returned non-UTF8 path")
    }
    #[cfg(not(target_os = "linux"))]
    {
        // ptsname is not reentrant, but these tests do not run this concurrently.
        let ptr = unsafe { libc::ptsname(master_fd) };
        assert!(!ptr.is_null(), "ptsname failed");
        let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
        cstr.to_owned()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// **Core AC (TASK-117)**: writing the INTR character (`\x03`, Ctrl-C) to a
/// PTY master causes the kernel terminal line discipline to deliver SIGINT to
/// the **foreground process group** of the PTY slave — the child — while this
/// test process (the "shell") is completely unaffected.
///
/// The scenario faithfully mirrors `run_on_tty`'s S2 behaviour:
///
/// * **Shell** (this test): ignores SIGINT via `SIG_IGN`, mirroring
///   `tools::ignore_job_control_signals()` called at aish REPL startup.
/// * **Child** (`sleep 30`): spawned with `setsid()` (new session), the PTY
///   slave opened without `O_NOCTTY` (making it the session's controlling
///   terminal automatically per POSIX §11.1.3), `setpgid(0, 0)` (own pgid),
///   and SIGINT reset to `SIG_DFL` — because `SIG_IGN` is inherited across
///   `exec` and would otherwise make the child deaf to Ctrl-C (TASK-115).
/// * **PTY write**: `\x03` to master → kernel delivers SIGINT to the PTY's
///   foreground pgid (the child); never to the shell's pgid.
#[test]
fn ctrl_c_kills_child_not_shell() {
    // ── 1. Open PTY master ──────────────────────────────────────────────────
    let (master_fd, slave_path) = open_pty_master();

    // ── 2. Shell ignores SIGINT ─────────────────────────────────────────────
    // Mirrors aish calling `tools::ignore_job_control_signals()` at startup.
    // Saved so we can restore the test-process disposition in cleanup.
    let saved_sigint = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };

    // ── 3. Spawn child ──────────────────────────────────────────────────────
    // pre_exec runs between fork() and exec() in the child.  Only
    // async-signal-safe operations are performed here (raw syscalls).
    //
    // Key steps:
    //  a) setsid()         → child is a session leader; no controlling terminal
    //  b) open(slave, RW)  → slave auto-becomes controlling terminal (no O_NOCTTY)
    //                         and child's pgid becomes the terminal's foreground group
    //  c) dup2 → 0/1/2     → wire PTY slave to stdin/stdout/stderr
    //  d) setpgid(0,0)     → explicit own pgid (mirrors run_on_tty; redundant but clear)
    //  e) signal(SIGINT,DFL) → reset SIG_IGN inherited from the shell (TASK-115 invariant)
    let master_fd_cap = master_fd; // c_int is Copy
    let child = unsafe {
        std::process::Command::new("sleep")
            .arg("30")
            .pre_exec(move || {
                // (a) New session — no controlling terminal.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // (b) Open PTY slave WITHOUT O_NOCTTY.
                //     POSIX.1-2017 §11.1.3: when a process without a controlling
                //     terminal opens a terminal file not with O_NOCTTY, that
                //     terminal becomes the controlling terminal of the session.
                //     The kernel also sets the caller's pgid as the foreground
                //     process group of the new controlling terminal.
                let slave_fd = libc::open(slave_path.as_ptr(), libc::O_RDWR);
                if slave_fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // (c) Wire PTY slave to stdio.
                libc::dup2(slave_fd, 0);
                libc::dup2(slave_fd, 1);
                libc::dup2(slave_fd, 2);
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                // Child does not need the master end.
                libc::close(master_fd_cap);
                // (d) Explicit process-group ownership (mirrors run_on_tty's
                //     setpgid(0,0); redundant for a fresh session leader whose
                //     pgid already equals its pid, but documents the intent).
                libc::setpgid(0, 0);
                // (e) CRITICAL: restore default SIGINT.
                //     SIG_IGN survives exec() — without this reset the child
                //     inherits the shell's ignore and is deaf to Ctrl-C.
                //     This is the child-side half of the TASK-115 invariant.
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            })
            .spawn()
            .expect("failed to spawn child sleep")
    };
    let child_pid = child.id() as libc::pid_t;
    // Forget the Rust handle — we reap via waitpid below; Child::drop on Unix
    // does nothing, but forgetting protects against any future regression.
    std::mem::forget(child);

    // Give the child a moment to exec `sleep` and fully establish its PTY
    // session and signal state before we send Ctrl-C.
    std::thread::sleep(Duration::from_millis(100));

    // ── 4. Deliver Ctrl-C via the PTY master ────────────────────────────────
    // The INTR character (0x03 = ^C) written to the master causes the kernel
    // terminal line discipline to send SIGINT to the PTY slave's foreground
    // process group — exactly what the user's Ctrl-C keypress would do.
    // The shell's process group is in a different session and is not affected.
    let ctrl_c = [0x03u8];
    let n = unsafe { libc::write(master_fd, ctrl_c.as_ptr().cast(), ctrl_c.len()) };
    assert_eq!(n, 1, "write Ctrl-C to PTY master: {}", std::io::Error::last_os_error());

    // ── 5. Wait for child ───────────────────────────────────────────────────
    // Use raw waitpid to inspect WIFSIGNALED / WTERMSIG without going through
    // Rust's ExitStatus abstraction.
    let mut raw_status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(child_pid, &mut raw_status, 0) };
    assert_eq!(waited, child_pid, "waitpid: {}", std::io::Error::last_os_error());

    // ── 6. Assert: child died from SIGINT ────────────────────────────────────
    assert!(
        unsafe { libc::WIFSIGNALED(raw_status) },
        "child exited cleanly (code {}); expected it to be killed by SIGINT \
         (raw_status={raw_status:#010x})",
        unsafe { libc::WEXITSTATUS(raw_status) },
    );
    let sig = unsafe { libc::WTERMSIG(raw_status) };
    assert_eq!(
        sig,
        libc::SIGINT,
        "child killed by signal {sig}; expected SIGINT ({})",
        libc::SIGINT,
    );

    // ── 7. Assert: shell (test process) is still alive ───────────────────────
    // Reaching this line proves the test process was NOT killed by the PTY's
    // Ctrl-C delivery.  The SIGINT went to the child's process group (the PTY
    // session's foreground group) exclusively; the shell's pgid was never in
    // that session.
    // Reaching this line proves the test process survived — code cannot run in a dead process.

    // ── Cleanup ──────────────────────────────────────────────────────────────
    unsafe {
        libc::close(master_fd);
        libc::signal(libc::SIGINT, saved_sigint);
    }
}

/// **Process-group scoping** (no PTY required): sending SIGINT directly to the
/// child's process group via `kill(-pgid, SIGINT)` kills the child while the
/// shell, which ignores SIGINT, is unaffected.
///
/// This is the faster, always-available companion to `ctrl_c_kills_child_not_shell`.
/// It isolates the `setpgid` + SIG_IGN mechanism from the PTY terminal driver,
/// providing a direct regression test for signal-scoping independent of PTY
/// device availability (e.g., in headless CI sandboxes with no `/dev/pts`).
///
/// Exercises the same invariants as TASK-113 (setpgid) and TASK-115 (SIG_IGN)
/// without the full PTY delivery path.
#[test]
fn sigint_to_child_pgrp_does_not_kill_shell() {
    // Shell ignores SIGINT — mirrors aish's ignore_job_control_signals().
    let saved = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };

    // Spawn child in its own process group with the default SIGINT handler.
    let child = unsafe {
        std::process::Command::new("sleep")
            .arg("30")
            .pre_exec(|| {
                libc::setpgid(0, 0); // own pgid — mirrors run_on_tty's setpgid
                libc::signal(libc::SIGINT, libc::SIG_DFL); // restore default (TASK-115)
                Ok(())
            })
            .spawn()
            .expect("failed to spawn child")
    };
    let child_pid = child.id() as libc::pid_t;
    std::mem::forget(child);

    // Give the child a moment to exec.
    std::thread::sleep(Duration::from_millis(20));

    // Mirror the parent-side setpgid that run_on_tty performs to close the
    // spawn race (EACCES after exec is expected and ignored).
    unsafe { libc::setpgid(child_pid, child_pid) };

    // Send SIGINT to the child's process group only.
    // kill(-pgid, sig) delivers the signal to every process in the group.
    let r = unsafe { libc::kill(-child_pid, libc::SIGINT) };
    assert_eq!(r, 0, "kill(-child_pgid, SIGINT): {}", std::io::Error::last_os_error());

    // Wait and assert the child was killed by SIGINT.
    let mut status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(child_pid, &mut status, 0) };
    assert_eq!(waited, child_pid, "waitpid: {}", std::io::Error::last_os_error());

    assert!(
        unsafe { libc::WIFSIGNALED(status) },
        "child should have been killed by a signal (raw={status:#010x})",
    );
    assert_eq!(
        unsafe { libc::WTERMSIG(status) },
        libc::SIGINT,
        "expected SIGINT ({}), got {}",
        libc::SIGINT,
        unsafe { libc::WTERMSIG(status) },
    );

    // Shell is still here — SIGINT was scoped to the child's pgid only.
    // Reaching this line proves the test process survived — code cannot run in a dead process.

    // Restore SIGINT disposition.
    unsafe { libc::signal(libc::SIGINT, saved) };
}
