use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// Number of concurrent jobs
    pub concurrent: u32,

    /// Check interval in seconds
    pub check_interval: u64,

    /// Runner authentication token
    pub runner_token: String,

    /// GitLab URL
    pub gitlab_url: String,

    /// Cache configuration
    pub cache_enabled: bool,
    pub redis_url: String,
    pub s3_bucket: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_prefix: String,

    /// Storage threshold (bytes)
    /// Files larger than this go to S3, smaller to Redis
    pub storage_threshold: usize,

    /// Executor configuration
    pub executor: ExecutorConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorConfig {
    pub executor_type: String, // "docker", "shell", "kubernetes"
    pub docker: DockerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    pub default_image: String,
    pub privileged: bool,
    pub volumes: Vec<String>,
    pub network_mode: String,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            concurrent: 4,
            check_interval: 3,
            runner_token: String::new(),
            gitlab_url: "https://gitlab.com".to_string(),
            cache_enabled: true,
            redis_url: "redis://127.0.0.1:6379".to_string(),
            s3_bucket: None,
            s3_endpoint: None,
            s3_prefix: "turboci".to_string(),
            storage_threshold: 10 * 1024 * 1024, // 10MB
            executor: ExecutorConfig::default(),
        }
    }
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            executor_type: "docker".to_string(),
            docker: DockerConfig::default(),
        }
    }
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            default_image: "alpine:latest".to_string(),
            privileged: false,
            volumes: vec![],
            network_mode: "bridge".to_string(),
        }
    }
}

impl RunnerConfig {
    /// Load configuration from TOML file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file: {:?}", path.as_ref()))?;

        let config: RunnerConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {:?}", path.as_ref()))?;

        Ok(config)
    }

    /// Save configuration to TOML file
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let toml_str = toml::to_string_pretty(self)
            .context("Failed to serialize config to TOML")?;

        fs::write(path.as_ref(), toml_str)
            .with_context(|| format!("Failed to write config file: {:?}", path.as_ref()))?;

        Ok(())
    }

    /// Create example configuration file
    pub fn create_example<P: AsRef<Path>>(path: P) -> Result<()> {
        let config = Self::default();
        config.save(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_config_load_save() {
        let temp_file = NamedTempFile::new().unwrap();
        let config = RunnerConfig::default();

        config.save(temp_file.path()).unwrap();
        let loaded = RunnerConfig::load(temp_file.path()).unwrap();

        assert_eq!(config.concurrent, loaded.concurrent);
        assert_eq!(config.gitlab_url, loaded.gitlab_url);
    }
}
