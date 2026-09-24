//! Runner authentication token rotation, like gitlab-runner: when GitLab gives
//! the token an expiry, a new one is requested after 3/4 of its lifetime.
//!
//! The service cannot write its config (it lives in /etc), so a rotated token
//! is kept in the state directory, tied to the config token it replaces:
//! putting another token in the config makes the runner use that one again.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Share of a token's lifetime after which it is reset (gitlab-runner's
/// TokenResetIntervalFactor)
const RESET_FACTOR: f64 = 0.75;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredToken {
    /// Fingerprint of the config token this one replaces
    replaces: String,
    pub token: String,
    /// Unix seconds
    pub obtained_at: i64,
    /// Unix seconds; `None` when the token does not expire
    pub expires_at: Option<i64>,
}

pub struct TokenStore {
    path: PathBuf,
    replaces: String,
}

impl TokenStore {
    pub fn new(state_dir: &str, config_token: &str) -> Self {
        let id = blake3::derive_key("turboci runner token", config_token.as_bytes());
        Self {
            path: Path::new(state_dir).join("runner_token.json"),
            replaces: id.iter().map(|b| format!("{:02x}", b)).collect(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The rotated token, unless the config token was changed since
    pub fn load(&self) -> Option<StoredToken> {
        let contents = std::fs::read_to_string(&self.path).ok()?;
        let stored: StoredToken = serde_json::from_str(&contents).ok()?;
        (stored.replaces == self.replaces && !stored.token.is_empty()).then_some(stored)
    }

    /// Write the token atomically, readable by the runner's user only
    pub fn save(&self, token: &str, obtained_at: i64, expires_at: Option<i64>) -> Result<()> {
        use std::io::Write;

        let stored = StoredToken {
            replaces: self.replaces.clone(),
            token: token.to_string(),
            obtained_at,
            expires_at,
        };
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create a file in {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(serde_json::to_string(&stored)?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .with_context(|| format!("Failed to save {}", self.path.display()))?;
        Ok(())
    }
}

/// When a token obtained at `obtained_at` and expiring at `expires_at` is reset
pub fn reset_time(obtained_at: i64, expires_at: i64) -> i64 {
    let lifetime = (expires_at - obtained_at).max(0);
    obtained_at + (lifetime as f64 * RESET_FACTOR) as i64
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Unix seconds as an RFC 3339 time, for logs
pub fn display(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0).map_or_else(|| at.to_string(), |at| at.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn stored_token_is_used_until_the_config_token_changes() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_str().unwrap();
        let store = TokenStore::new(state_dir, "glrt-config");
        assert_eq!(store.load(), None);

        store.save("glrt-rotated", 100, Some(200)).unwrap();
        let stored = store.load().unwrap();
        assert_eq!(
            (stored.token.as_str(), stored.obtained_at, stored.expires_at),
            ("glrt-rotated", 100, Some(200))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A new token in the config wins over the rotated one
        assert_eq!(TokenStore::new(state_dir, "glrt-new").load(), None);
    }

    #[test]
    fn resets_after_three_quarters_of_the_lifetime() {
        assert_eq!(reset_time(1000, 2000), 1750);
        assert_eq!(reset_time(1000, 900), 1000);
    }
}
