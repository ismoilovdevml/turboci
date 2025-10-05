use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub cache: CacheConfig,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_redis_url")]
    pub redis_url: String,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: usize,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            redis_url: default_redis_url(),
            ttl_seconds: default_ttl(),
            enabled: default_enabled(),
        }
    }
}

fn default_redis_url() -> String {
    "redis://127.0.0.1:6379".to_string()
}

fn default_ttl() -> usize {
    86400 // 24 hours
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    #[serde(default)]
    pub parallel: bool,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub run: String,
}

impl Config {
    /// Load configuration from YAML file
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;

        let config: Config = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse YAML config: {}", path))?;

        Ok(config)
    }

    /// Save configuration to YAML file
    #[allow(dead_code)]
    pub fn save(&self, path: &str) -> Result<()> {
        let yaml = serde_yaml::to_string(self).context("Failed to serialize config to YAML")?;

        fs::write(path, yaml).with_context(|| format!("Failed to write config file: {}", path))?;

        Ok(())
    }

    /// Create example configuration
    #[allow(dead_code)]
    pub fn example() -> Self {
        Self {
            name: "My TurboCI Pipeline".to_string(),
            version: "1.0".to_string(),
            cache: CacheConfig::default(),
            jobs: vec![
                Job {
                    name: "build".to_string(),
                    parallel: false,
                    steps: vec![
                        Step {
                            name: "Install dependencies".to_string(),
                            run: "npm install".to_string(),
                        },
                        Step {
                            name: "Build project".to_string(),
                            run: "npm run build".to_string(),
                        },
                    ],
                },
                Job {
                    name: "test".to_string(),
                    parallel: true,
                    steps: vec![
                        Step {
                            name: "Run unit tests".to_string(),
                            run: "npm run test:unit".to_string(),
                        },
                        Step {
                            name: "Run integration tests".to_string(),
                            run: "npm run test:integration".to_string(),
                        },
                    ],
                },
                Job {
                    name: "lint".to_string(),
                    parallel: true,
                    steps: vec![Step {
                        name: "Run linter".to_string(),
                        run: "npm run lint".to_string(),
                    }],
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_config_example() {
        let config = Config::example();
        assert_eq!(config.name, "My TurboCI Pipeline");
        assert_eq!(config.jobs.len(), 3);
    }

    #[test]
    fn test_config_save_load() {
        let config = Config::example();
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_str().unwrap();

        config.save(path).unwrap();
        let loaded = Config::load(path).unwrap();

        assert_eq!(config.name, loaded.name);
        assert_eq!(config.jobs.len(), loaded.jobs.len());
    }
}
