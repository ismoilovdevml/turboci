use regex::Regex;
use std::sync::OnceLock;

use crate::gitlab::Job;

/// Output kept back while streaming so a pattern match is not cut in half
const MIN_HOLDBACK: usize = 64;

/// Secret scrubber for masking sensitive information in logs and traces
#[derive(Clone)]
pub struct SecretScrubber {
    #[allow(dead_code)]
    secrets: Vec<String>,
    patterns: Vec<Regex>,
}

#[allow(dead_code)]
static DEFAULT_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

impl SecretScrubber {
    /// Create a new secret scrubber with custom secrets
    pub fn new(secrets: Vec<String>) -> Self {
        let patterns = Self::get_default_patterns();

        Self {
            secrets,
            patterns: patterns.clone(),
        }
    }

    /// Get default secret patterns (tokens, keys, passwords, etc.)
    fn get_default_patterns() -> &'static Vec<Regex> {
        DEFAULT_PATTERNS.get_or_init(|| {
            vec![
                // GitLab tokens
                Regex::new(r"glrt-[a-zA-Z0-9_-]{20,}").unwrap(),
                Regex::new(r"GR1348941[a-zA-Z0-9]{20,}").unwrap(),
                // GitHub tokens
                Regex::new(r"gh[pousr]_[A-Za-z0-9_]{36,}").unwrap(),
                // AWS credentials
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                Regex::new(r"aws_secret_access_key\s*=\s*[A-Za-z0-9/+=]{40}").unwrap(),
                // Generic API keys
                Regex::new(r"api[_-]?key\s*[:=]\s*[a-zA-Z0-9_-]{32,}").unwrap(),
                // Generic secrets
                Regex::new(r"secret\s*[:=]\s*[a-zA-Z0-9_-]{16,}").unwrap(),
                // Passwords
                Regex::new(r"password\s*[:=]\s*\S{8,}").unwrap(),
                // Private keys
                Regex::new(r"-----BEGIN [A-Z]+ PRIVATE KEY-----").unwrap(),
                // JWT tokens
                Regex::new(r"eyJ[a-zA-Z0-9_-]*\.eyJ[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*").unwrap(),
                // Database URLs
                Regex::new(r"(postgres|mysql|mongodb)://[^:]+:([^@]+)@").unwrap(),
                // Redis URLs with passwords
                Regex::new(r"redis://[^:]*:([^@]+)@").unwrap(),
            ]
        })
    }

    /// Scrub secrets from text, replacing them with [MASKED]
    pub fn scrub(&self, text: &str) -> String {
        let mut result = text.to_string();

        // Mask custom secrets (exact match)
        for secret in &self.secrets {
            if !secret.is_empty() && secret.len() > 4 {
                result = result.replace(secret, "[MASKED]");
            }
        }

        // Mask pattern-based secrets
        for pattern in &self.patterns {
            result = pattern.replace_all(&result, "[MASKED]").to_string();
        }

        result
    }

    /// Add a custom secret to the scrubber
    pub fn add_secret(&mut self, secret: String) {
        if !secret.is_empty() && !self.secrets.contains(&secret) {
            self.secrets.push(secret);
        }
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
                scrubber.add_secret(encoded);
            }
            scrubber.add_secret(value);
        }
        // Mask longer secrets first so a shorter one cannot break a longer match
        scrubber.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        scrubber
    }

    /// Byte ranges of everything `scrub` would mask
    fn match_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        for secret in self.secrets.iter().filter(|s| s.len() > 4) {
            ranges.extend(
                text.match_indices(secret.as_str())
                    .map(|(start, m)| (start, start + m.len())),
            );
        }
        for pattern in &self.patterns {
            ranges.extend(pattern.find_iter(text).map(|m| (m.start(), m.end())));
        }
        ranges
    }

    fn longest_secret(&self) -> usize {
        self.secrets.iter().map(String::len).max().unwrap_or(0)
    }

    /// Add a custom pattern to the scrubber
    #[allow(dead_code)]
    pub fn add_pattern(&mut self, pattern: Regex) {
        self.patterns.push(pattern);
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

/// Scrubs a stream of output chunks. The tail of each chunk is held back until the
/// next one arrives, so a secret split across chunks is still masked.
pub struct StreamScrubber<'a> {
    scrubber: &'a SecretScrubber,
    pending: String,
    holdback: usize,
}

impl<'a> StreamScrubber<'a> {
    pub fn new(scrubber: &'a SecretScrubber) -> Self {
        Self {
            scrubber,
            pending: String::new(),
            holdback: scrubber.longest_secret().max(MIN_HOLDBACK),
        }
    }

    /// Feed a chunk; returns scrubbed output that is safe to emit now
    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let len = self.pending.len();

        let mut boundary = len.saturating_sub(self.holdback);
        while !self.pending.is_char_boundary(boundary) {
            boundary -= 1;
        }
        // Never cut through a match, and keep matches touching the end (they may grow)
        for (start, end) in self.scrubber.match_ranges(&self.pending) {
            if start < boundary && (end > boundary || end == len) {
                boundary = start;
            }
        }

        let rest = self.pending.split_off(boundary);
        let ready = std::mem::replace(&mut self.pending, rest);
        self.scrubber.scrub(&ready)
    }

    /// Flush everything still held back
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        self.scrubber.scrub(&rest)
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
    fn stream_scrubber_holds_back_pattern_touching_chunk_end() {
        let scrubber = SecretScrubber::new(vec![]);
        let mut stream = StreamScrubber::new(&scrubber);
        let token = "glrt-ztr69pD0SBp9pB1og6xgE286MQp0OjEKdToxCw";

        let mut out = stream.push(&format!("{}{}", "x".repeat(200), &token[..30]));
        out.push_str(&stream.push(&format!("{} end", &token[30..])));
        out.push_str(&stream.finish());

        assert!(!out.contains("glrt-"), "{}", out);
        assert!(!out.contains(&token[30..]), "{}", out);
    }

    #[test]
    fn test_scrub_gitlab_token() {
        let scrubber = SecretScrubber::new(vec![]);
        let text = "Runner token: glrt-ztr69pD0SBp9pB1og6xgE286MQp0OjEKdToxCw";
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
}
