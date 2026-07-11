// Integration pattern for src/coordinator.rs
// This shows how each subsystem will read from config
//
// FILE: src/coordinator.rs (around line ~150-200 where constants are defined)
//
// CHANGE: Add config loading and use it for MAX_ROUNDS, MAX_FAILED_ATTEMPTS, etc.

// OLD CODE:
/*
const MAX_ROUNDS: usize = 36;  // hardcoded default
const MAX_FAILED_ATTEMPTS: usize = 3;
const FAILED_KEEP: usize = 50;
const FAILED_MAX_AGE_DAYS: usize = 14;

pub struct CoordinatorState {
    pub max_rounds: usize,
    pub max_failed_attempts: usize,
    // ...
}

impl CoordinatorState::new() {
    Self {
        max_rounds: MAX_ROUNDS,
        max_failed_attempts: MAX_FAILED_ATTEMPTS,
        // ...
    }
}
*/

// NEW CODE (INTEGRATION PATTERN):
use crate::config::Config;
use std::env;

/// Load coordinator configuration from env > config file > defaults
fn load_coordinator_config() -> (usize, usize, usize, usize) {
    let config = Config::load().unwrap_or_default();

    let max_rounds = env::var("AISH_COORDINATOR_MAX_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| config.coordinator.max_rounds)
        .unwrap_or(36);  // code default

    let max_failed_attempts = env::var("AISH_COORDINATOR_MAX_FAILED_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| config.coordinator.max_failed_attempts)
        .unwrap_or(3);

    let failed_keep = env::var("AISH_COORDINATOR_FAILED_KEEP")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| config.coordinator.failed_keep)
        .unwrap_or(50);

    let failed_max_age_days = env::var("AISH_COORDINATOR_FAILED_MAX_AGE_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| config.coordinator.failed_max_age_days)
        .unwrap_or(14);

    (max_rounds, max_failed_attempts, failed_keep, failed_max_age_days)
}

pub struct CoordinatorState {
    pub max_rounds: usize,
    pub max_failed_attempts: usize,
    pub failed_keep: usize,
    pub failed_max_age_days: usize,
    // ...other fields
}

impl CoordinatorState {
    pub fn new() -> Self {
        let (max_rounds, max_failed_attempts, failed_keep, failed_max_age_days) =
            load_coordinator_config();

        Self {
            max_rounds,
            max_failed_attempts,
            failed_keep,
            failed_max_age_days,
            // ...
        }
    }
}
