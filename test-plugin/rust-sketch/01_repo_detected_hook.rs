// signoz-observability :: Rust integration sketch #1
// RepoDetected hook event — the missing seam
// ===========================================================================
// FINDING: aish has NO `RepoDetected` / `RepoOpened` hook event today. The
// de-facto repo-open point is `engine::maybe_auto_index_repo`, which already
// dedups per canonical repo root via `session.codebase_indexed: HashSet<PathBuf>`
// and fires once per repo-open (called from the two prompt-entry sites in
// engine.rs). `CwdChanged` exists in the HookEvent enum but is DORMANT — no
// `fire_observe` call site emits it yet.
//
// This sketch shows the minimal, surgical change to give plugins a real
// repo-detection event, reusing the existing dedup so we never double-fire.
// ---------------------------------------------------------------------------

// --- 1. Add the variant to the HookEvent enum (src/hooks/mod.rs) -----------
pub enum HookEvent {
    SessionStart,
    CwdChanged { old: PathBuf, new: PathBuf },
    // ...existing variants...
    /// NEW: emitted the first time a given repo root becomes active in a
    /// session (deduped — mirrors maybe_auto_index_repo). Payload carries the
    /// canonical repo root and whether it's a git work-tree.
    RepoDetected { repo_root: PathBuf, is_git: bool },
    TurnEnd { answer_len: usize },
}

impl HookEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            HookEvent::RepoDetected { .. } => "RepoDetected",
            // ...
            _ => "…",
        }
    }
    /// Extra JSON fields merged into the payload handed to hook programs on stdin.
    pub fn extra_payload(&self) -> serde_json::Value {
        match self {
            HookEvent::RepoDetected { repo_root, is_git } => serde_json::json!({
                "repo_root": repo_root.display().to_string(),
                "is_git": is_git,
            }),
            _ => serde_json::Value::Null,
        }
    }
}

// --- 2. Fire it from the existing repo-open seam (src/engine.rs) ------------
// maybe_auto_index_repo already computes the canonical root and owns the
// `codebase_indexed` dedup set. Hook the FIRST-seen branch — one extra call,
// zero new dedup state.
impl Engine {
    async fn maybe_auto_index_repo(&mut self, cwd: &Path) -> anyhow::Result<()> {
        let repo_root = canonical_repo_root(cwd); // git top-level else cwd, canonicalized
        let first_seen = self.session.codebase_indexed.insert(repo_root.clone());
        if first_seen {
            let is_git = repo_root.join(".git").exists();
            // >>> NEW: notify plugins exactly once per repo-open, before indexing.
            self.hooks
                .fire_observe(HookEvent::RepoDetected { repo_root: repo_root.clone(), is_git })
                .await;
            // ...existing indexing work continues unchanged...
        }
        Ok(())
    }
}

// --- 3. (Alternative, zero-core-change path) -------------------------------
// If you cannot patch core: the plugin already subscribes to `CwdChanged`
// and `SessionStart` in hooks.json and self-dedups in bin/scan-repo.sh
// against state/registry.json (re-scan at most 1×/hour per root). That works
// TODAY without touching engine.rs — RepoDetected is the clean long-term seam,
// the CwdChanged+SessionStart pair is the ship-now fallback.
//
// NOTE: to make the CwdChanged fallback fire, wire the currently-dormant
// event at the cwd-mutation site (see sketch #3, §CwdChanged).
