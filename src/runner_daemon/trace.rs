//! Job log sent to GitLab. One writer per job owns the whole (scrubbed) trace and the
//! offset GitLab has acknowledged, so output is never lost or duplicated: after a
//! 416 range mismatch it resends from the offset the server reports.
//!
//! Only output GitLab has not acknowledged is kept in memory. Acknowledged output
//! is moved to an unlinked temporary file (the spool) and read back from there if
//! GitLab asks for it again.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
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
/// Room for the notice appended when the limit is reached
const NOTICE_ROOM: usize = 256;
/// Spool reads and writes up to this size (a usual flush: a copy to or from
/// the page cache) run on the async runtime; larger ones, after GitLab was
/// unreachable for a while, run on the blocking pool
const SPOOL_INLINE_BYTES: usize = 1024 * 1024;
/// Memory kept for unacknowledged output once it was spooled: a backlog that
/// grew the buffer past this does not keep it allocated for the rest of the job
const TAIL_RETAIN: usize = 2 * FLUSH_BYTES;

/// Where acknowledged output goes
enum Spool {
    /// Created with the first acknowledged output
    Unopened,
    Open(Arc<File>),
    /// Could not be created or written: output stays in memory from then on,
    /// what was spooled before is still read from the file
    Failed(Option<Arc<File>>),
}

pub struct TraceWriter<'a> {
    client: Option<&'a GitLabClient>,
    job_id: u64,
    token: String,
    scrubber: StreamScrubber<'a>,
    spool: Spool,
    /// Length of the trace prefix in the spool
    spooled: usize,
    /// The trace from `spooled` on
    tail: Vec<u8>,
    /// Bytes GitLab has acknowledged; below `spooled` only when GitLab asked
    /// for output again
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
            spool: Spool::Unopened,
            spooled: 0,
            tail: Vec::new(),
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

    /// Whether the log reached its limit: further output is dropped, so callers
    /// can stop producing (or decoding) it
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Append output; it is scrubbed and sent in batches
    pub async fn write(&mut self, text: &str) {
        if self.is_truncated() {
            // Dropped anyway: do not scrub it, but keep sending what is pending
            self.flush_if_due().await;
            return;
        }
        let safe = self.scrubber.push(text);
        self.append(&safe);
        if self.len() - self.sent >= FLUSH_BYTES {
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
        let len = self.len();
        let Some(client) = self.client else {
            self.sent = len;
            self.spool_acknowledged().await;
            return true;
        };
        if self.sent >= len {
            return true;
        }
        let Some(body) = self.unsent().await else {
            return false;
        };

        match client
            .patch_trace(self.job_id, &self.token, body, self.sent)
            .await
        {
            // On success this is the end of what we sent; on 416 it is what GitLab has
            Ok(patch) => {
                self.sent = patch.offset.min(len);
                self.cancel.update(patch.remote);
                if let Some(interval) = patch.update_interval {
                    self.update_interval = interval.min(MAX_UPDATE_INTERVAL);
                }
                if patch.remote == crate::gitlab::RemoteState::Aborted {
                    // GitLab no longer accepts output for this job
                    self.sent = len;
                }
            }
            Err(e) => warn!("Failed to send trace for job #{}: {}", self.job_id, e),
        }
        self.spool_acknowledged().await;
        self.sent >= len
    }

    /// Flush held-back output and make sure GitLab has the whole trace
    pub async fn finish(&mut self) {
        if !self.truncated {
            let rest = self.scrubber.finish();
            self.append(&rest);
        }
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
            self.len()
        );
    }

    /// Scrubbed output written so far
    #[cfg(test)]
    pub fn text(&self) -> String {
        let mut trace = vec![0; self.spooled];
        if let Some(file) = self.spool_file() {
            file.read_exact_at(&mut trace, 0).expect("read trace spool");
        }
        trace.extend_from_slice(&self.tail);
        String::from_utf8_lossy(&trace).into_owned()
    }

    /// Length of the whole trace
    fn len(&self) -> usize {
        self.spooled + self.tail.len()
    }

    fn spool_file(&self) -> Option<&Arc<File>> {
        match &self.spool {
            Spool::Open(file) | Spool::Failed(Some(file)) => Some(file),
            Spool::Unopened | Spool::Failed(None) => None,
        }
    }

    /// The trace from `sent` on, read back from the spool when GitLab asked for
    /// output it had acknowledged before
    async fn unsent(&self) -> Option<bytes::Bytes> {
        if self.sent >= self.spooled {
            return Some(bytes::Bytes::copy_from_slice(
                &self.tail[self.sent - self.spooled..],
            ));
        }
        let Some(file) = self.spool_file().map(Arc::clone) else {
            // Not expected: everything below `spooled` was written to the spool
            warn!("Job #{} trace from {} is gone", self.job_id, self.sent);
            return None;
        };
        let (offset, count, tail) = (self.sent, self.spooled - self.sent, self.tail.len());
        // Read straight into the body, with room for the tail after it
        let read = move || {
            let mut body = vec![0; count];
            body.reserve_exact(tail);
            file.read_exact_at(&mut body, offset as u64).map(|()| body)
        };
        let read = if count <= SPOOL_INLINE_BYTES {
            read()
        } else {
            tokio::task::spawn_blocking(read)
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        };
        match read {
            Ok(mut body) => {
                body.extend_from_slice(&self.tail);
                Some(body.into())
            }
            Err(e) => {
                warn!(
                    "Job #{} trace from {} could not be read back: {}",
                    self.job_id, self.sent, e
                );
                None
            }
        }
    }

    /// Move acknowledged output from memory to the spool. Cancel safe: the
    /// memory is released only once the spool has the output.
    async fn spool_acknowledged(&mut self) {
        let count = self.sent.saturating_sub(self.spooled).min(self.tail.len());
        if count == 0 {
            return;
        }
        if matches!(self.spool, Spool::Unopened) {
            self.spool = match tempfile::tempfile() {
                Ok(file) => Spool::Open(Arc::new(file)),
                Err(e) => {
                    warn!(
                        "Job #{} log kept in memory: no temporary file for it: {}",
                        self.job_id, e
                    );
                    Spool::Failed(None)
                }
            };
        }
        let file = match &self.spool {
            Spool::Open(file) => Arc::clone(file),
            Spool::Unopened | Spool::Failed(_) => return,
        };
        let offset = self.spooled as u64;
        let written = if count <= SPOOL_INLINE_BYTES {
            file.write_all_at(&self.tail[..count], offset)
        } else {
            // A copy, so the tail is untouched if this future is dropped
            let (file, acknowledged) = (Arc::clone(&file), self.tail[..count].to_vec());
            tokio::task::spawn_blocking(move || file.write_all_at(&acknowledged, offset))
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        };
        if let Err(e) = written {
            warn!(
                "Job #{} log kept in memory: writing it to a temporary file failed: {}",
                self.job_id, e
            );
            self.spool = Spool::Failed(Some(file));
            return;
        }
        self.tail.drain(..count);
        self.spooled += count;
        if self.tail.capacity() > TAIL_RETAIN {
            self.tail.shrink_to(FLUSH_BYTES.max(self.tail.len()));
        }
    }

    fn append(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let room = self.limit.saturating_sub(self.len());
        if text.len() <= room {
            self.reserve(text.len());
            self.tail.extend_from_slice(text.as_bytes());
            return;
        }
        let mut cut = room;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let notice = format!(
            "\nJob's log exceeded limit of {} bytes.\nJob execution will continue but no more output will be collected.\n",
            self.limit
        );
        self.reserve(cut + notice.len());
        self.tail.extend_from_slice(&text.as_bytes()[..cut]);
        self.tail.extend_from_slice(notice.as_bytes());
        self.truncated = true;
    }

    /// Grow like `Vec` does, but never past what is left of the limit plus the
    /// notice, so a full trace does not double the buffer
    fn reserve(&mut self, additional: usize) {
        let needed = self.tail.len() + additional;
        if needed > self.tail.capacity() {
            let target = (self.tail.capacity() * 2)
                .min((self.limit + NOTICE_ROOM).saturating_sub(self.spooled))
                .max(needed);
            self.tail.reserve_exact(target - self.tail.len());
        }
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

    /// The trace as GitLab stores it: each accepted PATCH continues at its offset
    async fn trace_on_gitlab(server: &MockServer) -> String {
        let mut trace = String::new();
        for (range, body) in patches(server).await {
            let start: usize = range.split_once('-').unwrap().0.parse().unwrap();
            if start <= trace.len() {
                trace.truncate(start);
                trace.push_str(&body);
            }
        }
        trace
    }

    async fn accept_all(server: &MockServer) {
        Mock::given(method("PATCH"))
            .and(path("/api/v4/jobs/7/trace"))
            .respond_with(ResponseTemplate::new(202))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn sends_consecutive_ranges_and_whole_trace() {
        let server = MockServer::start().await;
        accept_all(&server).await;
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
        accept_all(&server).await;
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
    async fn acknowledged_output_leaves_memory() {
        let server = MockServer::start().await;
        accept_all(&server).await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);

        let mut written = String::new();
        for i in 0..3 * 1024 {
            let line = format!("{:05} {}\n", i, "build output ".repeat(79));
            trace.write(&line).await;
            written.push_str(&line);
            // Memory holds at most what is waiting for GitLab, not the whole log
            assert!(trace.tail.len() < FLUSH_BYTES);
            assert!(trace.tail.capacity() <= TAIL_RETAIN);
        }
        assert!(written.len() > 3 * 1024 * 1024);
        assert!(trace.spooled > 2 * 1024 * 1024);
        trace.finish().await;

        assert_eq!(trace.spooled, written.len());
        assert!(trace.tail.is_empty());
        assert_eq!(trace.text(), written);
        assert_eq!(trace_on_gitlab(&server).await, written);
    }

    #[tokio::test]
    async fn resends_spooled_output_from_an_earlier_offset() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(202))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // GitLab lost part of what it had acknowledged
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(416).insert_header("Range", "0-4"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        accept_all(&server).await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);

        // Lines: the scrubber holds back a trailing token until it ends
        trace.write("0123456789\n").await;
        assert!(trace.flush().await);
        assert_eq!((trace.spooled, trace.tail.len()), (11, 0));
        trace.write("abc\n").await;
        assert!(!trace.flush().await);
        assert_eq!(trace.sent, 4);
        trace.finish().await;

        let sent = patches(&server).await;
        assert_eq!(sent[1], ("11-14".to_string(), "abc\n".to_string()));
        assert_eq!(sent[2], ("4-14".to_string(), "456789\nabc\n".to_string()));
        assert_eq!(trace.text(), "0123456789\nabc\n");
        assert_eq!(trace_on_gitlab(&server).await, "0123456789\nabc\n");
    }

    #[tokio::test]
    async fn resends_a_large_spooled_trace_from_the_start() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(202))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(416).insert_header("Range", "0-0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        accept_all(&server).await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);

        // Past SPOOL_INLINE_BYTES: spooled and read back on the blocking pool
        let big: String = (0..3 * SPOOL_INLINE_BYTES / 2)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .chain(std::iter::once('\n'))
            .collect();
        trace.write(&big).await;
        assert_eq!(trace.spooled, big.len());
        trace.write("end\n").await;
        assert!(!trace.flush().await);
        assert_eq!(trace.sent, 0);
        trace.finish().await;

        let sent = patches(&server).await;
        let whole = format!("{}end\n", big);
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[2].0, format!("0-{}", whole.len() - 1));
        assert!(sent[2].1 == whole);
        assert!(trace.text() == whole);
    }

    #[tokio::test]
    async fn keeps_the_log_in_memory_without_a_spool() {
        let server = MockServer::start().await;
        accept_all(&server).await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber);
        trace.spool = Spool::Failed(None);

        trace.write("first\n").await;
        assert!(trace.flush().await);
        trace.write("second\n").await;
        trace.finish().await;

        assert_eq!((trace.spooled, trace.sent), (0, 13));
        assert_eq!(trace.text(), "first\nsecond\n");
        assert_eq!(trace_on_gitlab(&server).await, "first\nsecond\n");
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

    #[tokio::test]
    async fn stops_collecting_at_limit_after_spooling() {
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, 7, "job-token", &scrubber).with_limit(100);

        trace.write(&"x\n".repeat(30)).await;
        trace.flush().await;
        assert_eq!(trace.spooled, 60);
        trace.write(&"y\n".repeat(30)).await;
        trace.finish().await;

        let text = trace.text();
        let (kept, notice) = text.split_at(100);
        assert_eq!(kept, format!("{}{}", "x\n".repeat(30), "y\n".repeat(20)));
        assert!(notice.starts_with("\nJob's log exceeded limit of 100 bytes."));
    }

    #[tokio::test]
    async fn stops_scrubbing_once_truncated() {
        let scrubber = SecretScrubber::new(vec!["some-masked-value".to_string()]);
        let mut trace = TraceWriter::new(None, 7, "job-token", &scrubber).with_limit(100);

        trace.write(&format!("{}\n", "x".repeat(150))).await;
        assert!(trace.is_truncated());
        let kept = trace.text();

        // Not scrubbed (a token at the end would be held back) and not kept
        trace.write("more output some-masked").await;
        assert_eq!(trace.scrubber.held_back(), "");
        trace.finish().await;
        assert_eq!(trace.text(), kept);
    }

    #[tokio::test]
    async fn sends_the_rest_after_truncation() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let client = GitLabClient::new(server.uri(), "t".to_string());
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(Some(&client), 7, "job-token", &scrubber).with_limit(10);

        trace.write("0123456789abc\n").await;
        assert!(trace.is_truncated());
        trace.last_flush -= DEFAULT_UPDATE_INTERVAL;
        trace.write("dropped\n").await;

        let body: String = patches(&server).await.into_iter().map(|(_, b)| b).collect();
        assert!(body.starts_with("0123456789\nJob's log exceeded limit of 10 bytes."));
        assert!(!body.contains("dropped"));
    }

    #[tokio::test]
    async fn buffer_does_not_grow_past_the_limit() {
        let scrubber = SecretScrubber::new(vec![]);
        let limit = 64 * 1024;
        let mut trace = TraceWriter::new(None, 7, "job-token", &scrubber).with_limit(limit);
        let line = "a line of build output\n";

        while !trace.is_truncated() {
            trace.write(line).await;
        }
        assert!(trace.len() > limit);
        assert!(trace.tail.capacity() <= limit + NOTICE_ROOM);
        trace.finish().await;
        assert!(trace.tail.capacity() <= limit + NOTICE_ROOM);
    }

    /// cargo test --release bench_trace_past_limit -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn bench_trace_past_limit() {
        let line = "2026-11-20T10:15:42Z password=notreallysecret0 api_key=0123456789abcdef0123456789abcdef ok\n";
        let chunk = line.repeat(8192 / line.len());
        let scrubber = SecretScrubber::new(vec!["some-masked-value".to_string()]);
        let mut trace = TraceWriter::new(None, 7, "job-token", &scrubber);
        let total = 50 * 1024 * 1024;
        let started = Instant::now();
        let mut written = 0;
        let mut peak = 0;
        while written < total {
            trace.write(&chunk).await;
            written += chunk.len();
            peak = peak.max(trace.tail.capacity());
        }
        trace.finish().await;
        let secs = started.elapsed().as_secs_f64();
        println!(
            "trace: {:.1} MB/s for {} bytes, kept {} (in memory: peak capacity {}, now {})",
            written as f64 / secs / 1e6,
            written,
            trace.len(),
            peak,
            trace.tail.capacity()
        );
    }
}
