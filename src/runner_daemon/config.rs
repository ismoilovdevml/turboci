use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Settings of the whole process rather than of one runner: with
/// `[[runners]]` they are set once, at the top of the file
pub const GLOBAL_KEYS: [&str; 6] = [
    "concurrent",
    "check_interval",
    "metrics_listen",
    "min_free_memory",
    "min_free_disk",
    "max_load_avg",
];

/// Runner daemon configuration. Every field has a default; `validate` rejects
/// values the runner cannot work with.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RunnerConfig {
    /// Name of the runner in logs and metrics (the `runner` label); every
    /// `[[runners]]` entry needs its own
    pub name: String,
    /// Jobs this runner runs at the same time at most, within `concurrent`
    /// (0 = only `concurrent` limits it), as gitlab-runner's `limit`
    pub limit: u32,

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
    /// Total size of the cache archives of all projects, such as "50g"; the
    /// least recently used are deleted beyond it (unset or "0" = no limit)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_max_size: Option<String>,
    /// Restore the ZIP archives versions up to 0.10 saved (`<key>.zip`),
    /// locally and from S3, for keys without a tar.zst archive. Each cache
    /// miss then costs a second S3 request; set false once every runner
    /// sharing the cache runs 0.11.0 or later.
    pub cache_legacy_zip: bool,
    /// Where the runner keeps state it must write, like a rotated runner token
    pub state_dir: String,
    /// Most job log kept, in KiB, as gitlab-runner's `output_limit`; output
    /// past it is dropped while the job keeps running
    pub output_limit: u64,

    /// `KEY=value` variables added to every job, as gitlab-runner's
    /// `environment`: the job's own variables (from GitLab and the pipeline)
    /// override them. The value is kept as written, spaces included.
    pub environment: Vec<String>,
    /// Scripts run for every job, as in gitlab-runner: before and after the
    /// checkout, and before and after the job's script (in the same shell)
    pub pre_get_sources_script: Option<String>,
    pub post_get_sources_script: Option<String>,
    pub pre_build_script: Option<String>,
    pub post_build_script: Option<String>,

    /// Admission control: no job is requested while the host has less memory
    /// available (MemAvailable, Linux), such as "2g" (unset or "0" = off)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_free_memory: Option<String>,
    /// No job is requested while the builds or cache directory's file system
    /// has less space free, such as "10g" (unset or "0" = off)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_free_disk: Option<String>,
    /// No job is requested while the 1-minute load average is above this (Linux)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_load_avg: Option<f64>,
    /// `IP:port` of the Prometheus endpoint (GET /metrics), such as
    /// "127.0.0.1:9252" (unset = off)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics_listen: Option<String>,

    /// S3-compatible storage shared by runners, used next to `cache_dir`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_s3: Option<S3CacheConfig>,

    /// Executor configuration
    pub executor: ExecutorConfig,
}

/// `[cache_s3]`: S3-compatible storage shared by runners, the cache's second layer.
/// `Debug` is written by hand so the secret key is never printed.
#[derive(Clone, Serialize, Deserialize)]
pub struct S3CacheConfig {
    /// `https://host[:port]/bucket[/prefix]`; `http://` for storage without TLS
    pub url: String,
    pub access_key: String,
    pub secret_key: String,
    /// Signing region; default from *.amazonaws.com hosts, else us-east-1
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

impl std::fmt::Debug for S3CacheConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3CacheConfig")
            .field("url", &self.url)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("region", &self.region)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutorConfig {
    pub executor_type: String, // "docker", "shell"
    /// A project's next job reuses its workspace (the checkout is fetched
    /// instead of cloned); workspaces no job used for this many days are
    /// deleted. 0 deletes every workspace when its job ends.
    pub workspace_max_age_days: u64,
    /// Total size of the kept workspaces, such as "50g": the least recently
    /// used are deleted beyond it, never one in use (unset or "0" = no limit)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_max_size: Option<String>,
    /// One workspace per project and slot for protected and unprotected refs
    /// alike, as gitlab-runner. Off: protected refs get workspaces of their
    /// own, which jobs of unprotected refs cannot leave files or hooks in.
    pub share_workspace_across_protection: bool,
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub shell: ShellConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    /// Host directory of the job workspaces, bind-mounted as /builds.
    /// Default: `<state_dir>/builds`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builds_dir: Option<String>,
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
    /// Relative CPU weight of job and service containers (Docker's default
    /// is 1024). Unlike `cpus` it only matters while the CPUs are busy: a
    /// container may use idle CPU whatever its weight
    pub cpu_shares: Option<i64>,
    /// Size of /dev/shm in job and service containers, e.g. "1g" (Docker's
    /// default of 64m is too small for browsers and some test runners)
    pub shm_size: Option<String>,
    /// OOM killer preference of job and service containers (-1000 to 1000)
    pub oom_score_adjust: Option<i64>,
    /// Most processes a job or service container may run (fork bomb guard)
    pub pids_limit: Option<i64>,
    /// Seconds the scripts wait for the services' ports to accept
    /// connections, like gitlab-runner's: 0 means 30, -1 does not wait
    pub wait_for_services_timeout: i64,
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
    /// tmpfs mounts of job containers: container path → mount options
    /// (empty for Docker's defaults), e.g. "/tmp" = "rw,noexec,size=1g"
    pub tmpfs: std::collections::BTreeMap<String, String>,
    /// Storage driver options of job and service containers, e.g.
    /// size = "20G" (overlay2 needs xfs with pquota for it)
    pub storage_opt: std::collections::BTreeMap<String, String>,

    // Container options of gitlab-runner's [runners.docker]. Like there,
    // services get only extra_hosts, dns, dns_search and runtime.
    /// `host:ip` entries added to /etc/hosts of job, service and checkout
    /// containers ("host-gateway" for the host's address)
    pub extra_hosts: Vec<String>,
    /// DNS servers (IP addresses) of job, service and checkout containers
    pub dns: Vec<String>,
    /// DNS search domains of job, service and checkout containers
    pub dns_search: Vec<String>,
    /// Linux capabilities added to job containers, e.g. "SYS_PTRACE"
    pub cap_add: Vec<String>,
    /// Linux capabilities dropped from job containers, e.g. "NET_RAW"
    pub cap_drop: Vec<String>,
    /// Security options of job containers, e.g. "no-new-privileges" or
    /// "seccomp=/etc/turboci/seccomp.json" (the file's JSON is sent)
    pub security_opt: Vec<String>,
    /// Host devices of job containers: "/dev/x[:/dev/y[:rwm]]"
    pub devices: Vec<String>,
    /// GPUs of job containers, as `docker run --gpus`: "all", a count, or
    /// e.g. "\"device=0,1\"" or "count=2,capabilities=utility"
    pub gpus: Option<String>,
    /// Kernel parameters of job containers, e.g.
    /// "net.ipv4.ip_unprivileged_port_start" = "0"
    pub sysctls: std::collections::BTreeMap<String, String>,
    /// Resource limits of job containers: name → "soft:hard" or one value
    /// for both, e.g. nofile = "1024:4096"
    pub ulimit: std::collections::BTreeMap<String, String>,
    /// OCI runtime of job and service containers, e.g. "nvidia"
    pub runtime: Option<String>,

    /// Glob patterns of the images jobs may use with `image:` (empty: any);
    /// `default_image` is always allowed
    pub allowed_images: Vec<String>,
    /// Glob patterns of the images jobs may use with `services:` (empty:
    /// any); the services configured here are always allowed
    pub allowed_services: Vec<String>,
    /// Credential helpers a job's DOCKER_AUTH_CONFIG may run on the runner's
    /// host: helper name (`ecr-login` for docker-credential-ecr-login) →
    /// glob patterns of the registries it may be asked about. Helpers in
    /// `auth_config_file` are the operator's and always run.
    pub allowed_docker_credential_helpers: std::collections::BTreeMap<String, Vec<String>>,

    /// Images no container uses and no job used for this many days are
    /// removed by the hourly housekeeping (0: every unused image; unset: off)
    pub image_max_age_days: Option<u64>,
    /// Total size of the images no container uses, e.g. "50g"; beyond it the
    /// least recently used are removed (unset or "0": no limit)
    pub image_max_size: Option<String>,
}

/// gitlab-runner's default `wait_for_services_timeout`, in seconds
const DEFAULT_WAIT_FOR_SERVICES_TIMEOUT: i64 = 30;
/// Longest `wait_for_services_timeout` (a day), far beyond any job's need
const MAX_WAIT_FOR_SERVICES_TIMEOUT: i64 = 86_400;

/// Docker's `--ulimit` names
const ULIMIT_NAMES: &[&str] = &[
    "core",
    "cpu",
    "data",
    "fsize",
    "locks",
    "memlock",
    "msgqueue",
    "nice",
    "nofile",
    "nproc",
    "rss",
    "rtprio",
    "rttime",
    "sigpending",
    "stack",
];

/// `pattern` compiled with gitlab-runner's (doublestar) glob rules: `*` and
/// `?` do not match `/`, `**` as a whole path segment matches any number of
/// segments, `[abc]` matches one of the characters
pub fn glob_pattern(field: &str, pattern: &str) -> Result<glob::Pattern> {
    if pattern.trim().is_empty() {
        anyhow::bail!("{}: empty pattern", field);
    }
    glob::Pattern::new(pattern).map_err(|e| {
        anyhow::anyhow!(
            "{}: {:?} is not a valid glob pattern ({}; `**` must be a whole path segment)",
            field,
            pattern,
            e.msg
        )
    })
}

/// Whether `name` matches one of `patterns` (see `glob_pattern`); invalid
/// patterns, rejected by validation, match nothing
pub fn glob_matches(patterns: &[String], name: &str) -> bool {
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    patterns.iter().any(|pattern| {
        glob::Pattern::new(pattern).is_ok_and(|pattern| pattern.matches_with(name, options))
    })
}

/// A `host:ip` (or `host=ip`) entry of `extra_hosts` as dockerd takes it
/// (`host:ip`, an IPv6 address without brackets)
pub fn extra_host(entry: &str) -> Result<String> {
    let invalid = || {
        anyhow::anyhow!(
            "executor.docker.extra_hosts: {:?} must be host:ip, e.g. \"gitlab.corp:10.0.0.5\" or \"host.docker.internal:host-gateway\"",
            entry
        )
    };
    let (host, address) = entry
        .split_once('=')
        .or_else(|| entry.split_once(':'))
        .ok_or_else(invalid)?;
    let address = address
        .strip_prefix('[')
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or(address);
    let valid_host = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !valid_host || (address != "host-gateway" && address.parse::<std::net::IpAddr>().is_err()) {
        return Err(invalid());
    }
    Ok(format!("{}:{}", host, address))
}

/// A `devices` entry, "host[:container[:permissions]]" like gitlab-runner
/// (the container path defaults to the host path, permissions to "rwm")
pub fn device_mapping(entry: &str) -> Result<super::docker_api::DeviceMapping> {
    let parts: Vec<&str> = entry.split(':').collect();
    let host = parts[0];
    let container = parts.get(1).copied().unwrap_or(host);
    let permissions = parts.get(2).copied().unwrap_or("rwm");
    let valid_permissions = !permissions.is_empty()
        && permissions.chars().all(|c| matches!(c, 'r' | 'w' | 'm'))
        && permissions.len() <= 3;
    if parts.len() > 3
        || !host.starts_with('/')
        || !container.starts_with('/')
        || !valid_permissions
    {
        anyhow::bail!(
            "executor.docker.devices: {:?} must be /dev/host[:/dev/container[:rwm]]",
            entry
        );
    }
    Ok(super::docker_api::DeviceMapping {
        path_on_host: host.to_string(),
        path_in_container: container.to_string(),
        cgroup_permissions: permissions.to_string(),
    })
}

/// Fields of a comma-separated line where "double quotes" protect commas,
/// as Go's csv reader (used by `docker run --gpus`) reads them
fn csv_fields(line: &str) -> Result<Vec<String>> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                fields.last_mut().expect("never empty").push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(String::new()),
            c => fields.last_mut().expect("never empty").push(c),
        }
    }
    if quoted {
        anyhow::bail!("unterminated quote");
    }
    Ok(fields)
}

/// The device request of a `gpus` value, parsed like `docker run --gpus`:
/// "all", a count, or `count=`, `device=`, `driver=`, `capabilities=` and
/// `options=` fields ("gpu" is always among the capabilities)
pub fn gpu_request(value: &str) -> Result<super::docker_api::DeviceRequest> {
    let invalid = |why: String| {
        anyhow::anyhow!(
            "executor.docker.gpus: {:?} is not a valid GPU request ({}); use \"all\", a count, or e.g. \"\\\"device=0,1\\\"\"",
            value,
            why
        )
    };
    let count = |text: &str| -> Result<i64> {
        match text {
            "all" => Ok(-1),
            text => text
                .parse::<i64>()
                .ok()
                .filter(|count| *count > 0)
                .ok_or_else(|| {
                    invalid(format!(
                        "count {:?} must be \"all\" or a positive number",
                        text
                    ))
                }),
        }
    };
    let mut request = super::docker_api::DeviceRequest::default();
    let mut seen = std::collections::HashSet::new();
    let mut counted = false;
    for field in csv_fields(value.trim()).map_err(|e| invalid(e.to_string()))? {
        let (key, val) = match field.split_once('=') {
            Some((key, val)) => (key, Some(val)),
            None => (field.as_str(), None),
        };
        let key_name = if val.is_none() { "count" } else { key };
        if !seen.insert(key_name.to_string()) {
            return Err(invalid(format!("{} is given twice", key_name)));
        }
        match (key, val) {
            (count_text, None) | ("count", Some(count_text)) => {
                request.count = count(count_text)?;
                counted = true;
            }
            ("driver", Some(driver)) => request.driver = driver.to_string(),
            ("device", Some(devices)) => {
                let ids: Vec<String> = devices.split(',').map(str::to_string).collect();
                if ids.iter().any(|id| id.trim().is_empty()) {
                    return Err(invalid("empty device id".to_string()));
                }
                request.device_ids = Some(ids);
            }
            ("capabilities", Some(capabilities)) => {
                let mut caps: Vec<String> = capabilities.split(',').map(str::to_string).collect();
                caps.push("gpu".to_string());
                request.capabilities = vec![caps];
            }
            ("options", Some(options)) => {
                for option in csv_fields(options).map_err(|e| invalid(e.to_string()))? {
                    let (key, val) = option.split_once('=').unwrap_or((option.as_str(), ""));
                    request.options.insert(key.to_string(), val.to_string());
                }
            }
            (key, Some(_)) => return Err(invalid(format!("unknown field {:?}", key))),
        }
    }
    if counted && request.device_ids.is_some() {
        return Err(invalid(
            "a count and device ids exclude each other".to_string(),
        ));
    }
    if !counted && request.device_ids.is_none() {
        request.count = 1;
    }
    if request.capabilities.is_empty() {
        request.capabilities = vec![vec!["gpu".to_string()]];
    }
    Ok(request)
}

/// A `ulimit` entry: "soft:hard", or one value for both (-1: unlimited)
pub fn ulimit(name: &str, value: &str) -> Result<super::docker_api::Ulimit> {
    let invalid = || {
        anyhow::anyhow!(
            "executor.docker.ulimit: {} = {:?} must be \"soft:hard\" or one number, with soft at most hard (-1: unlimited)",
            name,
            value
        )
    };
    if !ULIMIT_NAMES.contains(&name) {
        anyhow::bail!(
            "executor.docker.ulimit: unknown limit {:?}, expected one of {}",
            name,
            ULIMIT_NAMES.join(", ")
        );
    }
    let number = |text: &str| {
        text.trim()
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= -1)
            .ok_or_else(invalid)
    };
    let (soft, hard) = match value.split_once(':') {
        Some((soft, hard)) => (number(soft)?, number(hard)?),
        None => {
            let both = number(value)?;
            (both, both)
        }
    };
    // -1 is RLIM_INFINITY, above every other value
    let rank = |n: i64| if n == -1 { i64::MAX } else { n };
    if rank(soft) > rank(hard) {
        return Err(invalid());
    }
    Ok(super::docker_api::Ulimit {
        name: name.to_string(),
        soft,
        hard,
    })
}

/// A security option with a seccomp profile file replaced by the file's
/// JSON, which is what dockerd takes (the docker CLI does the same)
pub fn security_opt(option: &str) -> Result<String> {
    match option.split_once('=') {
        Some(("seccomp", profile))
            if !matches!(profile, "unconfined" | "builtin") && !profile.starts_with('{') =>
        {
            let json = fs::read_to_string(profile).with_context(|| {
                format!(
                    "executor.docker.security_opt: cannot read the seccomp profile {}",
                    profile
                )
            })?;
            let parsed: serde_json::Value = serde_json::from_str(&json).with_context(|| {
                format!(
                    "executor.docker.security_opt: seccomp profile {} is not JSON",
                    profile
                )
            })?;
            Ok(format!("seccomp={}", parsed))
        }
        _ => Ok(option.to_string()),
    }
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
    /// How long the scripts wait for services to accept connections; `None`
    /// when they do not wait (`wait_for_services_timeout` -1). Like
    /// gitlab-runner, 0 means the default of 30 seconds.
    pub fn service_wait(&self) -> Option<std::time::Duration> {
        match self.wait_for_services_timeout {
            seconds if seconds < 0 => None,
            0 => Some(std::time::Duration::from_secs(
                DEFAULT_WAIT_FOR_SERVICES_TIMEOUT as u64,
            )),
            seconds => Some(std::time::Duration::from_secs(seconds as u64)),
        }
    }

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
        if self.pids_limit.is_some_and(|limit| limit < 1) {
            anyhow::bail!("executor.docker.pids_limit must be at least 1 (unset for no limit)");
        }
        if !(-1..=MAX_WAIT_FOR_SERVICES_TIMEOUT).contains(&self.wait_for_services_timeout) {
            anyhow::bail!(
                "executor.docker.wait_for_services_timeout must be -1 (no wait), 0 (30) or \
                 seconds up to {}",
                MAX_WAIT_FOR_SERVICES_TIMEOUT
            );
        }
        // dockerd silently clamps weights outside this range
        if self
            .cpu_shares
            .is_some_and(|shares| !(2..=262_144).contains(&shares))
        {
            anyhow::bail!("executor.docker.cpu_shares must be between 2 and 262144");
        }
        for (path, options) in &self.tmpfs {
            let clean = Path::new(path).components().all(|part| {
                matches!(
                    part,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            });
            if !path.starts_with('/') || path == "/" || !clean || path.contains([':', ',']) {
                anyhow::bail!(
                    "executor.docker.tmpfs: {:?} must be an absolute container path other than /",
                    path
                );
            }
            if Path::new(path).starts_with("/builds") {
                anyhow::bail!(
                    "executor.docker.tmpfs: {:?} would hide the job's workspace (/builds)",
                    path
                );
            }
            if options.chars().any(char::is_whitespace) {
                anyhow::bail!(
                    "executor.docker.tmpfs: options for {} must not contain spaces, got {:?}",
                    path,
                    options
                );
            }
        }
        for (key, value) in &self.storage_opt {
            let valid = |text: &str| {
                !text.is_empty() && !text.contains('=') && !text.chars().any(char::is_whitespace)
            };
            if !valid(key) || !valid(value) {
                anyhow::bail!(
                    "executor.docker.storage_opt: {:?} = {:?} must be a non-empty option and value, e.g. size = \"20G\"",
                    key,
                    value
                );
            }
        }
        for policy in std::iter::once(&self.pull_policy).chain(&self.allowed_pull_policies) {
            if !matches!(policy.as_str(), "always" | "if-not-present" | "never") {
                anyhow::bail!(
                    "executor.docker pull policies must be always, if-not-present or never, got {:?}",
                    policy
                );
            }
        }
        if let Some(dir) = self.builds_dir.as_deref() {
            // dockerd rejects relative bind mount sources
            if !Path::new(dir).is_absolute() {
                anyhow::bail!(
                    "executor.docker.builds_dir must be an absolute path, got {:?}",
                    dir
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
        self.validate_container_options()?;
        self.validate_image_policies()?;
        Ok(())
    }

    /// Check the gitlab-runner container options (extra_hosts ... runtime)
    fn validate_container_options(&self) -> Result<()> {
        let word = |text: &str| !text.is_empty() && !text.chars().any(char::is_whitespace);
        for entry in &self.extra_hosts {
            extra_host(entry)?;
        }
        if let Some(bad) = self
            .dns
            .iter()
            .find(|server| server.parse::<std::net::IpAddr>().is_err())
        {
            anyhow::bail!(
                "executor.docker.dns: {:?} must be the IP address of a DNS server",
                bad
            );
        }
        if let Some(bad) = self
            .dns_search
            .iter()
            .find(|domain| !word(domain) || domain.contains(','))
        {
            anyhow::bail!(
                "executor.docker.dns_search: {:?} must be a domain, e.g. \"corp.local\"",
                bad
            );
        }
        let capability = |cap: &str| {
            cap.eq_ignore_ascii_case("ALL")
                || (!cap.is_empty() && cap.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        };
        if let Some(bad) = self
            .cap_add
            .iter()
            .chain(&self.cap_drop)
            .find(|cap| !capability(cap))
        {
            anyhow::bail!(
                "executor.docker cap_add/cap_drop: {:?} must be a capability such as \"NET_RAW\" or \"ALL\"",
                bad
            );
        }
        for option in &self.security_opt {
            if !word(option) {
                anyhow::bail!(
                    "executor.docker.security_opt: {:?} must not be empty or contain spaces",
                    option
                );
            }
            security_opt(option)?;
        }
        for entry in &self.devices {
            device_mapping(entry)?;
        }
        if let Some(gpus) = self.gpus.as_deref().filter(|gpus| !gpus.trim().is_empty()) {
            gpu_request(gpus)?;
        }
        for (key, value) in &self.sysctls {
            if !word(key) || key.contains('=') || value.contains(['\n', '\r']) {
                anyhow::bail!(
                    "executor.docker.sysctls: {:?} = {:?} must be a kernel parameter and its value",
                    key,
                    value
                );
            }
        }
        for (name, value) in &self.ulimit {
            ulimit(name, value)?;
        }
        if self
            .runtime
            .as_deref()
            .is_some_and(|runtime| !word(runtime))
        {
            anyhow::bail!(
                "executor.docker.runtime must be a runtime name such as \"nvidia\", got {:?}",
                self.runtime
            );
        }
        Ok(())
    }

    /// Check allowed_images, allowed_services, the credential helper
    /// allowlist and the image collection settings
    fn validate_image_policies(&self) -> Result<()> {
        for pattern in &self.allowed_images {
            glob_pattern("executor.docker.allowed_images", pattern)?;
        }
        for pattern in &self.allowed_services {
            glob_pattern("executor.docker.allowed_services", pattern)?;
        }
        for (helper, registries) in &self.allowed_docker_credential_helpers {
            if !super::credential_helper::valid_name(helper) {
                anyhow::bail!(
                    "executor.docker.allowed_docker_credential_helpers: {:?} must be a helper name such as \"ecr-login\" (for docker-credential-ecr-login)",
                    helper
                );
            }
            if registries.is_empty() {
                anyhow::bail!(
                    "executor.docker.allowed_docker_credential_helpers: {} needs registry patterns, e.g. [\"*.dkr.ecr.*.amazonaws.com\"]",
                    helper
                );
            }
            for pattern in registries {
                glob_pattern("executor.docker.allowed_docker_credential_helpers", pattern)?;
            }
        }
        self.image_max_size_bytes()?;
        Ok(())
    }

    /// `image_max_size` in bytes; `None` when unset or "0"
    pub fn image_max_size_bytes(&self) -> Result<Option<u64>> {
        optional_size(
            "executor.docker.image_max_size",
            self.image_max_size.as_deref(),
        )
    }

    /// Host devices of job containers (validated by `validate`)
    pub fn device_mappings(&self) -> Vec<super::docker_api::DeviceMapping> {
        self.devices
            .iter()
            .filter_map(|entry| device_mapping(entry).ok())
            .collect()
    }

    /// The GPU request of job containers (validated by `validate`)
    pub fn gpu_requests(&self) -> Vec<super::docker_api::DeviceRequest> {
        self.gpus
            .as_deref()
            .filter(|gpus| !gpus.trim().is_empty())
            .and_then(|gpus| gpu_request(gpus).ok())
            .into_iter()
            .collect()
    }

    /// Resource limits of job containers (validated by `validate`)
    pub fn ulimits(&self) -> Vec<super::docker_api::Ulimit> {
        self.ulimit
            .iter()
            .filter_map(|(name, value)| ulimit(name, value).ok())
            .collect()
    }

    /// `extra_hosts` as dockerd takes them (validated by `validate`)
    pub fn extra_host_entries(&self) -> Vec<String> {
        self.extra_hosts
            .iter()
            .filter_map(|entry| extra_host(entry).ok())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Host directory of the job workspaces. Default: `<state_dir>/builds`
    pub work_dir: String,
}

fn default_cache_dir() -> String {
    "/var/lib/turboci/cache".to_string()
}

/// Dotted paths of keys in `content` that the parsed config does not have
#[cfg(test)]
fn unknown_keys(content: &str, config: &RunnerConfig) -> Vec<String> {
    content
        .parse::<toml::Table>()
        .map(|given| unknown_table_keys(&given, config))
        .unwrap_or_default()
}

/// Dotted paths of keys in `given` that the parsed config does not have
fn unknown_table_keys(given: &toml::Table, config: &RunnerConfig) -> Vec<String> {
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

    let Ok(known) = toml::Table::try_from(config) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(given, &known, "", &mut out);
    out
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            // `finish` puts it in the config's state_dir
            work_dir: "/var/lib/turboci/builds".to_string(),
        }
    }
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            limit: 0,
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
            cache_max_size: None,
            cache_legacy_zip: true,
            state_dir: "/var/lib/turboci".to_string(),
            output_limit: DEFAULT_OUTPUT_LIMIT_KIB,
            environment: Vec::new(),
            pre_get_sources_script: None,
            post_get_sources_script: None,
            pre_build_script: None,
            post_build_script: None,
            min_free_memory: None,
            min_free_disk: None,
            max_load_avg: None,
            metrics_listen: None,
            cache_s3: None,
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
            workspace_max_age_days: 7,
            workspace_max_size: None,
            share_workspace_across_protection: false,
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
            cpu_shares: None,
            shm_size: None,
            oom_score_adjust: None,
            pids_limit: None,
            wait_for_services_timeout: DEFAULT_WAIT_FOR_SERVICES_TIMEOUT,
            services: Vec::new(),
            auth_config_file: None,
            insecure_registries: Vec::new(),
            registry_ca: std::collections::BTreeMap::new(),
            tmpfs: std::collections::BTreeMap::new(),
            storage_opt: std::collections::BTreeMap::new(),
            builds_dir: None,
            extra_hosts: Vec::new(),
            dns: Vec::new(),
            dns_search: Vec::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            security_opt: Vec::new(),
            devices: Vec::new(),
            gpus: None,
            sysctls: std::collections::BTreeMap::new(),
            ulimit: std::collections::BTreeMap::new(),
            runtime: None,
            allowed_images: Vec::new(),
            allowed_services: Vec::new(),
            allowed_docker_credential_helpers: std::collections::BTreeMap::new(),
            image_max_age_days: None,
            image_max_size: None,
        }
    }
}

impl RunnerConfig {
    /// Load a config file without [[runners]] (see `ConfigFile::load`)
    #[cfg(test)]
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
        Ok(config.finish(&content.parse()?, ""))
    }

    /// A runner's config from its table of a `[[runners]]` file
    fn from_table(given: toml::Table, name: &str) -> Result<Self> {
        let config: RunnerConfig = toml::Value::Table(given.clone()).try_into()?;
        Ok(config.finish(&given, &format!(" in [[runners]] {}", name)))
    }

    /// Warn about keys of `given` the runner does not know (`place` says
    /// where they are) and fill in defaults that depend on other settings
    fn finish(mut self, given: &toml::Table, place: &str) -> Self {
        for key in unknown_table_keys(given, &self) {
            tracing::warn!("Ignoring unknown config key `{}`{}", key, place);
        }
        // On disk with the runner's state: a /tmp cleaner (or tmpfs, or the
        // service's PrivateTmp, emptied on every restart) must not remove
        // parts of workspaces kept between jobs
        let builds = Path::new(&self.state_dir)
            .join("builds")
            .to_string_lossy()
            .into_owned();
        if self.executor.docker.builds_dir.is_none() {
            self.executor.docker.builds_dir = Some(builds.clone());
        }
        let work_dir_given = given
            .get("executor")
            .and_then(|executor| executor.get("shell"))
            .and_then(|shell| shell.get("work_dir"))
            .is_some();
        if !work_dir_given {
            self.executor.shell.work_dir = builds;
        }
        let git_defaults = self.environment_git_defaults();
        if !git_defaults.is_empty() {
            tracing::warn!(
                "environment{} sets {}: a job's own value overrides it (as in gitlab-runner), so it is only the default for jobs that set none",
                place,
                git_defaults.join(", ")
            );
        }
        self
    }

    /// Git settings the runner `environment` sets. Up to 0.12 they overrode
    /// the job's own; now they only apply to jobs that set none, which a
    /// config written for an older version may not expect.
    fn environment_git_defaults(&self) -> Vec<&str> {
        self.environment
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(key, _)| key.trim())
            .filter(|key| ["GIT_STRATEGY", "GIT_DEPTH", "GIT_CLEAN_FLAGS"].contains(key))
            .collect()
    }

    /// Host directory of the job workspaces of the configured executor
    pub fn builds_root(&self) -> &str {
        match self.executor.executor_type.as_str() {
            "shell" => &self.executor.shell.work_dir,
            // Always set by `finish`
            _ => self
                .executor
                .docker
                .builds_dir
                .as_deref()
                .unwrap_or_default(),
        }
    }

    /// Reject values the runner cannot work with
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.name) {
            anyhow::bail!(
                "name must be letters, digits, '.', '-' or '_', got {:?}",
                self.name
            );
        }
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
        self.cache_max_size_bytes()?;
        self.validate_daemon_options()?;
        if let Some(s3) = &self.cache_s3 {
            super::s3::S3Location::parse(&s3.url, s3.region.as_deref())?;
            if s3.access_key.trim().is_empty() || s3.secret_key.trim().is_empty() {
                anyhow::bail!("cache_s3 needs access_key and secret_key");
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

    /// `cache_max_size` in bytes; `None` when unset or 0 (no limit)
    pub fn cache_max_size_bytes(&self) -> Result<Option<u64>> {
        let Some(value) = self.cache_max_size.as_deref() else {
            return Ok(None);
        };
        let bytes = parse_size("cache_max_size", value)?;
        let bytes = u64::try_from(bytes)
            .map_err(|_| anyhow::anyhow!("cache_max_size must not be negative"))?;
        Ok((bytes > 0).then_some(bytes))
    }

    /// `cache_s3` to use: S3 is the job cache's second layer, so it is
    /// ignored (with a warning) while the cache is off
    pub fn s3_cache(&self) -> Option<&S3CacheConfig> {
        let s3 = self.cache_s3.as_ref()?;
        if !self.cache_enabled {
            tracing::warn!("cache_s3 is ignored because cache_enabled = false");
            return None;
        }
        Some(s3)
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

/// Whether `name` can name a runner: it ends up in directory names and
/// metric labels
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The config file: one runner, set at the top of the file as in every
/// earlier version, or several in `[[runners]]` as in gitlab-runner. The
/// runners of a file share the settings in `GLOBAL_KEYS`.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    /// Every runner of the file, each holding the global settings too
    pub runners: Vec<RunnerConfig>,
    /// The runners come from `[[runners]]`
    pub multi: bool,
}

impl ConfigFile {
    /// Load and check the config file at `path`
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file: {:?}", path.as_ref()))?;
        let file = Self::parse(&content)
            .with_context(|| format!("Invalid config file: {:?}", path.as_ref()))?;
        file.validate()?;
        Ok(file)
    }

    /// Parse a config file, with or without `[[runners]]`.
    ///
    /// With `[[runners]]` the top of the file holds the global settings and
    /// `state_dir`, the base of the runners' own state directories. Each
    /// entry is a runner config like a file without `[[runners]]` and needs a
    /// `name`; its `state_dir` defaults to `<state_dir>/runners/<name>`, its
    /// `cache_dir` to `<its state_dir>/cache` and its workspaces (Docker
    /// `builds_dir`, shell `work_dir`) to `<its state_dir>/builds`, so
    /// runners never share them by default.
    pub fn parse(content: &str) -> Result<Self> {
        let table: toml::Table = content.parse()?;
        let Some(entries) = table.get("runners") else {
            return Ok(Self {
                runners: vec![RunnerConfig::parse(content)?],
                multi: false,
            });
        };
        let toml::Value::Array(entries) = entries else {
            anyhow::bail!("runners must be a list of [[runners]] tables");
        };
        if entries.is_empty() {
            anyhow::bail!("runners is empty: a config needs at least one runner");
        }
        let mut top = table.clone();
        top.remove("runners");
        let base = match top.remove("state_dir") {
            Some(toml::Value::String(dir)) => dir,
            Some(_) => anyhow::bail!("state_dir must be a string"),
            None => RunnerConfig::default().state_dir,
        };
        if let Some(key) = top.keys().find(|key| !GLOBAL_KEYS.contains(&key.as_str())) {
            anyhow::bail!(
                "{} must be set in each [[runners]] entry: with [[runners]], only {} and state_dir go at the top of the file",
                key,
                GLOBAL_KEYS.join(", ")
            );
        }
        let mut runners = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let toml::Value::Table(entry) = entry else {
                anyhow::bail!("[[runners]] entry {} is not a table", index + 1);
            };
            let Some(toml::Value::String(name)) = entry.get("name") else {
                anyhow::bail!("[[runners]] entry {} needs a name", index + 1);
            };
            if let Some(key) = entry.keys().find(|key| GLOBAL_KEYS.contains(&key.as_str())) {
                anyhow::bail!(
                    "[[runners]] {}: {} is a setting of the whole runner process: set it once at the top of the file",
                    name,
                    key
                );
            }
            let mut merged = entry.clone();
            merged.extend(top.clone());
            let state_dir = match merged.get("state_dir") {
                Some(toml::Value::String(dir)) => dir.clone(),
                Some(_) => anyhow::bail!("[[runners]] {}: state_dir must be a string", name),
                None => Path::new(&base)
                    .join("runners")
                    .join(name)
                    .to_string_lossy()
                    .into_owned(),
            };
            let under_state = |dir: &str| {
                toml::Value::String(
                    Path::new(&state_dir)
                        .join(dir)
                        .to_string_lossy()
                        .into_owned(),
                )
            };
            merged
                .entry("state_dir")
                .or_insert_with(|| toml::Value::String(state_dir.clone()));
            merged
                .entry("cache_dir")
                .or_insert_with(|| under_state("cache"));
            let shell = table_in(table_in(&mut merged, "executor", name)?, "shell", name)?;
            shell
                .entry("work_dir")
                .or_insert_with(|| under_state("builds"));
            runners.push(
                RunnerConfig::from_table(merged, name)
                    .with_context(|| format!("[[runners]] {}", name))?,
            );
        }
        Ok(Self {
            runners,
            multi: true,
        })
    }

    /// Check every runner, and that no two runners share a name, state,
    /// workspaces or cache
    pub fn validate(&self) -> Result<()> {
        for runner in &self.runners {
            runner.validate().with_context(|| {
                if self.multi {
                    format!("[[runners]] {}", runner.name)
                } else {
                    "invalid config".to_string()
                }
            })?;
        }
        for (index, a) in self.runners.iter().enumerate() {
            for b in &self.runners[index + 1..] {
                if a.name == b.name {
                    anyhow::bail!("two [[runners]] are named {:?}: names must differ", a.name);
                }
                if Path::new(&a.state_dir) == Path::new(&b.state_dir) {
                    anyhow::bail!(
                        "[[runners]] {} and {} share the state_dir {:?}",
                        a.name,
                        b.name,
                        a.state_dir
                    );
                }
                // One runner's cleanup would remove the other's workspaces or archives
                for (what_a, dir_a) in
                    [("workspaces", a.builds_root()), ("cache_dir", &a.cache_dir)]
                {
                    for (what_b, dir_b) in
                        [("workspaces", b.builds_root()), ("cache_dir", &b.cache_dir)]
                    {
                        let (dir_a, dir_b) = (Path::new(dir_a), Path::new(dir_b));
                        if dir_a.starts_with(dir_b) || dir_b.starts_with(dir_a) {
                            anyhow::bail!(
                                "[[runners]] {} ({} {:?}) and {} ({} {:?}) must not share directories",
                                a.name,
                                what_a,
                                dir_a,
                                b.name,
                                what_b,
                                dir_b
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The settings of the whole process (`GLOBAL_KEYS`), the same in every runner
    pub fn global(&self) -> &RunnerConfig {
        &self.runners[0]
    }
}

/// The table under `key` in `table`, created when missing
fn table_in<'a>(table: &'a mut toml::Table, key: &str, name: &str) -> Result<&'a mut toml::Table> {
    match table
        .entry(key)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
    {
        toml::Value::Table(inner) => Ok(inner),
        _ => anyhow::bail!("[[runners]] {}: {} must be a table", name, key),
    }
}

/// `output_limit` when unset: 4 MiB, gitlab-runner's default
const DEFAULT_OUTPUT_LIMIT_KIB: u64 = 4096;
/// Accepted `output_limit`, in KiB: 64 KiB to 1 GiB
const OUTPUT_LIMIT_KIB: std::ops::RangeInclusive<u64> = 64..=1024 * 1024;

/// An optional size that must not be negative; `None` when unset or 0 (off)
fn optional_size(field: &str, value: Option<&str>) -> Result<Option<u64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let bytes = u64::try_from(parse_size(field, value)?)
        .map_err(|_| anyhow::anyhow!("{} must not be negative", field))?;
    Ok((bytes > 0).then_some(bytes))
}

/// Daemon options: admission control, metrics and the workspace quota
impl RunnerConfig {
    /// `executor.workspace_max_size` in bytes; `None` when off
    pub fn workspace_max_size_bytes(&self) -> Result<Option<u64>> {
        optional_size(
            "executor.workspace_max_size",
            self.executor.workspace_max_size.as_deref(),
        )
    }

    /// Address of the metrics endpoint; `None` when off
    pub fn metrics_addr(&self) -> Result<Option<std::net::SocketAddr>> {
        self.metrics_listen
            .as_deref()
            .map(|value| {
                value.trim().parse().map_err(|_| {
                    anyhow::anyhow!(
                        "metrics_listen must be IP:port, such as \"127.0.0.1:9252\", got {:?}",
                        value
                    )
                })
            })
            .transpose()
    }

    /// Job log limit (`output_limit`) in bytes
    pub fn output_limit_bytes(&self) -> usize {
        // `validate` keeps it at most 1 GiB, which fits a usize
        usize::try_from(self.output_limit.saturating_mul(1024)).unwrap_or(usize::MAX)
    }

    /// `min_free_memory` in bytes; `None` when off
    pub fn min_free_memory_bytes(&self) -> Result<Option<u64>> {
        optional_size("min_free_memory", self.min_free_memory.as_deref())
    }

    /// `min_free_disk` in bytes; `None` when off
    pub fn min_free_disk_bytes(&self) -> Result<Option<u64>> {
        optional_size("min_free_disk", self.min_free_disk.as_deref())
    }

    fn validate_daemon_options(&self) -> Result<()> {
        if !OUTPUT_LIMIT_KIB.contains(&self.output_limit) {
            anyhow::bail!(
                "output_limit must be between {} and {} (KiB), got {}",
                OUTPUT_LIMIT_KIB.start(),
                OUTPUT_LIMIT_KIB.end(),
                self.output_limit
            );
        }
        self.min_free_memory_bytes()?;
        self.min_free_disk_bytes()?;
        self.metrics_addr()?;
        self.workspace_max_size_bytes()?;
        if self
            .max_load_avg
            .is_some_and(|load| !load.is_finite() || load <= 0.0)
        {
            anyhow::bail!("max_load_avg must be a number above 0");
        }
        Ok(())
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
        let docs = toml_block_after(include_str!("../../docs/configuration.md"), "## Example");
        let config = RunnerConfig::parse(&docs).unwrap();
        config.validate().unwrap();
        assert!(
            unknown_keys(&docs, &config).is_empty(),
            "{:?}",
            unknown_keys(&docs, &config)
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
            .replace("$SERVICE_USER", "turboci")
            .replace(
                "$INSECURE_LINE",
                "insecure_registries = [\"harbor.old.local\"]",
            )
            .replace("$REGISTRY_CA_BLOCK", "");
        // The registry CA table must parse where the template puts it
        let with_ca = rendered.clone()
            + "[executor.docker.registry_ca]\n\"harbor.corp\" = \"/etc/turboci-registry-ca/harbor.corp.pem\"\n";
        let parsed = RunnerConfig::parse(&with_ca).unwrap();
        assert!(unknown_keys(&with_ca, &parsed).is_empty());
        assert_eq!(parsed.executor.docker.registry_ca.len(), 1);
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
        assert!(!config.executor.share_workspace_across_protection);
    }

    #[test]
    fn shell_workspaces_default_to_the_state_dir() {
        let shell =
            "runner_token = \"t\"\nstate_dir = \"/srv/ci\"\n[executor]\nexecutor_type = \"shell\"";
        let config = RunnerConfig::parse(shell).unwrap();
        assert_eq!(config.builds_root(), "/srv/ci/builds");
        // Not in the service's private /tmp by default
        let config = RunnerConfig::parse("runner_token = \"t\"").unwrap();
        assert_eq!(config.executor.shell.work_dir, "/var/lib/turboci/builds");
        // A config that names it keeps it, /tmp included
        let config = RunnerConfig::parse(&format!(
            "{}\n[executor.shell]\nwork_dir = \"/tmp/turboci-builds\"",
            shell
        ))
        .unwrap();
        assert_eq!(config.builds_root(), "/tmp/turboci-builds");
    }

    #[test]
    fn workspace_sharing_and_git_defaults_in_environment() {
        let config = RunnerConfig::parse(
            "runner_token = \"t\"\nenvironment = [\"GIT_STRATEGY=clone\", \" GIT_DEPTH =1\", \"A=GIT_CLEAN_FLAGS\", \"GIT_CLEAN_FLAGS=-ffd\"]\n[executor]\nshare_workspace_across_protection = true",
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.executor.share_workspace_across_protection);
        assert_eq!(
            config.environment_git_defaults(),
            ["GIT_STRATEGY", "GIT_DEPTH", "GIT_CLEAN_FLAGS"]
        );
        assert!(RunnerConfig::parse(
            "runner_token = \"t\"\n[executor]\nshare_workspace_across_protection = \"yes\""
        )
        .is_err());
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
    fn wait_for_services_timeout_follows_gitlab_runner() {
        let wait = |line: &str| {
            let config = RunnerConfig::parse(&format!(
                "runner_token = \"t\"\n[executor.docker]\n{}",
                line
            ))
            .unwrap();
            config
                .validate()
                .map(|_| config.executor.docker.service_wait())
        };
        assert_eq!(wait("").unwrap(), Some(std::time::Duration::from_secs(30)));
        assert_eq!(
            wait("wait_for_services_timeout = 0").unwrap(),
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(
            wait("wait_for_services_timeout = 120").unwrap(),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(wait("wait_for_services_timeout = -1").unwrap(), None);
        for bad in ["-2", "86401"] {
            assert!(
                wait(&format!("wait_for_services_timeout = {}", bad)).is_err(),
                "accepted {}",
                bad
            );
        }
    }

    #[test]
    fn parses_docker_limits() {
        let config = RunnerConfig::parse(
            "runner_token = \"t\"\n[executor.docker]\nmemory = \"512m\"\ncpus = 1.5\npull_policy = \"if-not-present\"\npids_limit = 4096",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.executor.docker.memory_bytes().unwrap(),
            Some(512 << 20)
        );
        assert_eq!(config.executor.docker.pids_limit, Some(4096));

        for bad in [
            "memory = \"lots\"",
            "cpus = 0.0",
            "pull_policy = \"sometimes\"",
            "pids_limit = 0",
        ] {
            let config =
                RunnerConfig::parse(&format!("runner_token = \"t\"\n[executor.docker]\n{}", bad))
                    .unwrap();
            assert!(config.validate().is_err(), "accepted {}", bad);
        }
    }

    #[test]
    fn parses_cpu_shares_tmpfs_and_storage_options() {
        let config = RunnerConfig::parse(
            "runner_token = \"t\"\n[executor.docker]\ncpu_shares = 512\n\
             [executor.docker.tmpfs]\n\"/tmp\" = \"rw,noexec,size=1g\"\n\"/scratch\" = \"\"\n\
             [executor.docker.storage_opt]\nsize = \"20G\"\n",
        )
        .unwrap();
        config.validate().unwrap();
        let docker = &config.executor.docker;
        assert_eq!(docker.cpu_shares, Some(512));
        assert_eq!(docker.tmpfs["/tmp"], "rw,noexec,size=1g");
        assert_eq!(docker.tmpfs["/scratch"], "");
        assert_eq!(docker.storage_opt["size"], "20G");

        let defaults = RunnerConfig::parse("runner_token = \"t\"").unwrap();
        assert_eq!(defaults.executor.docker.cpu_shares, None);
        assert!(defaults.executor.docker.tmpfs.is_empty());
        assert!(defaults.executor.docker.storage_opt.is_empty());

        for bad in [
            "cpu_shares = 1",
            "cpu_shares = 262145",
            "cpu_shares = -5",
            "[executor.docker.tmpfs]\n\"tmp\" = \"\"",
            "[executor.docker.tmpfs]\n\"/\" = \"\"",
            "[executor.docker.tmpfs]\n\"/a/../etc\" = \"\"",
            "[executor.docker.tmpfs]\n\"/a:b\" = \"\"",
            "[executor.docker.tmpfs]\n\"/builds\" = \"\"",
            "[executor.docker.tmpfs]\n\"/builds/project\" = \"\"",
            "[executor.docker.tmpfs]\n\"/tmp\" = \"size=1g, noexec\"",
            "[executor.docker.storage_opt]\nsize = \"\"",
            "[executor.docker.storage_opt]\n\"size=1\" = \"20G\"",
            "[executor.docker.storage_opt]\nsize = \"20 G\"",
        ] {
            let config =
                RunnerConfig::parse(&format!("runner_token = \"t\"\n[executor.docker]\n{}", bad))
                    .unwrap();
            assert!(config.validate().is_err(), "accepted {}", bad);
        }
    }

    #[test]
    fn parses_the_cache_size_limit() {
        let size = |line: &str| {
            let config = RunnerConfig::parse(&format!("runner_token = \"t\"\n{}", line)).unwrap();
            config
                .validate()
                .map(|()| config.cache_max_size_bytes().unwrap())
        };

        assert_eq!(size("").unwrap(), None);
        assert_eq!(size("cache_max_size = \"0\"").unwrap(), None);
        assert_eq!(size("cache_max_size = \"20g\"").unwrap(), Some(20 << 30));
        assert_eq!(size("cache_max_size = \"1048576\"").unwrap(), Some(1 << 20));
        for bad in ["\"-1\"", "\"lots\"", "\"5t\""] {
            assert!(
                size(&format!("cache_max_size = {}", bad)).is_err(),
                "accepted {}",
                bad
            );
        }
        let known = RunnerConfig::parse("runner_token = \"t\"\ncache_max_size = \"1g\"").unwrap();
        assert!(unknown_keys("cache_max_size = \"1g\"", &known).is_empty());
    }

    fn docker_config(lines: &str) -> Result<RunnerConfig> {
        let config = RunnerConfig::parse(&format!(
            "runner_token = \"t\"\n[executor.docker]\n{}",
            lines
        ))?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parses_the_container_options() {
        let config = docker_config(
            "extra_hosts = [\"gitlab.corp:10.0.0.5\", \"gw=host-gateway\", \"v6:[fe80::1]\"]\n\
             dns = [\"10.0.0.53\", \"2001:db8::53\"]\n\
             dns_search = [\"corp.local\"]\n\
             cap_add = [\"SYS_PTRACE\", \"CAP_NET_ADMIN\"]\n\
             cap_drop = [\"ALL\"]\n\
             security_opt = [\"no-new-privileges\", \"seccomp=unconfined\", \"apparmor=docker-default\"]\n\
             devices = [\"/dev/fuse\", \"/dev/sda:/dev/xvda:rw\"]\n\
             gpus = \"all\"\n\
             runtime = \"nvidia\"\n\
             [executor.docker.sysctls]\n\"net.ipv4.ip_unprivileged_port_start\" = \"0\"\n\
             [executor.docker.ulimit]\nnofile = \"1024:4096\"\nmemlock = \"-1\"\ncore = \"0\"\n",
        )
        .unwrap();
        let docker = &config.executor.docker;
        assert_eq!(
            docker.extra_host_entries(),
            ["gitlab.corp:10.0.0.5", "gw:host-gateway", "v6:fe80::1"]
        );
        assert_eq!(docker.device_mappings().len(), 2);
        assert_eq!(docker.gpu_requests()[0].count, -1);
        let limits = docker.ulimits();
        assert_eq!(
            (limits[0].name.as_str(), limits[0].soft, limits[0].hard),
            ("core", 0, 0)
        );
        assert_eq!((limits[1].soft, limits[1].hard), (-1, -1));
        assert_eq!((limits[2].soft, limits[2].hard), (1024, 4096));
        assert_eq!(docker.runtime.as_deref(), Some("nvidia"));

        // Unset: nothing
        let defaults = docker_config("").unwrap().executor.docker;
        assert!(defaults.extra_host_entries().is_empty() && defaults.gpu_requests().is_empty());
        assert!(defaults.ulimits().is_empty() && defaults.device_mappings().is_empty());
    }

    #[test]
    fn rejects_unusable_container_options() {
        for bad in [
            "extra_hosts = [\"gitlab.corp\"]",
            "extra_hosts = [\"gitlab.corp:not-an-ip\"]",
            "extra_hosts = [\":10.0.0.5\"]",
            "extra_hosts = [\"bad host:10.0.0.5\"]",
            "dns = [\"dns.corp\"]",
            "dns_search = [\"a b\"]",
            "dns_search = [\"\"]",
            "cap_add = [\"NET-RAW\"]",
            "cap_drop = [\"\"]",
            "security_opt = [\"\"]",
            "security_opt = [\"seccomp=/nonexistent/profile.json\"]",
            "devices = [\"dev/fuse\"]",
            "devices = [\"/dev/fuse:fuse\"]",
            "devices = [\"/dev/fuse:/dev/fuse:rwx\"]",
            "devices = [\"/dev/fuse:/dev/fuse:rw:x\"]",
            "gpus = \"none\"",
            "gpus = \"0\"",
            "gpus = \"device=0,1\"",
            "gpus = \"count=1,count=2\"",
            "gpus = \"color=red\"",
            "gpus = \"\\\"device=0\"",
            "runtime = \"\"",
            "runtime = \"nvidia runc\"",
            "[executor.docker.sysctls]\n\"\" = \"1\"",
            "[executor.docker.sysctls]\n\"a=b\" = \"1\"",
            "[executor.docker.ulimit]\nfiles = \"1\"",
            "[executor.docker.ulimit]\nnofile = \"a\"",
            "[executor.docker.ulimit]\nnofile = \"4096:1024\"",
            "[executor.docker.ulimit]\nnofile = \"-1:1024\"",
            "[executor.docker.ulimit]\nnofile = \"-2\"",
        ] {
            assert!(docker_config(bad).is_err(), "{}", bad);
        }
    }

    #[test]
    fn gpu_requests_follow_docker_run_gpus() {
        let all = gpu_request("all").unwrap();
        assert_eq!((all.count, all.device_ids.clone()), (-1, None));
        assert_eq!(all.capabilities, vec![vec!["gpu".to_string()]]);
        assert_eq!(gpu_request("3").unwrap().count, 3);
        assert_eq!(gpu_request("count=all").unwrap().count, -1);
        let ids = gpu_request("\"device=0,GPU-3a23c669\"").unwrap();
        assert_eq!(
            ids.device_ids,
            Some(vec!["0".to_string(), "GPU-3a23c669".to_string()])
        );
        assert_eq!(ids.count, 0);
        // Only a driver: one device
        let driver = gpu_request("driver=nvidia").unwrap();
        assert_eq!((driver.count, driver.driver.as_str()), (1, "nvidia"));
        let options = gpu_request("1,\"options=a=b,c=d\"").unwrap();
        assert_eq!(options.options.get("a").map(String::as_str), Some("b"));
        assert_eq!(options.options.get("c").map(String::as_str), Some("d"));
        let caps = gpu_request("1,\"capabilities=compute,utility\"").unwrap();
        assert_eq!(
            caps.capabilities,
            vec![vec![
                "compute".to_string(),
                "utility".to_string(),
                "gpu".to_string()
            ]]
        );
    }

    #[test]
    fn seccomp_profiles_are_sent_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("seccomp.json");
        std::fs::write(&profile, "{\n  \"defaultAction\": \"SCMP_ACT_ALLOW\"\n}\n").unwrap();
        let option = format!("seccomp={}", profile.display());

        assert_eq!(
            security_opt(&option).unwrap(),
            "seccomp={\"defaultAction\":\"SCMP_ACT_ALLOW\"}"
        );
        for kept in [
            "seccomp=unconfined",
            "no-new-privileges",
            "seccomp={\"a\":1}",
        ] {
            assert_eq!(security_opt(kept).unwrap(), kept);
        }
        std::fs::write(&profile, "not json").unwrap();
        assert!(security_opt(&option).is_err());
        assert!(docker_config(&format!("security_opt = [{:?}]", option)).is_err());
    }

    #[test]
    fn image_patterns_and_collection_settings_are_validated() {
        let config = docker_config(
            "allowed_images = [\"ruby:*\", \"registry.corp/**\"]\n\
             allowed_services = [\"postgres:*\"]\n\
             image_max_age_days = 14\n\
             image_max_size = \"50g\"\n\
             [executor.docker.allowed_docker_credential_helpers]\n\
             ecr-login = [\"*.dkr.ecr.*.amazonaws.com\"]\n",
        )
        .unwrap();
        let docker = &config.executor.docker;
        assert_eq!(docker.image_max_age_days, Some(14));
        assert_eq!(docker.image_max_size_bytes().unwrap(), Some(50 << 30));
        assert_eq!(docker.allowed_docker_credential_helpers.len(), 1);
        assert_eq!(
            docker_config("image_max_size = \"0\"")
                .unwrap()
                .executor
                .docker
                .image_max_size_bytes()
                .unwrap(),
            None
        );

        for bad in [
            "allowed_images = [\"\"]",
            "allowed_images = [\"registry.corp/**x\"]",
            "allowed_services = [\"[postgres\"]",
            "image_max_size = \"lots\"",
            "image_max_size = \"-1\"",
            "image_max_age_days = -1",
            "[executor.docker.allowed_docker_credential_helpers]\n\"../sh\" = [\"*\"]",
            "[executor.docker.allowed_docker_credential_helpers]\necr-login = []",
            "[executor.docker.allowed_docker_credential_helpers]\necr-login = [\"**x\"]",
        ] {
            assert!(docker_config(bad).is_err(), "{}", bad);
        }
    }

    #[test]
    fn globs_follow_gitlab_runners_doublestar_rules() {
        let patterns = |list: &[&str]| list.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let ruby = patterns(&["ruby:*"]);
        assert!(glob_matches(&ruby, "ruby:3.3-alpine"));
        assert!(!glob_matches(&ruby, "ruby"));
        assert!(!glob_matches(&ruby, "ruby:3/evil"));
        let registry = patterns(&["registry.corp/**"]);
        assert!(glob_matches(&registry, "registry.corp/app:1"));
        assert!(glob_matches(&registry, "registry.corp/group/sub/app:1"));
        assert!(!glob_matches(&registry, "registry.corp.evil/app:1"));
        let nested = patterns(&["registry.corp/**/ci-*:*"]);
        assert!(glob_matches(&nested, "registry.corp/ci-tools:1"));
        assert!(glob_matches(&nested, "registry.corp/a/b/ci-tools:1"));
        assert!(!glob_matches(&nested, "registry.corp/a/tools:1"));
        let one = patterns(&["node:2?"]);
        assert!(glob_matches(&one, "node:22") && !glob_matches(&one, "node:222"));
        assert!(glob_matches(
            &patterns(&["*.dkr.ecr.*.amazonaws.com"]),
            "1.dkr.ecr.eu-west-1.amazonaws.com"
        ));
        assert!(!glob_matches(&patterns(&["[bad"]), "[bad"));
    }

    #[test]
    fn legacy_zip_caches_are_read_unless_turned_off() {
        let parse = |line: &str| RunnerConfig::parse(&format!("runner_token = \"t\"\n{}", line));

        assert!(parse("").unwrap().cache_legacy_zip);
        let off = parse("cache_legacy_zip = false").unwrap();
        assert!(!off.cache_legacy_zip);
        assert!(off.validate().is_ok());
        assert!(unknown_keys("cache_legacy_zip = false", &off).is_empty());
        assert!(parse("cache_legacy_zip = \"no\"").is_err());
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

    #[test]
    fn s3_cache_settings_are_validated() {
        let ok = RunnerConfig::parse(
            "runner_token = \"t\"\n[cache_s3]\nurl = \"http://minio:9000/ci\"\naccess_key = \"a\"\nsecret_key = \"s\"",
        )
        .unwrap();
        ok.validate().unwrap();
        assert!(unknown_keys("runner_token = \"t\"\n[cache_s3]\nurl = \"http://minio:9000/ci\"\naccess_key = \"a\"\nsecret_key = \"s\"", &ok).is_empty());

        for bad in [
            "[cache_s3]\nurl = \"http://minio:9000\"\naccess_key = \"a\"\nsecret_key = \"s\"",
            "[cache_s3]\nurl = \"http://minio:9000/ci\"\naccess_key = \"\"\nsecret_key = \"s\"",
        ] {
            let config = RunnerConfig::parse(&format!("runner_token = \"t\"\n{}", bad)).unwrap();
            assert!(config.validate().is_err(), "accepted {}", bad);
        }
        assert!(
            RunnerConfig::parse("runner_token = \"t\"\n[cache_s3]\nurl = \"http://m/ci\"").is_err(),
            "keys are required"
        );
    }

    #[test]
    fn s3_cache_is_ignored_when_the_cache_is_off() {
        let s3 = S3CacheConfig {
            url: "http://minio:9000/ci".to_string(),
            access_key: "AK".to_string(),
            secret_key: "SK".to_string(),
            region: None,
        };
        let enabled = RunnerConfig {
            cache_s3: Some(s3),
            ..RunnerConfig::default()
        };
        assert!(enabled.s3_cache().is_some());
        let disabled = RunnerConfig {
            cache_enabled: false,
            ..enabled
        };
        assert!(disabled.s3_cache().is_none());
    }

    #[test]
    fn debug_output_hides_the_s3_secret_key() {
        let config = RunnerConfig {
            cache_s3: Some(S3CacheConfig {
                url: "http://minio:9000/ci".to_string(),
                access_key: "AK".to_string(),
                secret_key: "s3-secret-value".to_string(),
                region: None,
            }),
            ..RunnerConfig::default()
        };
        assert!(!format!("{:?}", config).contains("s3-secret-value"));
    }

    /// A config with `runner_token` and `lines`
    fn daemon_config(lines: &str) -> RunnerConfig {
        RunnerConfig::parse(&format!("runner_token = \"t\"\n{}", lines)).unwrap()
    }

    #[test]
    fn parses_the_admission_limits() {
        let defaults = daemon_config("");
        defaults.validate().unwrap();
        assert_eq!(defaults.min_free_memory_bytes().unwrap(), None);
        assert_eq!(defaults.min_free_disk_bytes().unwrap(), None);
        assert_eq!(defaults.max_load_avg, None);

        let content = "min_free_memory = \"2g\"\nmin_free_disk = \"512m\"\nmax_load_avg = 12.5";
        let set = daemon_config(content);
        set.validate().unwrap();
        assert!(unknown_keys(content, &set).is_empty());
        assert_eq!(set.min_free_memory_bytes().unwrap(), Some(2 << 30));
        assert_eq!(set.min_free_disk_bytes().unwrap(), Some(512 << 20));
        assert_eq!(set.max_load_avg, Some(12.5));
        // "0" turns a limit off
        let off = daemon_config("min_free_memory = \"0\"\nmin_free_disk = \"0\"");
        off.validate().unwrap();
        assert_eq!(off.min_free_memory_bytes().unwrap(), None);
        assert_eq!(off.min_free_disk_bytes().unwrap(), None);

        for bad in [
            "min_free_memory = \"lots\"",
            "min_free_memory = \"-1\"",
            "min_free_disk = \"5t\"",
            "min_free_disk = \"-1\"",
            "max_load_avg = 0.0",
            "max_load_avg = -2.0",
            "max_load_avg = nan",
            "max_load_avg = inf",
        ] {
            assert!(daemon_config(bad).validate().is_err(), "accepted {}", bad);
        }
    }

    #[test]
    fn parses_the_workspace_quota() {
        assert_eq!(daemon_config("").workspace_max_size_bytes().unwrap(), None);
        let content = "[executor]\nworkspace_max_size = \"50g\"";
        let config = daemon_config(content);
        config.validate().unwrap();
        assert!(unknown_keys(content, &config).is_empty());
        assert_eq!(config.workspace_max_size_bytes().unwrap(), Some(50 << 30));
        let off = daemon_config("[executor]\nworkspace_max_size = \"0\"");
        off.validate().unwrap();
        assert_eq!(off.workspace_max_size_bytes().unwrap(), None);

        for bad in ["\"-1\"", "\"big\"", "\"5t\""] {
            let config = daemon_config(&format!("[executor]\nworkspace_max_size = {}", bad));
            assert!(config.validate().is_err(), "accepted {}", bad);
        }
    }

    #[test]
    fn parses_the_metrics_address() {
        assert_eq!(daemon_config("").metrics_addr().unwrap(), None);
        let content = "metrics_listen = \"127.0.0.1:9252\"";
        let config = daemon_config(content);
        config.validate().unwrap();
        assert!(unknown_keys(content, &config).is_empty());
        assert_eq!(
            config.metrics_addr().unwrap(),
            Some("127.0.0.1:9252".parse().unwrap())
        );
        daemon_config("metrics_listen = \"[::1]:9252\"")
            .validate()
            .unwrap();
        daemon_config("metrics_listen = \"0.0.0.0:9252\"")
            .validate()
            .unwrap();

        for bad in [
            "localhost:9252",
            "9252",
            "127.0.0.1",
            "",
            "http://127.0.0.1:9252",
        ] {
            let config = daemon_config(&format!("metrics_listen = {:?}", bad));
            assert!(config.validate().is_err(), "accepted {:?}", bad);
        }
    }

    #[test]
    fn a_file_without_runners_is_one_runner_as_before() {
        let content = "concurrent = 2\nrunner_token = \"t\"\nstate_dir = \"/srv/ci\"\ncache_dir = \"/srv/ci/cache\"";
        let file = ConfigFile::parse(content).unwrap();
        file.validate().unwrap();
        assert!(!file.multi);
        assert_eq!(file.runners.len(), 1);
        let runner = &file.runners[0];
        assert_eq!(runner.name, "default");
        assert_eq!(runner.limit, 0);
        assert_eq!(runner.concurrent, 2);
        assert_eq!(runner.builds_root(), "/srv/ci/builds");
        assert_eq!(
            format!("{:?}", runner),
            format!("{:?}", RunnerConfig::parse(content).unwrap())
        );
    }

    const TWO_RUNNERS: &str = r#"
concurrent = 6
check_interval = 5
metrics_listen = "127.0.0.1:9252"
state_dir = "/srv/ci"

[[runners]]
name = "docker-a"
runner_token = "glrt-a"
gitlab_url = "https://a.example"
limit = 4
[runners.executor.docker]
memory = "2g"

[[runners]]
name = "shell-b"
runner_token = "glrt-b"
gitlab_url = "https://b.example"
environment = ["B=1"]
[runners.executor]
executor_type = "shell"
"#;

    #[test]
    fn runners_share_the_global_settings_and_get_their_own_directories() {
        let file = ConfigFile::parse(TWO_RUNNERS).unwrap();
        file.validate().unwrap();
        assert!(file.multi);
        let [a, b] = &file.runners[..] else {
            panic!("two runners expected")
        };
        for runner in [a, b] {
            assert_eq!(runner.concurrent, 6);
            assert_eq!(runner.check_interval, 5);
            assert_eq!(runner.metrics_listen.as_deref(), Some("127.0.0.1:9252"));
        }
        assert_eq!((a.name.as_str(), a.limit), ("docker-a", 4));
        assert_eq!((b.name.as_str(), b.limit), ("shell-b", 0));
        assert_eq!(a.runner_token, "glrt-a");
        assert_eq!(b.gitlab_url, "https://b.example");
        assert_eq!(a.executor.docker.memory.as_deref(), Some("2g"));
        assert_eq!(b.environment, ["B=1"]);
        assert_eq!(a.state_dir, "/srv/ci/runners/docker-a");
        assert_eq!(a.cache_dir, "/srv/ci/runners/docker-a/cache");
        assert_eq!(a.builds_root(), "/srv/ci/runners/docker-a/builds");
        assert_eq!(b.builds_root(), "/srv/ci/runners/shell-b/builds");
        assert_eq!(b.cache_dir, "/srv/ci/runners/shell-b/cache");
        assert_eq!(file.global().concurrent, 6);

        // Directories set in an entry are kept
        let own = TWO_RUNNERS.replace(
            "limit = 4\n",
            "limit = 4\nstate_dir = \"/data/a\"\ncache_dir = \"/cache/a\"\n",
        );
        let file = ConfigFile::parse(&own).unwrap();
        file.validate().unwrap();
        assert_eq!(file.runners[0].state_dir, "/data/a");
        assert_eq!(file.runners[0].cache_dir, "/cache/a");
        assert_eq!(file.runners[0].builds_root(), "/data/a/builds");

        // The example in the docs
        let docs = toml_block_after(
            include_str!("../../docs/configuration.md"),
            "## Several runners",
        );
        let file = ConfigFile::parse(&docs).unwrap();
        file.validate().unwrap();
        assert_eq!(file.runners.len(), 2);
        assert_eq!(file.runners[0].limit, 6);
        assert_eq!(file.runners[1].executor.executor_type, "shell");
    }

    #[test]
    fn rejects_runners_that_would_clash() {
        let check = |content: &str| ConfigFile::parse(content).and_then(|file| file.validate());
        let error = |content: &str| format!("{:#}", check(content).unwrap_err());
        check(TWO_RUNNERS).unwrap();

        // A runner setting at the top, a global one in an entry
        assert!(error(&format!("cache_enabled = false\n{}", TWO_RUNNERS))
            .contains("cache_enabled must be set in each [[runners]] entry"));
        assert!(error(&TWO_RUNNERS.replace("limit = 4", "concurrent = 4"))
            .contains("docker-a: concurrent is a setting of the whole runner process"));
        assert!(error(&TWO_RUNNERS.replace("name = \"shell-b\"\n", ""))
            .contains("entry 2 needs a name"));
        assert!(error(&TWO_RUNNERS.replace("shell-b", "docker-a")).contains("named \"docker-a\""));
        assert!(error(&TWO_RUNNERS.replace("shell-b", "../x")).contains("name must be"));
        assert!(error("runners = []").contains("at least one runner"));
        assert!(error("[runners]\nname = \"x\"").contains("list of [[runners]]"));
        // Each runner is checked like a single one
        assert!(error(&TWO_RUNNERS.replace("glrt-b", " ")).contains("[[runners]] shell-b"));
        // Shared state, workspaces or cache
        assert!(error(&TWO_RUNNERS.replace(
            "environment",
            "state_dir = \"/srv/ci/runners/docker-a\"\nenvironment"
        ))
        .contains("share the state_dir"));
        assert!(error(&TWO_RUNNERS.replace(
            "environment",
            "cache_dir = \"/srv/ci/runners/docker-a/builds/x\"\nenvironment"
        ))
        .contains("must not share directories"));
        assert!(error(&TWO_RUNNERS.replace(
            "environment",
            "cache_dir = \"/srv/ci/runners/docker-a/cache\"\nenvironment"
        ))
        .contains("must not share directories"));
    }

    #[test]
    fn output_limit_is_in_kib_and_bounded() {
        assert_eq!(daemon_config("").output_limit_bytes(), 4 * 1024 * 1024);
        let content = "output_limit = 16384";
        let config = daemon_config(content);
        config.validate().unwrap();
        assert!(unknown_keys(content, &config).is_empty());
        assert_eq!(config.output_limit_bytes(), 16 * 1024 * 1024);
        for ok in [64, 1024 * 1024] {
            daemon_config(&format!("output_limit = {}", ok))
                .validate()
                .unwrap();
        }

        for bad in ["0", "63", "1048577"] {
            let config = daemon_config(&format!("output_limit = {}", bad));
            assert!(config.validate().is_err(), "accepted {}", bad);
        }
        assert!(RunnerConfig::parse("runner_token = \"t\"\noutput_limit = -1").is_err());
    }
}
