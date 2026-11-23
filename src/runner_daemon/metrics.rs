//! Prometheus metrics (`metrics_listen`). The daemon counts with atomics,
//! and a minimal HTTP/1.1 server answers `GET /metrics` with the text format;
//! every other request gets 404. Each runner has its own counters, rendered
//! with a `runner` label; a few belong to the whole process.
//!
//! Durations go to a [`Histogram`] (fixed [`DURATION_BUCKETS`], lock-free):
//! `Histogram::observe` records one, and `histogram_family` renders a set of
//! them with their labels. A new duration metric is a `Histogram` (or an
//! array of them, one per label value) in `Metrics`, an `observe_*` method
//! that records into it, and a `histogram_family` call in `render`; see
//! `observe_phase` and `turboci_job_phase_duration_seconds`.
//!
//! Where a job spends its time is collected while it runs in its
//! [`JobTimings`]: code on the job's path calls [`record`] or [`timed`] with
//! a [`Phase`], which go to the timings of the job whose task runs them (see
//! `JobTimings::scope`) and do nothing outside a job. When the job has
//! ended, `Metrics::record_job` turns them into the histograms.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::admission::Resource;
use crate::gitlab::FailureReason;

/// How a job ended
#[derive(Debug, Clone, Copy)]
pub enum JobEnd {
    Success,
    Failed,
    /// Canceled in GitLab
    Canceled,
}
const JOB_ENDS: [&str; 3] = ["success", "failed", "canceled"];

const FAILURE_REASONS: [&str; 5] = [
    "script_failure",
    "runner_system_failure",
    "job_execution_timeout",
    "image_pull_failure",
    "job_canceled",
];

/// What a job request to GitLab returned
#[derive(Debug, Clone, Copy)]
pub enum Request {
    Job,
    NoJob,
    Error,
    /// 409: another request took the job GitLab picked
    Conflict,
}
const REQUESTS: [&str; 4] = ["job", "no_job", "error", "conflict"];

/// Result of restoring one cache entry
#[derive(Debug, Clone, Copy)]
pub enum CacheRestore {
    Hit,
    Miss,
    Error,
}
const CACHE_RESTORES: [&str; 3] = ["hit", "miss", "error"];

/// What happened to a saved cache archive in S3
#[derive(Debug, Clone, Copy)]
pub enum CacheUpload {
    Sent,
    /// S3 held the same content already: nothing was sent
    Unchanged,
    Failed,
}
const CACHE_UPLOADS: [&str; 3] = ["sent", "unchanged", "failed"];

/// Outcome of a config reload (SIGHUP)
#[derive(Debug, Clone, Copy)]
pub enum ConfigReload {
    Success,
    Failure,
}
const CONFIG_RELOADS: [&str; 2] = ["success", "failure"];

/// Where a job spends its time. Phases of the Docker executor's two setup
/// branches (sources: helper, workspace_check, get_sources; containers:
/// pull, services, container) overlap; `prepare` is the time until the
/// job could start restoring and running its scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the workspace (a running eviction) and preparing it
    Claim,
    /// From the job's start until restoring cache and artifacts can begin
    Prepare,
    /// Docker: checking and pulling the job and service images
    Pull,
    /// Docker: the services' network, starting them and waiting for their ports
    Services,
    /// Docker: creating and starting the job container
    Container,
    /// Docker: pulling and starting the helper container that checks out sources
    Helper,
    /// Docker: walking the workspace for files of other users before the checkout
    WorkspaceCheck,
    GetSources,
    /// Docker: waiting for the job container and services after getting sources
    SetupWait,
    RestoreCache,
    DownloadArtifacts,
    /// The script steps (before_script and script)
    Script,
    AfterScript,
    /// Docker: stopping the job container and giving its files to the
    /// runner's user, so cache and artifacts can be read
    Ownership,
    ArchiveCache,
    UploadArtifacts,
    /// Removing the job's file variables
    Cleanup,
    /// Sending the rest of the log to GitLab
    UploadTrace,
    /// Sending the final state to GitLab
    Report,
    /// After the final update: removing containers, network and volumes,
    /// handing the workspace back, removing a workspace that is not kept
    Teardown,
}

impl Phase {
    pub const ALL: [Phase; 20] = [
        Phase::Claim,
        Phase::Prepare,
        Phase::Pull,
        Phase::Services,
        Phase::Container,
        Phase::Helper,
        Phase::WorkspaceCheck,
        Phase::GetSources,
        Phase::SetupWait,
        Phase::RestoreCache,
        Phase::DownloadArtifacts,
        Phase::Script,
        Phase::AfterScript,
        Phase::Ownership,
        Phase::ArchiveCache,
        Phase::UploadArtifacts,
        Phase::Cleanup,
        Phase::UploadTrace,
        Phase::Report,
        Phase::Teardown,
    ];

    /// The `phase` label
    pub fn name(self) -> &'static str {
        match self {
            Phase::Claim => "claim",
            Phase::Prepare => "prepare",
            Phase::Pull => "pull",
            Phase::Services => "services",
            Phase::Container => "container",
            Phase::Helper => "helper",
            Phase::WorkspaceCheck => "workspace_check",
            Phase::GetSources => "get_sources",
            Phase::SetupWait => "setup_wait",
            Phase::RestoreCache => "restore_cache",
            Phase::DownloadArtifacts => "download_artifacts",
            Phase::Script => "script",
            Phase::AfterScript => "after_script",
            Phase::Ownership => "ownership",
            Phase::ArchiveCache => "archive_cache",
            Phase::UploadArtifacts => "upload_artifacts",
            Phase::Cleanup => "cleanup",
            Phase::UploadTrace => "upload_trace",
            Phase::Report => "report",
            Phase::Teardown => "teardown",
        }
    }
}

/// What getting sources did with the checkout an earlier job left in the
/// workspace
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceReuse {
    /// Updated it (GIT_STRATEGY=fetch)
    Fetch,
    /// Threw it away and cloned again: GIT_STRATEGY=clone, submodules, or
    /// updating it failed
    Clone,
    /// There was none: the workspace's first job, or one of its own
    Fresh,
    /// No checkout: GIT_STRATEGY none or empty
    None,
}
const WORKSPACE_REUSES: [&str; 4] = ["fetch", "clone", "fresh", "none"];

/// Upper bounds of the duration histograms' buckets, in seconds
pub const DURATION_BUCKETS: [f64; 13] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// A Prometheus histogram of durations with `DURATION_BUCKETS`; observing
/// is a few atomic additions
#[derive(Debug, Default)]
pub struct Histogram {
    /// Observations per bucket (not cumulative); the last one is +Inf
    buckets: [AtomicU64; DURATION_BUCKETS.len() + 1],
    count: AtomicU64,
    sum_micros: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, took: Duration) {
        let secs = took.as_secs_f64();
        let bucket = DURATION_BUCKETS
            .iter()
            .position(|bound| secs <= *bound)
            .unwrap_or(DURATION_BUCKETS.len());
        self.buckets[bucket].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.sum_micros.fetch_add(micros, Relaxed);
    }

    fn count(&self) -> u64 {
        self.count.load(Relaxed)
    }
}

/// Durations of one job's phases, collected while it runs. Code on the
/// job's path records into the timings of the job whose task it runs in
/// (see `scope`, `record` and `timed`), so it needs no handle to them.
#[derive(Debug)]
pub struct JobTimings {
    started: Instant,
    /// Microseconds per phase (`Phase::ALL` order), added up when a phase
    /// happens more than once (e.g. several script steps)
    micros: [AtomicU64; Phase::ALL.len()],
    /// Bit per phase that was recorded
    seen: AtomicU32,
    /// `WorkspaceReuse` as 1 + its index; 0 before getting sources
    reuse: AtomicU8,
}

tokio::task_local! {
    static JOB_TIMINGS: Arc<JobTimings>;
}

impl Default for JobTimings {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            micros: Default::default(),
            seen: AtomicU32::new(0),
            reuse: AtomicU8::new(0),
        }
    }
}

impl JobTimings {
    /// Run `work` with these timings as the current job's
    pub async fn scope<T>(self: Arc<Self>, work: impl std::future::Future<Output = T>) -> T {
        JOB_TIMINGS.scope(self, work).await
    }

    /// Time since the job started (these timings were created)
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn add(&self, phase: Phase, took: Duration) {
        let index = phase as usize;
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.micros[index].fetch_add(micros, Relaxed);
        self.seen.fetch_or(1 << index, Relaxed);
    }

    /// How long `phase` took, if it happened
    pub fn get(&self, phase: Phase) -> Option<Duration> {
        let index = phase as usize;
        (self.seen.load(Relaxed) & (1 << index) != 0)
            .then(|| Duration::from_micros(self.micros[index].load(Relaxed)))
    }

    pub fn reuse(&self) -> Option<WorkspaceReuse> {
        match self.reuse.load(Relaxed) {
            1 => Some(WorkspaceReuse::Fetch),
            2 => Some(WorkspaceReuse::Clone),
            3 => Some(WorkspaceReuse::Fresh),
            4 => Some(WorkspaceReuse::None),
            _ => None,
        }
    }

    /// `name=1.23s` for every phase that happened, in `Phase::ALL` order
    pub fn summary(&self) -> String {
        Phase::ALL
            .iter()
            .filter_map(|&phase| {
                self.get(phase)
                    .map(|took| format!("{}={:.2}s", phase.name(), took.as_secs_f64()))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Add `took` to `phase` of the current job (nothing outside a job)
pub fn record(phase: Phase, took: Duration) {
    let _ = JOB_TIMINGS.try_with(|timings| timings.add(phase, took));
}

/// Run `work` and add how long it took to `phase` of the current job
pub async fn timed<T>(phase: Phase, work: impl std::future::Future<Output = T>) -> T {
    let started = Instant::now();
    let output = work.await;
    record(phase, started.elapsed());
    output
}

/// Record what getting sources did with the workspace's checkout
pub fn record_reuse(reuse: WorkspaceReuse) {
    let _ = JOB_TIMINGS.try_with(|timings| timings.reuse.store(reuse as u8 + 1, Relaxed));
}

/// The timings of the current job, if there is one
pub fn current_job() -> Option<Arc<JobTimings>> {
    JOB_TIMINGS.try_with(Arc::clone).ok()
}

/// The time since the current job started, if there is one
pub fn job_elapsed() -> Option<Duration> {
    JOB_TIMINGS.try_with(|timings| timings.elapsed()).ok()
}

/// Counters of the whole process
#[derive(Debug)]
pub struct Process {
    /// Seconds since the epoch the process started at
    started: f64,
    config_reloads: [AtomicU64; 2],
}

impl Default for Process {
    fn default() -> Self {
        Self {
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |since| since.as_secs_f64()),
            config_reloads: Default::default(),
        }
    }
}

impl Process {
    pub fn config_reload(&self, result: ConfigReload) {
        self.config_reloads[result as usize].fetch_add(1, Relaxed);
    }
}

/// Counters and states of a runner. Updates are single atomic operations:
/// nothing on a job's path waits for a lock.
#[derive(Debug)]
pub struct Metrics {
    jobs_finished: [AtomicU64; 3],
    jobs_failed: [AtomicU64; 5],
    job_requests: [AtomicU64; 4],
    admission_blocked: [AtomicBool; 3],
    cache_restores: [AtomicU64; 3],
    cache_uploads: [AtomicU64; 3],
    cache_s3_errors: AtomicU64,
    /// Size of the kept workspaces at the last sweep; `u64::MAX` before the
    /// first one, or without `workspace_max_size` (they are not measured)
    workspaces_size: AtomicU64,
    workspaces_evicted: AtomicU64,
    /// Per `Phase`
    phase_durations: [Histogram; Phase::ALL.len()],
    /// Per `JobEnd`: from the job's start until its final update was sent
    job_durations: [Histogram; 3],
    workspace_reuses: [AtomicU64; 4],
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            jobs_finished: Default::default(),
            jobs_failed: Default::default(),
            job_requests: Default::default(),
            admission_blocked: Default::default(),
            cache_restores: Default::default(),
            cache_uploads: Default::default(),
            cache_s3_errors: AtomicU64::new(0),
            workspaces_size: AtomicU64::new(u64::MAX),
            workspaces_evicted: AtomicU64::new(0),
            phase_durations: Default::default(),
            job_durations: Default::default(),
            workspace_reuses: Default::default(),
        }
    }
}

/// Values the daemon reads of a runner when the metrics are scraped
#[derive(Debug, Clone, Default)]
pub struct Gauges {
    /// Jobs running now
    pub running: u64,
    pub executor: String,
}

/// A runner's metrics, as rendered
pub struct Runner<'a> {
    /// Its name, the `runner` label
    pub name: &'a str,
    pub metrics: &'a Metrics,
    pub gauges: Gauges,
}

impl Metrics {
    pub fn job_finished(&self, end: JobEnd) {
        self.jobs_finished[end as usize].fetch_add(1, Relaxed);
    }

    /// A job reported as failed to GitLab, by its failure reason
    pub fn job_failed(&self, reason: &FailureReason) {
        let index = match reason {
            FailureReason::ScriptFailure => 0,
            FailureReason::RunnerSystemFailure => 1,
            FailureReason::JobExecutionTimeout => 2,
            FailureReason::ImagePullFailure => 3,
            FailureReason::JobCanceled => 4,
        };
        self.job_finished(JobEnd::Failed);
        self.jobs_failed[index].fetch_add(1, Relaxed);
    }

    pub fn request(&self, result: Request) {
        self.job_requests[result as usize].fetch_add(1, Relaxed);
    }

    pub fn set_admission_blocked(&self, resource: Resource, blocked: bool) {
        self.admission_blocked[resource as usize].store(blocked, Relaxed);
    }

    pub fn cache_restore(&self, result: CacheRestore) {
        self.cache_restores[result as usize].fetch_add(1, Relaxed);
    }

    pub fn cache_upload(&self, result: CacheUpload) {
        self.cache_uploads[result as usize].fetch_add(1, Relaxed);
    }

    /// Calls to S3 that failed while a job restored or saved its cache
    pub fn cache_s3_errors(&self, count: u64) {
        self.cache_s3_errors.fetch_add(count, Relaxed);
    }

    /// A sweep measured the workspaces and evicted `evicted` of them
    pub fn workspaces_swept(&self, size: u64, evicted: u64) {
        self.workspaces_size.store(size, Relaxed);
        self.workspaces_evicted.fetch_add(evicted, Relaxed);
    }

    pub fn observe_phase(&self, phase: Phase, took: Duration) {
        self.phase_durations[phase as usize].observe(took);
    }

    /// A job's duration until its final update was sent, by how it ended
    pub fn observe_job(&self, end: JobEnd, took: Duration) {
        self.job_durations[end as usize].observe(took);
    }

    pub fn workspace_reuse(&self, reuse: WorkspaceReuse) {
        self.workspace_reuses[reuse as usize].fetch_add(1, Relaxed);
    }

    /// Observe the phases a job went through and what it did with its workspace
    pub fn record_job(&self, timings: &JobTimings) {
        for phase in Phase::ALL {
            if let Some(took) = timings.get(phase) {
                self.observe_phase(phase, took);
            }
        }
        if let Some(reuse) = timings.reuse() {
            self.workspace_reuse(reuse);
        }
    }
}

/// The metrics of the process and its runners (`concurrent` slots in all) in
/// Prometheus text format (version 0.0.4)
pub fn render(process: &Process, concurrent: u64, runners: &[Runner]) -> String {
    let mut out = String::with_capacity(4096);
    let labeled = |name: &str, labels: &[&str], values: &[AtomicU64]| {
        labels
            .iter()
            .zip(values)
            .map(|(label, value)| {
                (
                    format!("{}=\"{}\"", name, label),
                    value.load(Relaxed).to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    // The samples `of` gives for each runner, labeled with its name
    let per_runner = |of: &dyn Fn(&Runner) -> Vec<(String, String)>| {
        runners
            .iter()
            .flat_map(|runner| {
                let name = format!("runner=\"{}\"", escape(runner.name));
                of(runner).into_iter().map(move |(labels, value)| {
                    if labels.is_empty() {
                        (name.clone(), value)
                    } else {
                        (format!("{},{}", name, labels), value)
                    }
                })
            })
            .collect::<Vec<_>>()
    };
    let single = |value: String| vec![(String::new(), value)];
    family(
        &mut out,
        "turboci_runner_info",
        "gauge",
        "Version and executor of each runner",
        per_runner(&|runner| {
            vec![(
                format!(
                    "version=\"{}\",revision=\"{}\",executor=\"{}\"",
                    escape(env!("CARGO_PKG_VERSION")),
                    escape(&super::revision()),
                    escape(&runner.gauges.executor)
                ),
                "1".to_string(),
            )]
        }),
    );
    family(
        &mut out,
        "turboci_jobs_running",
        "gauge",
        "Jobs running now",
        per_runner(&|runner| single(runner.gauges.running.to_string())),
    );
    family(
        &mut out,
        "turboci_concurrent_jobs",
        "gauge",
        "Jobs that may run at the same time (concurrent)",
        single(concurrent.to_string()),
    );
    family(
        &mut out,
        "turboci_jobs_finished_total",
        "counter",
        "Jobs that ended, by result",
        per_runner(&|runner| labeled("result", &JOB_ENDS, &runner.metrics.jobs_finished)),
    );
    family(
        &mut out,
        "turboci_jobs_failed_total",
        "counter",
        "Jobs reported failed to GitLab, by failure reason",
        per_runner(&|runner| labeled("reason", &FAILURE_REASONS, &runner.metrics.jobs_failed)),
    );
    family(
        &mut out,
        "turboci_job_requests_total",
        "counter",
        "Job requests to GitLab, by what they returned",
        per_runner(&|runner| labeled("result", &REQUESTS, &runner.metrics.job_requests)),
    );
    family(
        &mut out,
        "turboci_admission_blocked",
        "gauge",
        "1 while no job is requested because the host is short of this resource",
        per_runner(&|runner| {
            Resource::ALL
                .iter()
                .map(|resource| {
                    (
                        format!("resource=\"{}\"", resource.name()),
                        u8::from(
                            runner.metrics.admission_blocked[*resource as usize].load(Relaxed),
                        )
                        .to_string(),
                    )
                })
                .collect()
        }),
    );
    family(
        &mut out,
        "turboci_cache_restores_total",
        "counter",
        "Cache entries jobs restored, by result",
        per_runner(&|runner| labeled("result", &CACHE_RESTORES, &runner.metrics.cache_restores)),
    );
    family(
        &mut out,
        "turboci_cache_uploads_total",
        "counter",
        "Saved cache archives and their S3 upload, by result",
        per_runner(&|runner| labeled("result", &CACHE_UPLOADS, &runner.metrics.cache_uploads)),
    );
    family(
        &mut out,
        "turboci_cache_s3_errors_total",
        "counter",
        "Cache downloads and uploads that S3 failed (the job used the local cache)",
        per_runner(&|runner| single(runner.metrics.cache_s3_errors.load(Relaxed).to_string())),
    );
    family(
        &mut out,
        "turboci_config_reloads_total",
        "counter",
        "Config reloads (SIGHUP), by result; a failed one keeps the running config",
        labeled("result", &CONFIG_RELOADS, &process.config_reloads),
    );
    family(
        &mut out,
        "turboci_workspaces_evicted_total",
        "counter",
        "Workspaces deleted to stay within workspace_max_size",
        per_runner(&|runner| single(runner.metrics.workspaces_evicted.load(Relaxed).to_string())),
    );
    let sizes = per_runner(&|runner| {
        let size = runner.metrics.workspaces_size.load(Relaxed);
        if size == u64::MAX {
            Vec::new()
        } else {
            single(size.to_string())
        }
    });
    if !sizes.is_empty() {
        family(
            &mut out,
            "turboci_workspaces_size_bytes",
            "gauge",
            "Size of the kept workspaces at the last hourly sweep",
            sizes,
        );
    }
    family(
        &mut out,
        "turboci_workspace_reuse_total",
        "counter",
        "Jobs by what getting sources did with the checkout an earlier job left in the workspace",
        per_runner(&|runner| {
            labeled(
                "result",
                &WORKSPACE_REUSES,
                &runner.metrics.workspace_reuses,
            )
        }),
    );
    let phases: Vec<&str> = Phase::ALL.iter().map(|phase| phase.name()).collect();
    histogram_family(
        &mut out,
        "turboci_job_phase_duration_seconds",
        "Time jobs spent in each phase (some phases of the Docker executor overlap)",
        duration_series(runners, "phase", &phases, |metrics| {
            metrics.phase_durations.as_slice()
        }),
    );
    histogram_family(
        &mut out,
        "turboci_job_duration_seconds",
        "Time from a job's start in the runner until its final update was sent, by result",
        duration_series(runners, "result", &JOB_ENDS, |metrics| {
            metrics.job_durations.as_slice()
        }),
    );
    family(
        &mut out,
        "process_start_time_seconds",
        "gauge",
        "Start time of the process since the epoch, in seconds",
        single(format!("{:.3}", process.started)),
    );
    if let Some(bytes) = resident_memory() {
        family(
            &mut out,
            "process_resident_memory_bytes",
            "gauge",
            "Resident memory size in bytes",
            single(bytes.to_string()),
        );
    }
    out
}

/// Append one metric family: its HELP and TYPE lines, then a line per
/// sample (`labels` without braces, empty for none)
fn family(
    out: &mut String,
    name: &str,
    kind: &str,
    help: &str,
    samples: impl IntoIterator<Item = (String, String)>,
) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP {} {}", name, help);
    let _ = writeln!(out, "# TYPE {} {}", name, kind);
    for (labels, value) in samples {
        if labels.is_empty() {
            let _ = writeln!(out, "{} {}", name, value);
        } else {
            let _ = writeln!(out, "{}{{{}}} {}", name, labels, value);
        }
    }
}

/// The histograms `of` each runner's metrics that have observations, one
/// per label value in `names`, labeled with the runner, its executor and
/// `label`
fn duration_series<'a>(
    runners: &'a [Runner<'a>],
    label: &str,
    names: &[&str],
    of: fn(&'a Metrics) -> &'a [Histogram],
) -> Vec<(String, &'a Histogram)> {
    let mut series = Vec::new();
    for runner in runners {
        for (name, histogram) in names.iter().zip(of(runner.metrics)) {
            if histogram.count() > 0 {
                let labels = format!(
                    "runner=\"{}\",executor=\"{}\",{}=\"{}\"",
                    escape(runner.name),
                    escape(&runner.gauges.executor),
                    label,
                    name
                );
                series.push((labels, histogram));
            }
        }
    }
    series
}

/// Append a histogram family: HELP and TYPE, then for each histogram (with
/// its labels, without braces) its cumulative buckets, sum and count
pub fn histogram_family<'a>(
    out: &mut String,
    name: &str,
    help: &str,
    series: impl IntoIterator<Item = (String, &'a Histogram)>,
) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP {} {}", name, help);
    let _ = writeln!(out, "# TYPE {} histogram", name);
    for (labels, histogram) in series {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut cumulative = 0;
        for (bucket, bound) in histogram.buckets.iter().zip(DURATION_BUCKETS) {
            cumulative += bucket.load(Relaxed);
            let _ = writeln!(
                out,
                "{}_bucket{{{}{}le=\"{}\"}} {}",
                name, labels, sep, bound, cumulative
            );
        }
        cumulative += histogram.buckets[DURATION_BUCKETS.len()].load(Relaxed);
        let _ = writeln!(
            out,
            "{}_bucket{{{}{}le=\"+Inf\"}} {}",
            name, labels, sep, cumulative
        );
        let sum = histogram.sum_micros.load(Relaxed) as f64 / 1e6;
        let braced = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", labels)
        };
        let _ = writeln!(out, "{}_sum{} {:.6}", name, braced, sum);
        // The buckets' total, so the count always matches +Inf
        let _ = writeln!(out, "{}_count{} {}", name, braced, cumulative);
    }
}

/// A label value with `\`, `"` and newlines escaped
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Resident memory of this process (Linux: VmRSS in /proc/self/status)
fn resident_memory() -> Option<u64> {
    if cfg!(target_os = "linux") {
        parse_vm_rss(&std::fs::read_to_string("/proc/self/status").ok()?)
    } else {
        None
    }
}

fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?;
    let mut fields = line.split_whitespace();
    let kib: u64 = fields.next()?.parse().ok()?;
    (fields.next() == Some("kB")).then_some(kib.checked_mul(1024)?)
}

/// Renders the metrics for a scrape
pub type Render = Arc<dyn Fn() -> String + Send + Sync>;

/// Scrapes served at the same time; more connections are closed at once
const MAX_CONNECTIONS: usize = 32;
/// Time a client has to send its request and read the answer
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest request head read
const MAX_REQUEST: usize = 8192;

/// Answer scrapes on `listener` until the task is aborted; connections in
/// flight end with it, so the server never keeps the runner from exiting
pub async fn serve(listener: TcpListener, render: Render) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // Out of file descriptors and the like: try again shortly
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        while connections.try_join_next().is_some() {}
        if connections.len() >= MAX_CONNECTIONS {
            continue;
        }
        let render = render.clone();
        connections.spawn(async move {
            let _ = tokio::time::timeout(CONNECTION_TIMEOUT, answer(stream, render)).await;
        });
    }
}

async fn answer(mut stream: TcpStream, render: Render) -> std::io::Result<()> {
    let head = read_head(&mut stream).await?;
    let request_line = head.lines().next().unwrap_or_default();
    let (status, content_type, body) = if is_metrics_get(request_line) {
        (
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            render(),
        )
    } else {
        (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "404 page not found\n".to_string(),
        )
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        content_type,
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// The request up to the empty line that ends its head (or `MAX_REQUEST` bytes)
async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut head = Vec::with_capacity(512);
    let mut buffer = [0u8; 1024];
    while head.len() < MAX_REQUEST {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buffer[..read]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") || head.windows(2).any(|w| w == b"\n\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// `GET /metrics HTTP/1.x`, with or without a query string
fn is_metrics_get(request_line: &str) -> bool {
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    method == "GET" && path == "/metrics" && version.starts_with("HTTP/1.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gauges() -> Gauges {
        Gauges {
            running: 2,
            executor: "docker".to_string(),
        }
    }

    /// The metrics of a process with the single runner `metrics`
    fn render_one(process: &Process, metrics: &Metrics) -> String {
        render(
            process,
            4,
            &[Runner {
                name: "default",
                metrics,
                gauges: gauges(),
            }],
        )
    }

    #[test]
    fn renders_every_counter_in_the_text_format() {
        let metrics = Metrics::default();
        let process = Process::default();
        metrics.job_finished(JobEnd::Success);
        metrics.job_finished(JobEnd::Success);
        metrics.job_failed(&FailureReason::ScriptFailure);
        metrics.job_finished(JobEnd::Canceled);
        metrics.request(Request::Error);
        metrics.request(Request::NoJob);
        metrics.set_admission_blocked(Resource::Disk, true);
        metrics.cache_restore(CacheRestore::Hit);
        metrics.cache_restore(CacheRestore::Miss);
        metrics.cache_upload(CacheUpload::Unchanged);
        metrics.cache_s3_errors(3);
        process.config_reload(ConfigReload::Failure);
        assert!(!render_one(&process, &metrics).contains("turboci_workspaces_size_bytes"));
        metrics.workspaces_swept(4096, 2);
        metrics.workspace_reuse(WorkspaceReuse::Fetch);
        metrics.observe_phase(Phase::GetSources, Duration::from_millis(700));
        metrics.observe_job(JobEnd::Success, Duration::from_secs(3));

        let text = render_one(&process, &metrics);
        for line in [
            "# TYPE turboci_jobs_finished_total counter",
            "turboci_jobs_finished_total{runner=\"default\",result=\"success\"} 2",
            "turboci_jobs_finished_total{runner=\"default\",result=\"failed\"} 1",
            "turboci_jobs_finished_total{runner=\"default\",result=\"canceled\"} 1",
            "turboci_jobs_failed_total{runner=\"default\",reason=\"script_failure\"} 1",
            "turboci_jobs_failed_total{runner=\"default\",reason=\"runner_system_failure\"} 0",
            "turboci_job_requests_total{runner=\"default\",result=\"error\"} 1",
            "turboci_job_requests_total{runner=\"default\",result=\"job\"} 0",
            "turboci_jobs_running{runner=\"default\"} 2",
            "turboci_concurrent_jobs 4",
            "turboci_admission_blocked{runner=\"default\",resource=\"disk\"} 1",
            "turboci_admission_blocked{runner=\"default\",resource=\"memory\"} 0",
            "turboci_cache_restores_total{runner=\"default\",result=\"hit\"} 1",
            "turboci_cache_restores_total{runner=\"default\",result=\"miss\"} 1",
            "turboci_cache_uploads_total{runner=\"default\",result=\"unchanged\"} 1",
            "turboci_cache_s3_errors_total{runner=\"default\"} 3",
            "turboci_config_reloads_total{result=\"success\"} 0",
            "turboci_config_reloads_total{result=\"failure\"} 1",
            "turboci_workspaces_evicted_total{runner=\"default\"} 2",
            "turboci_workspaces_size_bytes{runner=\"default\"} 4096",
            "turboci_workspace_reuse_total{runner=\"default\",result=\"fetch\"} 1",
            "turboci_workspace_reuse_total{runner=\"default\",result=\"none\"} 0",
            "# TYPE turboci_job_phase_duration_seconds histogram",
            "turboci_job_phase_duration_seconds_bucket{runner=\"default\",executor=\"docker\",phase=\"get_sources\",le=\"0.5\"} 0",
            "turboci_job_phase_duration_seconds_bucket{runner=\"default\",executor=\"docker\",phase=\"get_sources\",le=\"1\"} 1",
            "turboci_job_phase_duration_seconds_bucket{runner=\"default\",executor=\"docker\",phase=\"get_sources\",le=\"+Inf\"} 1",
            "turboci_job_phase_duration_seconds_sum{runner=\"default\",executor=\"docker\",phase=\"get_sources\"} 0.700000",
            "turboci_job_phase_duration_seconds_count{runner=\"default\",executor=\"docker\",phase=\"get_sources\"} 1",
            "turboci_job_duration_seconds_bucket{runner=\"default\",executor=\"docker\",result=\"success\",le=\"2.5\"} 0",
            "turboci_job_duration_seconds_bucket{runner=\"default\",executor=\"docker\",result=\"success\",le=\"5\"} 1",
            "turboci_job_duration_seconds_count{runner=\"default\",executor=\"docker\",result=\"success\"} 1",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {:?} in\n{}",
                line,
                text
            );
        }
        assert!(text.contains(&format!(
            "turboci_runner_info{{runner=\"default\",version=\"{}\",",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(text.contains("executor=\"docker\"} 1\n"));
        if cfg!(target_os = "linux") {
            assert!(text.contains("\nprocess_resident_memory_bytes "));
        }
        // Phases and results nothing was observed for have no series
        assert!(!text.contains("phase=\"pull\""), "{}", text);
        assert!(!text.contains("result=\"failed\",le="), "{}", text);

        // Every sample is `name{labels} value`, and every family has HELP and
        // TYPE (a histogram's samples have suffixes)
        let sample =
            regex::Regex::new(r#"^[a-z_][a-z0-9_]*(\{([a-z_]+="[^"]*",?)+\})? [0-9.]+$"#).unwrap();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            assert!(sample.is_match(line), "bad sample line {:?}", line);
            let mut name = line.split(['{', ' ']).next().unwrap();
            if !text.contains(&format!("# TYPE {} ", name)) {
                for suffix in ["_bucket", "_sum", "_count"] {
                    name = name.strip_suffix(suffix).unwrap_or(name);
                }
                assert!(
                    text.contains(&format!("# TYPE {} histogram", name)),
                    "{}",
                    name
                );
            }
            assert!(text.contains(&format!("# HELP {} ", name)), "{}", name);
            assert!(text.contains(&format!("# TYPE {} ", name)), "{}", name);
        }
    }

    #[tokio::test]
    async fn job_timings_collect_the_phases_of_the_job_whose_task_records_them() {
        let timings = Arc::new(JobTimings::default());
        let other = Arc::new(JobTimings::default());
        // Outside a job, recording does nothing
        record(Phase::Script, Duration::from_secs(1));
        assert_eq!(job_elapsed(), None);

        timings
            .clone()
            .scope(async {
                record(Phase::Script, Duration::from_millis(1500));
                // Concurrent branches of one job record into the same timings
                tokio::join!(
                    timed(Phase::Pull, tokio::time::sleep(Duration::from_millis(20))),
                    async { record(Phase::Script, Duration::from_millis(500)) }
                );
                record_reuse(WorkspaceReuse::Clone);
                assert!(job_elapsed().is_some());
                other
                    .clone()
                    .scope(async { record(Phase::Teardown, Duration::from_secs(2)) })
                    .await;
            })
            .await;

        assert_eq!(timings.get(Phase::Script), Some(Duration::from_secs(2)));
        assert!(timings.get(Phase::Pull).unwrap() >= Duration::from_millis(20));
        assert_eq!(timings.get(Phase::Teardown), None);
        assert_eq!(other.get(Phase::Teardown), Some(Duration::from_secs(2)));
        assert_eq!(timings.reuse(), Some(WorkspaceReuse::Clone));
        assert_eq!(other.reuse(), None);
        let summary = timings.summary();
        assert!(summary.starts_with("pull=0.0"), "{}", summary);
        assert!(summary.ends_with(" script=2.00s"), "{}", summary);

        let metrics = Metrics::default();
        metrics.record_job(&timings);
        let text = render_one(&Process::default(), &metrics);
        assert!(text.contains(
            "turboci_job_phase_duration_seconds_count{runner=\"default\",executor=\"docker\",phase=\"script\"} 1\n"
        ));
        assert!(
            text.contains("turboci_workspace_reuse_total{runner=\"default\",result=\"clone\"} 1\n")
        );
    }

    #[test]
    fn histogram_buckets_hold_their_upper_bound() {
        let histogram = Histogram::default();
        for millis in [50, 51, 600_000, 700_000] {
            histogram.observe(Duration::from_millis(millis));
        }
        let mut out = String::new();
        histogram_family(&mut out, "d", "h", [(String::new(), &histogram)]);
        for line in [
            "d_bucket{le=\"0.05\"} 1",
            "d_bucket{le=\"0.1\"} 2",
            "d_bucket{le=\"600\"} 3",
            "d_bucket{le=\"+Inf\"} 4",
            "d_sum 1300.101000",
            "d_count 4",
        ] {
            assert!(
                out.lines().any(|l| l == line),
                "missing {:?} in\n{}",
                line,
                out
            );
        }
    }

    #[test]
    fn every_runner_has_its_own_samples_under_one_family() {
        let process = Process::default();
        let (a, b) = (Metrics::default(), Metrics::default());
        a.job_finished(JobEnd::Success);
        b.workspaces_swept(10, 0);
        let text = render(
            &process,
            6,
            &[
                Runner {
                    name: "a",
                    metrics: &a,
                    gauges: gauges(),
                },
                Runner {
                    name: "b",
                    metrics: &b,
                    gauges: Gauges {
                        running: 0,
                        executor: "shell".to_string(),
                    },
                },
            ],
        );

        for line in [
            "turboci_jobs_finished_total{runner=\"a\",result=\"success\"} 1",
            "turboci_jobs_finished_total{runner=\"b\",result=\"success\"} 0",
            "turboci_jobs_running{runner=\"a\"} 2",
            "turboci_jobs_running{runner=\"b\"} 0",
            "turboci_concurrent_jobs 6",
            "turboci_workspaces_size_bytes{runner=\"b\"} 10",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {:?} in\n{}",
                line,
                text
            );
        }
        // Only the runner whose workspaces were measured has a size
        assert!(!text.contains("turboci_workspaces_size_bytes{runner=\"a\"}"));
        assert!(text.contains("runner=\"b\",version="));
        assert!(text.contains("executor=\"shell\"} 1"));
        for name in ["turboci_jobs_running", "turboci_concurrent_jobs"] {
            assert_eq!(text.matches(&format!("# TYPE {} ", name)).count(), 1);
        }
    }

    #[test]
    fn reads_resident_memory_and_escapes_labels() {
        assert_eq!(
            parse_vm_rss("Name:\tturboci\nVmRSS:\t   12345 kB\nThreads: 4\n"),
            Some(12345 * 1024)
        );
        assert_eq!(parse_vm_rss("VmRSS: 12 MB\n"), None);
        assert_eq!(parse_vm_rss("Name: x\n"), None);
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn routes_only_get_metrics() {
        assert!(is_metrics_get("GET /metrics HTTP/1.1"));
        assert!(is_metrics_get("GET /metrics?name[]=x HTTP/1.0"));
        for line in [
            "GET / HTTP/1.1",
            "GET /metricsx HTTP/1.1",
            "GET /metrics/ HTTP/1.1",
            "POST /metrics HTTP/1.1",
            "HEAD /metrics HTTP/1.1",
            "GET /metrics",
            "GET /metrics SSH-2.0",
            "",
        ] {
            assert!(!is_metrics_get(line), "routed {:?}", line);
        }
    }

    /// Send `request` raw and return the whole response
    async fn exchange(addr: std::net::SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn serves_metrics_and_404_for_everything_else() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = Arc::new(Metrics::default());
        metrics.job_finished(JobEnd::Success);
        let render: Render = {
            let metrics = metrics.clone();
            Arc::new(move || render_one(&Process::default(), &metrics))
        };
        let server = tokio::spawn(serve(listener, render));

        let ok = exchange(addr, "GET /metrics HTTP/1.1\r\nHost: runner\r\n\r\n").await;
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"), "{}", ok);
        assert!(ok.contains("Content-Type: text/plain; version=0.0.4"));
        let (head, body) = ok.split_once("\r\n\r\n").unwrap();
        assert!(head.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(
            body.contains("turboci_jobs_finished_total{runner=\"default\",result=\"success\"} 1\n")
        );

        for request in [
            "GET / HTTP/1.1\r\n\r\n",
            "GET /debug HTTP/1.1\r\n\r\n",
            "POST /metrics HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
            "garbage\r\n\r\n",
        ] {
            let response = exchange(addr, request).await;
            assert!(
                response.starts_with("HTTP/1.1 404 Not Found\r\n"),
                "{:?}: {}",
                request,
                response
            );
        }

        // A real HTTP client understands the answers
        let url = format!("http://{}/metrics", addr);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("turboci_jobs_running{runner=\"default\"} 2"));
        let missing = client
            .get(format!("http://{}/", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        // Aborted with the daemon: the server and its connections are gone
        // (a port check would race with other tests taking the freed port)
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        assert_eq!(Arc::strong_count(&metrics), 1, "something still renders");
    }

    #[tokio::test]
    async fn a_silent_client_does_not_block_others() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let render: Render = Arc::new(|| "x 1\n".to_string());
        let server = tokio::spawn(serve(listener, render));

        // Connects and never sends anything
        let _silent = TcpStream::connect(addr).await.unwrap();
        let answered = tokio::time::timeout(
            Duration::from_secs(5),
            exchange(addr, "GET /metrics HTTP/1.1\r\n\r\n"),
        )
        .await
        .expect("answered while another client hangs");
        assert!(answered.ends_with("\r\n\r\nx 1\n"));
        server.abort();
    }
}
