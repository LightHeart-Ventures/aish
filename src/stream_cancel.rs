//! Cancel-in-flight for the streamed S6 preview surfaces (S8.3 / TASK-145).
//!
//! ## Why this exists
//! S8.2 ([`crate::stream_render`]) renders the `:rewrite` / `:suggest` candidate
//! token-by-token while [`crate::backend::Backend::complete_streaming`] runs.
//! That `.await` sits directly in the REPL keystroke loop: until S8.3 there was
//! no way to bail out of a slow, wrong, or no-longer-wanted stream — the user
//! had to wait for the model to finish before the prompt came back. This module
//! is the missing backpressure valve: **a new keystroke cancels the in-flight
//! stream cleanly.**
//!
//! ## How it works
//! [`with_cancel`] wraps the streaming future in a [`tokio::select!`] against a
//! stdin watcher. For the life of the stream we put stdin into **cbreak**
//! (ICANON + ECHO off so the cancel key is unbuffered and silent; ISIG left on
//! so Ctrl-C still raises SIGINT) and run a tiny std-thread reader that
//! `poll()`s fd 0. The first byte it sees fires a tokio channel the `select!`
//! awaits — whichever of {stream finishes, key pressed} resolves first wins.
//! When the key wins, the streaming future is simply **dropped**: reqwest tears
//! the SSE connection down on drop, so cancellation is clean and immediate with
//! no half-applied state. An RAII [`CancelWatch`] guard restores cooked mode and
//! joins the reader before returning, so the tty is always handed back clean for
//! the next `read_line` and the reader can never steal the next prompt's keys.
//!
//! ## Coexistence with the mid-turn watcher
//! The S6 previews run at the *idle* REPL (between `read_line`s), where no
//! [`crate::keywatch`] turn watcher is installed. As a belt-and-braces guard we
//! still refuse to install when `keywatch::installed()` is true, so the two
//! never fight over the tty's termios.
//!
//! ## Non-TTY
//! When stdin is not a terminal (pipe / script) — or termios can't be read, or a
//! turn watcher owns stdin — [`CancelWatch::install`] returns an inert guard
//! whose cancel future is `Pending` forever. The `select!` cancel branch then
//! never fires and streaming behaves exactly as it did before S8.3.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;

/// The result of driving a streaming future under a [`CancelWatch`].
#[derive(Debug)]
pub enum StreamOutcome<T> {
    /// The stream ran to completion; carries its normal output.
    Done(T),
    /// A keystroke cancelled the stream before it finished.
    Cancelled,
}

/// Drive `fut` (a streaming completion) while watching stdin for a cancel
/// keystroke. Resolves to [`StreamOutcome::Done`] with the future's output when
/// the stream finishes first, or [`StreamOutcome::Cancelled`] when a key is
/// pressed first (the future is dropped, tearing down the underlying stream).
///
/// `biased` polls the stream first, so a stream that is already complete on this
/// tick is never spuriously reported as cancelled by a simultaneously-ready key.
pub async fn with_cancel<F, T>(fut: F) -> StreamOutcome<T>
where
    F: Future<Output = T>,
{
    let mut watch = CancelWatch::install();
    tokio::select! {
        biased;
        out = fut => StreamOutcome::Done(out),
        _ = watch.cancelled() => StreamOutcome::Cancelled,
    }
}

/// Read the current termios of fd 0, or `None` if it can't be queried.
fn get_termios() -> Option<libc::termios> {
    // SAFETY: tcgetattr into a zeroed termios; return code checked.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) == 0 {
            Some(t)
        } else {
            None
        }
    }
}

fn set_termios(t: &libc::termios) {
    // SAFETY: tcsetattr on fd 0 with a valid termios.
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, t);
    }
}

/// Derive cbreak attrs from cooked: ICANON + ECHO off (unbuffered, silent) while
/// keeping ISIG on so Ctrl-C still generates SIGINT. VMIN=1/VTIME=0 so a `read`
/// after a readable `poll` returns promptly.
fn cbreak_from(cooked: &libc::termios) -> libc::termios {
    let mut raw = *cooked;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    raw
}

fn stdin_is_tty() -> bool {
    // SAFETY: plain isatty query on fd 0.
    unsafe { libc::isatty(0) == 1 }
}

/// Are we (aish's pgrp) the terminal's foreground process group? Reading stdin
/// while backgrounded would raise SIGTTIN and stop the process, so the reader
/// only reads while foreground. `tcgetpgrp<0` (no controlling tty) ⇒ treat as
/// foreground so a detached-tty edge case doesn't wedge the reader parked.
fn we_are_foreground() -> bool {
    // SAFETY: plain pgrp queries.
    unsafe {
        let fg = libc::tcgetpgrp(0);
        fg < 0 || fg == libc::getpgrp()
    }
}

/// The reader loop, run on a dedicated std thread for the life of one stream.
/// Polls fd 0 and, on the first byte while foreground, signals cancel and stops.
fn reader_loop(stop: Arc<AtomicBool>, tx: mpsc::UnboundedSender<()>) {
    let mut buf = [0u8; 64];
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Don't read while backgrounded (SIGTTIN hazard) — park and re-check.
        if !we_are_foreground() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on a single fd with a 20ms timeout — bounds teardown
        // latency (stop is checked each loop) without busy-spinning.
        let r = unsafe { libc::poll(&mut pfd, 1, 20) };
        if r <= 0 {
            continue; // timeout or (EINTR) error — re-check stop and retry.
        }
        if stop.load(Ordering::Acquire) || !we_are_foreground() {
            continue;
        }
        // SAFETY: read into a stack buffer; n bounded by buf.len().
        let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            continue;
        }
        // Any keystroke cancels. Fire once and stop reading so we don't swallow
        // more than the single cancel key.
        let _ = tx.send(());
        break;
    }
}

/// RAII guard installed for the duration of one streamed preview. On
/// construction it flips stdin into cbreak and spawns the reader; on drop it
/// stops the reader, restores cooked mode, and joins the thread. Inert (cancel
/// future stays Pending forever) when stdin isn't a tty, termios can't be read,
/// or a mid-turn key watcher already owns stdin.
pub struct CancelWatch {
    active: bool,
    rx: mpsc::UnboundedReceiver<()>,
    /// Held so the channel never closes while inert — `cancelled()` then stays
    /// Pending rather than resolving on a closed channel and spuriously firing.
    _keepalive: mpsc::UnboundedSender<()>,
    stop: Arc<AtomicBool>,
    cooked: Option<libc::termios>,
    handle: Option<JoinHandle<()>>,
}

impl CancelWatch {
    /// Install the cancel watcher for the current stream. A no-op inert guard
    /// when stdin isn't a tty, when termios can't be read, or when a mid-turn
    /// key watcher already owns stdin (see [`crate::keywatch::installed`]).
    pub fn install() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let inert = |rx, tx, stop| CancelWatch {
            active: false,
            rx,
            _keepalive: tx,
            stop,
            cooked: None,
            handle: None,
        };
        if !stdin_is_tty() || crate::keywatch::installed() {
            return inert(rx, tx, stop);
        }
        let Some(cooked) = get_termios() else {
            return inert(rx, tx, stop);
        };
        set_termios(&cbreak_from(&cooked));
        let reader_tx = tx.clone();
        let reader_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("aish-stream-cancel".into())
            .spawn(move || reader_loop(reader_stop, reader_tx))
            .ok();
        match handle {
            Some(h) => CancelWatch {
                active: true,
                rx,
                _keepalive: tx,
                stop,
                cooked: Some(cooked),
                handle: Some(h),
            },
            // Spawn failed: restore cooked immediately and go inert.
            None => {
                set_termios(&cooked);
                inert(rx, tx, stop)
            }
        }
    }

    /// Resolve when a cancel keystroke arrives. On an inert guard this is
    /// Pending forever, so the hosting `select!` cancel branch never fires.
    pub async fn cancelled(&mut self) {
        if !self.active {
            std::future::pending::<()>().await;
        }
        // Only resolves on a real `send(())`; the retained keepalive sender keeps
        // the channel open so a `None` (all senders dropped) can't fire cancel.
        let _ = self.rx.recv().await;
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Restore cooked AFTER the reader has stopped, so it can't steal the
        // next prompt's keystrokes and the tty is handed back clean.
        if let Some(c) = self.cooked {
            set_termios(&c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A non-tty (test harness) install is inert: no thread, cancel stays
    // pending, and the guard drops without touching termios.
    #[tokio::test]
    async fn inert_watch_never_cancels_off_a_tty() {
        // Under `cargo test` stdin is not a tty, so install() is inert.
        let mut w = CancelWatch::install();
        assert!(!w.active, "must be inert without a tty");
        // cancelled() is Pending forever — a short timeout must elapse.
        let timed_out =
            tokio::time::timeout(Duration::from_millis(50), w.cancelled()).await.is_err();
        assert!(timed_out, "inert cancel future must never resolve");
    }

    // with_cancel returns Done with the future's output when the stream
    // completes first (the common, uncancelled path).
    #[tokio::test]
    async fn with_cancel_returns_done_when_stream_completes() {
        let out = with_cancel(async { 42u32 }).await;
        assert!(matches!(out, StreamOutcome::Done(42)));
    }

    // A future that yields before completing still resolves to Done off a tty
    // (nothing cancels it).
    #[tokio::test]
    async fn with_cancel_done_survives_await_points() {
        let out = with_cancel(async {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            "ok"
        })
        .await;
        match out {
            StreamOutcome::Done(v) => assert_eq!(v, "ok"),
            StreamOutcome::Cancelled => panic!("must not cancel off a tty"),
        }
    }
}
