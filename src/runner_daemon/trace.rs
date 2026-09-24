//! Job log sent to GitLab. One writer per job owns the whole (scrubbed) trace and the
//! offset GitLab has acknowledged, so output is never lost or duplicated: after a
//! 416 range mismatch it resends from the offset the server reports.

use std::time::{Duration, Instant};
use tracing::warn;

use super::cancel::CancelSignal;
use crate::gitlab::GitLabClient;
use crate::security::secret_scrubber::{SecretScrubber, StreamScrubber};

/// Same default as gitlab-runner's `output_limit` (4 MiB)
const DEFAULT_LIMIT: usize = 4 * 1024 * 1024;
/// How often output is sent until GitLab asks for another interval
const DEFAULT_UPDATE_INTERVAL: Duration = Duration::from_secs(3);
/// Upper bound for a server-requested interval, like gitlab-runner
const MAX_UPDATE_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Output sent sooner than the interval once this much is pending
const FLUSH_BYTES: usize = 256 * 1024;
const FINISH_ATTEMPTS: u32 = 5;

pub struct TraceWriter<'a> {
    client: Option<&'a GitLabClient>,
    job_id: u64,
    token: String,
    scrubber: StreamScrubber<'a>,
    trace: Vec<u8>,
    /// Bytes GitLab has acknowledged
    sent: usize,
    last_flush: Instant,
    update_interval: Duration,
    limit: usize,
    truncated: bool,
    cancel: CancelSignal,
}

impl<'a> TraceWriter<'a> {
    /// `client` is `None` when output is only collected (no GitLab to stream to)
    pub fn new(
        client: Option<&'a GitLabClient>,
        job_id: u64,
        token: &str,
        scrubber: &'a SecretScrubber,
    ) -> Self {
        Self {
            client,
            job_id,
            token: token.to_string(),
            scrubber: StreamScrubber::new(scrubber),
            trace: Vec::new(),
            sent: 0,
            last_flush: Instant::now(),
            update_interval: DEFAULT_UPDATE_INTERVAL,
            limit: DEFAULT_LIMIT,
            truncated: false,
            cancel: CancelSignal::default(),
        }
    }

    /// Share `cancel` so remote cancellation seen on trace responses reaches the job
    pub fn with_cancel(mut self, cancel: CancelSignal) -> Self {
        self.cancel = cancel;
        self
    }

    /// The job's cancellation signal
    pub fn cancel(&self) -> CancelSignal {
        self.cancel.clone()
    }

    #[cfg(test)]
    fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Append output; it is scrubbed and sent in batches
    pub async fn write(&mut self, text: &str) {
        let safe = self.scrubber.push(text);
        self.append(&safe);
        if self.trace.len() - self.sent >= FLUSH_BYTES {
            self.flush().await;
        } else {
            self.flush_if_due().await;
        }
    }

    /// Open a collapsible section of the job log (`name` is [a-z0-9_]),
    /// shown under a highlighted `header`
    pub async fn section_start(&mut self, name: &str, header: &str) {
        self.write(&format!(
            "{}\x1b[0K\x1b[36;1m{}\x1b[0;m\n",
            section_marker("start", name),
            header
        ))
        .await;
    }

    /// Close the section opened with the same `name`
    pub async fn section_end(&mut self, name: &str) {
        self.write(&format!("{}\x1b[0K", section_marker("end", name)))
            .await;
    }

    /// Send pending output once the update interval has passed
    pub async fn flush_if_due(&mut self) {
        if self.last_flush.elapsed() >= self.update_interval {
            self.flush().await;
        }
    }

    /// Send everything not yet acknowledged; returns true when GitLab has it all
    pub async fn flush(&mut self) -> bool {
        self.last_flush = Instant::now();
        let Some(client) = self.client else {
            self.sent = self.trace.len();
            return true;
        };
        if self.sent >= self.trace.len() {
            return true;
        }

        match client
            .patch_trace(
                self.job_id,
                &self.token,
                &self.trace[self.sent..],
                self.sent,
            )
            .await
        {
            // On success this is the end of what we sent; on 416 it is what GitLab has
            Ok(patch) => {
                self.sent = patch.offset.min(self.trace.len());
                self.cancel.update(patch.remote);
                if let Some(interval) = patch.update_interval {
                    self.update_interval = interval.min(MAX_UPDATE_INTERVAL);
                }
                if patch.remote == crate::gitlab::RemoteState::Aborted {
                    // GitLab no longer accepts output for this job
                    self.sent = self.trace.len();
                }
            }
            Err(e) => warn!("Failed to send trace for job #{}: {}", self.job_id, e),
        }
        self.sent >= self.trace.len()
    }

    /// Flush held-back output and make sure GitLab has the whole trace
    pub async fn finish(&mut self) {
        let rest = self.scrubber.finish();
        self.append(&rest);
        for attempt in 0..FINISH_ATTEMPTS {
            if self.flush().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt))).await;
        }
        warn!(
            "Job #{} trace incomplete: GitLab acknowledged {} of {} bytes",
            self.job_id,
            self.sent,
            self.trace.len()
        );
    }

    /// Scrubbed output written so far
    #[cfg(test)]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.trace).into_owned()
    }

    fn append(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let room = self.limit.saturating_sub(self.trace.len());
        if text.len() <= room {
            self.trace.extend_from_slice(text.as_bytes());
            return;
        }
        let mut cut = room;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        self.trace.extend_from_slice(&text.as_bytes()[..cut]);
        self.trace.extend_from_slice(
            format!(
                "\nJob's log exceeded limit of {} bytes.\nJob execution will continue but no more output will be collected.\n",
                self.limit
            )
            .as_bytes(),
        );
        self.truncated = true;
    }
}

fn section_marker(kind: &str, name: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("section_{}:{}:{}\r", kind, now, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    async fn patches(server: &MockServer) -> Vec<(String, String)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r: &&Request| r.method.as_str() == "PATCH")
            .map(|r| {
                (
                    r.headers["Content-Range"].to_str().unwrap().to_string(),
                    String::from_utf8_lossy(&r.body).into_owned(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn sends_consecutive_ranges_and_whole_trace() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v4/jobs/7/trace"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);

        trace.write("Preparing...\n").await;
        trace.flush().await;
        trace.write("step output\n").await;
        trace.finish().await;

        let sent = patches(&server).await;
        let body: String = sent.iter().map(|(_, b)| b.as_str()).collect();
        assert_eq!(body, "Preparing...\nstep output\n");
        assert_eq!(sent[0].0, "0-12");
        assert_eq!(sent[1].0, "13-24");
    }

    #[tokio::test]
    async fn resends_from_server_offset_after_range_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v4/jobs/7/trace"))
            .respond_with(ResponseTemplate::new(416).insert_header("Range", "0-4"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v4/jobs/7/trace"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);

        trace.write("0123456789").await;
        trace.finish().await;

        let sent = patches(&server).await;
        assert_eq!(sent[0], ("0-9".to_string(), "0123456789".to_string()));
        assert_eq!(sent[1], ("4-9".to_string(), "456789".to_string()));
    }

    #[tokio::test]
    async fn follows_the_update_interval_gitlab_asks_for() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(
                ResponseTemplate::new(202).insert_header("X-GitLab-Trace-Update-Interval", "30"),
            )
            .mount(&server)
            .await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);
        assert_eq!(trace.update_interval, DEFAULT_UPDATE_INTERVAL);

        trace.write("first\n").await;
        trace.flush().await;
        assert_eq!(trace.update_interval, Duration::from_secs(30));

        // Not due yet: nothing more is sent until the interval passes
        trace.write("second\n").await;
        trace.flush_if_due().await;
        assert_eq!(patches(&server).await.len(), 1);

        trace.last_flush -= Duration::from_secs(31);
        trace.flush_if_due().await;
        assert_eq!(patches(&server).await.len(), 2);
    }

    #[tokio::test]
    async fn sections_use_gitlab_markers() {
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, 7, "job-token", &scrubber);

        trace.section_start("get_sources", "Getting source").await;
        trace.write("fetched\n").await;
        trace.section_end("get_sources").await;
        trace.finish().await;

        let text = trace.text();
        let (start, rest) = text.split_once('\r').unwrap();
        assert!(start.starts_with("section_start:"));
        assert!(start.ends_with(":get_sources"));
        assert!(rest.starts_with("\x1b[0K\x1b[36;1mGetting source\x1b[0;m\nfetched\nsection_end:"));
        assert!(text.ends_with(":get_sources\r\x1b[0K"));
    }

    #[tokio::test]
    async fn never_sends_secrets() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec!["hunter2-password".to_string()]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);

        trace.write("pass=hunter2-").await;
        trace.flush().await;
        trace.write("password ok\n").await;
        trace.finish().await;

        let body: String = patches(&server).await.into_iter().map(|(_, b)| b).collect();
        assert_eq!(body, "pass=[MASKED] ok\n");
    }

    #[tokio::test]
    async fn stops_collecting_at_limit() {
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, 7, "job-token", &scrubber).with_limit(100);

        trace.write(&"x".repeat(150)).await;
        trace.write(" tail").await;
        trace.finish().await;

        let text = trace.text();
        let (kept, notice) = text.split_at(100);
        assert_eq!(kept, "x".repeat(100));
        assert!(notice.starts_with("\nJob's log exceeded limit of 100 bytes."));
        assert!(!text.contains("tail"));
    }
}
