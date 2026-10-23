//! Quiet-by-default routing for llama.cpp / ggml log output.
//!
//! `LlamaBackend::init()` — and the Metal device probe behind it — writes ~45
//! lines of pipeline-compile and device-capability chatter STRAIGHT to stderr
//! before aish gets a say (`ggml_metal_library_compile_pipeline: …`,
//! `ggml_metal_device_init: GPU family …`, one `compiled X library in N sec`
//! line per embedded library). None of it is actionable for a normal user and
//! it buries aish's own output.
//!
//! `llama-cpp-2` can hand its logs to `tracing` instead of stderr
//! ([`llama_cpp_2::send_logs_to_tracing`]). aish installs no global tracing
//! subscriber, so routed events would be dropped wholesale — including genuine
//! load failures. So the routing is paired with a deliberately tiny subscriber
//! that only looks at the crate's own `llama-cpp-2` target and:
//!
//! - passes WARN/ERROR through to stderr (a real load failure must still reach
//!   the user),
//! - keeps the single useful roll-up line (`loaded N libraries …`) in aish's
//!   dim style, matching the neighbouring `loading model from …` / `model
//!   ready` messages,
//! - drops everything else.
//!
//! Verbosity follows the existing per-subsystem convention in this repo
//! (`AISH_CODEBASE_DEBUG`, `AISH_SKILL_MATCH_DEBUG`, `AISH_VOICE_DEBUG`):
//! setting [`DEBUG_ENV`] (`AISH_LOCAL_DEBUG=1`) skips the routing entirely, so
//! llama.cpp keeps its native stderr logging and every line shows up verbatim.

// Everything here is consumed by `init_backend_logging`, which only exists
// under `--features local`; in the default build these items are exercised by
// the unit tests alone. Same pattern as `super::local_plan`.
#![cfg_attr(not(feature = "local"), allow(dead_code))]

/// Environment variable that restores llama.cpp's native (noisy) stderr
/// logging. Off by default — see the module docs. Truthy values are
/// `1`/`true`/`on`/`yes`, matching `AISH_CODEBASE_DEBUG`.
pub const DEBUG_ENV: &str = "AISH_LOCAL_DEBUG";

/// Whether the operator asked for raw llama.cpp/ggml logs. Pure so the
/// truth-table is unit-testable without touching the process environment. An
/// empty/unset value is OFF.
pub fn debug_enabled(env: Option<&str>) -> bool {
    matches!(
        env.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "on" | "yes")
    )
}

/// What the quiet subscriber does with one routed llama.cpp log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineAction {
    /// Print verbatim to stderr — genuine warnings and errors.
    Surface,
    /// Print in aish's dim style — the one useful startup roll-up.
    Summary,
    /// Swallow — device-probe and pipeline-compile chatter.
    Drop,
}

/// Substring that identifies the single startup roll-up worth keeping, e.g.
/// `ggml_metal_library_compile_all: loaded 35 libraries from embedded data in
/// 0.045 sec (max single = 0.044 sec)`.
const SUMMARY_MARKER: &str = "libraries from embedded data";

/// Classify one routed log line. Pure: level + text in, action out.
pub fn classify_line(level: &tracing::Level, text: &str) -> LineAction {
    if *level == tracing::Level::ERROR || *level == tracing::Level::WARN {
        LineAction::Surface
    } else if text.contains(SUMMARY_MARKER) {
        LineAction::Summary
    } else {
        LineAction::Drop
    }
}

/// Install llama.cpp log routing. Idempotent and safe to call from anywhere;
/// only the first call does anything.
///
/// MUST run before `LlamaBackend::init()` — the Metal device-init lines are
/// emitted during that call, so routing installed afterwards would be too late
/// for the noisiest part of startup.
#[cfg(feature = "local")]
pub fn init_backend_logging() {
    use std::sync::Once;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if debug_enabled(std::env::var(DEBUG_ENV).ok().as_deref()) {
            // Operator asked for the raw firehose: leave llama.cpp's native
            // stderr logging alone.
            return;
        }
        // Only claim the global dispatcher if nothing else owns it; a real
        // subscriber installed elsewhere wins and still receives the events.
        if !tracing::dispatcher::has_been_set() {
            let _ = tracing::subscriber::set_global_default(quiet::QuietLlamaSubscriber);
        }
        llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default());
    });
}

/// The minimal subscriber backing [`init_backend_logging`].
#[cfg(feature = "local")]
mod quiet {
    use super::{LineAction, classify_line};
    use tracing::field::{Field, Visit};
    use tracing::{Event, Metadata, Subscriber, span};

    /// Target used by `llama-cpp-2` for every routed ggml/llama.cpp line.
    const LLAMA_TARGET: &str = "llama-cpp-2";

    /// Pulls the `message` field out of a routed event.
    #[derive(Default)]
    struct MessageVisitor(String);

    impl Visit for MessageVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.0 = value.to_string();
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" && self.0.is_empty() {
                self.0 = format!("{value:?}");
            }
        }
    }

    /// Drops llama.cpp's startup chatter while letting warnings, errors, and
    /// the one startup roll-up through. Deliberately inert for every other
    /// target so installing it does not change logging elsewhere in aish.
    pub(super) struct QuietLlamaSubscriber;

    impl Subscriber for QuietLlamaSubscriber {
        fn enabled(&self, meta: &Metadata<'_>) -> bool {
            meta.target() == LLAMA_TARGET && *meta.level() <= tracing::Level::INFO
        }

        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let meta = event.metadata();
            if meta.target() != LLAMA_TARGET {
                return;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            let text = visitor.0.trim_end();
            if text.is_empty() {
                return;
            }
            match classify_line(meta.level(), text) {
                LineAction::Surface => eprintln!("llama.cpp [{}]: {text}", meta.level()),
                LineAction::Summary => eprintln!("\x1b[2m  {text}\x1b[0m"),
                LineAction::Drop => {}
            }
        }

        fn enter(&self, _: &span::Id) {}

        fn exit(&self, _: &span::Id) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_env_truthy_values_enable_raw_logs() {
        for v in ["1", "true", "on", "yes", "TRUE", " on "] {
            assert!(debug_enabled(Some(v)), "{v} should enable raw logs");
        }
    }

    #[test]
    fn debug_env_unset_or_falsy_stays_quiet() {
        for v in [None, Some(""), Some("0"), Some("false"), Some("off")] {
            assert!(!debug_enabled(v), "{v:?} should stay quiet");
        }
    }

    #[test]
    fn errors_and_warnings_always_surface() {
        assert_eq!(
            classify_line(&tracing::Level::ERROR, "failed to load model"),
            LineAction::Surface
        );
        assert_eq!(
            classify_line(&tracing::Level::WARN, "something is off"),
            LineAction::Surface
        );
    }

    #[test]
    fn startup_chatter_is_dropped_but_the_rollup_survives() {
        assert_eq!(
            classify_line(
                &tracing::Level::INFO,
                "ggml_metal_library_compile_pipeline: compiling pipeline: base = 'dummy_kernel'"
            ),
            LineAction::Drop
        );
        assert_eq!(
            classify_line(
                &tracing::Level::INFO,
                "ggml_metal_device_init: GPU name:   MTL0 (Apple M5 Pro)"
            ),
            LineAction::Drop
        );
        assert_eq!(
            classify_line(
                &tracing::Level::INFO,
                "ggml_metal_library_compile_all: loaded 35 libraries from embedded data in 0.045 sec"
            ),
            LineAction::Summary
        );
    }
}
