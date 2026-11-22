use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{error, info, warn, Instrument};

use crate::gitlab::{
    ArtifactUpload, FailureReason, GitLabClient, Job, JobState, Polled, RemoteState, Variable,
};
use crate::security::secret_scrubber::SecretScrubber;
use cancel::{CancelSignal, Stop};
use executor::{JobFailure, JobOutcome};
use trace::TraceWriter;

pub mod admission;
pub mod artifacts;
pub mod cancel;
pub mod capacity;
pub mod config;
pub mod credential_helper;
pub mod docker_api;
pub mod executor;
pub mod git;
pub mod housekeeping;
pub mod image;
pub mod job_cache;
pub mod metrics;
pub mod reload;
pub mod runners;
pub mod s3;
pub mod script;
pub mod system_id;
pub mod token;
pub mod trace;

/// Retry count for a transfer, from a job variable such as
/// ARTIFACT_DOWNLOAD_ATTEMPTS (1-10, default 1)
fn attempts(job: &Job, key: &str) -> u32 {
    job.variables
        .iter()
        .rev()
        .find(|v| v.key == key)
        .and_then(|v| v.value.as_deref())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, 10)
}

/// Add the runner config's proxy, `environment`, hook scripts and workspace
/// sharing to a job
fn apply_runner_config(job: &mut Job, config: &config::RunnerConfig) {
    job.share_workspace_across_protection = config.executor.share_workspace_across_protection;
    if let Some(proxy) = &config.proxy {
        // Service aliases only exist on the job's network, never behind the proxy
        let values = job_variables(job);
        let aliases: Vec<String> = config
            .executor
            .docker
            .services
            .iter()
            .map(|s| (s.name.clone(), s.alias.clone()))
            .chain(
                job.services
                    .iter()
                    .map(|s| (s.name.clone(), s.alias.clone())),
            )
            .flat_map(|(name, alias)| {
                image::service_aliases(&script::expand(&name, &values), alias.as_deref())
            })
            .collect();
        let no_proxy = crate::net::effective_no_proxy(config.no_proxy.as_deref(), &aliases);
        // First, so the job's own variables and `environment` win
        let proxy_variables = [
            ("no_proxy", no_proxy.as_str()),
            ("NO_PROXY", no_proxy.as_str()),
            ("https_proxy", proxy.as_str()),
            ("http_proxy", proxy.as_str()),
            ("HTTPS_PROXY", proxy.as_str()),
            ("HTTP_PROXY", proxy.as_str()),
        ]
        .map(|(key, value)| Variable {
            raw: true,
            ..runner_variable(key, value.to_string())
        });
        prepend_variables(job, proxy_variables);
    }
    job.variables.extend(
        config
            .environment
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(key, value)| runner_variable(key.trim(), value.to_string())),
    );
    // Runner hooks run before the job's own hooks of the same name
    for (name, script) in [
        ("post_get_sources_script", &config.post_get_sources_script),
        ("pre_get_sources_script", &config.pre_get_sources_script),
    ] {
        if let Some(script) = script {
            job.hooks
                .insert(0, serde_json::json!({ "name": name, "script": [script] }));
        }
    }
    for step in job.steps.iter_mut().filter(|step| step.name == "script") {
        if let Some(script) = &config.pre_build_script {
            step.before_script.insert(0, script.clone());
        }
        if let Some(script) = &config.post_build_script {
            step.script.push(script.clone());
        }
    }
}

/// A public variable the runner adds to a job
fn runner_variable(key: &str, value: String) -> Variable {
    Variable {
        key: key.to_string(),
        value: Some(value),
        public: true,
        masked: false,
        raw: false,
        file: false,
        internal: true,
        variable_type: None,
    }
}

/// Put `variables` before the job's own, which then override them
fn prepend_variables(job: &mut Job, variables: impl IntoIterator<Item = Variable>) {
    let own = std::mem::take(&mut job.variables);
    job.variables = variables.into_iter().chain(own).collect();
}

/// Commit the binary was built from (set by CI builds), or the version
fn revision() -> String {
    option_env!("GITHUB_SHA")
        .map(|sha| sha.chars().take(8).collect())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Platform in Go's naming, e.g. linux/amd64
fn executable_arch() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    format!("{}/{}", crate::gitlab::go_os(), arch)
}

/// First 9 characters of the runner token after its prefix (glrt-, t1_, ...):
/// identifies the runner without revealing the token
fn short_token(token: &str) -> String {
    let mut rest = token
        .strip_prefix("glrtr-")
        .or_else(|| token.strip_prefix("glrt-"))
        .unwrap_or(token);
    if let [b't', b'1'..=b'3', b'_', ..] = rest.as_bytes() {
        rest = &rest[3..];
    }
    rest.chars().take(9).collect()
}

/// Hands out CI_CONCURRENT_ID (lowest free slot on this runner) and
/// CI_CONCURRENT_PROJECT_ID (lowest free slot among the project's jobs)
#[derive(Default)]
struct ConcurrencySlots {
    runner: std::collections::BTreeSet<u32>,
    projects: std::collections::HashMap<u64, std::collections::BTreeSet<u32>>,
}

impl ConcurrencySlots {
    fn lowest_free(used: &std::collections::BTreeSet<u32>) -> u32 {
        (0..).find(|id| !used.contains(id)).unwrap_or(0)
    }

    fn acquire(&mut self, project: u64) -> (u32, u32) {
        let id = Self::lowest_free(&self.runner);
        self.runner.insert(id);
        let used = self.projects.entry(project).or_default();
        let project_id = Self::lowest_free(used);
        used.insert(project_id);
        (id, project_id)
    }

    fn release(&mut self, project: u64, (id, project_id): (u32, u32)) {
        self.runner.remove(&id);
        if let Some(used) = self.projects.get_mut(&project) {
            used.remove(&project_id);
            if used.is_empty() {
                self.projects.remove(&project);
            }
        }
    }
}

/// Aborts a spawned task when dropped
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Restores into the workspace once the executor has checked out sources
struct WorkspaceRestore<'a> {
    daemon: &'a RunnerDaemon,
    variables: &'a Variables,
}

impl executor::Restore for WorkspaceRestore<'_> {
    async fn restore(&self, job: &Job, trace: &mut TraceWriter<'_>, stop: &Stop) -> JobOutcome {
        self.daemon
            .restore_workspace(job, self.variables, trace, stop)
            .await
    }
}

/// Values of a job's variables by name
type Variables = std::collections::HashMap<String, String>;

/// Values of the job's variables, for expanding cache keys and artifact paths
fn job_variables(job: &Job) -> Variables {
    script::job_env(
        job,
        "",
        std::path::Path::new(""),
        &script::RunnerVars::default(),
    )
    .vars
    .into_iter()
    .collect()
}

fn project_id(job: &Job) -> u64 {
    job.job_info.as_ref().map_or(0, |info| info.project_id)
}

/// Wait for a free job slot, then ask GitLab for a job.
///
/// The slot is taken before the request, so the runner never accepts more jobs
/// than `concurrent`; it is returned with the job and released when the job ends.
#[cfg(test)]
async fn claim_job(
    capacity: &Arc<capacity::Capacity>,
    gitlab: &GitLabClient,
    runner_token: &str,
) -> Result<Option<(Job, capacity::Slot)>> {
    let slot = capacity.acquire().await;
    Ok(
        match claim_with_slot(slot, gitlab, runner_token, std::future::pending()).await? {
            Claimed::Job(job, slot) => Some((*job, slot)),
            Claimed::NoJob | Claimed::Conflict => None,
        },
    )
}

/// What a job request for a reserved slot got
enum Claimed<S> {
    /// A job, with the slot it runs in
    Job(Box<Job>, S),
    NoJob,
    /// GitLab answered 409 (see `Polled::Conflict`)
    Conflict,
}

/// Ask GitLab for a job for an already reserved slot, until `stop` (see
/// `GitLabClient::request_job_until`); the slot is released when no job is
/// returned
async fn claim_with_slot<S>(
    slot: S,
    gitlab: &GitLabClient,
    runner_token: &str,
    stop: impl std::future::Future<Output = ()>,
) -> Result<Claimed<S>> {
    Ok(match gitlab.request_job_until(runner_token, stop).await? {
        Polled::Job(job) => Claimed::Job(job, slot),
        Polled::NoJob => Claimed::NoJob,
        Polled::Conflict => Claimed::Conflict,
    })
}

/// Longest wait between job requests that fail
const MAX_REQUEST_BACKOFF: Duration = Duration::from_secs(60);

/// Wait after the first 409 Conflict in a row; doubled for each next one, up
/// to the check interval
const CONFLICT_RETRY: Duration = Duration::from_millis(250);

/// Waits between job requests. Requests GitLab answers start one check
/// interval apart, counted from the start of the previous one, as
/// gitlab-runner feeds its runners on a fixed tick: a request Workhorse held
/// for long polling is followed by the next one at once. After a 409 Conflict
/// the next request goes out sooner. Failed requests wait the check interval,
/// doubled after each failure in a row up to `MAX_REQUEST_BACKOFF`, with
/// jitter so runners that lost GitLab at the same time do not retry in
/// lockstep.
struct RequestBackoff {
    check_interval: Duration,
    failures: u32,
    conflicts: u32,
}

impl RequestBackoff {
    fn new(check_interval: Duration) -> Self {
        Self {
            check_interval,
            failures: 0,
            conflicts: 0,
        }
    }

    /// Use a new check interval (a config reload changed it)
    fn set_check_interval(&mut self, check_interval: Duration) {
        self.check_interval = check_interval;
    }

    /// GitLab gave a job: the next request goes out at once
    fn got_job(&mut self) {
        self.failures = 0;
        self.conflicts = 0;
    }

    /// Wait after GitLab answered "no job" to a request that took `took`:
    /// the rest of the check interval, nothing after a long poll
    fn no_job(&mut self, took: Duration) -> Duration {
        self.got_job();
        self.check_interval.saturating_sub(took)
    }

    /// Wait after a 409 Conflict: `CONFLICT_RETRY`, doubled for each conflict
    /// in a row, never more than the check interval
    fn conflict(&mut self) -> Duration {
        self.failures = 0;
        let wait = CONFLICT_RETRY
            .saturating_mul(1 << self.conflicts.min(16))
            .min(self.check_interval);
        self.conflicts = self.conflicts.saturating_add(1);
        wait
    }

    /// Wait after a failed request: between half of and the full ceiling
    fn failed(&mut self) -> Duration {
        self.conflicts = 0;
        self.failures = self.failures.saturating_add(1);
        let ceiling = self.ceiling();
        ceiling / 2 + ceiling.mul_f64(random_fraction() / 2.0)
    }

    fn ceiling(&self) -> Duration {
        if self.failures == 0 {
            return self.check_interval;
        }
        let doublings = (self.failures - 1).min(16);
        self.check_interval
            .saturating_mul(1 << doublings)
            .min(MAX_REQUEST_BACKOFF.max(self.check_interval))
    }
}

/// How long to wait after a failed job request: the backoff, or longer when
/// GitLab answered with a Retry-After
fn retry_wait(error: &anyhow::Error, backoff: Duration) -> Duration {
    match error.downcast_ref::<crate::gitlab::RetryAfter>() {
        Some(after) => backoff.max(after.0),
        None => backoff,
    }
}

/// A random number in [0, 1)
fn random_fraction() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    // Every RandomState is seeded differently
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// How the runner is stopping
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shutdown {
    Running,
    /// Take no new jobs, let running jobs finish (SIGQUIT)
    Graceful,
    /// Take no new jobs, stop running jobs and report them failed (SIGTERM/SIGINT)
    Abort,
}

/// Requests a shutdown of a running daemon
#[derive(Clone)]
pub struct ShutdownHandle(Arc<watch::Sender<Shutdown>>);

impl ShutdownHandle {
    /// Escalate the shutdown mode (a graceful request never undoes an abort)
    pub fn request(&self, mode: Shutdown) {
        self.0.send_if_modified(|current| {
            let escalates = mode > *current;
            if escalates {
                *current = mode;
            }
            escalates
        });
    }
}

/// Resolve once an abort shutdown is requested
async fn abort_requested(shutdown: &mut watch::Receiver<Shutdown>) {
    // Drop the borrowed value right away: it must not be held across awaits
    let _ = shutdown
        .wait_for(|mode| *mode == Shutdown::Abort)
        .await
        .map(|_| ());
}

/// Interval between keep-alive updates that also detect remote cancellation
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// What a job runs with, taken when it starts: a config reload swaps it for
/// the jobs that start later, running jobs keep theirs
#[derive(Clone)]
struct Settings {
    config: Arc<config::RunnerConfig>,
    executor: Arc<executor::ExecutorType>,
    cache: job_cache::LocalCache,
}

/// The jobs a runner may run at once from its `limit` (0: no limit of its own)
fn runner_limit(limit: u32) -> u32 {
    if limit == 0 {
        u32::MAX
    } else {
        limit
    }
}

/// The local job cache of `config` (without its S3 layer)
fn local_cache(config: &config::RunnerConfig) -> job_cache::LocalCache {
    job_cache::LocalCache::new(&config.cache_dir)
        .with_max_age_days(config.cache_max_age_days)
        // Checked by `RunnerConfig::validate` when it was loaded
        .with_max_size(config.cache_max_size_bytes().unwrap_or(None))
        .with_legacy_zip(config.cache_legacy_zip)
}

/// A runner. Clones share its state; `config`, `executor` and `cache` are the
/// settings this clone works with (see `current`).
#[derive(Clone)]
pub struct RunnerDaemon {
    cache: job_cache::LocalCache,
    config: Arc<config::RunnerConfig>,
    gitlab: Arc<GitLabClient>,
    executor: Arc<executor::ExecutorType>,
    /// The settings new jobs get, swapped by a config reload
    settings: Arc<std::sync::RwLock<Settings>>,
    /// The cache's S3 layer, kept when a reload rebuilds the cache
    remote_cache: Option<s3::Bucket>,
    /// Job slots: this runner's `limit` within the shared `concurrent`,
    /// both changed by a config reload
    capacity: Arc<capacity::Capacity>,
    /// Seconds between job requests, changed by a config reload
    check_interval: Arc<std::sync::atomic::AtomicU64>,
    /// Admission limits, changed by a config reload
    limits: Arc<std::sync::RwLock<admission::Limits>>,
    scrubber: Arc<std::sync::RwLock<SecretScrubber>>,
    shutdown: Arc<watch::Sender<Shutdown>>,
    heartbeat_interval: Duration,
    slots: Arc<std::sync::Mutex<ConcurrencySlots>>,
    /// The runner token in use: the config's, or the one it was rotated to
    runner_token: Arc<std::sync::RwLock<String>>,
    tokens: Arc<token::TokenStore>,
    /// Held while a token is reset and saved, so shutdown never cuts it short
    rotating: Arc<tokio::sync::Mutex<()>>,
    /// Workspaces of running jobs, never evicted
    workspaces_in_use: Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
    /// Held for writing while workspaces are evicted, and for reading while a
    /// job claims its workspace: an eviction only removes workspaces that were
    /// not in use when it started, so no job may claim one until it ends
    evicting: Arc<tokio::sync::RwLock<()>>,
    /// Reads the host's free memory, disk space and load for admission control
    probe: Arc<dyn admission::Probe>,
    /// This runner's counters served on `metrics_listen`
    metrics: Arc<metrics::Metrics>,
    /// The process's counters, shared by its runners
    process: Arc<metrics::Process>,
}

/// How often workspaces and cache archives past their limits are looked for
const EVICTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

impl RunnerDaemon {
    pub fn new(
        config: config::RunnerConfig,
        gitlab: GitLabClient,
        executor: executor::ExecutorType,
    ) -> Self {
        let tokens = token::TokenStore::new(&config.state_dir, &config.runner_token);
        let runner_token = match tokens.load() {
            Some(stored) => {
                info!("   Using the rotated runner token from {:?}", tokens.path());
                stored.token
            }
            None => config.runner_token.clone(),
        };

        // Create secret scrubber with runner token
        let mut scrubber = SecretScrubber::new(vec![config.runner_token.clone()]);
        scrubber.add_secret(runner_token.clone());

        // Add GitLab URL as potential secret location
        if config.gitlab_url.contains('@') {
            scrubber.add_secret(config.gitlab_url.clone());
        }
        // A proxy URL with a password must not show up in job logs
        if let Some(proxy) = config.proxy.as_ref().filter(|p| p.contains('@')) {
            scrubber.add_secret(proxy.clone());
        }

        // Checked by `RunnerConfig::validate` when the config was loaded
        let limits = admission::Limits::from_config(&config).unwrap_or_default();
        let settings = Settings {
            cache: local_cache(&config),
            config: Arc::new(config),
            executor: Arc::new(executor),
        };
        Self {
            capacity: capacity::Capacity::within(
                runner_limit(settings.config.limit),
                capacity::Capacity::new(settings.config.concurrent),
            ),
            check_interval: Arc::new(settings.config.check_interval.into()),
            limits: Arc::new(std::sync::RwLock::new(limits)),
            cache: settings.cache.clone(),
            config: settings.config.clone(),
            gitlab: Arc::new(gitlab),
            executor: settings.executor.clone(),
            settings: Arc::new(std::sync::RwLock::new(settings)),
            remote_cache: None,
            scrubber: Arc::new(std::sync::RwLock::new(scrubber)),
            shutdown: Arc::new(watch::Sender::new(Shutdown::Running)),
            heartbeat_interval: HEARTBEAT_INTERVAL,
            slots: Arc::default(),
            runner_token: Arc::new(std::sync::RwLock::new(runner_token)),
            tokens: Arc::new(tokens),
            rotating: Arc::default(),
            workspaces_in_use: Arc::default(),
            evicting: Arc::default(),
            probe: Arc::new(admission::HostProbe),
            metrics: Arc::default(),
            process: Arc::default(),
        }
    }

    /// Read the host's resources through `probe` instead
    #[cfg(test)]
    fn with_probe(mut self, probe: Arc<dyn admission::Probe>) -> Self {
        self.probe = probe;
        self
    }

    /// Share the job cache with other runners through S3
    pub fn with_s3_cache(mut self, bucket: s3::Bucket) -> Self {
        self.cache = self.cache.with_remote(bucket.clone());
        self.remote_cache = Some(bucket);
        let cache = self.cache.clone();
        self.update_settings(|settings| settings.cache = cache);
        self
    }

    /// The settings new jobs get now
    fn settings(&self) -> Settings {
        match self.settings.read() {
            Ok(settings) => settings.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn update_settings(&self, update: impl FnOnce(&mut Settings)) {
        match self.settings.write() {
            Ok(mut settings) => update(&mut settings),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    /// This runner with the settings a job starting now gets: a config
    /// reload may have changed them since this clone was made
    fn current(&self) -> RunnerDaemon {
        let settings = self.settings();
        RunnerDaemon {
            cache: settings.cache,
            config: settings.config,
            executor: settings.executor,
            ..self.clone()
        }
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle(self.shutdown.clone())
    }

    /// Wait between job requests now (the config's, or a reloaded one)
    fn check_interval(&self) -> Duration {
        Duration::from_secs(
            self.check_interval
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Admission limits now (the config's, or reloaded ones)
    fn current_limits(&self) -> admission::Limits {
        match self.limits.read() {
            Ok(limits) => limits.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    async fn shutdown_requested(&self) {
        let mut rx = self.shutdown.subscribe();
        let _ = rx.wait_for(|mode| *mode != Shutdown::Running).await;
    }

    /// Start the runner daemon with this runner alone (see `Manager::start`)
    #[cfg(test)]
    pub async fn start(&self) -> Result<()> {
        runners::Manager::new(vec![self.clone()]).start().await
    }

    /// Use the slots, check interval, admission limits, shutdown signal and
    /// process metrics of `first`: the runners of one process share them
    fn share_from(&mut self, first: &RunnerDaemon) {
        self.capacity = capacity::Capacity::within(self.capacity.limit(), first.capacity.shared());
        self.check_interval = first.check_interval.clone();
        self.limits = first.limits.clone();
        self.shutdown = first.shutdown.clone();
        self.probe = first.probe.clone();
        self.process = first.process.clone();
    }

    /// Take jobs from GitLab and run them until a shutdown, then wait for
    /// this runner's jobs to end
    async fn run(&self) {
        info!("   Cache enabled: {}", self.config.cache_enabled);
        let mut gate = admission::Gate::default();
        self.executor.sweep_orphans().await;
        let rotation = AbortOnDrop(tokio::spawn(self.clone().rotate_token().in_current_span()));
        let housekeeping = AbortOnDrop(tokio::spawn(self.clone().housekeeping().in_current_span()));

        let mut connected = false;
        let mut backoff = RequestBackoff::new(self.check_interval());
        loop {
            // Wait for a free slot; a shutdown interrupts this wait, the sleeps
            // and a job request GitLab holds (long polling), never one it has
            // already answered
            let slot = tokio::select! {
                slot = self.capacity.acquire() => slot,
                _ = self.shutdown_requested() => break,
            };
            backoff.set_check_interval(self.check_interval());
            // Short of memory, disk space or CPU: ask again after the check
            // interval, without a request to GitLab
            if !self.admit(&self.current_limits(), &mut gate).await {
                drop(slot);
                tokio::select! {
                    _ = sleep(self.check_interval()) => {}
                    _ = self.shutdown_requested() => break,
                }
                continue;
            }
            let requested = std::time::Instant::now();
            let claimed = claim_with_slot(
                slot,
                &self.gitlab,
                &self.current_token(),
                self.shutdown_requested(),
            )
            .await;
            if claimed.is_ok() && !connected {
                connected = true;
                info!(
                    "✅ Connected to GitLab at {}, waiting for jobs",
                    self.config.gitlab_url
                );
            }
            let wait = match claimed {
                Ok(Claimed::Job(job, slot)) => {
                    let job_name = job
                        .job_info
                        .as_ref()
                        .map(|ji| ji.name.as_str())
                        .unwrap_or("unknown");
                    info!("📦 Received job #{} ({})", job.id, job_name);

                    self.metrics.request(metrics::Request::Job);
                    // The job keeps the settings it starts with
                    let daemon = self.current();
                    tokio::spawn(
                        async move {
                            let _slot = slot;
                            if let Err(e) = daemon.execute_job(*job).await {
                                error!("Job execution failed: {}", e);
                            }
                        }
                        .in_current_span(),
                    );
                    backoff.got_job();
                    None
                }
                Ok(Claimed::NoJob) => {
                    self.metrics.request(metrics::Request::NoJob);
                    Some(backoff.no_job(requested.elapsed()))
                }
                Ok(Claimed::Conflict) => {
                    self.metrics.request(metrics::Request::Conflict);
                    Some(backoff.conflict())
                }
                Err(e) => {
                    self.metrics.request(metrics::Request::Error);
                    // GitLab's own Retry-After (429/503) wins over a shorter backoff
                    let wait = retry_wait(&e, backoff.failed());
                    warn!(
                        "Failed to request job: {:#} (retrying in {:.1}s)",
                        e,
                        wait.as_secs_f64()
                    );
                    Some(wait)
                }
            };
            if let Some(wait) = wait {
                tokio::select! {
                    _ = sleep(wait) => {}
                    _ = self.shutdown_requested() => break,
                }
            }
        }

        info!("⏳ Waiting for running jobs to finish...");
        self.capacity.idle().await;
        // Canceled jobs leave multipart uploads for S3 to discard
        self.cache.finish_aborts().await;
        // Let a token reset in flight be saved, or the new token would be lost
        let _rotating = self.rotating.lock().await;
        drop(rotation);
        drop(housekeeping);
    }

    /// The metrics as served on `metrics_listen`, of this runner alone
    #[cfg(test)]
    fn render_metrics(&self) -> String {
        runners::Manager::new(vec![self.clone()]).render_metrics()
    }

    /// What the metrics show of this runner now
    fn gauges(&self) -> metrics::Gauges {
        metrics::Gauges {
            running: self.capacity.running().into(),
            executor: match self.executor.as_ref() {
                executor::ExecutorType::Docker(_) => "docker",
                executor::ExecutorType::Shell(_) => "shell",
            }
            .to_string(),
        }
    }

    /// Whether the host meets the admission limits; changes are logged once
    async fn admit(&self, limits: &admission::Limits, gate: &mut admission::Gate) -> bool {
        let violations = if limits.is_off() {
            Vec::new()
        } else {
            self.check_admission(limits).await
        };
        match gate.update(&violations) {
            Some(admission::Transition::Blocked(reasons)) => {
                warn!("⏸️  Not taking new jobs: {}", reasons)
            }
            Some(admission::Transition::Admitted) => {
                info!("▶️  Taking jobs again: the host is within the admission limits")
            }
            None => {}
        }
        for resource in admission::Resource::ALL {
            self.metrics
                .set_admission_blocked(resource, gate.is_blocked(resource));
        }
        violations.is_empty()
    }

    /// The admission limits the host violates now
    async fn check_admission(&self, limits: &admission::Limits) -> Vec<admission::Violation> {
        let mut dirs = vec![self.executor.builds_root()];
        if self.config.cache_enabled {
            dirs.push(std::path::PathBuf::from(&self.config.cache_dir));
        }
        let (probe, checked) = (self.probe.clone(), limits.clone());
        // statvfs on a hung network file system must not stall a worker
        tokio::task::spawn_blocking(move || admission::check(&checked, probe.as_ref(), &dirs))
            .await
            .unwrap_or_default()
    }

    /// The checked-out project on the host (see `script::project_subdir`)
    fn project_dir(&self, job: &Job) -> std::path::PathBuf {
        self.executor
            .job_dir(job)
            .join(script::project_subdir(job).unwrap_or_else(|_| "project".to_string()))
    }

    fn current_token(&self) -> String {
        self.runner_token
            .read()
            .map(|token| token.clone())
            .unwrap_or_default()
    }

    /// Reset the runner token after 3/4 of its lifetime, as long as GitLab
    /// gives it an expiry
    async fn rotate_token(self) {
        const RETRY: Duration = Duration::from_secs(10 * 60);

        let mut expiry = self
            .tokens
            .load()
            .and_then(|stored| Some((stored.obtained_at, stored.expires_at?)));
        loop {
            let (obtained_at, expires_at) = match expiry {
                Some(expiry) => expiry,
                None => match self.gitlab.verify_runner(&self.current_token()).await {
                    Ok(Some(expires_at)) => (token::now(), expires_at),
                    // The token does not expire: nothing to rotate
                    Ok(None) => return,
                    Err(e) => {
                        tracing::debug!("Could not check the runner token expiry: {:#}", e);
                        sleep(Duration::from_secs(60 * 60)).await;
                        continue;
                    }
                },
            };
            expiry = Some((obtained_at, expires_at));
            let wait = token::reset_time(obtained_at, expires_at) - token::now();
            if wait > 0 {
                info!(
                    "Runner token expires at {}, resetting it at {}",
                    token::display(expires_at),
                    token::display(token::reset_time(obtained_at, expires_at))
                );
                sleep(Duration::from_secs(wait as u64)).await;
            }

            let _rotating = self.rotating.lock().await;
            let old = self.current_token();
            // GitLab invalidates the old token right away: only reset when the
            // new one can be kept, or the runner would lose it on restart
            if let Err(e) = self.tokens.save(&old, obtained_at, Some(expires_at)) {
                error!(
                    "Runner token expires at {} but cannot be rotated: {:#}. \
                     Put a new token in the config before it expires.",
                    token::display(expires_at),
                    e
                );
                return;
            }
            match self.gitlab.reset_runner_token(&old).await {
                Ok((new, new_expiry)) => {
                    let now = token::now();
                    if let Err(e) = self.tokens.save(&new, now, new_expiry) {
                        error!("Failed to save the rotated runner token: {:#}", e);
                    }
                    if let Ok(mut scrubber) = self.scrubber.write() {
                        scrubber.add_secret(new.clone());
                    }
                    if let Ok(mut token) = self.runner_token.write() {
                        *token = new;
                    }
                    info!("🔑 Runner token rotated");
                    match new_expiry {
                        Some(expires_at) => expiry = Some((now, expires_at)),
                        None => return,
                    }
                }
                Err(e) => {
                    warn!("Failed to reset the runner token: {:#}", e);
                    drop(_rotating);
                    sleep(RETRY).await;
                }
            }
        }
    }

    /// Run a job end to end and always report a final state to GitLab
    async fn execute_job(&self, mut job: Job) -> Result<()> {
        // Slot numbers scripts use for per-slot directories, like gitlab-runner
        let project = project_id(&job);
        let slot = self
            .slots
            .lock()
            .map(|mut slots| slots.acquire(project))
            .unwrap_or_default();
        // Variables the runner adds, as gitlab-runner does; the job's own come later
        // and win
        let runner_variables = [
            ("CI_RUNNER_SHORT_TOKEN", short_token(&self.current_token())),
            ("CI_RUNNER_EXECUTABLE_ARCH", executable_arch()),
            ("CI_RUNNER_REVISION", revision()),
            ("CI_RUNNER_VERSION", env!("CARGO_PKG_VERSION").to_string()),
            ("CI_CONCURRENT_PROJECT_ID", slot.1.to_string()),
            ("CI_CONCURRENT_ID", slot.0.to_string()),
        ]
        .map(|(key, value)| runner_variable(key, value));
        prepend_variables(&mut job, runner_variables);
        job.project_slot = slot.1;
        apply_runner_config(&mut job, &self.config);
        let result = self.run_job(job).await;
        if let Ok(mut slots) = self.slots.lock() {
            slots.release(project, slot);
        }
        result
    }

    async fn run_job(&self, job: Job) -> Result<()> {
        let job_name = job
            .job_info
            .as_ref()
            .map(|ji| ji.name.as_str())
            .unwrap_or("unknown");
        info!("▶️  Starting job #{}: {}", job.id, job_name);

        // Mask the job token, dependency tokens and masked variables
        let scrubber = match self.scrubber.read() {
            Ok(scrubber) => scrubber.with_job_secrets(&job),
            Err(poisoned) => poisoned.into_inner().with_job_secrets(&job),
        };
        let cancel = CancelSignal::default();
        let mut trace = TraceWriter::new(Some(&self.gitlab), job.id, &job.token, &scrubber)
            .with_limit(self.config.output_limit_bytes())
            .with_spool_dir(&self.config.state_dir)
            .with_cancel(cancel.clone());
        // Aborted when dropped, even if the job panics, so GitLab is not told
        // forever that a dead job is running
        let _heartbeat = AbortOnDrop(self.spawn_heartbeat(&job, cancel.clone()));
        trace
            .write(&format!(
                "Running with TurboCI {}\n",
                env!("CARGO_PKG_VERSION")
            ))
            .await;

        // The job's variables are final now: expanded once for the whole job
        let variables = job_variables(&job);
        let project_dir = self.project_dir(&job);
        let workspace = self.executor.job_dir(&job);
        self.claim_workspace(&workspace).await;
        let prepared = match self.executor.prepare(&job).await {
            Ok(()) => tokio::fs::create_dir_all(&project_dir)
                .await
                .map_err(anyhow::Error::from),
            Err(e) => Err(e),
        };
        let mut outcome = prepared
            .map_err(|e| JobFailure::system(format!("Failed to create workspace: {:#}", e)));
        if outcome.is_ok() {
            outcome = self
                .executor
                .execute(
                    &job,
                    &mut trace,
                    &WorkspaceRestore {
                        daemon: self,
                        variables: &variables,
                    },
                )
                .await;
        }
        // Aborted jobs (runner shutdown, or gone from GitLab) upload nothing
        if cancel.state() != RemoteState::Aborted {
            // An upload failure fails a job that otherwise succeeded (the next
            // stage would miss its inputs); the job's own failure takes precedence
            let result = if outcome.is_ok() {
                "successful"
            } else {
                "failed"
            };
            let section = if outcome.is_ok() {
                "upload_artifacts_on_success"
            } else {
                "upload_artifacts_on_failure"
            };
            if job.artifacts.as_ref().is_some_and(|a| !a.is_empty()) {
                trace
                    .section_start(section, &format!("Uploading artifacts for {} job", result))
                    .await;
            }
            let uploaded = self
                .upload_artifacts(&job, &variables, &mut trace, outcome.is_ok())
                .await;
            if job.artifacts.as_ref().is_some_and(|a| !a.is_empty()) {
                trace.section_end(section).await;
            }
            if outcome.is_ok() {
                outcome = uploaded;
            }
            let saves_cache = self.config.cache_enabled
                && job
                    .cache
                    .iter()
                    .any(|c| c.policy == "push" || c.policy == "pull-push");
            if saves_cache {
                let result = if outcome.is_ok() {
                    "successful"
                } else {
                    "failed"
                };
                trace
                    .section_start("archive_cache", &format!("Saving cache for {} job", result))
                    .await;
            }
            self.upload_cache(&job, &variables, &mut trace, outcome.is_ok())
                .await;
            if saves_cache {
                trace.section_end("archive_cache").await;
            }
        }
        self.executor
            .cleanup(&job, self.config.executor.workspace_max_age_days > 0)
            .await;
        if let Ok(mut in_use) = self.workspaces_in_use.lock() {
            in_use.remove(&workspace);
        }
        drop(_heartbeat);

        // Jobs stopped by a runner shutdown are the runner's failure, not the user's
        let stopped_by_shutdown = *self.shutdown.borrow() == Shutdown::Abort;
        let outcome = outcome.map_err(|mut failure| {
            if stopped_by_shutdown && failure.reason == FailureReason::JobCanceled {
                failure.reason = FailureReason::RunnerSystemFailure;
                failure.message = "the runner is shutting down".to_string();
            }
            failure
        });

        let (state, reason, exit_code) = match &outcome {
            Ok(()) => {
                info!("✅ Job #{} succeeded", job.id);
                trace.write("\nJob succeeded\n").await;
                (JobState::Success, None, None)
            }
            Err(failure) => {
                let message = scrubber.scrub(&failure.message);
                error!("❌ Job #{} failed: {}", job.id, message);
                trace
                    .write(&format!("\nERROR: Job failed: {}\n", message))
                    .await;
                (JobState::Failed, Some(failure.reason), failure.exit_code)
            }
        };
        trace.finish().await;

        if cancel.state() == RemoteState::Aborted && !stopped_by_shutdown {
            // Canceled in GitLab: it already has the final state and rejects updates
            info!("🛑 Job #{} was canceled", job.id);
            self.metrics.job_finished(metrics::JobEnd::Canceled);
            return Ok(());
        }
        match &outcome {
            Ok(()) => self.metrics.job_finished(metrics::JobEnd::Success),
            Err(failure) => self.metrics.job_failed(&failure.reason),
        }
        self.gitlab
            .update_job(job.id, &job.token, state, reason, exit_code)
            .await
    }

    /// Mark a workspace in use, once no eviction that may remove it is running
    async fn claim_workspace(&self, workspace: &std::path::Path) {
        let _no_eviction = self.evicting.read().await;
        if let Ok(mut in_use) = self.workspaces_in_use.lock() {
            in_use.insert(workspace.to_path_buf());
        }
    }

    /// Sweep at start and every `EVICTION_INTERVAL` (see `sweep`), with the
    /// limits in effect then
    async fn housekeeping(self) {
        loop {
            self.current().sweep().await;
            sleep(EVICTION_INTERVAL).await;
        }
    }

    /// Remove workspaces no job used for `workspace_max_age_days`, the least
    /// recently used beyond `workspace_max_size`, and the cache archives of
    /// every project past `cache_max_age_days` or `cache_max_size`, and
    /// unused Docker images (`image_max_age_days`, `image_max_size`).
    /// Workspaces in use are never touched.
    async fn sweep(&self) {
        let root = self.executor.builds_root();
        let max_size = self.config.workspace_max_size_bytes().unwrap_or(None);
        // Measured before taking the lock: walking a large builds directory
        // takes a while, and jobs cannot claim workspaces during the lock
        let scanned = match max_size {
            Some(_) => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || housekeeping::scan(&root))
                    .await
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        {
            let _evicting = self.evicting.write().await;
            let in_use = self
                .workspaces_in_use
                .lock()
                .map(|in_use| in_use.clone())
                .unwrap_or_default();
            let days = self.config.executor.workspace_max_age_days;
            if days > 0 {
                self.executor
                    .evict_workspaces(Duration::from_secs(days * 24 * 60 * 60), &in_use)
                    .await;
            }
            if let Some(max_size) = max_size {
                // Only renames happen under the lock; deleting comes after it
                let evicted = tokio::task::spawn_blocking(move || {
                    // The age limit may have removed some already
                    let present: Vec<_> = scanned.into_iter().filter(|w| w.path.exists()).collect();
                    let total: u64 = present.iter().map(|w| w.size).sum();
                    let evicted: Vec<housekeeping::Workspace> =
                        housekeeping::over_quota(&present, max_size, &in_use)
                            .into_iter()
                            .filter(|w| housekeeping::detach(&w.path))
                            .cloned()
                            .collect();
                    (total, evicted)
                })
                .await;
                if let Ok((total, evicted)) = evicted {
                    let freed: u64 = evicted.iter().map(|w| w.size).sum();
                    if !evicted.is_empty() {
                        info!(
                            "💾 Workspaces take {}, above workspace_max_size {}: removing the {} least recently used ({})",
                            admission::human(total),
                            admission::human(max_size),
                            evicted.len(),
                            admission::human(freed)
                        );
                    }
                    if total - freed > max_size {
                        warn!(
                            "Workspaces in use take {}, above workspace_max_size {}",
                            admission::human(total - freed),
                            admission::human(max_size)
                        );
                    }
                    self.metrics
                        .workspaces_swept(total - freed, evicted.len() as u64);
                }
            }
        }
        let _ = tokio::task::spawn_blocking(move || housekeeping::remove_detached(&root)).await;
        self.cache.evict_all().await;
        self.executor.collect_images().await;
    }

    /// Keep-alive for the job: learns about remote cancellation even when the job
    /// prints nothing, and aborts the job when the runner is told to abort
    fn spawn_heartbeat(&self, job: &Job, cancel: CancelSignal) -> tokio::task::JoinHandle<()> {
        let gitlab = self.gitlab.clone();
        let mut shutdown = self.shutdown.subscribe();
        let interval = self.heartbeat_interval;
        let (job_id, token) = (job.id, job.token.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sleep(interval) => {
                        if let Ok(state) = gitlab.touch_job(job_id, &token).await {
                            cancel.update(state);
                        }
                    }
                    _ = abort_requested(&mut shutdown) => {
                        cancel.update(RemoteState::Aborted);
                        return;
                    }
                }
            }
        })
    }

    /// Restore cache and dependency artifacts into the checked-out project,
    /// until `stop`
    async fn restore_workspace(
        &self,
        job: &Job,
        variables: &Variables,
        trace: &mut TraceWriter<'_>,
        stop: &Stop,
    ) -> JobOutcome {
        let project_dir = self.project_dir(job);
        let workspace = project_dir.to_string_lossy();

        let pulls_cache = job
            .cache
            .iter()
            .any(|c| c.policy == "pull" || c.policy == "pull-push");
        if self.config.cache_enabled && pulls_cache {
            trace
                .section_start("restore_cache", "Restoring cache")
                .await;
            for cache_entry in &job.cache {
                if cache_entry.policy != "pull" && cache_entry.policy != "pull-push" {
                    continue;
                }
                // CACHE_FALLBACK_KEY is the last resort, after the entry's own
                // fallbacks. Like gitlab-runner, it may not name a protected
                // branch's cache, or any branch could read it.
                let mut fallback = variables.get("CACHE_FALLBACK_KEY").cloned();
                if fallback.as_deref().is_some_and(|key| {
                    job_cache::resolve_key(key, variables).ends_with("-protected")
                }) {
                    trace
                        .write("WARNING: CACHE_FALLBACK_KEY ending in -protected is not allowed, ignoring it\n")
                        .await;
                    fallback = None;
                }
                let keys: Vec<String> = std::iter::once(&cache_entry.key)
                    .chain(&cache_entry.fallback_keys)
                    .chain(fallback.as_ref())
                    .map(|key| job_cache::resolve_key(key, variables))
                    .collect();
                trace
                    .write(&format!("Restoring cache {}...\n", keys[0]))
                    .await;
                // A pull-push cache the job leaves unchanged is not archived
                // again; untracked files are only listed when saving
                let paths: Vec<String> = cache_entry
                    .paths
                    .iter()
                    .map(|path| script::expand(path, variables))
                    .collect();
                let tracking = (cache_entry.policy == "pull-push"
                    && !cache_entry.untracked.unwrap_or(false))
                .then_some(job_cache::Tracking {
                    job_id: job.id,
                    paths: &paths,
                });
                let tries = attempts(job, "RESTORE_CACHE_ATTEMPTS");
                let mut restored = self
                    .cache
                    .restore_job(project_id(job), &keys, &workspace, tracking, stop)
                    .await;
                for _ in 1..tries {
                    if restored.is_ok() {
                        break;
                    }
                    restored = self
                        .cache
                        .restore_job(project_id(job), &keys, &workspace, tracking, stop)
                        .await;
                }
                self.metrics.cache_restore(match &restored {
                    Ok(restored) if restored.hit.is_some() => metrics::CacheRestore::Hit,
                    Ok(_) => metrics::CacheRestore::Miss,
                    Err(_) => metrics::CacheRestore::Error,
                });
                if let Ok(restored) = &restored {
                    self.metrics.cache_s3_errors(restored.warnings.len() as u64);
                }
                match restored {
                    Ok(restored) => {
                        for warning in &restored.warnings {
                            warn!("{}", warning);
                            trace.write(&format!("WARNING: {}\n", warning)).await;
                        }
                        match restored.hit {
                            Some((key, source)) => {
                                trace
                                    .write(&format!(
                                        "Successfully restored cache {} ({})\n",
                                        key, source
                                    ))
                                    .await
                            }
                            None => trace.write("No cache found\n").await,
                        }
                    }
                    Err(e) => {
                        warn!("Failed to restore cache {}: {}", keys[0], e);
                        trace
                            .write(&format!("WARNING: Failed to restore cache: {}\n", e))
                            .await;
                    }
                }
            }
            trace.section_end("restore_cache").await;
        }

        // Jobs without artifacts have nothing to download; a failed download
        // fails the job, as the script would run without its inputs
        if !job.dependencies.iter().any(|d| d.artifacts_file.is_some()) {
            return Ok(());
        }
        trace
            .section_start("download_artifacts", "Downloading artifacts")
            .await;
        let downloaded = self
            .download_dependencies(job, trace, &workspace, stop)
            .await;
        trace.section_end("download_artifacts").await;
        downloaded
    }

    /// Download and extract the artifacts of the jobs this one depends on,
    /// until `stop` (the executor sets it on the job timeout or a cancel)
    async fn download_dependencies(
        &self,
        job: &Job,
        trace: &mut TraceWriter<'_>,
        workspace: &str,
        stop: &Stop,
    ) -> JobOutcome {
        for dependency in job
            .dependencies
            .iter()
            .filter(|d| d.artifacts_file.is_some())
        {
            trace
                .write(&format!(
                    "Downloading artifacts from job #{} ({})...\n",
                    dependency.id, dependency.name
                ))
                .await;
            let job_dir = self.executor.job_dir(job);
            let tries = attempts(job, "ARTIFACT_DOWNLOAD_ATTEMPTS");
            let mut result = Err(anyhow::anyhow!("not attempted"));
            for attempt in 1..=tries {
                result = artifacts::download_and_extract_artifacts(
                    &self.gitlab,
                    dependency.id,
                    &dependency.token,
                    workspace,
                    &job_dir,
                    stop,
                )
                .await;
                if result.is_ok() || attempt == tries {
                    break;
                }
                warn!(
                    "Artifact download attempt {}/{} failed, retrying",
                    attempt, tries
                );
            }
            result.map_err(|e| JobFailure {
                reason: FailureReason::ScriptFailure,
                exit_code: None,
                message: format!(
                    "failed to download artifacts from job #{} ({}): {:#}",
                    dependency.id, dependency.name, e
                ),
            })?;
        }
        Ok(())
    }

    /// Upload artifacts whose `when` matches the job result (default: on_success)
    async fn upload_artifacts(
        &self,
        job: &Job,
        variables: &Variables,
        trace: &mut TraceWriter<'_>,
        succeeded: bool,
    ) -> JobOutcome {
        let Some(ref job_artifacts) = job.artifacts else {
            return Ok(());
        };
        let mut failure = None;
        let cancel = trace.cancel();
        // Paths, exclude patterns and names may use `variables`
        let untracked = self
            .untracked_if_needed(job, job_artifacts.iter().any(|a| a.untracked), trace)
            .await;
        let expand_all = |items: &[String]| -> Vec<String> {
            items
                .iter()
                .map(|item| script::expand(item, variables))
                .collect()
        };
        let workspace = self.project_dir(job);

        for artifact in job_artifacts {
            let wanted = match artifact.when.as_deref() {
                Some("always") => true,
                Some("on_failure") => !succeeded,
                _ => succeeded,
            };
            let mut paths = expand_all(&artifact.paths);
            if artifact.untracked {
                paths.extend(untracked.iter().cloned());
            }
            if !wanted || paths.is_empty() {
                continue;
            }
            let name = artifact
                .name
                .as_deref()
                .map(|name| script::expand(name, variables))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "artifacts".to_string());
            let name = name.as_str();
            let artifact_type = artifact.artifact_type.as_deref().unwrap_or("archive");
            let format = artifact.artifact_format.as_deref().unwrap_or("zip");
            trace
                .write(&format!(
                    "Uploading artifacts ({}, {})...\n",
                    name, artifact_type
                ))
                .await;

            // Staged next to the workspace (same disk, not a possibly RAM-backed /tmp)
            let archive_and_upload = async {
                let Some(archive) = artifacts::create_archive(
                    &workspace.to_string_lossy(),
                    &paths,
                    &expand_all(&artifact.exclude),
                    format,
                    &self.executor.job_dir(job),
                )
                .await?
                else {
                    return Ok(false);
                };
                let extension = match format {
                    "gzip" => ".gz",
                    "zip" => ".zip",
                    _ => "",
                };
                self.gitlab
                    .upload_artifacts(
                        job.id,
                        &job.token,
                        ArtifactUpload {
                            path: archive.file.path().to_path_buf(),
                            len: archive.len,
                            file_name: format!("{}{}", name, extension),
                            format: format.to_string(),
                            artifact_type: artifact_type.to_string(),
                            expire_in: artifact.expire_in.clone(),
                        },
                    )
                    .await
                    .map(|()| true)
            };
            // An aborted job uploads nothing more: an upload may take up to an
            // hour, longer than the service manager waits for the runner to
            // stop. A dropped archive still being written runs to its end in
            // the background, into a staging file that is then removed.
            let result = tokio::select! {
                biased;
                _ = cancel.reached(RemoteState::Aborted) => {
                    trace.write("Artifacts not uploaded: job canceled\n").await;
                    return Err(JobFailure {
                        reason: FailureReason::JobCanceled,
                        exit_code: None,
                        message: "canceled".to_string(),
                    });
                }
                result = archive_and_upload => result,
            };
            match result {
                Ok(false) => {
                    trace
                        .write("WARNING: No files matched the artifact paths, nothing uploaded\n")
                        .await;
                }
                Ok(true) => info!("✅ Uploaded artifact: {}", name),
                Err(e) => {
                    warn!("Failed to upload artifact {}: {:#}", name, e);
                    trace
                        .write(&format!("ERROR: Uploading artifacts failed: {:#}\n", e))
                        .await;
                    failure.get_or_insert(JobFailure {
                        reason: FailureReason::ScriptFailure,
                        exit_code: None,
                        message: format!("uploading artifacts ({}) failed: {:#}", name, e),
                    });
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Untracked files as literal path patterns, when some entry asks for them
    async fn untracked_if_needed(
        &self,
        job: &Job,
        wanted: bool,
        trace: &mut TraceWriter<'_>,
    ) -> Vec<String> {
        if !wanted {
            return Vec::new();
        }
        match self.executor.untracked_files(job).await {
            // File names are matched literally, not as glob patterns
            Ok(files) => files.iter().map(|f| glob::Pattern::escape(f)).collect(),
            Err(e) => {
                trace
                    .write(&format!(
                        "WARNING: Could not list untracked files: {:#}\n",
                        e
                    ))
                    .await;
                Vec::new()
            }
        }
    }

    /// Save cache entries whose policy pushes and whose `when` matches the result
    async fn upload_cache(
        &self,
        job: &Job,
        variables: &Variables,
        trace: &mut TraceWriter<'_>,
        succeeded: bool,
    ) {
        if !self.config.cache_enabled {
            return;
        }
        let workspace = self.project_dir(job).to_string_lossy().into_owned();
        let cancel = trace.cancel();
        let wants_untracked = job
            .cache
            .iter()
            .any(|entry| entry.untracked.unwrap_or(false));
        let untracked = self.untracked_if_needed(job, wants_untracked, trace).await;

        for cache_entry in &job.cache {
            let wanted = match cache_entry.when.as_deref() {
                Some("always") => true,
                Some("on_failure") => !succeeded,
                _ => succeeded,
            };
            let mut paths: Vec<String> = cache_entry
                .paths
                .iter()
                .map(|path| script::expand(path, variables))
                .collect();
            if cache_entry.untracked.unwrap_or(false) {
                paths.extend(untracked.iter().cloned());
            }
            if !wanted
                || paths.is_empty()
                || (cache_entry.policy != "push" && cache_entry.policy != "pull-push")
            {
                continue;
            }
            let key = job_cache::resolve_key(&cache_entry.key, variables);
            trace.write(&format!("Saving cache {}...\n", key)).await;
            // An aborted job saves nothing more; checked first, so no later
            // entry starts. A dropped save only reads the workspace: a zip
            // still being written runs to its end in the background, into a
            // staging file in the cache directory that is then removed.
            let saved = tokio::select! {
                biased;
                _ = cancel.reached(RemoteState::Aborted) => {
                    trace.write("Cache not saved: job canceled\n").await;
                    return;
                }
                saved = self.cache.save_job(Some(job.id), project_id(job), &key, &workspace, &paths) => saved,
            };
            if let Ok(Some(saved)) = &saved {
                match &saved.upload {
                    Some(Ok(job_cache::Upload::Sent)) => {
                        self.metrics.cache_upload(metrics::CacheUpload::Sent)
                    }
                    Some(Ok(job_cache::Upload::Unchanged)) => {
                        self.metrics.cache_upload(metrics::CacheUpload::Unchanged)
                    }
                    Some(Err(_)) => {
                        self.metrics.cache_upload(metrics::CacheUpload::Failed);
                        self.metrics.cache_s3_errors(1);
                    }
                    None => {}
                }
            }
            match saved {
                Ok(Some(saved)) if saved.unchanged_since_restore => {
                    trace
                        .write(&format!(
                            "Cache {} unchanged since restore: not saved\n",
                            key
                        ))
                        .await
                }
                Ok(Some(saved)) => {
                    trace
                        .write(&format!("Created cache {} ({} bytes)\n", key, saved.size))
                        .await;
                    match saved.upload {
                        Some(Ok(job_cache::Upload::Unchanged)) => {
                            trace
                                .write(&format!(
                                    "Cache {} unchanged, S3 holds it already: not uploaded\n",
                                    key
                                ))
                                .await
                        }
                        Some(Ok(job_cache::Upload::Sent)) => {
                            trace
                                .write(&format!(
                                    "Uploaded cache {} to S3 ({:.1} MB)\n",
                                    key,
                                    saved.size as f64 / 1_048_576.0
                                ))
                                .await
                        }
                        Some(Err(e)) => {
                            warn!("Cache {} not uploaded to S3: {}", key, e);
                            trace
                                .write(&format!(
                                    "WARNING: cache {} saved locally, not uploaded to S3: {}\n",
                                    key, e
                                ))
                                .await
                        }
                        None => {}
                    }
                }
                Ok(None) => {
                    trace
                        .write(&format!(
                            "No files matched the paths of cache {}, not saved\n",
                            key
                        ))
                        .await
                }
                Err(e) => {
                    warn!("Failed to save cache {}: {}", key, e);
                    trace
                        .write(&format!("WARNING: Failed to save cache: {}\n", e))
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn claim_job_does_not_request_while_all_slots_are_busy() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"id": 1, "token": "job-token"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let gitlab = GitLabClient::new(server.uri(), "runner-token".to_string());
        let capacity = capacity::Capacity::new(1);

        let (_job, slot) = claim_job(&capacity, &gitlab, "runner-token")
            .await
            .unwrap()
            .expect("job expected");

        // The only slot is taken by the running job: no second request may be sent
        let second = tokio::time::timeout(
            Duration::from_millis(200),
            claim_job(&capacity, &gitlab, "runner-token"),
        )
        .await;
        assert!(second.is_err(), "requested a job with no free slot");

        drop(slot);
        assert_eq!(capacity.running(), 0);
    }

    /// A daemon with the shell executor talking to a mock GitLab
    async fn daemon(server: &MockServer, dir: &std::path::Path) -> RunnerDaemon {
        daemon_with(server, dir, |_| {}).await
    }

    async fn daemon_with(
        server: &MockServer,
        dir: &std::path::Path,
        configure: impl FnOnce(&mut config::RunnerConfig),
    ) -> RunnerDaemon {
        job_routes(server).await;
        let mut config = config::RunnerConfig {
            runner_token: "glrt-test".to_string(),
            gitlab_url: server.uri(),
            cache_dir: dir.join("cache").to_string_lossy().into_owned(),
            state_dir: dir.to_string_lossy().into_owned(),
            ..config::RunnerConfig::default()
        };
        configure(&mut config);
        let executor = executor::ExecutorType::Shell(executor::ShellExecutor::new(Some(
            dir.join("builds").to_string_lossy().into_owned(),
        )));
        RunnerDaemon::new(
            config,
            GitLabClient::new(server.uri(), "glrt-test".to_string()),
            executor,
        )
    }

    /// GitLab's answers to trace, final state and artifact requests
    async fn job_routes(server: &MockServer) {
        for (verb, route, status) in [
            ("PATCH", r"^/api/v4/jobs/\d+/trace$", 202),
            ("PUT", r"^/api/v4/jobs/\d+$", 200),
            ("POST", r"^/api/v4/jobs/\d+/artifacts$", 201),
        ] {
            Mock::given(method(verb))
                .and(wiremock::matchers::path_regex(route))
                .respond_with(ResponseTemplate::new(status))
                .mount(server)
                .await;
        }
    }

    fn lifecycle_job(id: u64, script: &[&str], artifacts_when: &str) -> Job {
        serde_json::from_value(serde_json::json!({
            "id": id, "token": format!("job-token-{}", id),
            "job_info": {"name": "build", "stage": "test", "project_id": 5, "project_name": "app"},
            "variables": [{"key": "SECRET", "value": "s3cr3t-value", "masked": true}],
            "steps": [{"name": "script", "script": script, "when": "on_success", "timeout": 60}],
            "artifacts": [{"name": "dist", "paths": ["out/"], "when": artifacts_when}],
            "cache": [{"key": "deps", "paths": ["vendor/"], "policy": "pull-push"}]
        }))
        .unwrap()
    }

    async fn requests(server: &MockServer, verb: &str, job_id: u64) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| {
                r.method.as_str() == verb && r.url.path().contains(&format!("/jobs/{}", job_id))
            })
            .collect()
    }

    async fn trace_of(server: &MockServer, job_id: u64) -> String {
        requests(server, "PATCH", job_id)
            .await
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect()
    }

    async fn final_update(server: &MockServer, job_id: u64) -> serde_json::Value {
        let updates = requests(server, "PUT", job_id).await;
        assert_eq!(updates.len(), 1, "exactly one final update expected");
        serde_json::from_slice(&updates[0].body).unwrap()
    }

    #[tokio::test]
    async fn successful_job_streams_trace_uploads_artifacts_saves_cache_and_cleans_up() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job = lifecycle_job(
            11,
            &[
                "mkdir -p out vendor",
                "echo built > out/app.txt",
                "echo dep > vendor/lib.txt",
                "echo secret=$SECRET",
            ],
            "on_success",
        );

        daemon.execute_job(job).await.unwrap();

        let update = final_update(&server, 11).await;
        assert_eq!(update["state"], "success");
        assert!(update.get("failure_reason").is_none());

        let trace = trace_of(&server, 11).await;
        assert!(trace.contains("$ echo built > out/app.txt"), "{}", trace);
        assert!(trace.contains("secret=[MASKED]"), "{}", trace);
        assert!(!trace.contains("s3cr3t-value"));
        assert!(trace.ends_with("Job succeeded\n"), "{}", trace);

        let uploads = requests(&server, "POST", 11).await;
        assert_eq!(uploads.len(), 1);
        assert!(String::from_utf8_lossy(&uploads[0].body).contains("filename=\"dist.zip\""));

        assert_eq!(daemon.cache.stats().0, 1, "cache archive saved");
        assert!(
            !dir.path().join("builds/job-11").exists(),
            "workspace removed"
        );
    }

    #[tokio::test]
    async fn failed_job_reports_reason_and_only_uploads_on_failure_artifacts() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;

        daemon
            .execute_job(lifecycle_job(
                21,
                &["mkdir -p out", "echo log > out/log.txt", "exit 2"],
                "on_success",
            ))
            .await
            .unwrap();
        daemon
            .execute_job(lifecycle_job(
                22,
                &["mkdir -p out", "echo log > out/log.txt", "exit 2"],
                "on_failure",
            ))
            .await
            .unwrap();

        for id in [21, 22] {
            let update = final_update(&server, id).await;
            assert_eq!(update["state"], "failed");
            assert_eq!(update["failure_reason"], "script_failure");
            assert_eq!(update["exit_code"], 2);
            assert!(trace_of(&server, id)
                .await
                .contains("ERROR: Job failed: exit code 2"));
        }
        assert!(
            requests(&server, "POST", 21).await.is_empty(),
            "on_success artifacts skipped"
        );
        assert_eq!(
            requests(&server, "POST", 22).await.len(),
            1,
            "on_failure artifacts uploaded"
        );
        assert_eq!(daemon.cache.stats().0, 0, "no cache saved for failed jobs");
    }

    #[tokio::test]
    async fn artifact_paths_and_names_use_variables() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 81, "token": "t",
            "variables": [{"key": "OUT", "value": "build/release", "public": true},
                          {"key": "FLAVOR", "value": "linux", "public": true}],
            "steps": [{"name": "script", "when": "on_success",
                       "script": ["mkdir -p build/release", "echo bin > build/release/app", "echo map > build/release/app.map"]}],
            "artifacts": [{"name": "app-$FLAVOR", "paths": ["$OUT/"], "exclude": ["$OUT/*.map"]}]
        }))
        .unwrap();

        daemon.execute_job(job).await.unwrap();

        assert_eq!(final_update(&server, 81).await["state"], "success");
        let uploads = requests(&server, "POST", 81).await;
        assert_eq!(uploads.len(), 1);
        let body = String::from_utf8_lossy(&uploads[0].body);
        assert!(
            body.contains("filename=\"app-linux.zip\""),
            "{}",
            &body[..200.min(body.len())]
        );
        assert!(body.contains("build/release/app"));
        assert!(!body.contains("app.map"));
    }

    #[tokio::test]
    async fn output_of_a_quiet_job_reaches_gitlab_while_it_runs() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 91, "token": "t",
            "steps": [{"name": "script", "when": "on_success",
                       "script": ["echo started-and-waiting", "sleep 8"]}]
        }))
        .unwrap();
        let running = tokio::spawn(async move { daemon.execute_job(job).await });

        let mut seen_at = None;
        for tenth in 0..70 {
            sleep(Duration::from_millis(100)).await;
            if trace_of(&server, 91).await.contains("started-and-waiting") {
                seen_at = Some(tenth);
                break;
            }
        }
        assert!(!running.is_finished(), "job ended before the check");
        assert!(
            seen_at.is_some(),
            "output held back while the job was quiet"
        );
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cache_fallback_key_restores_another_branch_cache() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let with_cache = |id: u64, key: &str, script: &str, vars: serde_json::Value| -> Job {
            serde_json::from_value(serde_json::json!({
                "id": id, "token": "t",
                "job_info": {"name": "j", "stage": "s", "project_id": 9, "project_name": "p"},
                "variables": vars,
                "steps": [{"name": "script", "script": [script], "when": "on_success"}],
                "cache": [{"key": key, "paths": ["vendor/"], "policy": "pull-push"}]
            }))
            .unwrap()
        };

        daemon
            .execute_job(with_cache(
                101,
                "main-deps",
                "mkdir -p vendor && echo from-main > vendor/x",
                serde_json::json!([]),
            ))
            .await
            .unwrap();
        daemon
            .execute_job(with_cache(
                102,
                "feature-deps",
                "cat vendor/x",
                serde_json::json!([{"key": "CACHE_FALLBACK_KEY", "value": "main-deps"}]),
            ))
            .await
            .unwrap();

        let trace = trace_of(&server, 102).await;
        assert!(
            trace.contains("Successfully restored cache main-deps"),
            "{}",
            trace
        );
        assert!(trace.contains("from-main"), "{}", trace);
    }

    #[tokio::test]
    async fn cache_fallback_key_cannot_reach_protected_caches() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let with_cache = |id: u64, key: &str, script: &str, vars: serde_json::Value| -> Job {
            serde_json::from_value(serde_json::json!({
                "id": id, "token": "t",
                "job_info": {"name": "j", "stage": "s", "project_id": 9, "project_name": "p"},
                "variables": vars,
                "steps": [{"name": "script", "script": [script], "when": "on_success"}],
                "cache": [{"key": key, "paths": ["vendor/"], "policy": "pull-push"}]
            }))
            .unwrap()
        };

        daemon
            .execute_job(with_cache(
                111,
                "deps-protected",
                "mkdir -p vendor && echo from-protected > vendor/x",
                serde_json::json!([]),
            ))
            .await
            .unwrap();
        daemon
            .execute_job(with_cache(
                112,
                "deps-non_protected",
                "cat vendor/x || echo nothing-restored",
                serde_json::json!([{"key": "CACHE_FALLBACK_KEY", "value": "deps-protected"}]),
            ))
            .await
            .unwrap();

        let trace = trace_of(&server, 112).await;
        assert!(
            trace.contains("ending in -protected is not allowed"),
            "{}",
            trace
        );
        assert!(!trace.contains("from-protected"), "{}", trace);
        assert!(trace.contains("nothing-restored"), "{}", trace);
    }

    #[tokio::test]
    async fn untracked_artifacts_contain_files_git_does_not_track() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let origin = dir.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("README"), "tracked").unwrap();
        std::fs::write(origin.join(".gitignore"), "*.log").unwrap();
        git(&origin, &["add", "README", ".gitignore"]);
        git(&origin, &["commit", "-q", "-m", "init"]);
        let sha = git(&origin, &["rev-parse", "HEAD"]);
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 111, "token": "t",
            "git_info": {"repo_url": format!("file://{}", origin.display()), "ref": "main",
                         "ref_type": "branch", "sha": sha, "before_sha": "",
                         "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]},
            "steps": [{"name": "script", "when": "on_success",
                       "script": ["mkdir -p sub", "echo out > build.out", "echo gen > 'sub/gen [1].txt'", "echo log > debug.log"]}],
            "artifacts": [{"name": "untracked", "untracked": true}]
        }))
        .unwrap();

        daemon.execute_job(job).await.unwrap();

        assert_eq!(final_update(&server, 111).await["state"], "success");
        let uploads = requests(&server, "POST", 111).await;
        assert_eq!(uploads.len(), 1, "{}", trace_of(&server, 111).await);
        let body = String::from_utf8_lossy(&uploads[0].body);
        assert!(body.contains("build.out"));
        assert!(
            body.contains("sub/gen [1].txt"),
            "glob characters in names are literal"
        );
        assert!(
            body.contains("debug.log"),
            "ignored files are untracked too"
        );
        assert!(!body.contains("README"));
    }

    /// Origin repository with one commit; its file:// URL and the commit
    fn origin_repo(dir: &std::path::Path) -> (String, String) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.join("origin"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        std::fs::create_dir(dir.join("origin")).unwrap();
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("origin/README"), "tracked").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-q", "-m", "init"]);
        let sha = git(&["rev-parse", "HEAD"]);
        (format!("file://{}", dir.join("origin").display()), sha)
    }

    fn checkout_job(id: u64, url: &str, sha: &str, protected: bool, script: &[&str]) -> Job {
        serde_json::from_value(serde_json::json!({
            "id": id, "token": "t", "allow_git_fetch": true,
            "job_info": {"name": "build", "stage": "test", "project_id": 5, "project_name": "app"},
            "git_info": {"repo_url": url, "ref": "main", "ref_type": "branch", "sha": sha,
                         "before_sha": "", "protected": protected,
                         "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]},
            "variables": [{"key": "DEPLOY_KEY", "value": "key-content", "file": true}],
            "steps": [{"name": "script", "when": "on_success", "timeout": 60, "script": script}]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn the_next_job_of_a_project_fetches_into_its_workspace() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let (url, sha) = origin_repo(dir.path());
        let workspace = dir.path().join("builds/project-5-0");

        let first = checkout_job(
            301,
            &url,
            &sha,
            false,
            &["echo x > leftover", "cat \"$DEPLOY_KEY\""],
        );
        daemon.execute_job(first).await.unwrap();
        assert_eq!(final_update(&server, 301).await["state"], "success");
        assert!(workspace.join("project/.git").is_dir());
        assert!(
            !workspace.join("project.tmp").exists(),
            "file variables are removed when the job ends"
        );

        let second = checkout_job(
            302,
            &url,
            &sha,
            false,
            &["test ! -e leftover", "cat README"],
        );
        daemon.execute_job(second).await.unwrap();
        let trace = trace_of(&server, 302).await;
        assert_eq!(
            final_update(&server, 302).await["state"],
            "success",
            "{}",
            trace
        );
        assert!(
            trace.contains("Reusing the checkout of an earlier job"),
            "{}",
            trace
        );

        // A protected ref never gets an unprotected ref's workspace
        let protected = checkout_job(303, &url, &sha, true, &["test ! -e leftover"]);
        daemon.execute_job(protected).await.unwrap();
        let trace = trace_of(&server, 303).await;
        assert!(!trace.contains("Reusing"), "{}", trace);
        assert!(dir
            .path()
            .join("builds/project-5-0-protected/project")
            .is_dir());
    }

    #[tokio::test]
    async fn workspaces_are_removed_after_each_job_when_not_kept() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon_with(&server, dir.path(), |config| {
            config.executor.workspace_max_age_days = 0;
        })
        .await;
        let (url, sha) = origin_repo(dir.path());

        daemon
            .execute_job(checkout_job(311, &url, &sha, false, &["true"]))
            .await
            .unwrap();
        assert_eq!(final_update(&server, 311).await["state"], "success");
        assert!(!dir.path().join("builds/project-5-0").exists());
    }

    #[tokio::test]
    async fn file_variables_are_found_with_a_custom_clone_path() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let (url, sha) = origin_repo(dir.path());
        let mut job = checkout_job(
            321,
            &url,
            &sha,
            false,
            &["grep -q key-content \"$DEPLOY_KEY\""],
        );
        job.variables.push(
            serde_json::from_value(serde_json::json!(
                {"key": "GIT_CLONE_PATH", "value": "$CI_BUILDS_DIR/group/app"}
            ))
            .unwrap(),
        );

        daemon.execute_job(job).await.unwrap();
        let trace = trace_of(&server, 321).await;
        assert_eq!(
            final_update(&server, 321).await["state"],
            "success",
            "{}",
            trace
        );
    }

    #[tokio::test]
    async fn rotates_the_runner_token_before_it_expires() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let expires = chrono::Utc::now() + chrono::Duration::seconds(2);
        Mock::given(method("POST"))
            .and(path("/api/v4/runners/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "glrt-test", "token_expires_at": expires.to_rfc3339()
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v4/runners/reset_authentication_token"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"token": "glrt-test"}),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "glrt-rotated", "token_expires_at": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        let runner = daemon(&server, dir.path()).await;

        tokio::time::timeout(Duration::from_secs(10), runner.clone().rotate_token())
            .await
            .expect("rotation finishes once the new token does not expire");

        assert_eq!(runner.current_token(), "glrt-rotated");
        // A restart picks up the rotated token instead of the config's old one
        let restarted = daemon(&server, dir.path()).await;
        assert_eq!(restarted.current_token(), "glrt-rotated");
        assert!(restarted
            .scrubber
            .read()
            .unwrap()
            .scrub("x glrt-rotated")
            .contains("[MASKED]"));
    }

    #[tokio::test]
    async fn runner_environment_and_hooks_apply_to_jobs() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut runner = daemon(&server, dir.path()).await;
        let mut config = (*runner.config).clone();
        config.environment = vec!["FROM_RUNNER=yes".into(), "OVERRIDDEN=runner".into()];
        config.pre_get_sources_script = Some("echo pre-sources".into());
        config.post_get_sources_script = Some("echo post-sources".into());
        config.pre_build_script = Some("echo pre-build".into());
        config.post_build_script = Some("echo post-build".into());
        runner.config = Arc::new(config);
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 131, "token": "t",
            "variables": [{"key": "OVERRIDDEN", "value": "job"}],
            "steps": [{"name": "script", "when": "on_success",
                       "before_script": ["echo before"],
                       "script": ["echo env=$FROM_RUNNER/$OVERRIDDEN"]}]
        }))
        .unwrap();

        runner.execute_job(job).await.unwrap();

        let trace = trace_of(&server, 131).await;
        let order: Vec<usize> = [
            "pre-sources",
            "post-sources",
            "pre-build",
            "before",
            "env=yes/runner",
            "post-build",
        ]
        .iter()
        .map(|text| {
            trace
                .find(&format!("\n{}", text))
                .unwrap_or_else(|| panic!("{} missing:\n{}", text, trace))
        })
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{}", trace);
    }

    #[tokio::test]
    async fn proxy_reaches_jobs_and_yields_to_job_variables() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut runner = daemon(&server, dir.path()).await;
        let mut config = (*runner.config).clone();
        config.proxy = Some("http://proxy.corp:3128".into());
        config.no_proxy = Some(".corp.local".into());
        runner.config = Arc::new(config);
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 132, "token": "t",
            "variables": [{"key": "https_proxy", "value": "http://job-proxy:8080"}],
            "services": [{"name": "docker:27-dind", "alias": "docker"}],
            "steps": [{"name": "script", "when": "on_success", "script": [
                "echo \"http=$HTTP_PROXY lower=$http_proxy\"",
                "echo \"https=$HTTPS_PROXY job=$https_proxy\"",
                "echo \"no=$NO_PROXY\""
            ]}]
        }))
        .unwrap();

        runner.execute_job(job).await.unwrap();

        let trace = trace_of(&server, 132).await;
        assert!(
            trace.contains("http=http://proxy.corp:3128 lower=http://proxy.corp:3128"),
            "{}",
            trace
        );
        assert!(
            trace.contains("https=http://proxy.corp:3128 job=http://job-proxy:8080"),
            "{}",
            trace
        );
        // docker:27-dind with alias "docker" is reachable only as "docker"
        assert!(
            trace.contains("no=.corp.local,localhost,127.0.0.1,::1,docker\n"),
            "{}",
            trace
        );
    }

    #[tokio::test]
    async fn proxy_with_credentials_is_masked_in_job_logs() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        // Mounts the GitLab mocks; the runner under test is built below because
        // the scrubber is filled in RunnerDaemon::new
        let base = daemon(&server, dir.path()).await;
        let mut config = (*base.config).clone();
        config.proxy = Some("http://user:s3cr3t-pass@proxy.corp:3128".into());
        let runner = RunnerDaemon::new(
            config,
            GitLabClient::new(server.uri(), "glrt-test".to_string()),
            executor::ExecutorType::Shell(executor::ShellExecutor::new(Some(
                dir.path().join("builds").to_string_lossy().into_owned(),
            ))),
        );
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 133, "token": "t",
            "steps": [{"name": "script", "when": "on_success", "script": ["echo \"p=$HTTP_PROXY\""]}]
        }))
        .unwrap();

        runner.execute_job(job).await.unwrap();

        let trace = trace_of(&server, 133).await;
        assert!(!trace.contains("s3cr3t-pass"), "{}", trace);
        assert!(trace.contains("p=[MASKED]"), "{}", trace);
    }

    #[test]
    fn runner_proxy_comes_before_job_variables_and_environment_after() {
        let mut job: Job = serde_json::from_value(serde_json::json!({
            "id": 1, "token": "t",
            "variables": [{"key": "JOB", "value": "x"}]
        }))
        .unwrap();
        let config = config::RunnerConfig {
            proxy: Some("http://proxy:3128".to_string()),
            environment: vec!["ENV = y".to_string()],
            ..config::RunnerConfig::default()
        };

        apply_runner_config(&mut job, &config);

        let keys: Vec<&str> = job.variables.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "no_proxy",
                "NO_PROXY",
                "https_proxy",
                "http_proxy",
                "HTTPS_PROXY",
                "HTTP_PROXY",
                "JOB",
                "ENV"
            ]
        );
        assert!(job.variables[..6].iter().all(|v| v.raw && v.internal));
        assert_eq!(job.variables[7].value.as_deref(), Some(" y"));
        assert!(!job.variables[7].raw);
    }

    #[test]
    fn short_token_skips_prefixes() {
        assert_eq!(short_token("glrt-t1_AbCdEfGhIjKl"), "AbCdEfGhI");
        assert_eq!(short_token("glrt-AbCdEfGhIjKl"), "AbCdEfGhI");
        assert_eq!(short_token("t2_xyz"), "xyz");
        assert!(executable_arch().contains('/'));
        assert!(!executable_arch().starts_with("macos/"));
    }

    #[test]
    fn concurrency_slots_reuse_the_lowest_free_ids() {
        let mut slots = ConcurrencySlots::default();
        let a = slots.acquire(1);
        let b = slots.acquire(1);
        let c = slots.acquire(2);
        assert_eq!((a, b, c), ((0, 0), (1, 1), (2, 0)));

        slots.release(1, a);
        assert_eq!(slots.acquire(3), (0, 0));
        assert_eq!(slots.acquire(1), (3, 0));
    }

    #[tokio::test]
    async fn jobs_get_concurrency_ids() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 121, "token": "t",
            "steps": [{"name": "script", "when": "on_success",
                       "script": ["echo ids=$CI_CONCURRENT_ID/$CI_CONCURRENT_PROJECT_ID runner=$CI_RUNNER_VERSION/$CI_RUNNER_SHORT_TOKEN"]}]
        }))
        .unwrap();

        daemon.execute_job(job).await.unwrap();

        let trace = trace_of(&server, 121).await;
        assert!(trace.contains("ids=0/0"), "{}", trace);
        assert!(
            trace.contains(&format!("runner={}/test", env!("CARGO_PKG_VERSION"))),
            "{}",
            trace
        );
    }

    #[tokio::test]
    async fn failed_artifact_upload_fails_the_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(
                r"^/api/v4/jobs/\d+/artifacts$",
            ))
            .respond_with(ResponseTemplate::new(413))
            .with_priority(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;

        daemon
            .execute_job(lifecycle_job(
                61,
                &["mkdir -p out", "echo x > out/a.txt"],
                "on_success",
            ))
            .await
            .unwrap();

        let update = final_update(&server, 61).await;
        assert_eq!(update["state"], "failed");
        assert_eq!(update["failure_reason"], "script_failure");
        assert!(trace_of(&server, 61)
            .await
            .contains("ERROR: Uploading artifacts failed"));
    }

    #[tokio::test]
    async fn dependency_artifacts_are_required_only_when_they_exist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v4/jobs/5/artifacts"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let with_deps = |id: u64, deps: serde_json::Value| -> Job {
            serde_json::from_value(serde_json::json!({
                "id": id, "token": "t",
                "steps": [{"name": "script", "script": ["echo script-ran"], "when": "on_success"}],
                "dependencies": deps
            }))
            .unwrap()
        };

        // A dependency without artifacts is skipped silently
        daemon
            .execute_job(with_deps(
                71,
                serde_json::json!([{"id": 4, "name": "lint", "token": "d"}]),
            ))
            .await
            .unwrap();
        assert_eq!(final_update(&server, 71).await["state"], "success");
        assert!(requests(&server, "GET", 4).await.is_empty());

        // A failed download of existing artifacts fails the job before its script
        daemon
            .execute_job(with_deps(
                72,
                serde_json::json!([{"id": 5, "name": "build", "token": "d",
                "artifacts_file": {"filename": "artifacts.zip", "size": 10}}]),
            ))
            .await
            .unwrap();
        assert_eq!(final_update(&server, 72).await["state"], "failed");
        let trace = trace_of(&server, 72).await;
        assert!(
            trace.contains("failed to download artifacts from job #5"),
            "{}",
            trace
        );
        assert!(!trace.contains("\nscript-ran"), "{}", trace);
    }

    #[tokio::test]
    async fn next_job_restores_cache_of_previous_job() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;

        daemon
            .execute_job(lifecycle_job(
                31,
                &["mkdir -p vendor", "echo cached-dep > vendor/lib.txt"],
                "on_success",
            ))
            .await
            .unwrap();
        daemon
            .execute_job(lifecycle_job(32, &["cat vendor/lib.txt"], "on_success"))
            .await
            .unwrap();

        assert_eq!(final_update(&server, 32).await["state"], "success");
        let trace = trace_of(&server, 32).await;
        assert!(
            trace.contains("Successfully restored cache deps"),
            "{}",
            trace
        );
        assert!(trace.contains("cached-dep"), "{}", trace);
    }

    /// A pull-push cache the job did not touch is neither archived nor
    /// uploaded again; one it changed (same size) is saved
    #[tokio::test]
    async fn an_unchanged_pull_push_cache_is_not_saved_again() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        daemon
            .execute_job(lifecycle_job(
                41,
                &["mkdir -p vendor", "echo cached-dep > vendor/lib.txt"],
                "on_success",
            ))
            .await
            .unwrap();

        daemon
            .execute_job(lifecycle_job(42, &["cat vendor/lib.txt"], "on_success"))
            .await
            .unwrap();
        let unchanged = trace_of(&server, 42).await;
        assert!(
            unchanged.contains("Cache deps unchanged since restore: not saved"),
            "{}",
            unchanged
        );
        assert!(!unchanged.contains("Created cache"), "{}", unchanged);

        daemon
            .execute_job(lifecycle_job(
                43,
                &["echo changed-dp > vendor/lib.txt"],
                "on_success",
            ))
            .await
            .unwrap();
        let changed = trace_of(&server, 43).await;
        assert!(changed.contains("Created cache deps"), "{}", changed);
        daemon
            .execute_job(lifecycle_job(44, &["cat vendor/lib.txt"], "on_success"))
            .await
            .unwrap();
        assert!(trace_of(&server, 44).await.contains("changed-dp"));
    }

    #[tokio::test]
    async fn s3_errors_are_job_log_warnings_and_the_local_cache_is_used() {
        let server = MockServer::start().await;
        let s3 = MockServer::start().await;
        Mock::given(wiremock::matchers::path_regex(
            r"^/ci/project-5/deps\.(tar\.zst|zip)$",
        ))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("<Error><Code>AccessDenied</Code></Error>"),
        )
        .mount(&s3)
        .await;
        let bucket = s3::Bucket::new(
            &config::S3CacheConfig {
                url: format!("{}/ci", s3.uri()),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::new(),
        )
        .unwrap()
        .with_retry_delay(Duration::from_millis(10));
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await.with_s3_cache(bucket);

        daemon
            .execute_job(lifecycle_job(
                33,
                &["mkdir -p vendor", "echo cached-dep > vendor/lib.txt"],
                "on_success",
            ))
            .await
            .unwrap();
        daemon
            .execute_job(lifecycle_job(34, &["cat vendor/lib.txt"], "on_success"))
            .await
            .unwrap();

        assert_eq!(final_update(&server, 33).await["state"], "success");
        let saving = trace_of(&server, 33).await;
        assert!(
            saving.contains("WARNING: cache deps saved locally, not uploaded to S3: S3 answered 403 Forbidden (AccessDenied)"),
            "{}",
            saving
        );
        assert_eq!(final_update(&server, 34).await["state"], "success");
        let restoring = trace_of(&server, 34).await;
        assert!(
            restoring.contains(
                "WARNING: S3 unavailable for cache deps: S3 answered 403 Forbidden (AccessDenied)"
            ),
            "{}",
            restoring
        );
        assert!(
            restoring.contains("Successfully restored cache deps (local copy)"),
            "{}",
            restoring
        );
        assert!(restoring.contains("cached-dep"), "{}", restoring);
    }

    #[tokio::test]
    async fn job_canceled_in_gitlab_stops_and_runs_after_script() {
        let server = MockServer::start().await;
        // Keep-alive updates learn that the job is being canceled
        Mock::given(method("PUT"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"state": "running"}),
            ))
            .respond_with(ResponseTemplate::new(200).insert_header("Job-Status", "canceling"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = daemon(&server, dir.path()).await;
        daemon.heartbeat_interval = Duration::from_millis(200);
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 41, "token": "job-token-41",
            "steps": [
                {"name": "script", "script": ["sleep 30"], "when": "on_success", "timeout": 60},
                {"name": "after_script", "script": ["echo cleanup-ran"], "when": "always",
                 "allow_failure": true, "timeout": 10}
            ]
        }))
        .unwrap();

        let started = std::time::Instant::now();
        daemon.execute_job(job).await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        let trace = trace_of(&server, 41).await;
        assert!(trace.contains("cleanup-ran"), "{}", trace);
        assert!(trace.contains("ERROR: Job failed: canceled"), "{}", trace);
    }

    #[tokio::test]
    async fn abort_shutdown_fails_running_job_as_runner_failure() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job = lifecycle_job(51, &["sleep 30"], "always");
        let handle = daemon.shutdown_handle();
        tokio::spawn(async move {
            sleep(Duration::from_millis(500)).await;
            handle.request(Shutdown::Abort);
        });

        let started = std::time::Instant::now();
        daemon.execute_job(job).await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        let update = final_update(&server, 51).await;
        assert_eq!(update["state"], "failed");
        assert_eq!(update["failure_reason"], "runner_system_failure");
        assert!(trace_of(&server, 51)
            .await
            .contains("the runner is shutting down"));
        assert!(
            requests(&server, "POST", 51).await.is_empty(),
            "aborted jobs upload nothing, even artifacts with when: always"
        );
    }

    #[tokio::test]
    async fn an_abort_stops_saving_the_cache() {
        let server = MockServer::start().await;
        let s3 = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s3)
            .await;
        // S3 takes the upload but answers only after 10 s
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .mount(&s3)
            .await;
        let bucket = s3::Bucket::new(
            &config::S3CacheConfig {
                url: format!("{}/ci", s3.uri()),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::new(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await.with_s3_cache(bucket);
        let handle = daemon.shutdown_handle();
        let job = lifecycle_job(
            52,
            &["mkdir -p vendor", "echo dep > vendor/lib.txt"],
            "on_success",
        );
        // Abort once the upload has reached S3
        let abort = async {
            while !s3
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.method.as_str() == "PUT")
            {
                sleep(Duration::from_millis(20)).await;
            }
            handle.request(Shutdown::Abort);
        };

        let started = std::time::Instant::now();
        let (executed, _) = tokio::join!(
            daemon.execute_job(job),
            tokio::time::timeout(Duration::from_secs(10), abort)
        );
        executed.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let trace = trace_of(&server, 52).await;
        assert!(trace.contains("Cache not saved: job canceled"), "{}", trace);
        assert!(!trace.contains("Uploaded cache"), "{}", trace);
    }

    #[tokio::test]
    async fn an_abort_stops_uploading_artifacts() {
        let server = MockServer::start().await;
        // GitLab takes the upload but answers only after 10 s
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(
                r"^/api/v4/jobs/\d+/artifacts$",
            ))
            .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(10)))
            .with_priority(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let handle = daemon.shutdown_handle();
        let job = lifecycle_job(53, &["mkdir -p out", "echo x > out/a.txt"], "on_success");
        // Abort once the upload has reached GitLab
        let abort = async {
            while requests(&server, "POST", 53).await.is_empty() {
                sleep(Duration::from_millis(20)).await;
            }
            handle.request(Shutdown::Abort);
        };

        let started = std::time::Instant::now();
        let (executed, _) = tokio::join!(
            daemon.execute_job(job),
            tokio::time::timeout(Duration::from_secs(10), abort)
        );
        executed.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let trace = trace_of(&server, 53).await;
        assert!(
            trace.contains("Artifacts not uploaded: job canceled"),
            "{}",
            trace
        );
        let update = final_update(&server, 53).await;
        assert_eq!(update["state"], "failed");
        assert_eq!(update["failure_reason"], "runner_system_failure");
    }

    #[tokio::test]
    async fn start_returns_after_shutdown_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let handle = daemon.shutdown_handle();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            handle.request(Shutdown::Graceful);
        });

        tokio::time::timeout(Duration::from_secs(5), daemon.start())
            .await
            .expect("daemon stopped")
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_ends_a_job_request_gitlab_holds() {
        let server = MockServer::start().await;
        // Long polling: GitLab answers only after a while
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let handle = daemon.shutdown_handle();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            handle.request(Shutdown::Abort);
        });

        tokio::time::timeout(Duration::from_secs(5), daemon.start())
            .await
            .expect("daemon stopped while the job request was held")
            .unwrap();
    }

    #[tokio::test]
    async fn a_job_claims_its_workspace_only_after_a_running_eviction() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let workspace = dir.path().join("builds/project-5-0");
        let in_use = |daemon: &RunnerDaemon| {
            daemon
                .workspaces_in_use
                .lock()
                .unwrap()
                .contains(&workspace)
        };

        let eviction = daemon.evicting.clone().write_owned().await;
        let claim = tokio::spawn({
            let (daemon, workspace) = (daemon.clone(), workspace.clone());
            async move { daemon.claim_workspace(&workspace).await }
        });
        sleep(Duration::from_millis(100)).await;
        assert!(!claim.is_finished(), "claimed during an eviction");
        assert!(!in_use(&daemon));

        drop(eviction);
        tokio::time::timeout(Duration::from_secs(5), claim)
            .await
            .expect("claimed once the eviction ended")
            .unwrap();
        assert!(in_use(&daemon));
    }

    #[tokio::test]
    async fn no_job_is_requested_while_the_host_is_short_of_resources() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(admission::FixedProbe::default());
        probe.set(None, None, Some(1 << 20));
        let daemon = daemon_with(&server, dir.path(), |config| {
            config.check_interval = 1;
            config.min_free_disk = Some("1g".to_string());
        })
        .await
        .with_probe(probe.clone());
        let job_requests = || async {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == "/api/v4/jobs/request")
                .count()
        };
        let handle = daemon.shutdown_handle();
        let running = tokio::spawn(async move { daemon.start().await });

        sleep(Duration::from_millis(1500)).await;
        assert_eq!(job_requests().await, 0, "requested a job with a full disk");

        probe.set(None, None, Some(5 << 30));
        sleep(Duration::from_millis(1500)).await;
        assert!(
            job_requests().await > 0,
            "no request once the disk had space"
        );

        handle.request(Shutdown::Graceful);
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("daemon stopped")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn jobs_and_cache_restores_are_counted() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let script = ["mkdir -p out vendor", "echo x > out/a", "echo y > vendor/b"];

        daemon
            .execute_job(lifecycle_job(61, &script, "on_success"))
            .await
            .unwrap();
        daemon
            .execute_job(lifecycle_job(62, &script, "on_success"))
            .await
            .unwrap();
        daemon
            .execute_job(lifecycle_job(63, &["exit 3"], "on_success"))
            .await
            .unwrap();

        let text = daemon.render_metrics();
        for line in [
            "turboci_jobs_finished_total{runner=\"default\",result=\"success\"} 2",
            "turboci_jobs_finished_total{runner=\"default\",result=\"failed\"} 1",
            "turboci_jobs_failed_total{runner=\"default\",reason=\"script_failure\"} 1",
            // The first job found no cache, the next two the first one's
            "turboci_cache_restores_total{runner=\"default\",result=\"miss\"} 1",
            "turboci_cache_restores_total{runner=\"default\",result=\"hit\"} 2",
            "turboci_jobs_running{runner=\"default\"} 0",
            "turboci_concurrent_jobs 4",
            "turboci_admission_blocked{runner=\"default\",resource=\"disk\"} 0",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {:?} in\n{}",
                line,
                text
            );
        }
        assert!(text.contains("executor=\"shell\"} 1"), "{}", text);
    }

    #[tokio::test]
    async fn a_taken_metrics_port_stops_the_runner_at_start() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap().to_string();
        let daemon = daemon_with(&server, dir.path(), |config| {
            config.metrics_listen = Some(addr.clone())
        })
        .await;

        let error = tokio::time::timeout(Duration::from_secs(5), daemon.start())
            .await
            .expect("start returned")
            .unwrap_err();
        assert!(
            format!("{:#}", error).contains("metrics_listen"),
            "{:#}",
            error
        );
    }

    /// A daemon with the shell executor, started from the config file `file`
    fn daemon_from_file(
        server: &MockServer,
        dir: &std::path::Path,
        file: &std::path::Path,
    ) -> RunnerDaemon {
        let config = config::RunnerConfig::load(file).unwrap();
        let executor = executor::ExecutorType::Shell(executor::ShellExecutor::new(Some(
            dir.join("builds").to_string_lossy().into_owned(),
        )));
        RunnerDaemon::new(
            config,
            GitLabClient::new(server.uri(), "glrt-test".to_string()),
            executor,
        )
    }

    #[tokio::test]
    async fn a_reload_applies_live_settings_and_reports_the_rest() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("runner.toml");
        let write = |settings: &str| {
            std::fs::write(
                &file,
                format!(
                    "runner_token = \"glrt-test\"\ngitlab_url = \"{}\"\nstate_dir = \"{}\"\ncache_dir = \"{}/cache\"\n{}\n",
                    server.uri(),
                    dir.path().display(),
                    dir.path().display(),
                    settings
                ),
            )
            .unwrap()
        };
        write("concurrent = 4\ncheck_interval = 3");
        let daemon = daemon_from_file(&server, dir.path(), &file);
        let running = [
            daemon.capacity.acquire().await,
            daemon.capacity.acquire().await,
        ];

        write("concurrent = 1\ncheck_interval = 7\nmin_free_disk = \"1g\"\ncache_max_age_days = 3\nmetrics_listen = \"127.0.0.1:0\"\ncache_enabled = false");
        let reloaded = daemon.reload(&file).unwrap();
        assert_eq!(
            reloaded.applied,
            [
                "concurrent 4 → 1",
                "check_interval 3s → 7s",
                "admission limits off → min_free_disk 1.0 GB",
                "for new jobs: cache_max_age_days",
            ]
        );
        assert_eq!(reloaded.restart_needed, ["metrics_listen", "cache_enabled"]);
        assert_eq!(daemon.capacity.shared().limit(), 1);
        assert_eq!(daemon.check_interval(), Duration::from_secs(7));
        assert_eq!(daemon.current_limits().min_free_disk, Some(1 << 30));
        // Jobs running above the new limit keep their slots
        assert_eq!(daemon.capacity.running(), 2);
        drop(running);

        // The same file again changes nothing
        let again = daemon.reload(&file).unwrap();
        assert!(again.applied.is_empty(), "{:?}", again.applied);
    }

    #[tokio::test]
    async fn a_reload_gives_new_jobs_new_settings_and_running_jobs_keep_theirs() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("runner.toml");
        let write = |settings: &str| {
            std::fs::write(
                &file,
                format!(
                    "runner_token = \"glrt-test\"\ngitlab_url = \"{}\"\nstate_dir = \"{}\"\ncache_dir = \"{}/cache\"\n{}\n",
                    server.uri(),
                    dir.path().display(),
                    dir.path().display(),
                    settings
                ),
            )
            .unwrap()
        };
        write("environment = [\"STAGE=old\"]\n[executor]\nexecutor_type = \"shell\"");
        job_routes(&server).await;
        let daemon = daemon_from_file(&server, dir.path(), &file);
        // What a job that started before the reload runs with
        let started = daemon.current();

        write(
            "environment = [\"STAGE=new\"]\npre_build_script = \"echo hook ran\"\ncache_max_age_days = 3\nproxy = \"http://proxy.corp:3128\"\n[executor]\nexecutor_type = \"shell\"\nworkspace_max_age_days = 0\n[executor.shell]\nwork_dir = \"/elsewhere\"",
        );
        let reloaded = daemon.reload(&file).unwrap();
        assert_eq!(
            reloaded.applied,
            ["for new jobs: cache_max_age_days, environment, executor.workspace_max_age_days, pre_build_script"]
        );
        assert_eq!(
            reloaded.restart_needed,
            ["executor.shell.work_dir", "proxy"]
        );

        let next = daemon.current();
        assert_eq!(next.config.environment, ["STAGE=new"]);
        // Settings needing a restart keep their running values
        assert_eq!(next.config.proxy, None);
        assert_eq!(
            next.config.executor.shell.work_dir,
            started.config.executor.shell.work_dir
        );
        assert_eq!(started.config.environment, ["STAGE=old"]);

        let script = ["echo \"stage=$STAGE\""];
        next.execute_job(lifecycle_job(91, &script, "on_success"))
            .await
            .unwrap();
        let trace = trace_of(&server, 91).await;
        assert!(
            trace.contains("stage=new") && trace.contains("hook ran"),
            "{}",
            trace
        );

        started
            .execute_job(lifecycle_job(92, &script, "on_success"))
            .await
            .unwrap();
        let trace = trace_of(&server, 92).await;
        assert!(
            trace.contains("stage=old") && !trace.contains("hook ran"),
            "{}",
            trace
        );

        // Reloading the same file again changes nothing more
        let again = daemon.reload(&file).unwrap();
        assert!(again.applied.is_empty(), "{:?}", again.applied);
        assert_eq!(again.restart_needed, reloaded.restart_needed);
    }

    #[tokio::test]
    async fn an_invalid_config_on_reload_keeps_the_running_one() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("runner.toml");
        let write = |settings: &str| {
            std::fs::write(
                &file,
                format!(
                    "runner_token = \"glrt-test\"\ngitlab_url = \"{}\"\nstate_dir = \"{}\"\n{}\n",
                    server.uri(),
                    dir.path().display(),
                    settings
                ),
            )
            .unwrap()
        };
        write("concurrent = 2\ncheck_interval = 5\nmax_load_avg = 4.0");
        let daemon = daemon_from_file(&server, dir.path(), &file);
        let reloader = daemon.reloader(&file);

        // Each file also changes valid live settings: none may be applied
        for bad in [
            "concurrent = 0\ncheck_interval = 30",
            "concurrent = 3\ncheck_interval = \"soon\"",
            "concurrent = 3\ncheck_interval = 30\nmin_free_memory = \"lots\"",
            "concurrent = 3\ncheck_interval = 30\nmax_load_avg = -1.0",
            "concurrent = 3\ncheck_interval = 30\n[[[",
        ] {
            write(bad);
            assert!(daemon.reload(&file).is_err(), "accepted {}", bad);
            reloader.reload();
            assert_eq!(daemon.capacity.shared().limit(), 2, "{}", bad);
            assert_eq!(daemon.check_interval(), Duration::from_secs(5), "{}", bad);
            assert_eq!(daemon.current_limits().max_load_avg, Some(4.0), "{}", bad);
        }
        std::fs::remove_file(&file).unwrap();
        assert!(daemon.reload(&file).is_err());
        assert!(daemon
            .render_metrics()
            .contains("turboci_config_reloads_total{result=\"failure\"} 5\n"));
    }

    /// The runners of the config file `file` with shell executors, as
    /// `runner-start` builds them
    fn manager_from_file(file: &std::path::Path) -> runners::Manager {
        let config = config::ConfigFile::load(file).unwrap();
        runners::Manager::new(
            config
                .runners
                .into_iter()
                .map(|runner| {
                    let executor = executor::ExecutorType::Shell(executor::ShellExecutor::new(
                        Some(runner.executor.shell.work_dir.clone()),
                    ));
                    let gitlab =
                        GitLabClient::new(runner.gitlab_url.clone(), runner.runner_token.clone());
                    RunnerDaemon::new(runner, gitlab, executor)
                })
                .collect(),
        )
    }

    /// A config with runners `a` and `b` of the GitLab servers `a` and `b`;
    /// `b_settings` go into runner b's entry, `more` at the end of the file
    fn two_runners(
        dir: &std::path::Path,
        a: &MockServer,
        b: &MockServer,
        global: &str,
        b_settings: &str,
        more: &str,
    ) -> String {
        format!(
            "{global}\ncheck_interval = 1\nstate_dir = \"{dir}\"\n\n\
             [[runners]]\nname = \"a\"\nrunner_token = \"glrt-a\"\ngitlab_url = \"{a}\"\n\
             [runners.executor]\nexecutor_type = \"shell\"\n\n\
             [[runners]]\nname = \"b\"\nrunner_token = \"glrt-b\"\ngitlab_url = \"{b}\"\n{b_settings}\n\
             [runners.executor]\nexecutor_type = \"shell\"\n{more}",
            dir = dir.display(),
            a = a.uri(),
            b = b.uri(),
        )
    }

    #[tokio::test]
    async fn several_runners_take_jobs_from_their_own_gitlab_in_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (MockServer::start().await, MockServer::start().await);
        for (server, id) in [(&a, 71), (&b, 72)] {
            job_routes(server).await;
            let job = serde_json::json!({
                "id": id, "token": format!("job-token-{}", id),
                "job_info": {"name": "build", "stage": "test", "project_id": 5, "project_name": "app"},
                "steps": [{"name": "script", "script": ["echo side=$SIDE."], "when": "on_success", "timeout": 60}]
            });
            Mock::given(method("POST"))
                .and(path("/api/v4/jobs/request"))
                .respond_with(ResponseTemplate::new(201).set_body_json(job))
                .up_to_n_times(1)
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v4/jobs/request"))
                .respond_with(ResponseTemplate::new(204))
                .mount(server)
                .await;
        }
        let file = dir.path().join("runner.toml");
        std::fs::write(
            &file,
            two_runners(
                dir.path(),
                &a,
                &b,
                "concurrent = 2",
                "environment = [\"SIDE=b\"]",
                "",
            ),
        )
        .unwrap();
        let manager = manager_from_file(&file);
        let handle = manager.shutdown_handle();
        let running = tokio::spawn({
            let manager = manager.clone();
            async move { manager.start().await }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while requests(&a, "PUT", 71).await.is_empty() || requests(&b, "PUT", 72).await.is_empty() {
            assert!(std::time::Instant::now() < deadline, "jobs did not finish");
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(final_update(&a, 71).await["state"], "success");
        assert_eq!(final_update(&b, 72).await["state"], "success");
        // Each runner asks its own GitLab with its own token
        for (server, token) in [(&a, "glrt-a"), (&b, "glrt-b")] {
            for request in server.received_requests().await.unwrap() {
                if request.url.path() == "/api/v4/jobs/request" {
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(body["token"], token);
                }
            }
        }
        // Runner settings stay with their runner
        assert!(trace_of(&a, 71).await.contains("side=."));
        assert!(trace_of(&b, 72).await.contains("side=b."));
        let text = manager.render_metrics();
        for line in [
            "turboci_jobs_finished_total{runner=\"a\",result=\"success\"} 1",
            "turboci_jobs_finished_total{runner=\"b\",result=\"success\"} 1",
            "turboci_concurrent_jobs 2",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {:?} in\n{}",
                line,
                text
            );
        }

        handle.request(Shutdown::Graceful);
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("every runner stopped")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_reload_applies_each_runners_settings_and_reports_added_runners() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (MockServer::start().await, MockServer::start().await);
        let file = dir.path().join("runner.toml");
        let write = |global: &str, b_settings: &str, more: &str| {
            std::fs::write(
                &file,
                two_runners(dir.path(), &a, &b, global, b_settings, more),
            )
            .unwrap()
        };
        write("concurrent = 2", "environment = [\"SIDE=b\"]", "");
        let manager = manager_from_file(&file);
        let [first, second] = &manager.runners[..] else {
            panic!("two runners expected")
        };
        // The runners share the process's slots and shutdown signal
        assert!(Arc::ptr_eq(
            &first.capacity.shared(),
            &second.capacity.shared()
        ));
        assert!(Arc::ptr_eq(&first.shutdown, &second.shutdown));
        assert!(!Arc::ptr_eq(&first.capacity, &second.capacity));

        write(
            "concurrent = 3",
            "limit = 1\nenvironment = [\"SIDE=new\"]",
            &format!(
                "\n[[runners]]\nname = \"c\"\nrunner_token = \"glrt-c\"\ngitlab_url = \"{}\"\n",
                a.uri()
            ),
        );
        let reloaded = manager.reload(&file).unwrap();
        assert_eq!(
            reloaded.applied,
            [
                "concurrent 2 → 3",
                "b: limit 0 → 1",
                "b: for new jobs: environment"
            ]
        );
        assert_eq!(reloaded.restart_needed, ["[[runners]] c (added)"]);
        assert_eq!(first.capacity.shared().limit(), 3);
        assert_eq!(second.capacity.limit(), 1);
        assert_eq!(first.capacity.limit(), u32::MAX);
        assert_eq!(second.current().config.environment, ["SIDE=new"]);
        assert_eq!(first.current().config.environment, Vec::<String>::new());

        // Removing a runner needs a restart too; it keeps running until then
        std::fs::write(
            &file,
            format!(
                "concurrent = 3\ncheck_interval = 1\nstate_dir = \"{}\"\n[[runners]]\nname = \"a\"\nrunner_token = \"glrt-a\"\ngitlab_url = \"{}\"\n[runners.executor]\nexecutor_type = \"shell\"\n",
                dir.path().display(),
                a.uri()
            ),
        )
        .unwrap();
        let reloaded = manager.reload(&file).unwrap();
        assert!(reloaded.applied.is_empty(), "{:?}", reloaded.applied);
        assert_eq!(reloaded.restart_needed, ["[[runners]] b (removed)"]);
        assert_eq!(second.current().config.environment, ["SIDE=new"]);
    }

    /// A workspace of `bytes` that a job last used `hours` ago
    fn old_workspace(builds: &std::path::Path, name: &str, bytes: usize, hours: u64) {
        let dir = builds.join(name);
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::fs::write(dir.join("project/data"), vec![b'x'; bytes]).unwrap();
        let marker = dir.join(".turboci-last-used");
        std::fs::write(&marker, b"").unwrap();
        std::fs::File::options()
            .write(true)
            .open(marker)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(hours * 3600))
            .unwrap();
    }

    fn entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn the_sweep_keeps_workspaces_within_the_quota_but_never_one_in_use() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon_with(&server, dir.path(), |config| {
            config.executor.workspace_max_size = Some("24k".to_string());
            config.cache_max_age_days = 1;
        })
        .await;
        let builds = dir.path().join("builds");
        // 32 KiB in all, oldest first; the oldest one is in use by a job
        old_workspace(&builds, "project-1-0", 8192, 40);
        old_workspace(&builds, "project-2-0", 8192, 30);
        old_workspace(&builds, "project-3-0", 8192, 20);
        old_workspace(&builds, "project-4-0", 8192, 10);
        daemon.claim_workspace(&builds.join("project-1-0")).await;
        // A cache archive of a project without jobs for two days
        let archive = dir.path().join("cache/project-9/old.zip");
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        std::fs::write(&archive, b"zip").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 86400))
            .unwrap();

        daemon.sweep().await;

        // 32 KiB > 24 KiB: the least recently used one not in use goes, and 24 KiB fit
        assert_eq!(
            entries(&builds),
            ["project-1-0", "project-3-0", "project-4-0"]
        );
        assert!(
            !archive.exists(),
            "cache archive past cache_max_age_days kept"
        );
        let metrics = daemon.render_metrics();
        assert!(
            metrics.contains("turboci_workspaces_evicted_total{runner=\"default\"} 1\n"),
            "{}",
            metrics
        );
        assert!(metrics.contains(&format!(
            "turboci_workspaces_size_bytes{{runner=\"default\"}} {}\n",
            3 * 8192
        )));

        // A quota the job's own workspace exceeds: everything else goes, it stays
        let daemon = daemon_with(&server, dir.path(), |config| {
            config.executor.workspace_max_size = Some("1k".to_string());
        })
        .await;
        daemon.claim_workspace(&builds.join("project-1-0")).await;
        daemon.sweep().await;
        assert_eq!(entries(&builds), ["project-1-0"]);
    }

    #[tokio::test]
    async fn the_sweep_without_a_quota_keeps_recent_workspaces() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let builds = dir.path().join("builds");
        old_workspace(&builds, "project-1-0", 8192, 40);
        // Unused for longer than workspace_max_age_days (7)
        old_workspace(&builds, "project-2-0", 10, 8 * 24);

        daemon.sweep().await;

        assert_eq!(entries(&builds), ["project-1-0"]);
        assert!(!daemon
            .render_metrics()
            .contains("turboci_workspaces_size_bytes"));
    }

    #[test]
    fn retry_after_from_gitlab_lengthens_the_wait_but_never_shortens_it() {
        let limited = anyhow::anyhow!("Job request failed: 429")
            .context(crate::gitlab::RetryAfter(Duration::from_secs(60)));
        assert_eq!(
            retry_wait(&limited, Duration::from_secs(5)),
            Duration::from_secs(60)
        );
        assert_eq!(
            retry_wait(&limited, Duration::from_secs(90)),
            Duration::from_secs(90)
        );
        let plain = anyhow::anyhow!("network down");
        assert_eq!(
            retry_wait(&plain, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn request_backoff_grows_to_a_cap_and_resets_on_success() {
        let mut backoff = RequestBackoff::new(Duration::from_secs(3));
        assert_eq!(backoff.no_job(Duration::ZERO), Duration::from_secs(3));

        let mut ceilings = Vec::new();
        for _ in 0..8 {
            let wait = backoff.failed();
            let ceiling = backoff.ceiling();
            assert!(wait >= ceiling / 2 && wait <= ceiling, "{:?}", wait);
            ceilings.push(ceiling.as_secs());
        }
        assert_eq!(ceilings, [3, 6, 12, 24, 48, 60, 60, 60]);

        assert_eq!(backoff.no_job(Duration::ZERO), Duration::from_secs(3));
        backoff.failed();
        assert_eq!(backoff.ceiling(), Duration::from_secs(3));
        backoff.got_job();
        assert_eq!(backoff.ceiling(), Duration::from_secs(3));

        // A check interval above the cap is never shortened
        let mut slow = RequestBackoff::new(Duration::from_secs(120));
        slow.failed();
        slow.failed();
        assert_eq!(slow.ceiling(), Duration::from_secs(120));
    }

    #[tokio::test]
    async fn workspaces_past_their_age_are_only_renamed_while_jobs_wait() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let builds = dir.path().join("builds");
        old_workspace(&builds, "project-1-0", 10, 1);
        old_workspace(&builds, "project-2-0", 10, 8 * 24);

        // What runs under the eviction lock: a rename, whatever the size
        daemon
            .executor
            .evict_workspaces(Duration::from_secs(7 * 86400), &Default::default())
            .await;
        let names = entries(&builds);
        assert_eq!(names.len(), 2, "{:?}", names);
        assert_eq!(names[1], "project-1-0");
        assert!(
            names[0].starts_with(".turboci-evicted-project-2-0-"),
            "{:?}",
            names
        );
        // A renamed one is not evicted again; deleting it comes after the lock
        daemon
            .executor
            .evict_workspaces(Duration::ZERO, &[builds.join("project-1-0")].into())
            .await;
        assert_eq!(entries(&builds), names);
        housekeeping::remove_detached(&builds);
        assert_eq!(entries(&builds), ["project-1-0"]);
    }

    #[test]
    fn requests_start_a_check_interval_apart_and_conflicts_retry_sooner() {
        let secs = Duration::from_secs;
        let mut backoff = RequestBackoff::new(secs(3));
        // The rest of the interval after a quick answer; nothing after a
        // request GitLab held for long polling
        assert_eq!(
            backoff.no_job(Duration::from_millis(200)),
            Duration::from_millis(2800)
        );
        assert_eq!(backoff.no_job(secs(3)), Duration::ZERO);
        assert_eq!(backoff.no_job(secs(50)), Duration::ZERO);

        // 409s in a row: a quick retry, growing up to the check interval
        let waits: Vec<u128> = (0..6).map(|_| backoff.conflict().as_millis()).collect();
        assert_eq!(waits, [250, 500, 1000, 2000, 3000, 3000]);
        // Any other answer starts over
        backoff.no_job(Duration::ZERO);
        assert_eq!(backoff.conflict(), Duration::from_millis(250));
        backoff.got_job();
        assert_eq!(backoff.conflict(), Duration::from_millis(250));
        // A conflict also ends a run of failures, and a failure one of conflicts
        backoff.failed();
        backoff.failed();
        backoff.conflict();
        assert_eq!(backoff.ceiling(), secs(3));
        backoff.conflict();
        backoff.failed();
        assert_eq!(backoff.conflict(), Duration::from_millis(250));

        // A short check interval caps the conflict wait too
        let mut quick = RequestBackoff::new(secs(1));
        let waits: Vec<u128> = (0..4).map(|_| quick.conflict().as_millis()).collect();
        assert_eq!(waits, [250, 500, 1000, 1000]);
    }

    /// Job requests GitLab received so far
    async fn job_requests(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/api/v4/jobs/request")
            .count()
    }

    #[tokio::test]
    async fn a_job_request_gitlab_held_is_followed_at_once() {
        let server = MockServer::start().await;
        // Workhorse holds each request (long polling) longer than check_interval
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_millis(2500)))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon_with(&server, dir.path(), |config| config.check_interval = 2).await;
        let handle = daemon.shutdown_handle();
        let running = tokio::spawn(async move { daemon.start().await });

        // Held until 2.5s; the second request starts right then, not a check
        // interval later (4.5s)
        sleep(Duration::from_millis(3700)).await;
        assert_eq!(job_requests(&server).await, 2);

        handle.request(Shutdown::Abort);
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("daemon stopped")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_conflict_is_retried_soon_but_not_in_a_hot_loop() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon_with(&server, dir.path(), |config| config.check_interval = 5).await;
        let handle = daemon.shutdown_handle();
        let running = tokio::spawn({
            let daemon = daemon.clone();
            async move { daemon.start().await }
        });

        // Requests at 0, 0.25s, 0.75s and 1.75s, instead of one per 5s
        sleep(Duration::from_millis(1300)).await;
        let early = job_requests(&server).await;
        assert_eq!(early, 3, "requests in the first 1.3s");
        sleep(Duration::from_millis(1000)).await;
        assert_eq!(job_requests(&server).await, 4, "requests in the first 2.3s");

        handle.request(Shutdown::Graceful);
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("daemon stopped")
            .unwrap()
            .unwrap();
        assert!(daemon
            .render_metrics()
            .contains("turboci_job_requests_total{runner=\"default\",result=\"conflict\"} 4\n"));
    }

    #[tokio::test]
    async fn claim_job_releases_slot_when_no_job_is_available() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let gitlab = GitLabClient::new(server.uri(), "runner-token".to_string());
        let capacity = capacity::Capacity::new(1);

        assert!(claim_job(&capacity, &gitlab, "runner-token")
            .await
            .unwrap()
            .is_none());
        assert_eq!(capacity.running(), 0);
    }
}
