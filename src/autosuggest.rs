//! Async suggestion plumbing (S6.1 / TASK-135).
//!
//! fish's hard-won async-I/O lesson: a suggestion — especially one backed by a
//! model call that can take *seconds* — must NEVER stall the keystroke loop, and
//! any in-flight suggestion must be cancelled the moment the next keystroke
//! makes it stale. A shell that blocks the cursor while it "thinks" is broken;
//! a shell that paints a suggestion computed for a line you've already edited
//! past is worse. This module is the provider-agnostic primitive that makes both
//! impossible by construction.
//!
//! [`AutosuggestEngine`] drives an async [`SuggestionSource`] *off* the editor
//! thread and guarantees two things — the card's two acceptance criteria:
//!
//! 1. **Non-blocking.** [`AutosuggestEngine::request`] spawns the work and
//!    returns instantly; the editor reads the answer later via the
//!    non-blocking [`AutosuggestEngine::poll`]. A source that takes an hour
//!    never costs the keystroke loop more than a `tokio::spawn`.
//! 2. **Cancel-in-flight on keystroke.** Each `request` supersedes the previous
//!    one: the prior task is `abort()`-ed (real cancellation — a model call's
//!    HTTP future is dropped at its next await point) AND every result is
//!    stamped with a monotonic *generation*, so a stale answer that finished
//!    racing the abort is discarded by `poll` rather than painted. The two
//!    mechanisms are belt-and-suspenders: abort stops the work, the generation
//!    guard stops a late result, and correctness never depends on the source's
//!    internals.
//!
//! This is the seam S6.2 (history ghost-text — [`crate::editor`]) and S6.3
//! (model next-command suggestion) plug into: each supplies a
//! [`SuggestionSource`] (a fast in-memory history scan, or one tool-less
//! `backend.complete` call) and renders whatever `poll` hands back. The engine
//! is fully exercised here against injected fake sources — fast, slow, and
//! cancellation-observing — so the non-blocking + cancel-in-flight contract is
//! pinned without a TTY or a live model.
#![allow(dead_code)] // Plumbing landed ahead of its consumers (S6.2 / S6.3).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The future a [`SuggestionSource`] returns: it resolves to the suggestion text
/// for the query, or `None` when the source has nothing to offer (which still
/// surfaces, so the caller can clear any stale ghost text). Boxed + `Send` so a
/// heterogeneous set of sources — a synchronous history scan, an async model
/// call — share one type and can be `tokio::spawn`-ed.
pub type SuggestFuture = Pin<Box<dyn Future<Output = Option<String>> + Send>>;

/// A pluggable source of inline suggestions. Given the current query (typically
/// the line buffer, possibly with context the caller folds in), it produces a
/// candidate suggestion asynchronously. Implemented by S6.2's history scanner
/// and S6.3's model call; the blanket impl below also lets any
/// `Fn(String) -> impl Future<Output = Option<String>>` be a source, which is
/// what the unit tests (and trivial wirings) use.
pub trait SuggestionSource: Send + Sync + 'static {
    /// Compute a suggestion for `query`. Must be cancellation-safe: the engine
    /// drops (aborts) the returned future when a newer keystroke supersedes it,
    /// so any cleanup belongs in `Drop`, not after an await that may never
    /// complete.
    fn suggest(&self, query: String) -> SuggestFuture;
}

/// Any async closure `Fn(String) -> Future<Output = Option<String>>` is a
/// [`SuggestionSource`]. This is the ergonomic path the tests and simple
/// call-sites take; a richer source (one that needs to hold a `Backend` handle,
/// say) implements the trait directly.
impl<F, Fut> SuggestionSource for F
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<String>> + Send + 'static,
{
    fn suggest(&self, query: String) -> SuggestFuture {
        Box::pin((self)(query))
    }
}

/// One computed suggestion handed back by [`AutosuggestEngine::poll`]. Carries
/// the `generation` it was computed for (the engine only ever returns the
/// current one — stale generations are dropped), the `query` it answers (so the
/// caller can confirm it still matches the live buffer before painting), and the
/// `text` (`None` when the source declined — the signal to clear ghost text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The monotonic request generation this answer was computed for.
    pub generation: u64,
    /// The query string the suggestion answers.
    pub query: String,
    /// The suggestion text, or `None` when the source had nothing to offer.
    pub text: Option<String>,
}

/// Non-blocking, cancel-in-flight suggestion driver (S6.1 / TASK-135).
///
/// Owns the active [`SuggestionSource`], the monotonic generation counter, the
/// single in-flight task handle, and the channel completed suggestions land on.
/// Drive it from the editor loop: [`request`](Self::request) on each keystroke,
/// [`poll`](Self::poll) when you're about to repaint, [`cancel`](Self::cancel)
/// when the line is submitted or cleared.
pub struct AutosuggestEngine {
    source: Arc<dyn SuggestionSource>,
    /// The latest request's generation. Bumped on every `request`/`cancel`; the
    /// poll-side guard returns only results stamped with THIS value, discarding
    /// any that belong to a superseded keystroke.
    generation: u64,
    /// Handle to the currently-running suggestion task, if any. Aborted when a
    /// new request supersedes it (real, immediate cancellation of the work).
    inflight: Option<JoinHandle<()>>,
    tx: mpsc::UnboundedSender<Suggestion>,
    rx: mpsc::UnboundedReceiver<Suggestion>,
}

impl AutosuggestEngine {
    /// Build an engine driven by `source`. No work runs until the first
    /// [`request`](Self::request).
    pub fn new<S: SuggestionSource>(source: S) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            source: Arc::new(source),
            generation: 0,
            inflight: None,
            tx,
            rx,
        }
    }

    /// Build an engine from an already-shared source handle (when the source is
    /// `Arc`-held elsewhere — e.g. it wraps a backend the REPL also owns).
    pub fn from_arc(source: Arc<dyn SuggestionSource>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            source,
            generation: 0,
            inflight: None,
            tx,
            rx,
        }
    }

    /// Request a suggestion for `query`, superseding any in-flight one.
    ///
    /// Returns instantly (AC1): the work is `tokio::spawn`-ed and the new
    /// generation is returned. The previous request — if still running — is
    /// `abort()`-ed (AC2), and because the generation is bumped, even a result
    /// that finished racing the abort is dropped by [`poll`](Self::poll). The
    /// returned generation is the token a caller can compare against a later
    /// [`Suggestion::generation`] to know whether an answer is still current.
    pub fn request(&mut self, query: impl Into<String>) -> u64 {
        let query = query.into();
        // Supersede the in-flight request: abort the work AND invalidate any
        // result it may already have queued (the generation bump below).
        self.abort_inflight();
        self.generation += 1;
        let generation = self.generation;

        let source = self.source.clone();
        let tx = self.tx.clone();
        let q = query.clone();
        self.inflight = Some(tokio::spawn(async move {
            let text = source.suggest(q.clone()).await;
            // Best-effort: a closed receiver means the engine was dropped — there
            // is nothing to deliver to, so the send simply no-ops.
            let _ = tx.send(Suggestion { generation, query: q, text });
        }));
        generation
    }

    /// Non-blocking read of the freshest *current* suggestion, or `None` when
    /// none is ready.
    ///
    /// Drains every queued result, discarding any stamped with a superseded
    /// generation (the abort-race safety net), and returns the newest one that
    /// matches the live generation. Never blocks — safe to call on every
    /// repaint. Returns `Some(Suggestion { text: None, .. })` when the current
    /// source declined, which the caller treats as "clear the ghost text".
    pub fn poll(&mut self) -> Option<Suggestion> {
        let mut latest = None;
        while let Ok(s) = self.rx.try_recv() {
            if s.generation == self.generation {
                latest = Some(s); // a newer same-generation result supersedes
            }
            // Stale generation → silently discarded (superseded keystroke).
        }
        latest
    }

    /// Cancel any in-flight suggestion and invalidate every queued result.
    ///
    /// Use when the line is submitted or cleared: there is no longer a buffer to
    /// suggest against, so the running work is aborted and the generation is
    /// bumped past anything already produced, guaranteeing a subsequent
    /// [`poll`](Self::poll) returns `None` until the next [`request`](Self::request).
    pub fn cancel(&mut self) {
        self.abort_inflight();
        // Move the generation past anything already in the channel so a result
        // that beat the abort is treated as stale by `poll`.
        self.generation += 1;
    }

    /// Whether a suggestion task is currently running (spawned and not yet
    /// finished). Useful for a caller that wants to show a subtle "thinking"
    /// affordance — but never required: `poll` is the contract.
    pub fn is_pending(&self) -> bool {
        self.inflight.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// The current request generation (0 before the first request). Exposed so a
    /// caller can correlate a painted suggestion with the keystroke that asked
    /// for it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Abort the in-flight task, if any. Aborting an already-finished task is a
    /// harmless no-op, so this is safe to call unconditionally.
    fn abort_inflight(&mut self) {
        if let Some(handle) = self.inflight.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// AC1 — non-blocking. Even a source that would take an hour, `request`
    /// returns in well under the keystroke budget, and an immediate `poll` finds
    /// nothing ready (the editor paints the bare line and moves on).
    #[tokio::test]
    async fn request_never_blocks_the_keystroke_loop() {
        let src = |_q: String| async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Some("would-be answer".to_string())
        };
        let mut engine = AutosuggestEngine::new(src);

        let t0 = std::time::Instant::now();
        let g = engine.request("git che");
        let elapsed = t0.elapsed();

        assert_eq!(g, 1, "first request is generation 1");
        assert!(
            elapsed < Duration::from_millis(50),
            "request blocked the keystroke loop for {elapsed:?}"
        );
        assert!(engine.poll().is_none(), "a slow suggestion must not be ready yet");
    }

    /// AC2 — cancel-in-flight on keystroke. A slow request superseded by a fast
    /// one must never surface its (stale) answer, and the superseded task must be
    /// genuinely cancelled: its post-await side effect (the completion counter)
    /// is observed for the fast query only.
    #[tokio::test]
    async fn next_keystroke_cancels_in_flight_work() {
        let completed = Arc::new(AtomicUsize::new(0));
        let counter = completed.clone();
        let src = move |q: String| {
            let counter = counter.clone();
            async move {
                // The slow query parks past its supersession; the fast one is
                // instant. The increment AFTER the await is the side effect that
                // must NOT run when the task is aborted mid-flight.
                if q == "slow" {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                counter.fetch_add(1, Ordering::SeqCst);
                Some(format!("done:{q}"))
            }
        };
        let mut engine = AutosuggestEngine::new(src);

        let g_slow = engine.request("slow");
        // A keystroke arrives before the slow suggestion can finish.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let g_fast = engine.request("fast");
        assert!(g_fast > g_slow, "a later keystroke gets a newer generation");

        // Let the fast one finish and the slow one's original deadline elapse.
        tokio::time::sleep(Duration::from_millis(600)).await;

        let s = engine.poll().expect("the fast suggestion is ready");
        assert_eq!(s.generation, g_fast, "only the current generation surfaces");
        assert_eq!(s.text.as_deref(), Some("done:fast"));
        assert_eq!(s.query, "fast");
        assert_eq!(
            completed.load(Ordering::SeqCst),
            1,
            "the superseded task was aborted — its post-await effect never ran"
        );
        assert!(engine.poll().is_none(), "the channel is fully drained");
    }

    /// A fast source's answer surfaces through `poll` once the spawned task gets
    /// to run, stamped with the request's generation and carrying its query.
    #[tokio::test]
    async fn fast_suggestion_surfaces_with_its_generation() {
        let src = |q: String| async move { Some(format!("> {q}")) };
        let mut engine = AutosuggestEngine::new(src);

        let g = engine.request("ls");
        let s = poll_until_ready(&mut engine).await;

        assert_eq!(s.generation, g);
        assert_eq!(s.query, "ls");
        assert_eq!(s.text.as_deref(), Some("> ls"));
    }

    /// The abort-race safety net: a result stamped with a superseded generation
    /// is discarded by `poll` even if it already landed in the channel. Both
    /// queries are instant, so gen-1's answer really is queued when gen-2
    /// supersedes — and must not surface.
    #[tokio::test]
    async fn stale_generation_results_are_discarded() {
        let src = |q: String| async move { Some(q) };
        let mut engine = AutosuggestEngine::new(src);

        let _g1 = engine.request("first");
        // Let generation 1 finish and enqueue its answer.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let g2 = engine.request("second");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let s = engine.poll().expect("a fresh suggestion is available");
        assert_eq!(s.generation, g2, "the stale gen-1 result was dropped");
        assert_eq!(s.text.as_deref(), Some("second"));
        assert!(engine.poll().is_none(), "no stale result lingers");
    }

    /// `cancel` invalidates everything in flight or queued: after it, `poll`
    /// returns `None` even though a result had already been produced.
    #[tokio::test]
    async fn cancel_invalidates_queued_results() {
        let src = |q: String| async move { Some(q) };
        let mut engine = AutosuggestEngine::new(src);

        engine.request("typed");
        tokio::time::sleep(Duration::from_millis(20)).await; // let it enqueue
        engine.cancel();

        assert!(engine.poll().is_none(), "cancel drops the queued result");
        assert!(!engine.is_pending(), "no work remains after cancel");
    }

    /// Generations increase by exactly one per request and are unique — the
    /// monotonic key the staleness guard depends on. `cancel` also advances it,
    /// so a post-cancel request can't collide with a pre-cancel result.
    #[tokio::test]
    async fn generations_are_monotonic() {
        let src = |_q: String| async move { None::<String> };
        let mut engine = AutosuggestEngine::new(src);

        assert_eq!(engine.generation(), 0, "no requests yet");
        assert_eq!(engine.request("a"), 1);
        assert_eq!(engine.request("b"), 2);
        engine.cancel(); // advances the generation past gen-2
        assert_eq!(engine.request("c"), 4);
    }

    /// A declining source (`None`) still surfaces a (text-less) suggestion, so
    /// the caller can erase stale ghost text for the current line rather than
    /// leave a suggestion for a query that no longer has one.
    #[tokio::test]
    async fn declined_suggestions_still_surface_to_clear_ghost_text() {
        let src = |_q: String| async move { None::<String> };
        let mut engine = AutosuggestEngine::new(src);

        let g = engine.request("xyzzy");
        let s = poll_until_ready(&mut engine).await;

        assert_eq!(s.generation, g);
        assert!(s.text.is_none(), "a declined suggestion carries no text");
        assert_eq!(s.query, "xyzzy");
    }

    /// The trait can be implemented directly (not just via the closure blanket
    /// impl) — the shape S6.3's model-backed source will take.
    #[tokio::test]
    async fn explicit_trait_impl_works() {
        struct EchoSource;
        impl SuggestionSource for EchoSource {
            fn suggest(&self, query: String) -> SuggestFuture {
                Box::pin(async move { Some(format!("echo {query}")) })
            }
        }
        let mut engine = AutosuggestEngine::new(EchoSource);
        engine.request("hi");
        let s = poll_until_ready(&mut engine).await;
        assert_eq!(s.text.as_deref(), Some("echo hi"));
    }

    /// Spin `poll` (yielding to the runtime) until a suggestion is ready. Only
    /// used by tests with fast sources, so it converges immediately.
    async fn poll_until_ready(engine: &mut AutosuggestEngine) -> Suggestion {
        loop {
            if let Some(s) = engine.poll() {
                return s;
            }
            tokio::task::yield_now().await;
        }
    }
}
