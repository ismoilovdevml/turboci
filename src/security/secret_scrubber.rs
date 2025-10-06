use regex::Regex;
use std::sync::OnceLock;

/// Secret scrubber for masking sensitive information in logs and traces
#[derive(Clone)]
pub struct SecretScrubber {
    secrets: Vec<String>,
    patterns: Vec<Regex>,
}

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

    /// Add a custom pattern to the scrubber
    #[allow(dead_code)]
    pub fn add_pattern(&mut self, pattern: Regex) {
        self.patterns.push(pattern);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
