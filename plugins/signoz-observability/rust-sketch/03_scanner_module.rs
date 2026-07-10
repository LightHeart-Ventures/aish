// signoz-observability :: Rust integration sketch #3
// Instrumentation scanner as a native module (optional core upgrade)
// ===========================================================================
// The SHIPPING scanner is bin/scan-repo.sh (fork/exec, language-agnostic,
// filesystem pattern-matching). This sketch is the native-Rust equivalent for
// operators who prefer the scan in-core (faster, typed, testable). Same
// detection matrix; writes the same registry.json schema so the two are
// drop-in interchangeable.
// ---------------------------------------------------------------------------
use std::path::{Path, PathBuf};
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct RepoProfile {
    pub detected_at: String,
    pub last_scanned: String,
    pub languages: Vec<String>,
    pub services: Vec<String>,
    pub endpoints: Vec<String>,
    pub markers: Vec<String>,
    pub instrumented: bool,
}

/// (file glob, needle, language, marker) detection matrix — the single source
/// of truth shared conceptually with scan-repo.sh.
const MATRIX: &[(&str, &str, &str, &str)] = &[
    ("package.json",     "@opentelemetry/",              "node",   "package.json:@opentelemetry"),
    ("requirements.txt", "opentelemetry-sdk",            "python", "requirements:opentelemetry"),
    ("pyproject.toml",   "opentelemetry",                "python", "pyproject:opentelemetry"),
    ("Cargo.toml",       "opentelemetry",                "rust",   "Cargo.toml:opentelemetry"),
    ("go.mod",           "go.opentelemetry.io/otel",     "go",     "go.mod:otel"),
    ("pom.xml",          "io.opentelemetry",             "java",   "pom.xml:io.opentelemetry"),
    ("build.gradle",     "io.opentelemetry",             "java",   "gradle:io.opentelemetry"),
];

const PRUNE: &[&str] = &["node_modules", "target", ".git", "dist", "build", ".venv", "__pycache__"];

pub fn scan_repo(repo_root: &Path) -> anyhow::Result<RepoProfile> {
    let mut p = RepoProfile { instrumented: false, ..Default::default() };
    for entry in walk(repo_root, 4) {                 // bounded depth, PRUNE-aware
        let name = entry.file_name().and_then(|s| s.to_str()).unwrap_or("");
        for (glob, needle, lang, marker) in MATRIX {
            if name == *glob {
                if let Ok(txt) = std::fs::read_to_string(&entry) {
                    if txt.contains(needle) {
                        push_uniq(&mut p.languages, lang);
                        push_uniq(&mut p.markers, marker);
                        p.instrumented = true;
                        if *glob == "package.json" {
                            if let Some(svc) = json_name(&txt) { push_uniq(&mut p.services, &svc); }
                        }
                    }
                }
            }
        }
        // env / config sweep for service names + OTLP endpoints
        if is_env_or_config(name) {
            if let Ok(txt) = std::fs::read_to_string(&entry) {
                for svc in grep_service_names(&txt) { push_uniq(&mut p.services, &svc); }
                for ep  in grep_otlp_endpoints(&txt) { push_uniq(&mut p.endpoints, &ep); }
            }
        }
    }
    let now = now_iso();
    p.last_scanned = now.clone();
    p.detected_at = now;
    Ok(p)
}

// grep_service_names: regex `OTEL_SERVICE_NAME[=: ]+["']?([A-Za-z0-9_.-]+)`
// grep_otlp_endpoints: regex `https?://[\w.-]+:431[78]|localhost:431[78]`
// walk/push_uniq/json_name/is_env_or_config/now_iso: see impl notes in DESIGN.md

// --- Where this would be called (src/engine.rs) ----------------------------
// Inside maybe_auto_index_repo's first-seen branch (sketch #1 §2), instead of
// (or alongside) firing the RepoDetected hook:
//
//     if first_seen {
//         if let Ok(profile) = signoz::scan_repo(&repo_root) {
//             registry::upsert(&repo_root, &profile)?;   // merge into registry.json
//         }
//     }

// ===========================================================================
// §CwdChanged — waking the dormant event (needed for the ship-now fallback)
// The HookEvent::CwdChanged variant exists but is never emitted. Fire it at
// the single cwd-mutation seam so both the scanner hook AND any future plugin
// get cwd transitions:
impl Engine {
    fn set_cwd(&mut self, new: PathBuf) {
        let old = std::mem::replace(&mut self.session.cwd, new.clone());
        if old != new {
            // >>> NEW: emit the previously-dormant event.
            let hooks = self.hooks.clone();
            tokio::spawn(async move {
                hooks.fire_observe(HookEvent::CwdChanged { old, new }).await;
            });
        }
    }
}
// With this one wire-up, bin/scan-repo.sh fires on every directory change
// (self-throttled to 1 scan/hour/root), giving repo detection with ZERO other
// core changes while RepoDetected (sketch #1) lands as the clean seam.
