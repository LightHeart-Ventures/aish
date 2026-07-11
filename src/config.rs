// src/config.rs
//! Runtime configuration parsing — INI file at ~/.aish/aish.config
//!
//! Precedence (highest to lowest):
//! 1. Environment variable (e.g., AISH_COORDINATOR_MAX_ROUNDS)
//! 2. Config file (~/.aish/aish.config)
//! 3. Code default (hardcoded)
//!
//! The config file is OPTIONAL. If missing, all defaults apply.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Runtime configuration loaded from ~/.aish/aish.config
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub coordinator: CoordinatorConfig,
    pub serial_chain: SerialChainConfig,
    pub alerts: AlertsConfig,
    pub dispatch: DispatchConfig,
    pub plugins: PluginsConfig,
    pub telemetry: TelemetryConfig,
    pub inference: InferenceConfig,
    pub worker: WorkerConfig,
    pub updates: UpdatesConfig,
    pub session: SessionConfig,
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Default)]
pub struct CoordinatorConfig {
    pub max_rounds: Option<usize>,
    pub max_failed_attempts: Option<usize>,
    pub failed_keep: Option<usize>,
    pub failed_max_age_days: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct SerialChainConfig {
    pub yield_depth: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct AlertsConfig {
    pub bell: Option<bool>,
    pub bell_cmd: Option<String>,
    pub bell_worker: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct DispatchConfig {
    pub dedup_secs: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct PluginsConfig {
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TelemetryConfig {
    pub reasoning_log: Option<String>,
    pub reasoning_rotate_mb: Option<usize>,
    pub reasoning_memo: Option<String>,
    pub codebase_log: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct InferenceConfig {
    pub local_model_path: Option<String>,
    pub local_n_gpu_layers: Option<usize>,
    pub hf_base: Option<String>,
    pub hf_revision: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WorkerConfig {
    pub runtime: Option<String>,
    pub cpus: Option<String>,
    pub network: Option<String>,
    pub worker_state_dir: Option<String>,
    pub worktree_dir: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdatesConfig {
    pub channel: Option<String>,
    pub repo: Option<String>,
    pub github_raw_base: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub launch_session_name: Option<String>,
    pub startup_digest: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolsConfig {
    pub allowlist: Option<String>,
}

impl Config {
    /// Load configuration from ~/.aish/aish.config
    /// Returns an empty Config if the file doesn't exist (which is fine — all env/defaults apply).
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Config::config_path();

        if !config_path.exists() {
            // Config file is optional; return empty config
            return Ok(Config::default());
        }

        let content = fs::read_to_string(&config_path)?;
        Ok(Config::parse(&content))
    }

    /// Parse INI format configuration from a string
    fn parse(content: &str) -> Self {
        let mut config = Config::default();
        let mut current_section = String::new();
        let mut pairs: HashMap<String, String> = HashMap::new();

        for line in content.lines() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Parse section header [section_name]
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                current_section = trimmed[1..trimmed.len() - 1].to_string();
                continue;
            }

            // Parse key = value
            if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim().to_string();
                let value = trimmed[eq_pos + 1..].trim().to_string();

                let full_key = if current_section.is_empty() {
                    key
                } else {
                    format!("{}.{}", current_section, key)
                };

                pairs.insert(full_key, value);
            }
        }

        // Populate config from parsed pairs
        config.coordinator.max_rounds = pairs.get("coordinator.max_rounds").and_then(|s| s.parse().ok());
        config.coordinator.max_failed_attempts = pairs.get("coordinator.max_failed_attempts").and_then(|s| s.parse().ok());
        config.coordinator.failed_keep = pairs.get("coordinator.failed_keep").and_then(|s| s.parse().ok());
        config.coordinator.failed_max_age_days = pairs.get("coordinator.failed_max_age_days").and_then(|s| s.parse().ok());

        config.serial_chain.yield_depth = pairs.get("serial_chain.yield_depth").and_then(|s| s.parse().ok());

        config.alerts.bell = pairs.get("alerts.bell").and_then(|s| s.parse().ok());
        config.alerts.bell_cmd = pairs.get("alerts.bell_cmd").cloned().filter(|s| !s.is_empty());
        config.alerts.bell_worker = pairs.get("alerts.bell_worker").and_then(|s| s.parse().ok());

        config.dispatch.dedup_secs = pairs.get("dispatch.dedup_secs").and_then(|s| s.parse().ok());

        config.plugins.dir = pairs.get("plugins.dir").cloned().filter(|s| !s.is_empty());

        config.telemetry.reasoning_log = pairs.get("telemetry.reasoning_log").cloned().filter(|s| !s.is_empty());
        config.telemetry.reasoning_rotate_mb = pairs.get("telemetry.reasoning_rotate_mb").and_then(|s| s.parse().ok());
        config.telemetry.reasoning_memo = pairs.get("telemetry.reasoning_memo").cloned().filter(|s| !s.is_empty());
        config.telemetry.codebase_log = pairs.get("telemetry.codebase_log").cloned().filter(|s| !s.is_empty());

        config.inference.local_model_path = pairs.get("inference.local_model_path").cloned().filter(|s| !s.is_empty());
        config.inference.local_n_gpu_layers = pairs.get("inference.local_n_gpu_layers").and_then(|s| s.parse().ok());
        config.inference.hf_base = pairs.get("inference.hf_base").cloned().filter(|s| !s.is_empty());
        config.inference.hf_revision = pairs.get("inference.hf_revision").cloned().filter(|s| !s.is_empty());

        config.worker.runtime = pairs.get("worker.runtime").cloned().filter(|s| !s.is_empty());
        config.worker.cpus = pairs.get("worker.cpus").cloned().filter(|s| !s.is_empty());
        config.worker.network = pairs.get("worker.network").cloned().filter(|s| !s.is_empty());
        config.worker.worker_state_dir = pairs.get("worker.worker_state_dir").cloned().filter(|s| !s.is_empty());
        config.worker.worktree_dir = pairs.get("worker.worktree_dir").cloned().filter(|s| !s.is_empty());

        config.updates.channel = pairs.get("updates.channel").cloned().filter(|s| !s.is_empty());
        config.updates.repo = pairs.get("updates.repo").cloned().filter(|s| !s.is_empty());
        config.updates.github_raw_base = pairs.get("updates.github_raw_base").cloned().filter(|s| !s.is_empty());

        config.session.launch_session_name = pairs.get("session.launch_session_name").cloned().filter(|s| !s.is_empty());
        config.session.startup_digest = pairs.get("session.startup_digest").and_then(|s| s.parse().ok());

        config.tools.allowlist = pairs.get("tools.allowlist").cloned().filter(|s| !s.is_empty());

        config
    }

    /// Get the path to the config file
    fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".aish").join("aish.config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty() {
        let config = Config::parse("");
        assert_eq!(config.coordinator.max_rounds, None);
        assert_eq!(config.alerts.bell, None);
    }

    #[test]
    fn test_parse_coordinator() {
        let ini = r#"
[coordinator]
max_rounds = 100
max_failed_attempts = 2
failed_keep = 75
failed_max_age_days = 21
"#;
        let config = Config::parse(ini);
        assert_eq!(config.coordinator.max_rounds, Some(100));
        assert_eq!(config.coordinator.max_failed_attempts, Some(2));
        assert_eq!(config.coordinator.failed_keep, Some(75));
        assert_eq!(config.coordinator.failed_max_age_days, Some(21));
    }

    #[test]
    fn test_parse_alerts() {
        let ini = r#"
[alerts]
bell = true
bell_cmd = paplay /path/to/sound.oga
bell_worker = false
"#;
        let config = Config::parse(ini);
        assert_eq!(config.alerts.bell, Some(true));
        assert_eq!(config.alerts.bell_cmd, Some("paplay /path/to/sound.oga".to_string()));
        assert_eq!(config.alerts.bell_worker, Some(false));
    }

    #[test]
    fn test_parse_worker() {
        let ini = r#"
[worker]
runtime = podman
cpus = 2
worktree_dir = /mnt/nvme/aish-worktrees
"#;
        let config = Config::parse(ini);
        assert_eq!(config.worker.runtime, Some("podman".to_string()));
        assert_eq!(config.worker.cpus, Some("2".to_string()));
        assert_eq!(config.worker.worktree_dir, Some("/mnt/nvme/aish-worktrees".to_string()));
    }

    #[test]
    fn test_parse_comments_and_blanks() {
        let ini = r#"
# This is a comment
[coordinator]
# Another comment
max_rounds = 100

# Blank lines above and below

max_failed_attempts = 3
"#;
        let config = Config::parse(ini);
        assert_eq!(config.coordinator.max_rounds, Some(100));
        assert_eq!(config.coordinator.max_failed_attempts, Some(3));
    }

    #[test]
    fn test_empty_values_are_none() {
        let ini = r#"
[alerts]
bell_cmd = 
"#;
        let config = Config::parse(ini);
        assert_eq!(config.alerts.bell_cmd, None); // empty string becomes None
    }
}
