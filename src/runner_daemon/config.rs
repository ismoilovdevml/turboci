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
    /// Proxy (http:// or https://) for the runner's requests and its jobs
    pub proxy: Option<String>,
    /// Comma-separated hosts, domains (.corp.local), IPs and CIDRs reached
    /// without the proxy
    pub no_proxy: Option<String>,

    /// Cache configuration
    pub cache_enabled: bool,
    /// Directory of the local job cache (`cache:` in .gitlab-ci.yml)
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// Cache archives not used for this many days are deleted (0 = keep forever)
    pub cache_max_age_days: u64,
    /// Where the runner keeps state it must write, like a rotated runner token
    pub state_dir: String,

    /// `KEY=value` variables added to every job (they override the job's own)
    pub environment: Vec<String>,
    /// Scripts run for every job, as in gitlab-runner: before and after the
    /// checkout, and before and after the job's script (in the same shell)
    pub pre_get_sources_script: Option<String>,
    pub post_get_sources_script: Option<String>,
    pub pre_build_script: Option<String>,
    pub post_build_script: Option<String>,

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
    /// Memory plus swap per container ("-1" = unlimited swap)
    pub memory_swap: Option<String>,
    /// Soft memory limit per container
    pub memory_reservation: Option<String>,
    /// CPU limit per container, e.g. 1.5
    pub cpus: Option<f64>,
    /// Size of /dev/shm in job and service containers, e.g. "1g" (Docker's
    /// default of 64m is too small for browsers and some test runners)
    pub shm_size: Option<String>,
    /// OOM killer preference of job and service containers (-1000 to 1000)
    pub oom_score_adjust: Option<i64>,
    /// Services started for every job, before the job's own `services:`
    pub services: Vec<ServiceConfig>,
    /// Docker client config (`auths`) with registry logins used for pulls,
    /// like ~/.docker/config.json. Default: the service user's
    /// ~/.docker/config.json when it exists
    pub auth_config_file: Option<String>,
    /// Registries reached over HTTP or without verifying their certificate:
    /// docker:dind services get `--insecure-registry` for each
    pub insecure_registries: Vec<String>,
    /// Registry (`host[:port]`) → PEM CA file, mounted into docker:dind
    /// services as /etc/docker/certs.d/<registry>/ca.crt
    pub registry_ca: std::collections::BTreeMap<String, String>,
}

/// A service in `[[executor.docker.services]]`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceConfig {
    pub name: String,
    pub alias: Option<String>,
    pub entrypoint: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
}

/// A size such as "512m", "2g" or "1073741824" in bytes; `field` names it in errors
pub fn parse_size(field: &str, value: &str) -> Result<i64> {
    let value = value.trim().to_ascii_lowercase();
    if value == "-1" {
        return Ok(-1);
    }
    let (number, unit) = value.split_at(
        value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    let multiplier: i64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        _ => anyhow::bail!("{}: unknown unit in {:?}", field, value),
    };
    let number: i64 = number
        .parse()
        .with_context(|| format!("{}: invalid value {:?}", field, value))?;
    number
        .checked_mul(multiplier)
        .with_context(|| format!("{}: {:?} is too large", field, value))
}

impl DockerConfig {
    /// Memory limit in bytes
    pub fn memory_bytes(&self) -> Result<Option<i64>> {
        self.size("memory", &self.memory)
    }

    pub fn memory_swap_bytes(&self) -> Result<Option<i64>> {
        self.size("memory_swap", &self.memory_swap)
    }

    pub fn memory_reservation_bytes(&self) -> Result<Option<i64>> {
        self.size("memory_reservation", &self.memory_reservation)
    }

    pub fn shm_size_bytes(&self) -> Result<Option<i64>> {
        self.size("shm_size", &self.shm_size)
    }

    fn size(&self, field: &str, value: &Option<String>) -> Result<Option<i64>> {
        value
            .as_deref()
            .map(|value| parse_size(&format!("executor.docker.{}", field), value))
            .transpose()
    }

    /// Check every setting that can be wrong
    pub fn validate(&self) -> Result<()> {
        for size in [
            self.memory_bytes()?,
            self.memory_reservation_bytes()?,
            self.shm_size_bytes()?,
        ]
        .into_iter()
        .flatten()
        {
            if size < 0 {
                anyhow::bail!(
                    "executor.docker: sizes must not be negative (only memory_swap may be -1)"
                );
            }
        }
        if let (Some(swap), Some(memory)) = (self.memory_swap_bytes()?, self.memory_bytes()?) {
            if swap != -1 && swap < memory {
                anyhow::bail!("executor.docker.memory_swap must be at least memory (or -1)");
            }
        } else if self.memory_swap_bytes()?.is_some_and(|swap| swap != -1) {
            anyhow::bail!("executor.docker.memory_swap needs memory to be set too");
        }
        if self
            .cpus
            .is_some_and(|cpus| !cpus.is_finite() || cpus <= 0.0 || cpus > 1024.0)
        {
            anyhow::bail!("executor.docker.cpus must be a number between 0 and 1024");
        }
        if self
            .oom_score_adjust
            .is_some_and(|score| !(-1000..=1000).contains(&score))
        {
            anyhow::bail!("executor.docker.oom_score_adjust must be between -1000 and 1000");
        }
        for policy in std::iter::once(&self.pull_policy).chain(&self.allowed_pull_policies) {
            if !matches!(policy.as_str(), "always" | "if-not-present" | "never") {
                anyhow::bail!(
                    "executor.docker pull policies must be always, if-not-present or never, got {:?}",
                    policy
                );
            }
        }
        if let Some(bad) = self.services.iter().find(|s| s.name.trim().is_empty()) {
            anyhow::bail!(
                "executor.docker.services: every service needs a name ({:?})",
                bad
            );
        }
        if let Some(bad) = self
            .volumes
            .iter()
            .find(|v| !v.starts_with('/') && !v.contains(':'))
        {
            anyhow::bail!(
                "executor.docker.volumes: {:?} must be an absolute container path or host:container",
                bad
            );
        }
        let valid_registry = |host: &str| {
            !host.is_empty() && !host.contains('/') && !host.chars().any(char::is_whitespace)
        };
        if let Some(bad) = self
            .insecure_registries
            .iter()
            .chain(self.registry_ca.keys())
            .find(|host| !valid_registry(host))
        {
            anyhow::bail!(
                "executor.docker: registry {:?} must be host or host:port, without a scheme or path",
                bad
            );
        }
        for (registry, file) in &self.registry_ca {
            // dockerd mounts it into dind services and rejects relative sources
            if !Path::new(file).is_absolute() {
                anyhow::bail!(
                    "executor.docker.registry_ca: {} for {} must be an absolute path",
                    file,
                    registry
                );
            }
            let pem = fs::read(file).with_context(|| {
                format!(
                    "executor.docker.registry_ca: cannot read {} for {}",
                    file, registry
                )
            })?;
            let certificates = reqwest::Certificate::from_pem_bundle(&pem).unwrap_or_default();
            if certificates.is_empty() {
                anyhow::bail!(
                    "executor.docker.registry_ca: {} for {} holds no PEM certificate",
                    file,
                    registry
                );
            }
        }
        Ok(())
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
            proxy: None,
            no_proxy: None,
            cache_enabled: true,
            cache_dir: default_cache_dir(),
            cache_max_age_days: 14,
            state_dir: "/var/lib/turboci".to_string(),
            environment: Vec::new(),
            pre_get_sources_script: None,
            post_get_sources_script: None,
            pre_build_script: None,
            post_build_script: None,
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
            memory_swap: None,
            memory_reservation: None,
            cpus: None,
            shm_size: None,
            oom_score_adjust: None,
            services: Vec::new(),
            auth_config_file: None,
            insecure_registries: Vec::new(),
            registry_ca: std::collections::BTreeMap::new(),
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
        match &self.proxy {
            Some(proxy) => {
                let valid = reqwest::Url::parse(proxy).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                });
                if !valid {
                    // The value is not shown: it may hold a password
                    anyhow::bail!("proxy must be an http:// or https:// URL with a host");
                }
            }
            None if self.no_proxy.is_some() => anyhow::bail!("no_proxy needs proxy to be set"),
            None => {}
        }
        self.executor.docker.validate()?;
        if let Some(bad) = self.environment.iter().find(|entry| {
            entry
                .split_once('=')
                .is_none_or(|(key, _)| key.trim().is_empty())
        }) {
            anyhow::bail!(
                "environment entries must look like KEY=value, got {:?}",
                bad
            );
        }
        match self.executor.executor_type.as_str() {
            "docker" | "shell" => Ok(()),
            other => anyhow::bail!(
                "executor_type must be \"docker\" or \"shell\", got {:?}",
                other
            ),
        }
    }

    /// Proxy and CA settings for the runner's HTTP clients
    pub fn network(&self) -> Result<crate::net::Network> {
        let ca_pem = match &self.tls_ca_file {
            Some(path) => Some(
                fs::read_to_string(path)
                    .with_context(|| format!("Cannot read tls_ca_file {}", path))?,
            ),
            None => None,
        };
        Ok(crate::net::Network {
            proxy: self.proxy.clone(),
            no_proxy: self.no_proxy.clone(),
            ca_pem,
        })
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
            .replace(
                "$PROXY_LINES",
                "proxy = \"http://proxy.corp:3128\"\nno_proxy = \"localhost,.corp.local\"",
            )
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

    #[test]
    fn proxy_settings_are_validated() {
        let ok = RunnerConfig::parse(
            "runner_token = \"t\"\nproxy = \"http://user:secret@proxy.corp:3128\"\nno_proxy = \"localhost,.corp.local,10.0.0.0/8\"",
        )
        .unwrap();
        ok.validate().unwrap();
        let network = ok.network().unwrap();
        assert_eq!(
            network.proxy.as_deref(),
            Some("http://user:secret@proxy.corp:3128")
        );
        assert_eq!(
            network.no_proxy.as_deref(),
            Some("localhost,.corp.local,10.0.0.0/8")
        );

        for bad in [
            "proxy = \"proxy.corp:3128\"",
            "proxy = \"socks5://proxy.corp:1080\"",
            "proxy = \"http://\"",
            "no_proxy = \"localhost\"",
        ] {
            let config = RunnerConfig::parse(&format!("runner_token = \"t\"\n{}", bad)).unwrap();
            assert!(config.validate().is_err(), "accepted {}", bad);
        }
    }

    /// A self-signed certificate made with openssl, as PEM
    fn test_certificate(dir: &std::path::Path) -> std::path::PathBuf {
        let cert = dir.join("ca.pem");
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=registry.test",
            ])
            .arg("-keyout")
            .arg(dir.join("key.pem"))
            .arg("-out")
            .arg(&cert)
            .output()
            .unwrap()
            .status;
        assert!(status.success());
        cert
    }

    #[test]
    fn registry_settings_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let cert = test_certificate(dir.path());
        let not_pem = dir.path().join("not.pem");
        std::fs::write(&not_pem, "hello").unwrap();
        let config = |body: &str| {
            RunnerConfig::parse(&format!(
                "runner_token = \"t\"\n[executor.docker]\n{}",
                body
            ))
            .unwrap()
        };

        let ok = config(&format!(
            "insecure_registries = [\"harbor.old.local\", \"10.0.0.5:5000\"]\n[executor.docker.registry_ca]\n\"harbor.corp:8443\" = {:?}",
            cert.to_str().unwrap()
        ));
        ok.validate().unwrap();
        assert_eq!(ok.executor.docker.registry_ca.len(), 1);
        // The same certificate as a path relative to the working directory:
        // readable here, but dockerd rejects relative bind-mount sources
        let relative = std::env::current_dir()
            .unwrap()
            .components()
            .skip(1)
            .map(|_| "..")
            .collect::<std::path::PathBuf>()
            .join(cert.strip_prefix("/").unwrap());
        assert!(std::fs::metadata(&relative).is_ok(), "{:?}", relative);

        for bad in [
            "insecure_registries = [\"\"]".to_string(),
            "insecure_registries = [\"https://harbor.corp\"]".to_string(),
            "insecure_registries = [\"harbor.corp/library\"]".to_string(),
            "[executor.docker.registry_ca]\n\"harbor.corp\" = \"/nonexistent.pem\"".to_string(),
            format!(
                "[executor.docker.registry_ca]\n\"harbor.corp\" = {:?}",
                not_pem.to_str().unwrap()
            ),
            format!(
                "[executor.docker.registry_ca]\n\"https://harbor.corp\" = {:?}",
                cert.to_str().unwrap()
            ),
            format!(
                "[executor.docker.registry_ca]\n\"harbor.corp\" = {:?}",
                relative.to_str().unwrap()
            ),
        ] {
            assert!(config(&bad).validate().is_err(), "accepted {}", bad);
        }
    }

    #[test]
    fn network_reads_the_ca_file() {
        let missing = RunnerConfig {
            tls_ca_file: Some("/nonexistent/ca.pem".to_string()),
            ..RunnerConfig::default()
        };
        assert!(missing.network().is_err());
    }
}
