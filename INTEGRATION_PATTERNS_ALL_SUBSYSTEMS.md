// Integration patterns for other aish subsystems
//
// This document shows the pattern each subsystem will follow when reading config.
// Once src/config.rs lands (in this PR), follow-up PRs will integrate each subsystem.

// ===== src/engine.rs =====
// Knob: AISH_SERIAL_CHAIN_YIELD_DEPTH
//
// use crate::config::Config;
// use std::env;
//
// fn get_serial_chain_yield_depth() -> usize {
//     let config = Config::load().unwrap_or_default();
//     env::var("AISH_SERIAL_CHAIN_YIELD_DEPTH")
//         .ok()
//         .and_then(|s| s.parse().ok())
//         .or_else(|| config.serial_chain.yield_depth)
//         .unwrap_or(1024)  // code default
// }

// ===== src/tools.rs (or src/alerts.rs) =====
// Knobs: AISH_WORKER_BELL, AISH_ALERT_BELL, AISH_ALERT_BELL_CMD
//
// use crate::config::Config;
// use std::env;
//
// struct AlertConfig {
//     bell_enabled: bool,
//     bell_cmd: Option<String>,
//     bell_worker: bool,
// }
//
// fn load_alert_config() -> AlertConfig {
//     let config = Config::load().unwrap_or_default();
//
//     let bell_enabled = env::var("AISH_ALERT_BELL")
//         .ok()
//         .and_then(|s| s.parse().ok())
//         .or_else(|| config.alerts.bell)
//         .unwrap_or(true);
//
//     let bell_cmd = env::var("AISH_ALERT_BELL_CMD")
//         .ok()
//         .filter(|s| !s.is_empty())
//         .or_else(|| config.alerts.bell_cmd.clone());
//
//     let bell_worker = env::var("AISH_ALERT_BELL_WORKER")
//         .ok()
//         .and_then(|s| s.parse().ok())
//         .or_else(|| config.alerts.bell_worker)
//         .unwrap_or(false);
//
//     AlertConfig {
//         bell_enabled,
//         bell_cmd,
//         bell_worker,
//     }
// }

// ===== src/update.rs =====
// Knobs: AISH_UPDATE_CHANNEL, AISH_UPDATE_REPO
//
// use crate::config::Config;
// use std::env;
//
// enum UpdateChannel {
//     Dev,
//     Stable,
// }
//
// fn load_update_config() -> (UpdateChannel, String) {
//     let config = Config::load().unwrap_or_default();
//
//     let channel = env::var("AISH_UPDATE_CHANNEL")
//         .ok()
//         .or_else(|| config.updates.channel.clone())
//         .unwrap_or_else(|_| "stable".to_string());
//
//     let repo = env::var("AISH_UPDATE_REPO")
//         .ok()
//         .or_else(|| config.updates.repo.clone())
//         .unwrap_or_else(|_| "LightHeart-Ventures/aish".to_string());
//
//     let channel = match channel.as_str() {
//         "dev" => UpdateChannel::Dev,
//         _ => UpdateChannel::Stable,
//     };
//
//     (channel, repo)
// }

// ===== src/session.rs =====
// Knobs: AISH_LAUNCH_SESSION_NAME, AISH_STARTUP_DIGEST
//
// use crate::config::Config;
// use std::env;
//
// struct SessionConfig {
//     launch_session_name: Option<String>,
//     startup_digest: bool,
// }
//
// fn load_session_config() -> SessionConfig {
//     let config = Config::load().unwrap_or_default();
//
//     let launch_session_name = env::var("AISH_LAUNCH_SESSION_NAME")
//         .ok()
//         .filter(|s| !s.is_empty())
//         .or_else(|| config.session.launch_session_name.clone());
//
//     let startup_digest = env::var("AISH_STARTUP_DIGEST")
//         .ok()
//         .and_then(|s| s.parse().ok())
//         .or_else(|| config.session.startup_digest)
//         .unwrap_or(true);
//
//     SessionConfig {
//         launch_session_name,
//         startup_digest,
//     }
// }

// ===== src/worker.rs (or wherever worker settings are) =====
// Knobs: AISH_WORKER_RUNTIME, AISH_WORKER_CPUS, AISH_WORKER_NETWORK, etc.
//
// use crate::config::Config;
// use std::env;
//
// struct WorkerConfig {
//     runtime: String,
//     cpus: Option<String>,
//     network: Option<String>,
//     worker_state_dir: Option<String>,
//     worktree_dir: Option<String>,
// }
//
// fn load_worker_config() -> WorkerConfig {
//     let config = Config::load().unwrap_or_default();
//
//     let runtime = env::var("AISH_WORKER_RUNTIME")
//         .ok()
//         .or_else(|| config.worker.runtime.clone())
//         .unwrap_or_else(|_| "docker".to_string());
//
//     let cpus = env::var("AISH_WORKER_CPUS")
//         .ok()
//         .filter(|s| !s.is_empty())
//         .or_else(|| config.worker.cpus.clone());
//
//     let network = env::var("AISH_WORKER_NETWORK")
//         .ok()
//         .filter(|s| !s.is_empty())
//         .or_else(|| config.worker.network.clone());
//
//     let worker_state_dir = env::var("AISH_WORKER_STATE_DIR")
//         .ok()
//         .filter(|s| !s.is_empty())
//         .or_else(|| config.worker.worker_state_dir.clone());
//
//     let worktree_dir = env::var("AISH_WORKTREE_DIR")
//         .ok()
//         .filter(|s| !s.is_empty())
//         .or_else(|| config.worker.worktree_dir.clone());
//
//     WorkerConfig {
//         runtime,
//         cpus,
//         network,
//         worker_state_dir,
//         worktree_dir,
//     }
// }

// ===== src/main.rs (optional: expose :config show command) =====
// This is NOT required for the MVP but useful for operators.
// Once config loading is integrated into all subsystems, a follow-up PR can add:
//
// `:config show` — print loaded configuration (env overrides + config file + defaults)
// `:config edit` — open ~/.aish/aish.config in $EDITOR
// `:config validate` — check syntax and report any errors
//
// For now, just loading and using the config is sufficient.

// ===== TESTING PATTERN =====
// Each integration PR should include:
// 1. Unit test in the subsystem that loads config and verifies precedence
// 2. Integration test that:
//    a) Creates a temporary ~/.aish/aish.config with a knob
//    b) Sets an env var to override it
//    c) Calls the subsystem's load function
//    d) Verifies env var takes precedence
// 3. Regression test ensuring existing tests still pass (no behavior change)
