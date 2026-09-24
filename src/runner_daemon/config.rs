use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Runner daemon configuration. Every field has a default; `validate` rejects
/// values the runner cannot work with.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RunnerConfig {
    /// Number of concurrent jobs
    pub concurrent: u32,

    /// Check interval in seconds
    pub check_interval: u64,

    /// Runner authentication token
    pub runner_token: String,

    /// GitLab URL
    pub gitlab_url: String,

    /// PEM file with the CA that signed the GitLab server's certificate
    /// (self-signed or internal CA); trusted by the runner and given to jobs
    pub tls_ca_file: Option<String>,

    /// Cache configuration
    pub cache_enabled: bool,
    /// Directory of the local job cache (`cache:` in .gitlab-ci.yml)
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// Cache archives not used for this many days are deleted (0 = keep forever)
    pub cache_max_age_days: u64,

    /// Executor configuration
    pub executor: ExecutorConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutorConfig {
    pub executor_type: String, // "docker", "shell"
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub shell: ShellConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    pub default_image: String,
    pub privileged: bool,
    /// Extra binds for job containers, e.g. "/cache:/cache:rw"
    pub volumes: Vec<String>,
    /// Network for jobs without services (jobs with services get their own network)
    pub network_mode: String,
    /// "always", "if-not-present" or "never" (a job's image:pull_policy overrides it)
    pub pull_policy: String,
    /// Pull policies jobs may choose with `image:pull_policy`; empty = only `pull_policy`
    pub allowed_pull_policies: Vec<String>,
    /// Image with git and sh that checks out sources (job images need neither)
    pub helper_image: String,
    /// Memory limit per container, e.g. "2g" or "512m"
    pub memory: Option<String>,
    /// CPU limit per container, e.g. 1.5
    pub cpus: Option<f64>,
}

impl DockerConfig {
    /// Memory limit in bytes
    pub fn memory_bytes(&self) -> Result<Option<i64>> {
        let Some(memory) = self.memory.as_deref() else {
            return Ok(None);
        };
        let memory = memory.trim().to_ascii_lowercase();
        let (number, unit) = memory.split_at(
            memory
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(memory.len()),
        );
        let multiplier: i64 = match unit {
            "" | "b" => 1,
            "k" | "kb" => 1 << 10,
            "m" | "mb" => 1 << 20,
            "g" | "gb" => 1 << 30,
            _ => anyhow::bail!("executor.docker.memory: unknown unit in {:?}", memory),
        };
        let number: i64 = number
            .parse()
            .with_context(|| format!("executor.docker.memory: invalid value {:?}", memory))?;
        let bytes = number
            .checked_mul(multiplier)
            .with_context(|| format!("executor.docker.memory: {:?} is too large", memory))?;
        Ok(Some(bytes))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub work_dir: String,
}

fn default_cache_dir() -> String {
    "/var/lib/turboci/cache".to_string()
}

/// Dotted paths of keys in `content` that the parsed config does not have
fn unknown_keys(content: &str, config: &RunnerConfig) -> Vec<String> {
    fn walk(given: &toml::Table, known: &toml::Table, prefix: &str, out: &mut Vec<String>) {
        for (key, value) in given {
            let path = format!("{}{}", prefix, key);
            match (value, known.get(key)) {
                (toml::Value::Table(given), Some(toml::Value::Table(known))) => {
                    walk(given, known, &format!("{}.", path), out)
                }
                (_, Some(_)) => {}
                (_, None) => out.push(path),
            }
        }
    }

    let (Ok(given), Ok(known)) = (
        content.parse::<toml::Table>(),
        toml::Table::try_from(config),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(&given, &known, "", &mut out);
    out
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            work_dir: "/tmp/turboci-builds".to_string(),
        }
    }
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            concurrent: 4,
            check_interval: 3,
            runner_token: String::new(),
            gitlab_url: "https://gitlab.com".to_string(),
            tls_ca_file: None,
            cache_enabled: true,
            cache_dir: default_cache_dir(),
            cache_max_age_days: 14,
            executor: ExecutorConfig::default(),
        }
    }
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            // Docker isolates jobs from the host. The shell executor runs job
            // scripts directly as the runner's user and must be chosen explicitly.
            executor_type: "docker".to_string(),
            docker: DockerConfig::default(),
            shell: ShellConfig::default(),
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
            pull_policy: "always".to_string(),
            allowed_pull_policies: Vec::new(),
            helper_image: "alpine/git:latest".to_string(),
            memory: None,
            cpus: None,
        }
    }
}

impl RunnerConfig {
    /// Load configuration from TOML file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file: {:?}", path.as_ref()))?;

        let config = Self::parse(&content)
            .with_context(|| format!("Invalid config file: {:?}", path.as_ref()))?;
        config.validate()?;
        Ok(config)
    }

    /// Parse TOML, warning about keys the runner does not know (they are ignored)
    pub fn parse(content: &str) -> Result<Self> {
        let config: RunnerConfig = toml::from_str(content)?;
        for key in unknown_keys(content, &config) {
            tracing::warn!("Ignoring unknown config key `{}`", key);
        }
        Ok(config)
    }

    /// Reject values the runner cannot work with
    pub fn validate(&self) -> Result<()> {
        if self.runner_token.trim().is_empty() {
            anyhow::bail!("runner_token is empty: set the runner authentication token (glrt-...)");
        }
        if !(self.gitlab_url.starts_with("https://") || self.gitlab_url.starts_with("http://")) {
            anyhow::bail!(
                "gitlab_url must start with https:// or http://, got {:?}",
                self.gitlab_url
            );
        }
        if self.concurrent == 0 {
            anyhow::bail!("concurrent must be at least 1");
        }
        if self.check_interval == 0 {
            anyhow::bail!("check_interval must be at least 1 second");
        }
        let docker = &self.executor.docker;
        docker.memory_bytes()?;
        if docker
            .cpus
            .is_some_and(|cpus| !cpus.is_finite() || cpus <= 0.0 || cpus > 1024.0)
        {
            anyhow::bail!("executor.docker.cpus must be a number between 0 and 1024");
        }
        for policy in std::iter::once(&docker.pull_policy).chain(&docker.allowed_pull_policies) {
            if !matches!(policy.as_str(), "always" | "if-not-present" | "never") {
                anyhow::bail!(
                    "executor.docker pull policies must be always, if-not-present or never, got {:?}",
                    policy
                );
            }
        }
        match self.executor.executor_type.as_str() {
            "docker" | "shell" => Ok(()),
            other => anyhow::bail!(
                "executor_type must be \"docker\" or \"shell\", got {:?}",
                other
            ),
        }
    }

    /// Save configuration to TOML file
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let toml_str =
            toml::to_string_pretty(self).context("Failed to serialize config to TOML")?;

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
        let config = RunnerConfig {
            runner_token: "glrt-test".to_string(),
            ..RunnerConfig::default()
        };

        config.save(temp_file.path()).unwrap();
        let loaded = RunnerConfig::load(temp_file.path()).unwrap();

        assert_eq!(config.concurrent, loaded.concurrent);
        assert_eq!(config.gitlab_url, loaded.gitlab_url);
    }

    fn with_token(content: &str) -> String {
        content.replace("runner_token = \"\"", "runner_token = \"glrt-test\"")
    }

    /// The config block that follows `marker` in a Markdown file
    fn toml_block_after(markdown: &str, marker: &str) -> String {
        let rest = &markdown[markdown.find(marker).expect("marker")..];
        let start = rest.find("```toml\n").expect("toml block") + "```toml\n".len();
        let end = rest[start..].find("```").expect("block end");
        rest[start..start + end].to_string()
    }

    #[test]
    fn shipped_configs_load() {
        let readme = toml_block_after(include_str!("../../README.md"), "Minimal configuration");
        let config = RunnerConfig::parse(&readme).unwrap();
        config.validate().unwrap();
        assert!(
            unknown_keys(&readme, &config).is_empty(),
            "{:?}",
            unknown_keys(&readme, &config)
        );

        let example = include_str!("../../runner-config.example.toml");
        let config = RunnerConfig::parse(example).unwrap();
        assert!(
            unknown_keys(example, &config).is_empty(),
            "{:?}",
            unknown_keys(example, &config)
        );

        // The template install.sh writes (runner_token is left for the user to fill in)
        let install = include_str!("../../install.sh");
        let start = install.find("cat > \"$CONFIG_FILE\" << EOF\n").unwrap();
        let body = &install[start..];
        let body = &body[body.find('\n').unwrap() + 1..body.find("\nEOF\n").unwrap()];
        let rendered = body
            .replace("$CONCURRENT", "4")
            .replace("$RUNNER_TOKEN", "")
            .replace("$GITLAB_URL", "https://gitlab.com")
            .replace("$TLS_CA_LINE", "tls_ca_file = \"/etc/turboci-ca.pem\"")
            .replace("$EXECUTOR", "docker")
            .replace("$STATE_DIR", "/var/lib/turboci")
            .replace("$SERVICE_USER", "turboci");
        let config = RunnerConfig::parse(&rendered).unwrap();
        assert!(unknown_keys(&rendered, &config).is_empty());
        assert!(
            config.validate().is_err(),
            "an empty token must not validate"
        );
        RunnerConfig::parse(&with_token(&rendered))
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let config = RunnerConfig::parse("runner_token = \"glrt-x\"").unwrap();

        config.validate().unwrap();
        assert_eq!(config.gitlab_url, "https://gitlab.com");
        assert_eq!(config.executor.executor_type, "docker");
        assert_eq!(config.check_interval, 3);
    }

    #[test]
    fn rejects_unusable_values() {
        for bad in [
            "runner_token = \"\"",
            "runner_token = \"t\"\nconcurrent = 0",
            "runner_token = \"t\"\ncheck_interval = 0",
            "runner_token = \"t\"\ngitlab_url = \"gitlab.com\"",
            "runner_token = \"t\"\n[executor]\nexecutor_type = \"kubernetes\"",
            "runner_token = \"t\"\n[executor.docker]\ncpus = nan",
            "runner_token = \"t\"\n[executor.docker]\ncpus = inf",
            "runner_token = \"t\"\n[executor.docker]\nmemory = \"99999999999g\"",
        ] {
            let config = RunnerConfig::parse(bad).unwrap();
            assert!(config.validate().is_err(), "accepted: {}", bad);
        }
    }

    #[test]
    fn parses_docker_limits() {
        let config = RunnerConfig::parse(
            "runner_token = \"t\"\n[executor.docker]\nmemory = \"512m\"\ncpus = 1.5\npull_policy = \"if-not-present\"",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.executor.docker.memory_bytes().unwrap(),
            Some(512 << 20)
        );

        for bad in [
            "memory = \"lots\"",
            "cpus = 0.0",
            "pull_policy = \"sometimes\"",
        ] {
            let config =
                RunnerConfig::parse(&format!("runner_token = \"t\"\n[executor.docker]\n{}", bad))
                    .unwrap();
            assert!(config.validate().is_err(), "accepted {}", bad);
        }
    }

    #[test]
    fn reports_unknown_keys() {
        let content =
            "runner_token = \"t\"\ncache_ttl_seconds = 60\n[executor.docker]\nimage = \"x\"";
        let config = RunnerConfig::parse(content).unwrap();

        assert_eq!(
            unknown_keys(content, &config),
            vec![
                "cache_ttl_seconds".to_string(),
                "executor.docker.image".to_string()
            ]
        );
    }

    #[test]
    fn test_default_executor_is_docker() {
        assert_eq!(RunnerConfig::default().executor.executor_type, "docker");
        assert_eq!(ExecutorConfig::default().executor_type, "docker");
    }
}
