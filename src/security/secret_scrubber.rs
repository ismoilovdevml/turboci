use aho_corasick::{AhoCorasick, MatchKind};
use regex::{Regex, RegexSet};
use std::borrow::Cow;
use std::sync::OnceLock;

use crate::gitlab::Job;

/// Longest run of token-like characters held back while streaming
const MAX_HOLDBACK: usize = 256;
/// Most output ever held back: an unbounded pattern (e.g. `password=` followed by
/// megabytes without spaces) must not stop the log or grow memory without limit
const MAX_PENDING: usize = 16 * 1024;
/// Shorter exact secrets are not masked (they would mask ordinary words)
const MIN_SECRET_LEN: usize = 5;
const MASK: &str = "[MASKED]";

/// Secrets that look like tokens, keys or passwords, masked in every job
const DEFAULT_PATTERNS: [&str; 12] = [
    // GitLab tokens
    r"glrt-[a-zA-Z0-9_-]{20,}",
    r"GR1348941[a-zA-Z0-9]{20,}",
    // GitHub tokens
    r"gh[pousr]_[A-Za-z0-9_]{36,}",
    // AWS credentials
    r"AKIA[0-9A-Z]{16}",
    r"aws_secret_access_key\s*=\s*[A-Za-z0-9/+=]{40}",
    // Generic API keys
    r"api[_-]?key\s*[:=]\s*[a-zA-Z0-9_-]{32,}",
    // Generic secrets
    r"secret\s*[:=]\s*[a-zA-Z0-9_-]{16,}",
    // Passwords
    r"password\s*[:=]\s*\S{8,}",
    // Private keys
    r"-----BEGIN [A-Z]+ PRIVATE KEY-----",
    // JWT tokens
    r"eyJ[a-zA-Z0-9_-]*\.eyJ[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*",
    // Database URLs
    r"(postgres|mysql|mongodb)://[^:]+:([^@]+)@",
    // Redis URLs with passwords
    r"redis://[^:]*:([^@]+)@",
];

/// The default patterns, compiled once and shared by every job
struct Patterns {
    list: Vec<Regex>,
    /// All of `list` in one pass, to skip text none of them matches
    set: RegexSet,
}

fn default_patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        list: DEFAULT_PATTERNS
            .iter()
            .map(|p| Regex::new(p).unwrap())
            .collect(),
        set: RegexSet::new(DEFAULT_PATTERNS).unwrap(),
    })
}

/// Characters that can be part of a token, key or password matched by the patterns
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-_+/=.:@%~".contains(c)
}

/// Length of the longest proper prefix of `secret` that `text` ends with
fn suffix_prefix_len(text: &str, secret: &str) -> usize {
    let Some(&last) = text.as_bytes().last() else {
        return 0;
    };
    // Only a prefix ending in the same byte as `text` can match
    let bytes = secret.as_bytes();
    (1..secret.len().min(text.len() + 1))
        .rev()
        .filter(|&k| bytes[k - 1] == last)
        .find(|&k| secret.is_char_boundary(k) && text.ends_with(&secret[..k]))
        .unwrap_or(0)
}

/// Secret scrubber for masking sensitive information in logs and traces
#[derive(Clone)]
pub struct SecretScrubber {
    secrets: Vec<String>,
    /// Finds any maskable secret in one pass; `None` when there is none
    /// (or the automaton could not be built)
    finder: Option<AhoCorasick>,
}

impl SecretScrubber {
    /// Create a new secret scrubber with custom secrets
    pub fn new(secrets: Vec<String>) -> Self {
        let mut scrubber = Self {
            secrets,
            finder: None,
        };
        scrubber.build_finder();
        scrubber
    }

    fn build_finder(&mut self) {
        let maskable: Vec<&str> = self.maskable_secrets().collect();
        self.finder = if maskable.is_empty() {
            None
        } else {
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(maskable)
                .ok()
        };
    }

    /// Secrets in masking order, without those too short to mask
    fn maskable_secrets(&self) -> impl Iterator<Item = &str> {
        self.secrets
            .iter()
            .map(String::as_str)
            .filter(|s| s.len() >= MIN_SECRET_LEN)
    }

    /// Whether `text` contains any secret that is masked
    fn contains_secret(&self, text: &str) -> bool {
        match &self.finder {
            Some(finder) => finder.is_match(text),
            // No automaton: check each secret (there is none in the common case)
            None => self.maskable_secrets().any(|s| text.contains(s)),
        }
    }

    /// Scrub secrets from text, replacing them with [MASKED]. Text with nothing
    /// to mask is returned as it is, without a copy.
    pub fn scrub<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let patterns = default_patterns();
        if !self.contains_secret(text) && !patterns.set.is_match(text) {
            return Cow::Borrowed(text);
        }

        // Mask custom secrets (exact match), in order: longer ones first
        let mut result = text.to_string();
        for secret in self.maskable_secrets() {
            if result.contains(secret) {
                result = result.replace(secret, MASK);
            }
        }

        // Mask pattern-based secrets one pattern after the other: a pattern can
        // match text that includes what an earlier one masked
        for pattern in &patterns.list {
            if let Cow::Owned(masked) = pattern.replace_all(&result, MASK) {
                result = masked;
            }
        }

        Cow::Owned(result)
    }

    /// `scrub` for an owned string, which is returned as it is when nothing is masked
    fn scrub_owned(&self, text: String) -> String {
        match self.scrub(&text) {
            Cow::Owned(masked) => masked,
            Cow::Borrowed(_) => text,
        }
    }

    /// Add a custom secret to the scrubber
    pub fn add_secret(&mut self, secret: String) {
        if self.push_secret(secret) {
            self.build_finder();
        }
    }

    /// Adds `secret` unless it is empty or known; the finder is not rebuilt
    fn push_secret(&mut self, secret: String) -> bool {
        let new = !secret.is_empty() && !self.secrets.contains(&secret);
        if new {
            self.secrets.push(secret);
        }
        new
    }

    /// Copy of this scrubber that also masks the job's token, dependency tokens
    /// and masked variables (plus their URL-encoded forms)
    pub fn with_job_secrets(&self, job: &Job) -> Self {
        let mut scrubber = self.clone();
        let values = job
            .variables
            .iter()
            .filter(|v| v.masked)
            .filter_map(|v| v.value.clone())
            .chain(std::iter::once(job.token.clone()))
            .chain(job.dependencies.iter().map(|d| d.token.clone()));
        for value in values {
            let encoded = percent_encode(&value);
            if encoded != value {
                scrubber.push_secret(encoded);
            }
            scrubber.push_secret(value);
        }
        // Mask longer secrets first so a shorter one cannot break a longer match
        scrubber.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        scrubber.build_finder();
        scrubber
    }

    /// Byte ranges of everything `scrub` would mask
    fn match_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        if self.contains_secret(text) {
            for secret in self.maskable_secrets() {
                ranges.extend(
                    text.match_indices(secret)
                        .map(|(start, m)| (start, start + m.len())),
                );
            }
        }
        let patterns = default_patterns();
        if patterns.set.is_match(text) {
            for pattern in &patterns.list {
                ranges.extend(pattern.find_iter(text).map(|m| (m.start(), m.end())));
            }
        }
        ranges
    }

    fn longest_secret(&self) -> usize {
        self.secrets.iter().map(String::len).max().unwrap_or(0)
    }
}

/// Percent-encode everything except RFC 3986 unreserved characters
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Scrubs a stream of output chunks. Only a tail that could be the start of a
/// secret is held back until the next chunk, so a secret split across chunks is
/// still masked while ordinary output (e.g. a finished line) goes out right away.
pub struct StreamScrubber<'a> {
    scrubber: &'a SecretScrubber,
    pending: String,
    /// Inside an over-long match that was already masked: drop output until whitespace
    swallowing: bool,
}

impl<'a> StreamScrubber<'a> {
    pub fn new(scrubber: &'a SecretScrubber) -> Self {
        Self {
            scrubber,
            pending: String::new(),
            swallowing: false,
        }
    }

    /// Feed a chunk; returns scrubbed output that is safe to emit now
    pub fn push(&mut self, chunk: &str) -> String {
        let mut chunk = chunk;
        if self.swallowing {
            // Unbounded patterns end at whitespace
            match chunk.find(char::is_whitespace) {
                Some(end) => {
                    chunk = &chunk[end..];
                    self.swallowing = false;
                }
                None => return String::new(),
            }
        }
        self.pending.push_str(chunk);
        let len = self.pending.len();

        // The start of an exact secret, or a token still being written
        let secret_start = self
            .scrubber
            .secrets
            .iter()
            .map(|secret| suffix_prefix_len(&self.pending, secret))
            .max()
            .unwrap_or(0);
        // Token characters are ASCII, so this many chars is MAX_HOLDBACK bytes
        let token_run = self
            .pending
            .chars()
            .rev()
            .take(MAX_HOLDBACK)
            .take_while(|&c| is_token_char(c))
            .count();
        let mut boundary = len - secret_start.max(token_run);
        while !self.pending.is_char_boundary(boundary) {
            boundary -= 1;
        }
        // Never cut through a match, and keep matches touching the end (they may
        // grow). Moving the boundary back can put it inside another match, so
        // repeat until no match is cut.
        let matches = self.scrubber.match_ranges(&self.pending);
        let mut moved = true;
        while moved {
            moved = false;
            for &(start, end) in &matches {
                if start < boundary && (end > boundary || end == len) {
                    boundary = start;
                    moved = true;
                }
            }
        }
        // ...unless that holds back too much. Exact secrets are never split (the
        // limit is longer than the longest one); a pattern match still growing at
        // the limit is masked as a whole and its continuation dropped
        let limit = MAX_PENDING.max(self.scrubber.longest_secret() + MAX_HOLDBACK);
        if len - boundary > limit {
            if let Some(start) = matches
                .iter()
                .filter(|&&(_, end)| end == len)
                .map(|&(start, _)| start)
                .min()
            {
                let head = self.pending[..start].to_string();
                self.pending.clear();
                self.swallowing = true;
                return self.scrubber.scrub(&head).into_owned() + MASK;
            }
            boundary = len - limit;
            while !self.pending.is_char_boundary(boundary) {
                boundary += 1;
            }
        }

        let rest = self.pending.split_off(boundary);
        let ready = std::mem::replace(&mut self.pending, rest);
        // Every secret or pattern match inside `ready` starts before the boundary
        // (the search above reports the leftmost one), so without such a match
        // there is nothing to mask
        if matches.iter().all(|&(start, _)| start >= boundary) {
            return ready;
        }
        self.scrubber.scrub_owned(ready)
    }

    /// Flush everything still held back
    pub fn finish(&mut self) -> String {
        self.swallowing = false;
        let rest = std::mem::take(&mut self.pending);
        self.scrubber.scrub_owned(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_with_secrets() -> Job {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "token": "job-token-abc123",
            "variables": [
                {"key": "DEPLOY_KEY", "value": "s3cr3t/with+chars", "masked": true},
                {"key": "PUBLIC", "value": "visible-value", "masked": false}
            ],
            "dependencies": [{"id": 2, "name": "build", "token": "dep-token-xyz789"}]
        }))
        .unwrap()
    }

    #[test]
    fn job_scrubber_masks_token_dependencies_masked_vars_and_encoded_forms() {
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(&job_with_secrets());
        let text = "clone https://gitlab-ci-token:job-token-abc123@host/r.git \
                    dep=dep-token-xyz789 key=s3cr3t/with+chars enc=s3cr3t%2Fwith%2Bchars \
                    public=visible-value";

        let out = scrubber.scrub(text);

        for secret in [
            "job-token-abc123",
            "dep-token-xyz789",
            "s3cr3t/with+chars",
            "s3cr3t%2Fwith%2Bchars",
        ] {
            assert!(!out.contains(secret), "{} leaked: {}", secret, out);
        }
        assert!(out.contains("visible-value"));
    }

    #[test]
    fn stream_scrubber_masks_secret_split_across_chunks() {
        let scrubber = SecretScrubber::new(vec!["supersecretvalue".to_string()]);
        let mut stream = StreamScrubber::new(&scrubber);

        let mut out = String::new();
        for chunk in ["echo super", "secret", "value done\n"] {
            out.push_str(&stream.push(chunk));
        }
        out.push_str(&stream.finish());

        assert_eq!(out, "echo [MASKED] done\n");
    }

    #[test]
    fn stream_scrubber_emits_everything_and_handles_multibyte() {
        let scrubber = SecretScrubber::new(vec!["hidden-secret".to_string()]);
        let mut stream = StreamScrubber::new(&scrubber);
        let input = "ok ✓ ".repeat(100) + "hidden-secret ✓";
        let chars: Vec<char> = input.chars().collect();

        let mut out = String::new();
        for chunk in chars.chunks(7) {
            out.push_str(&stream.push(&chunk.iter().collect::<String>()));
        }
        out.push_str(&stream.finish());

        assert_eq!(out, "ok ✓ ".repeat(100) + "[MASKED] ✓");
    }

    #[test]
    fn stream_scrubber_emits_finished_lines_immediately() {
        let scrubber = SecretScrubber::new(vec!["some-long-secret-value".to_string()]);
        let mut stream = StreamScrubber::new(&scrubber);

        assert_eq!(stream.push("Preparing...\n"), "Preparing...\n");
        assert_eq!(stream.push("value: some-long"), "value: ");
        assert_eq!(stream.push("-secret-value\n"), "[MASKED]\n");
    }

    #[test]
    fn stream_scrubber_holds_back_a_bounded_amount_and_stays_fast() {
        let scrubber = SecretScrubber::new(vec![]);
        let mut stream = StreamScrubber::new(&scrubber);
        let chunk = "a".repeat(8 * 1024);
        let started = std::time::Instant::now();

        let mut out = stream.push("log: password=");
        for _ in 0..256 {
            out.push_str(&stream.push(&chunk));
            assert!(stream.pending.len() <= MAX_PENDING + chunk.len());
        }
        out.push_str(&stream.push(" next line\n"));
        out.push_str(&stream.finish());

        // The over-long "password" is masked as a whole, output resumes after it
        assert_eq!(out, "log: [MASKED] next line\n");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn stream_scrubber_holds_back_pattern_touching_chunk_end() {
        let scrubber = SecretScrubber::new(vec![]);
        let mut stream = StreamScrubber::new(&scrubber);
        let token = "glrt-TESTtoken0000synthetic1111notreal22";

        let mut out = stream.push(&format!("{}{}", "x".repeat(200), &token[..30]));
        out.push_str(&stream.push(&format!("{} end", &token[30..])));
        out.push_str(&stream.finish());

        assert!(!out.contains("glrt-"), "{}", out);
        assert!(!out.contains(&token[30..]), "{}", out);
    }

    /// Stream `text` split at every position and check each result against `scrub`
    fn assert_stream_matches_scrub_at_every_split(scrubber: &SecretScrubber, text: &str) {
        let whole = scrubber.scrub(text);
        for split in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
            let mut stream = StreamScrubber::new(scrubber);
            let mut out = stream.push(&text[..split]);
            out.push_str(&stream.push(&text[split..]));
            out.push_str(&stream.finish());
            assert_eq!(out, whole, "split at {split}");
        }
    }

    /// Exact secrets, and pattern matches without whitespace, are masked however the
    /// output is split. (A match with whitespace, like `password: value`, is not
    /// held back at the space: split there, its value goes out unmasked.)
    #[test]
    fn stream_scrubber_masks_secrets_and_patterns_split_anywhere() {
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(&job_with_secrets());
        let text = "clone https://gitlab-ci-token:job-token-abc123@host/r.git\n\
                    key=s3cr3t/with+chars enc=s3cr3t%2Fwith%2Bchars ✓\n\
                    Runner token: glrt-TESTtoken0000synthetic1111notreal22 done\n\
                    password=MyP@ssw0rd123\n";

        assert_stream_matches_scrub_at_every_split(&scrubber, text);
        let masked = scrubber.scrub(text);
        for leaked in ["job-token", "s3cr3t", "glrt-", "MyP@ss"] {
            assert!(!masked.contains(leaked), "{leaked} leaked: {masked}");
        }
    }

    #[test]
    fn longest_overlapping_secret_wins() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 1,
            "token": "job-token-abc123",
            "variables": [
                {"key": "SHORT", "value": "cdef12", "masked": true},
                {"key": "LONG", "value": "abcdef1234", "masked": true}
            ]
        }))
        .unwrap();
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(&job);
        let text = "key=abcdef1234 and cdef12\n";

        assert_eq!(scrubber.scrub(text), "key=[MASKED] and [MASKED]\n");
        assert_stream_matches_scrub_at_every_split(&scrubber, text);
    }

    #[test]
    fn text_without_secrets_is_returned_as_is() {
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(&job_with_secrets());
        let text = "   Compiling serde v1.0.200\nnpm WARN deprecated inflight@1.0.6 ✓\n";

        let scrubbed = scrubber.scrub(text);
        assert!(matches!(scrubbed, Cow::Borrowed(_)));
        assert_eq!(scrubbed, text);

        let mut stream = StreamScrubber::new(&scrubber);
        assert_eq!(stream.push(text), text);
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn stream_scrubber_holds_back_a_secret_that_overlaps_a_held_back_match() {
        // The secret ends inside a pattern match that runs to the end of the
        // chunk: holding back from the match's start alone would cut the secret
        let scrubber = SecretScrubber::new(vec!["pw-s3cr3t-glrt-ab".to_string()]);
        let mut stream = StreamScrubber::new(&scrubber);
        let first = format!("x pw-s3cr3t-glrt-ab{}", "c".repeat(300));

        let mut out = stream.push(&first);
        assert_eq!(out, "x ");
        out.push_str(&stream.push(" end\n"));
        out.push_str(&stream.finish());

        assert!(!out.contains("s3cr3t"), "{out}");
        assert_eq!(out, scrubber.scrub(&format!("{first} end\n")));
    }

    #[test]
    fn test_scrub_gitlab_token() {
        let scrubber = SecretScrubber::new(vec![]);
        let text = "Runner token: glrt-TESTtoken0000synthetic1111notreal22";
        let scrubbed = scrubber.scrub(text);
        assert_eq!(scrubbed, "Runner token: [MASKED]");
    }

    #[test]
    fn test_scrub_custom_secret() {
        let scrubber = SecretScrubber::new(vec!["my-super-secret-123".to_string()]);
        let text = "Using secret: my-super-secret-123 for auth";
        let scrubbed = scrubber.scrub(text);
        assert_eq!(scrubbed, "Using secret: [MASKED] for auth");
    }

    #[test]
    fn test_scrub_password() {
        let scrubber = SecretScrubber::new(vec![]);
        let text = "password: MyP@ssw0rd123";
        let scrubbed = scrubber.scrub(text);
        assert_eq!(scrubbed, "[MASKED]");
    }

    #[test]
    fn test_scrub_aws_key() {
        let scrubber = SecretScrubber::new(vec![]);
        let text = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let scrubbed = scrubber.scrub(text);
        assert_eq!(scrubbed, "AWS_ACCESS_KEY_ID=[MASKED]");
    }

    #[test]
    fn test_scrub_database_url() {
        let scrubber = SecretScrubber::new(vec![]);
        let text = "postgres://user:secretpass123@localhost/db";
        let scrubbed = scrubber.scrub(text);
        assert!(scrubbed.contains("[MASKED]"));
        assert!(!scrubbed.contains("secretpass123"));
    }

    /// Log lines like a build prints, with a masked value now and then
    fn bench_log(bytes: usize) -> String {
        let lines = [
            "   Compiling serde_json v1.0.120 (/builds/group/project/vendor/serde_json)\n",
            "npm WARN deprecated inflight@1.0.6: This module is not supported, and leaks memory.\n",
            "2026-11-20T10:15:42.123Z INFO  app::server > listening on 0.0.0.0:8080 (pid 4242)\n",
            "Step 7/12 : RUN apt-get update && apt-get install -y --no-install-recommends curl\n",
            "test integration::cache::restores_archive_from_s3 ... ok\n",
            "Downloading https://registry.example.com/v2/library/node/blobs/sha256:4f4fb700ef54\n",
        ];
        let mut log = String::with_capacity(bytes + 128);
        let mut i = 0;
        while log.len() < bytes {
            if i % 1000 == 999 {
                log.push_str("export DEPLOY=s3cr3t-value-07 done\n");
            }
            log.push_str(lines[i % lines.len()]);
            i += 1;
        }
        log
    }

    fn bench_scrubber() -> SecretScrubber {
        let variables: Vec<_> = (0..9)
            .map(|i| serde_json::json!({"key": format!("V{i}"), "value": format!("s3cr3t-value-{i:02}"), "masked": true}))
            .collect();
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 1, "token": "glcbt-64_benchjobtoken0000000000", "variables": variables
        }))
        .unwrap();
        SecretScrubber::new(vec!["glrt-benchrunnertoken00000000000".to_string()])
            .with_job_secrets(&job)
    }

    fn bench_run(name: &str, log: &str, scrubber: &SecretScrubber) {
        use std::hash::{Hash, Hasher};
        let report = |what: &str, secs: f64, out: &str| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            out.hash(&mut hash);
            println!(
                "{name} {what}: {:.1} MB/s (output hash {:x})",
                log.len() as f64 / secs / 1e6,
                hash.finish()
            );
        };

        let mut out = String::with_capacity(log.len());
        let mut stream = StreamScrubber::new(scrubber);
        let started = std::time::Instant::now();
        for chunk in log.as_bytes().chunks(8192) {
            out.push_str(&stream.push(std::str::from_utf8(chunk).unwrap()));
        }
        out.push_str(&stream.finish());
        report("stream", started.elapsed().as_secs_f64(), &out);

        out.clear();
        let started = std::time::Instant::now();
        for chunk in log.as_bytes().chunks(8192) {
            out.push_str(&scrubber.scrub(std::str::from_utf8(chunk).unwrap()));
        }
        report("scrub", started.elapsed().as_secs_f64(), &out);
    }

    /// cargo test --release bench_stream_scrubber -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_stream_scrubber() {
        let scrubber = bench_scrubber();
        bench_run("plain", &bench_log(50 * 1024 * 1024), &scrubber);
        let line = "2026-11-20T10:15:42Z password=notreallysecret0 api_key=0123456789abcdef0123456789abcdef s3cr3t-value-03 ok\n";
        bench_run(
            "matching",
            &line.repeat(50 * 1024 * 1024 / line.len()),
            &scrubber,
        );
    }
}
