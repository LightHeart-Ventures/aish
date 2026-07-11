use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Pluggable configuration source abstraction
#[derive(Debug, Clone)]
pub enum ConfigSource {
    Environment { prefix: String },
    File { path: PathBuf },
    Remote { url: String },
}

/// Trait for loading configuration from heterogeneous sources
#[async_trait::async_trait]
pub trait ConfigLoader: Send + Sync {
    async fn load(&self, source: &ConfigSource) -> Result<Config, ConfigError>;
    async fn watch(&self, source: &ConfigSource) -> Result<(), ConfigError>;
}

/// Runtime configuration schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: String,
    pub runtime: RuntimeConfig,
    pub features: HashMap<String, bool>,
    pub limits: LimitsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub max_workers: usize,
    pub max_coordinator_turns: usize,
    pub token_budget: u32,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    pub max_file_read_bytes: usize,
    pub max_glob_results: usize,
    pub max_background_jobs: usize,
}

#[derive(Debug)]
pub enum ConfigError {
    IoError(String),
    ParseError(String),
    ValidationError(String),
}

/// Hot-reload configuration manager
pub struct ConfigManager {
    current: Arc<RwLock<Config>>,
    loader: Arc<dyn ConfigLoader>,
}

impl ConfigManager {
    pub fn new(loader: Arc<dyn ConfigLoader>) -> Self {
        Self {
            current: Arc::new(RwLock::new(Self::default_config())),
            loader,
        }
    }

    pub async fn load_from_source(&self, source: &ConfigSource) -> Result<(), ConfigError> {
        let config = self.loader.load(source).await?;
        let mut current = self.current.write().await;
        *current = config;
        Ok(())
    }

    pub async fn get(&self) -> Config {
        self.current.read().await.clone()
    }

    pub async fn watch(&self, source: &ConfigSource) -> Result<(), ConfigError> {
        self.loader.watch(source).await
    }

    fn default_config() -> Config {
        Config {
            version: "1.0.0".to_string(),
            runtime: RuntimeConfig {
                max_workers: 10,
                max_coordinator_turns: 5,
                token_budget: 200_000,
                timeout_secs: 120,
            },
            features: HashMap::new(),
            limits: LimitsConfig {
                max_file_read_bytes: 5_000,
                max_glob_results: 1000,
                max_background_jobs: 50,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ConfigManager::default_config();
        assert_eq!(config.runtime.max_workers, 10);
        assert_eq!(config.limits.max_file_read_bytes, 5_000);
    }
}
