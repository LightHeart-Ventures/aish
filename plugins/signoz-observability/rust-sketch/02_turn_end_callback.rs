// signoz-observability :: Rust integration sketch #2
// TurnEnd post-turn callback — where to fire the exception poll
// ===========================================================================
// FINDING: aish's turn loop (src/engine.rs::run_turn / the REPL dispatch in
// src/repl.rs) already has a natural end-of-turn point right after the final
// assistant answer is committed and BEFORE the prompt is re-drawn. Hooks are
// fired there via `self.hooks.fire_observe(HookEvent::TurnEnd { .. })`.
// `fire_observe` = fire-and-forget, non-blocking: it spawns each matching
// hook program detached and does NOT await it, so a slow SigNoz poll can
// never stall the prompt. That is EXACTLY the semantic we want for async
// exception monitoring.
// ---------------------------------------------------------------------------

// --- The integration point (already exists — annotated) --------------------
impl Engine {
    pub async fn run_turn(&mut self, input: &str) -> anyhow::Result<TurnOutcome> {
        let outcome = self.drive_model_loop(input).await?;   // tool calls + synthesis
        self.render_answer(&outcome).await;                  // answer hits the screen

        // >>> TURN-END SEAM: fire observe hooks. The signoz plugin's TurnEnd
        //     entry in hooks.json matches here → bin/poll-exceptions.sh runs
        //     detached. Fire-and-forget: we do not await it.
        self.hooks
            .fire_observe(HookEvent::TurnEnd { answer_len: outcome.answer.len() })
            .await;

        Ok(outcome)
    }
}

// --- Why a hook, not an in-core callback -----------------------------------
// The plugin deliberately rides the EXISTING TurnEnd hook rather than adding a
// bespoke Rust callback registry, because:
//   * Hooks are already fork/exec + fire-and-forget (non-blocking) — no risk
//     to prompt latency.
//   * The poller is MCP-free (curl → SigNoz REST), so it needs no agent loop.
//   * Zero core code change to ship: hooks.json wires it declaratively.
//
// TWO firing cadences, by design (see hooks.json + timer):
//   1. TurnEnd hook  — reacts right after each turn (bursty, user-driven).
//   2. 30s timer     — turn-INDEPENDENT sweep so exceptions surface even when
//      the operator is idle and no turn is ending. aish timers stream their
//      output to the statusline cache; poll-exceptions.sh --source=timer
//      prints the summary line the statusline segment reads.
//
// --- If you DO want a native Rust post-turn callback later -----------------
// Minimal registry shape (drop into src/hooks/mod.rs), for plugins that want
// in-process callbacks instead of fork/exec:
pub type PostTurnFn = Arc<dyn Fn(&TurnContext) + Send + Sync>;

#[derive(Default)]
pub struct PostTurnRegistry { cbs: Vec<PostTurnFn> }

impl PostTurnRegistry {
    pub fn register(&mut self, f: PostTurnFn) { self.cbs.push(f); }
    /// Called from run_turn AFTER render_answer. Each callback is spawned so a
    /// slow one cannot block the prompt (preserve fire_observe semantics).
    pub fn fire(&self, ctx: TurnContext) {
        for cb in &self.cbs {
            let (cb, ctx) = (cb.clone(), ctx.clone());
            tokio::task::spawn_blocking(move || cb(&ctx));
        }
    }
}
// The plugin does NOT require this — it exists only as the upgrade path if the
// fork/exec hook proves too coarse.
