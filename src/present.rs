//! Who displays finished background-job results.
//!
//! Interactive REPL: a presenter task drains results and prints them via
//! rustyline's `ExternalPrinter` at a pause in work (so they don't blurt over a
//! command mid-flight or the user's typing). Headless (`-c` / `--coordinator`):
//! there's no prompt to protect, so jobs print inline as they finish.
//!
//! The REPL flips this on once, at startup, when it installs the presenter.

use std::sync::atomic::{AtomicBool, Ordering};

static DEFERRED: AtomicBool = AtomicBool::new(false);

/// Called once by the interactive REPL when it installs a presenter: from now on
/// background jobs queue their results instead of printing them.
pub fn enable_deferred() {
    DEFERRED.store(true, Ordering::Relaxed);
}

/// True when a presenter owns background-result display (so `on_complete` should
/// stay quiet and let the presenter drain at a pause).
pub fn deferred() -> bool {
    DEFERRED.load(Ordering::Relaxed)
}
