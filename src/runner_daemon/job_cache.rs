//! Local job cache (`cache:` in .gitlab-ci.yml). GitLab has no cache API: like
//! gitlab-runner without a distributed cache, archives are kept on the runner host,
//! at `<root>/project-<id>/<encoded key>.zip`.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::artifacts;
use super::script;

#[derive(Debug, Clone)]
pub struct LocalCache {
    root: PathBuf,
}

/// Percent-encode a cache key into a single safe file name component
fn encode_key(key: &str) -> String {
    key.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Expand variables in a cache key; an empty key means `default`, as in GitLab
pub fn resolve_key(key: &str, variables: &HashMap<String, String>) -> String {
    let expanded = script::expand(key, variables);
    let trimmed = expanded.trim();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

impl LocalCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn archive_path(&self, project_id: u64, key: &str) -> PathBuf {
        self.root
            .join(format!("project-{}", project_id))
            .join(format!("{}.zip", encode_key(key)))
    }

    /// Number of cache archives and their total size in bytes
    pub fn stats(&self) -> (usize, u64) {
        walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "zip"))
            .fold((0, 0), |(count, size), entry| {
                (count + 1, size + entry.metadata().map_or(0, |m| m.len()))
            })
    }

    /// Extract the first key that has an archive; returns that key
    pub async fn restore(
        &self,
        project_id: u64,
        keys: &[String],
        workspace: &str,
    ) -> Result<Option<String>> {
        for key in keys {
            let path = self.archive_path(project_id, key);
            match tokio::fs::read(&path).await {
                Ok(data) => {
                    artifacts::extract_archive(data, workspace).await?;
                    return Ok(Some(key.clone()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("Failed to read {}", path.display()))
                }
            }
        }
        Ok(None)
    }

    /// Archive `paths` from the workspace under `key`; returns the archive size,
    /// or `None` when no file matched (nothing is saved)
    pub async fn save(
        &self,
        project_id: u64,
        key: &str,
        workspace: &str,
        paths: &[String],
    ) -> Result<Option<usize>> {
        let Some(data) = artifacts::create_zip_from_paths(workspace, paths).await? else {
            return Ok(None);
        };
        let path = self.archive_path(project_id, key);
        let dir = path.parent().unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("Failed to create cache directory {}", dir.display()))?;

        // Write then rename, so concurrent jobs never read a partial archive
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            encode_key(key),
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&tmp, &data).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(Some(data.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn save_then_restore_round_trip_with_fallback() {
        let cache_root = tempdir().unwrap();
        let cache = LocalCache::new(cache_root.path());
        let producer = tempdir().unwrap();
        std::fs::create_dir(producer.path().join("vendor")).unwrap();
        std::fs::write(producer.path().join("vendor/lib.txt"), b"dep").unwrap();

        cache
            .save(
                42,
                "main",
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();

        let consumer = tempdir().unwrap();
        let hit = cache
            .restore(
                42,
                &["feature-branch".to_string(), "main".to_string()],
                consumer.path().to_str().unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(hit.as_deref(), Some("main"));
        assert_eq!(
            std::fs::read(consumer.path().join("vendor/lib.txt")).unwrap(),
            b"dep"
        );
        let miss = cache
            .restore(7, &["main".to_string()], consumer.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(miss.is_none(), "caches must not leak between projects");
        assert_eq!(cache.stats().0, 1);
    }

    #[tokio::test]
    async fn key_cannot_escape_cache_directory() {
        let root = tempdir().unwrap();
        let cache_root = root.path().join("cache");
        let cache = LocalCache::new(&cache_root);
        let ws = tempdir().unwrap();
        std::fs::write(ws.path().join("f"), b"x").unwrap();

        cache
            .save(
                1,
                "../../escaped",
                ws.path().to_str().unwrap(),
                &["f".to_string()],
            )
            .await
            .unwrap();

        let entries: Vec<_> = std::fs::read_dir(cache_root.join("project-1"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["%2E%2E%2F%2E%2E%2Fescaped.zip".to_string()]);
        assert!(!root.path().join("escaped.zip").exists());
    }

    #[test]
    fn keys_are_expanded_and_default_when_empty() {
        let vars = HashMap::from([("CI_COMMIT_REF_SLUG".to_string(), "main".to_string())]);

        assert_eq!(resolve_key("deps-$CI_COMMIT_REF_SLUG", &vars), "deps-main");
        assert_eq!(resolve_key("$UNSET", &vars), "default");
    }
}
