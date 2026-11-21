//! Prometheus metrics (`metrics_listen`). The daemon counts with atomics,
//! and a minimal HTTP/1.1 server answers `GET /metrics` with the text format;
//! every other request gets 404. Each runner has its own counters, rendered
//! with a `runner` label; a few belong to the whole process.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
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

        // Every sample is `name{labels} value`, and every family has HELP and TYPE
        let sample =
            regex::Regex::new(r#"^[a-z_][a-z0-9_]*(\{([a-z_]+="[^"]*",?)+\})? [0-9.]+$"#).unwrap();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            assert!(sample.is_match(line), "bad sample line {:?}", line);
            let name = line.split(['{', ' ']).next().unwrap();
            assert!(text.contains(&format!("# HELP {} ", name)), "{}", name);
            assert!(text.contains(&format!("# TYPE {} ", name)), "{}", name);
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
