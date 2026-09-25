//! Job cache (`cache:` in .gitlab-ci.yml). GitLab has no cache API: archives are
//! kept on the runner host, at `<root>/project-<id>/<encoded key>.zip`, and with
//! `cache_s3` also in a bucket shared by runners (gitlab-runner's distributed
//! cache). The local copy is the first layer; `<archive>.etag` holds the ETag
//! of its S3 object, so an unchanged object is not downloaded again.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::artifacts;
use super::script;

#[derive(Debug, Clone)]
pub struct LocalCache {
    root: PathBuf,
    /// Archives unused for longer are deleted; `None` keeps them forever
    max_age: Option<std::time::Duration>,
    /// S3 storage shared with other runners; the local directory is the first layer
    remote: Option<super::s3::Bucket>,
}

/// Where a restored archive came from
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Local,
    S3NotModified,
    S3Downloaded(u64),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Local => write!(f, "local copy"),
            Source::S3NotModified => write!(f, "S3: not modified, local copy"),
            Source::S3Downloaded(bytes) => {
                write!(
                    f,
                    "downloaded {:.1} MB from S3",
                    *bytes as f64 / 1_048_576.0
                )
            }
        }
    }
}

/// Outcome of a restore: the key found (if any) and S3 problems on the way
#[derive(Debug, Default)]
pub struct Restore {
    pub hit: Option<(String, Source)>,
    pub warnings: Vec<String>,
}

/// A saved archive and, with S3, whether it was uploaded
#[derive(Debug)]
pub struct Saved {
    pub size: u64,
    pub upload: Option<std::result::Result<(), String>>,
}

/// File next to an archive holding the ETag of its S3 object
fn sidecar(archive: &Path) -> PathBuf {
    let mut name = archive.as_os_str().to_owned();
    name.push(".etag");
    PathBuf::from(name)
}

/// Percent-encode a cache key into a single safe file name component; long keys
/// are hashed so the name stays within file system limits
fn encode_key(key: &str) -> String {
    let encoded: String = key
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect();
    if encoded.len() <= 200 {
        encoded
    } else {
        format!("h-{}", blake3::hash(key.as_bytes()).to_hex())
    }
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
        Self {
            root: root.into(),
            max_age: None,
            remote: None,
        }
    }

    /// Share archives with other runners through S3
    pub fn with_remote(mut self, bucket: super::s3::Bucket) -> Self {
        self.remote = Some(bucket);
        self
    }

    /// Delete archives not used for `days` days (0 keeps them forever)
    pub fn with_max_age_days(mut self, days: u64) -> Self {
        self.max_age = (days > 0).then(|| std::time::Duration::from_secs(days * 24 * 3600));
        self
    }

    /// Remove archives of a project that were not used within `max_age`, and
    /// staging files a crash left behind
    fn evict(&self, dir: &Path) {
        let Some(max_age) = self.max_age else { return };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // A sidecar goes with its archive (its own age says nothing about use)
            if path.extension().is_some_and(|ext| ext == "etag") {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > max_age);
            if old {
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(sidecar(&path));
            }
        }
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

    /// Extract the first key that has an archive, in S3 or locally
    pub async fn restore(
        &self,
        project_id: u64,
        keys: &[String],
        workspace: &str,
    ) -> Result<Restore> {
        let mut outcome = Restore::default();
        for key in keys {
            let path = self.archive_path(project_id, key);
            let mut source = Source::Local;
            if let Some(remote) = &self.remote {
                match self.fetch(remote, project_id, key, &path).await {
                    Ok(Some(fetched)) => source = fetched,
                    Ok(None) => {}
                    Err(e) => outcome
                        .warnings
                        .push(format!("S3 unavailable for cache {}: {:#}", key, e)),
                }
            }
            match tokio::fs::metadata(&path).await {
                Ok(_) => {
                    // Mark the archive as used, so eviction keeps it
                    if let Ok(file) = std::fs::File::options().append(true).open(&path) {
                        let _ = file.set_modified(std::time::SystemTime::now());
                    }
                    artifacts::extract_archive_file(&path, workspace).await?;
                    outcome.hit = Some((key.clone(), source));
                    return Ok(outcome);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("Failed to read {}", path.display()))
                }
            }
        }
        Ok(outcome)
    }

    /// Bring the local archive of `key` up to date with S3: `Some` when S3 has
    /// it (downloaded or unchanged), `None` when S3 does not
    async fn fetch(
        &self,
        remote: &super::s3::Bucket,
        project_id: u64,
        key: &str,
        path: &Path,
    ) -> Result<Option<Source>> {
        use super::s3::Download;

        let dir = path.parent().unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(dir).await?;
        let etag = if path.exists() {
            tokio::fs::read_to_string(sidecar(path)).await.ok()
        } else {
            None
        };
        let object = remote.object_key(project_id, &format!("{}.zip", encode_key(key)));
        match remote.get_to_file(&object, etag.as_deref(), dir).await? {
            Download::NotModified => Ok(Some(Source::S3NotModified)),
            Download::NotFound => Ok(None),
            Download::Downloaded { file, etag, bytes } => {
                file.persist(path)
                    .with_context(|| format!("Failed to save {}", path.display()))?;
                match etag {
                    Some(etag) => tokio::fs::write(sidecar(path), etag).await?,
                    None => {
                        let _ = tokio::fs::remove_file(sidecar(path)).await;
                    }
                }
                Ok(Some(Source::S3Downloaded(bytes)))
            }
        }
    }

    /// Archive `paths` from the workspace under `key` and upload it to S3, if
    /// any; `None` when no file matched (nothing is saved)
    pub async fn save(
        &self,
        project_id: u64,
        key: &str,
        workspace: &str,
        paths: &[String],
    ) -> Result<Option<Saved>> {
        let path = self.archive_path(project_id, key);
        let dir = path.parent().unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("Failed to create cache directory {}", dir.display()))?;
        // Staged in the cache directory and renamed into place, so concurrent
        // jobs never read a partial archive and nothing is held in memory
        let Some(archive) = artifacts::create_zip_from_paths(workspace, paths, dir).await? else {
            return Ok(None);
        };
        let len = archive.len;
        archive
            .file
            .persist(&path)
            .with_context(|| format!("Failed to save {}", path.display()))?;
        // The local archive changed: its old ETag no longer describes it
        let _ = tokio::fs::remove_file(sidecar(&path)).await;
        let upload = match &self.remote {
            None => None,
            Some(remote) => {
                let object = remote.object_key(project_id, &format!("{}.zip", encode_key(key)));
                Some(match remote.put_file(&object, &path).await {
                    Ok(etag) => {
                        if let Some(etag) = etag {
                            let _ = tokio::fs::write(sidecar(&path), etag).await;
                        }
                        Ok(())
                    }
                    Err(e) => Err(format!("{:#}", e)),
                })
            }
        };
        self.evict(dir);
        Ok(Some(Saved { size: len, upload }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn remote(server: &MockServer) -> crate::runner_daemon::s3::Bucket {
        crate::runner_daemon::s3::Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url: format!("{}/ci", server.uri()),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::builder(),
        )
        .unwrap()
        .with_retry_delay(std::time::Duration::from_millis(10))
    }

    /// A workspace with vendor/lib.txt = `content`
    fn workspace(content: &str) -> tempfile::TempDir {
        let ws = tempdir().unwrap();
        std::fs::create_dir(ws.path().join("vendor")).unwrap();
        std::fs::write(ws.path().join("vendor/lib.txt"), content).unwrap();
        ws
    }

    #[tokio::test]
    async fn saves_upload_and_other_hosts_download() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/ci/project-5/main.zip"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"v1\""))
            .expect(1)
            .mount(&server)
            .await;
        let host_a = tempdir().unwrap();
        let cache_a = LocalCache::new(host_a.path()).with_remote(remote(&server));
        let producer = workspace("dep");

        let saved = cache_a
            .save(
                5,
                "main",
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.upload, Some(Ok(())));
        assert_eq!(
            std::fs::read_to_string(host_a.path().join("project-5/main.zip.etag")).unwrap(),
            "\"v1\""
        );

        // Host B has nothing locally: it downloads what host A uploaded
        let uploaded = server.received_requests().await.unwrap()[0].body.clone();
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.zip"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(uploaded),
            )
            .mount(&server)
            .await;
        let host_b = tempdir().unwrap();
        let cache_b = LocalCache::new(host_b.path()).with_remote(remote(&server));
        let consumer = tempdir().unwrap();

        let restored = cache_b
            .restore(5, &["main".to_string()], consumer.path().to_str().unwrap())
            .await
            .unwrap();

        assert!(
            matches!(restored.hit, Some((ref key, Source::S3Downloaded(_))) if key == "main"),
            "{:?}",
            restored.hit
        );
        assert_eq!(
            std::fs::read_to_string(consumer.path().join("vendor/lib.txt")).unwrap(),
            "dep"
        );
        assert!(
            host_b.path().join("project-5/main.zip").exists(),
            "kept as the local layer"
        );
    }

    #[tokio::test]
    async fn unchanged_objects_are_not_downloaded_again() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"v1\""))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let ws = workspace("dep");
        cache
            .save(
                5,
                "main",
                ws.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();

        let consumer = tempdir().unwrap();
        let restored = cache
            .restore(5, &["main".to_string()], consumer.path().to_str().unwrap())
            .await
            .unwrap();

        assert!(
            matches!(restored.hit, Some((_, Source::S3NotModified))),
            "{:?}",
            restored.hit
        );
        assert!(restored.warnings.is_empty());
        assert_eq!(
            std::fs::read_to_string(consumer.path().join("vendor/lib.txt")).unwrap(),
            "dep"
        );
    }

    #[tokio::test]
    async fn s3_error_falls_back_to_local_archive() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error><Code>AccessDenied</Code></Error>"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error><Code>AccessDenied</Code></Error>"),
            )
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let ws = workspace("dep");

        let saved = cache
            .save(
                5,
                "main",
                ws.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&saved.upload, Some(Err(e)) if e.contains("AccessDenied")),
            "{:?}",
            saved.upload
        );
        assert!(!root.path().join("project-5/main.zip.etag").exists());

        let consumer = tempdir().unwrap();
        let restored = cache
            .restore(5, &["main".to_string()], consumer.path().to_str().unwrap())
            .await
            .unwrap();

        assert!(
            matches!(restored.hit, Some((_, Source::Local))),
            "{:?}",
            restored.hit
        );
        assert_eq!(restored.warnings.len(), 1);
        assert!(
            restored.warnings[0].contains("AccessDenied"),
            "{:?}",
            restored.warnings
        );
    }

    #[tokio::test]
    async fn fallback_keys_are_tried_in_s3() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci/project-5/feature.zip"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let producer = workspace("from-main");
        let staging = tempdir().unwrap();
        LocalCache::new(staging.path())
            .save(
                5,
                "main",
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();
        let archive = std::fs::read(staging.path().join("project-5/main.zip")).unwrap();
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let consumer = tempdir().unwrap();

        let restored = cache
            .restore(
                5,
                &["feature".to_string(), "main".to_string()],
                consumer.path().to_str().unwrap(),
            )
            .await
            .unwrap();

        assert!(
            matches!(restored.hit, Some((ref key, _)) if key == "main"),
            "{:?}",
            restored.hit
        );
        assert!(
            !root.path().join("project-5/main.zip.etag").exists(),
            "no ETag header, no sidecar"
        );
    }

    #[tokio::test]
    async fn eviction_removes_the_sidecar_with_its_archive() {
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_max_age_days(1);
        let ws = workspace("x");
        let ws_path = ws.path().to_str().unwrap();
        cache
            .save(1, "stale", ws_path, &["vendor".to_string()])
            .await
            .unwrap();
        let stale = cache.archive_path(1, "stale");
        std::fs::write(sidecar(&stale), "\"e\"").unwrap();
        let two_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
        std::fs::File::options()
            .append(true)
            .open(&stale)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();

        cache
            .save(1, "fresh", ws_path, &["vendor".to_string()])
            .await
            .unwrap();

        assert!(!stale.exists() && !sidecar(&stale).exists());
    }

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
        let restored = cache
            .restore(
                42,
                &["feature-branch".to_string(), "main".to_string()],
                consumer.path().to_str().unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(restored.hit.as_ref().map(|(k, _)| k.as_str()), Some("main"));
        assert_eq!(
            std::fs::read(consumer.path().join("vendor/lib.txt")).unwrap(),
            b"dep"
        );
        let miss = cache
            .restore(7, &["main".to_string()], consumer.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(miss.hit.is_none(), "caches must not leak between projects");
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
    fn long_keys_are_hashed_into_short_file_names() {
        let long = "k".repeat(1000);
        let name = encode_key(&long);
        assert!(name.starts_with("h-") && name.len() < 100, "{}", name);
        assert_eq!(encode_key("main"), "main");
    }

    #[tokio::test]
    async fn old_archives_are_evicted_on_save() {
        let cache_root = tempdir().unwrap();
        let cache = LocalCache::new(cache_root.path()).with_max_age_days(1);
        let ws = tempdir().unwrap();
        std::fs::write(ws.path().join("f"), b"x").unwrap();
        let ws_path = ws.path().to_str().unwrap();

        cache
            .save(1, "stale", ws_path, &["f".to_string()])
            .await
            .unwrap();
        let stale = cache.archive_path(1, "stale");
        let two_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
        // A staging file left behind by a killed runner
        let leftover = stale.with_file_name(".tmpAbC123");
        std::fs::write(&leftover, b"partial").unwrap();
        for file in [&stale, &leftover] {
            std::fs::File::options()
                .append(true)
                .open(file)
                .unwrap()
                .set_modified(two_days_ago)
                .unwrap();
        }

        cache
            .save(1, "fresh", ws_path, &["f".to_string()])
            .await
            .unwrap();

        assert!(!stale.exists(), "stale archive kept");
        assert!(!leftover.exists(), "stale staging file kept");
        assert!(cache.archive_path(1, "fresh").exists());
    }

    #[test]
    fn keys_are_expanded_and_default_when_empty() {
        let vars = HashMap::from([("CI_COMMIT_REF_SLUG".to_string(), "main".to_string())]);

        assert_eq!(resolve_key("deps-$CI_COMMIT_REF_SLUG", &vars), "deps-main");
        assert_eq!(resolve_key("$UNSET", &vars), "default");
    }
}
