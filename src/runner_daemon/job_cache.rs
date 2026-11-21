//! Job cache (`cache:` in .gitlab-ci.yml). GitLab has no cache API: archives are
//! kept on the runner host, at `<root>/project-<id>/<encoded key>.tar.zst`, and
//! with `cache_s3` also in a bucket shared by runners (gitlab-runner's
//! distributed cache). The local copy is the first layer; `<archive>.etag`
//! holds the ETag of its S3 object, so an unchanged object is not downloaded
//! again, and `<archive>.sha256` the hash of that object, so an archive with
//! the same content is not uploaded again.
//!
//! Versions up to 0.10 wrote ZIPs, as `<encoded key>.zip`. They are still
//! restored, locally and from S3, when a key has no tar.zst archive; the new
//! format has its own name so that older runners sharing the bucket never
//! read an archive they cannot extract.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::artifacts;
use super::cancel::Stop;
use super::script;

/// Extension of cache archives (format 2)
const ARCHIVE_EXTENSION: &str = ".tar.zst";

/// Extension of the archives versions up to 0.10 wrote (format 1)
const LEGACY_EXTENSION: &str = ".zip";

/// Downloaded chunks queued for the extraction (backpressure on S3)
const STREAM_QUEUE: usize = 32;

/// Whether `path` names a cache archive of either format
fn is_archive(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(ARCHIVE_EXTENSION) || name.ends_with(LEGACY_EXTENSION))
}

/// Reads the chunks a download sends, blocking: for the extraction, which
/// runs off the async runtime. The end of the channel is the end of the
/// stream; a download that failed ends it early, which the extraction
/// reports as a truncated archive. Every chunk is also hashed and written
/// to the local copy here, on the extraction's thread rather than on the
/// async workers that other jobs share.
struct ChannelReader {
    chunks: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    chunk: bytes::Bytes,
    hasher: sha2::Sha256,
    /// The local copy, until a write to it fails
    copy: Option<std::io::BufWriter<std::fs::File>>,
    copy_error: Option<std::io::Error>,
}

/// What a `ChannelReader` saw once the extraction was done with it
struct Received {
    /// Bytes that came after what the extraction read
    extra: u64,
    /// SHA-256 (hex) of the whole stream
    sha256: String,
    /// Whether the local copy holds the whole stream
    copy: std::io::Result<()>,
}

impl ChannelReader {
    fn new(chunks: tokio::sync::mpsc::Receiver<bytes::Bytes>, copy: std::fs::File) -> Self {
        use sha2::Digest;
        Self {
            chunks,
            chunk: bytes::Bytes::new(),
            hasher: sha2::Sha256::new(),
            // Network chunks are small (often 16 KiB): written in larger pieces
            copy: Some(std::io::BufWriter::with_capacity(256 * 1024, copy)),
            copy_error: None,
        }
    }

    /// The next chunk, hashed and copied; `None` at the end of the stream
    fn next_chunk(&mut self) -> Option<bytes::Bytes> {
        use sha2::Digest;
        use std::io::Write;
        let chunk = self.chunks.blocking_recv()?;
        self.hasher.update(&chunk);
        if let Some(copy) = &mut self.copy {
            if let Err(e) = copy.write_all(&chunk) {
                self.copy = None;
                self.copy_error = Some(e);
            }
        }
        Some(chunk)
    }

    /// Read what the extraction left to the end of the stream (a download
    /// longer than the archive), and complete the local copy
    fn finish(mut self) -> Received {
        use sha2::Digest;
        use std::io::Write;
        let mut extra = self.chunk.len() as u64;
        while let Some(chunk) = self.next_chunk() {
            extra += chunk.len() as u64;
        }
        let copy = match (self.copy_error.take(), self.copy.take()) {
            (Some(e), _) => Err(e),
            (None, Some(mut copy)) => copy.flush(),
            (None, None) => Ok(()),
        };
        Received {
            extra,
            sha256: format!("{:x}", self.hasher.finalize()),
            copy,
        }
    }
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.chunk.is_empty() {
            match self.next_chunk() {
                Some(chunk) => self.chunk = chunk,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len());
        buf[..n].copy_from_slice(&self.chunk.split_to(n));
        Ok(n)
    }
}

/// What S3 had for a key
#[derive(Debug)]
enum Fetched {
    /// Downloaded (this many bytes) and extracted on the way
    Extracted(u64),
    /// The local archive at this path is current: S3 holds it unchanged,
    /// or it was just downloaded
    Local(PathBuf, Source),
    /// S3 has no archive for the key
    Missing,
}

#[derive(Debug, Clone)]
pub struct LocalCache {
    root: PathBuf,
    /// Archives unused for longer are deleted; `None` keeps them forever
    max_age: Option<std::time::Duration>,
    /// Total size of all projects' archives; the least recently used go
    /// first. `None`: no limit.
    max_size: Option<u64>,
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

/// What happened to a saved archive in S3
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upload {
    Sent,
    /// S3 holds an object with the same content already: nothing was sent
    Unchanged,
}

/// A saved archive and, with S3, whether it was uploaded
#[derive(Debug)]
pub struct Saved {
    pub size: u64,
    pub upload: Option<std::result::Result<Upload, String>>,
}

/// File next to an archive holding the ETag of its S3 object
fn sidecar(archive: &Path) -> PathBuf {
    with_suffix(archive, ".etag")
}

/// File next to an archive holding `<sha256> <etag>`: the hash of an S3
/// object this host uploaded or downloaded, and that object's ETag. A new
/// archive with that hash is not uploaded again while S3 still has that
/// ETag. The pair is one fact about S3, true whatever the local archive is.
fn hash_sidecar(archive: &Path) -> PathBuf {
    with_suffix(archive, ".sha256")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Sidecar extensions: a sidecar goes with its archive
const SIDECARS: [&str; 2] = ["etag", "sha256"];

async fn read_hash_sidecar(archive: &Path) -> Option<(String, String)> {
    let text = tokio::fs::read_to_string(hash_sidecar(archive))
        .await
        .ok()?;
    let (sha256, etag) = text.split_once(' ')?;
    (!sha256.is_empty() && !etag.is_empty()).then(|| (sha256.to_string(), etag.to_string()))
}

/// Record (or forget, without an ETag) the S3 object the archive came from
/// or went to
async fn write_sidecars(archive: &Path, etag: Option<&str>, sha256: &str) {
    let _ = match etag {
        Some(etag) => {
            let _ = tokio::fs::write(sidecar(archive), etag).await;
            tokio::fs::write(hash_sidecar(archive), format!("{} {}", sha256, etag)).await
        }
        None => {
            let _ = tokio::fs::remove_file(sidecar(archive)).await;
            tokio::fs::remove_file(hash_sidecar(archive)).await
        }
    };
}

/// The ETag of the S3 object the local archive at `path` came from or went
/// to; `None` without the archive
async fn current_etag(path: &Path) -> Option<String> {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        tokio::fs::read_to_string(sidecar(path)).await.ok()
    } else {
        None
    }
}

/// File name of the archive of `key`, locally and in S3
fn archive_name(key: &str) -> String {
    format!("{}{}", encode_key(key), ARCHIVE_EXTENSION)
}

/// File name of the archive of `key` in the format of versions up to 0.10
fn legacy_name(key: &str) -> String {
    format!("{}{}", encode_key(key), LEGACY_EXTENSION)
}

/// Remove an archive and its sidecars, off the async runtime
async fn remove_archive(archive: &Path) {
    let archive = archive.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || remove_with_sidecars(&archive)).await;
}

fn remove_with_sidecars(archive: &Path) -> bool {
    let removed = std::fs::remove_file(archive).is_ok();
    for extension in SIDECARS {
        let _ = std::fs::remove_file(with_suffix(archive, &format!(".{}", extension)));
    }
    removed
}

/// Remove archives of a project that were not used within `max_age`, and
/// staging files a crash left behind
fn evict_old(dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // A sidecar goes with its archive (its own age says nothing about use)
        if path
            .extension()
            .is_some_and(|ext| SIDECARS.iter().any(|sidecar| ext == *sidecar))
        {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if old {
            remove_with_sidecars(&path);
        }
    }
}

/// Remove the least recently used archives of all projects under `root`
/// until they take at most `max_size` bytes. `keep`, the archive just
/// saved or downloaded, stays even if it is larger on its own.
fn evict_over_size(root: &Path, max_size: u64, keep: &Path) {
    let mut archives: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return;
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if !is_archive(&path) {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            if meta.is_file() {
                let used = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                archives.push((used, meta.len(), path));
            }
        }
    }
    let mut total: u64 = archives.iter().map(|(_, len, _)| len).sum();
    if total <= max_size {
        return;
    }
    archives.sort();
    let (mut removed, mut freed) = (0usize, 0u64);
    for (_, len, path) in archives {
        if total <= max_size {
            break;
        }
        if path != keep && remove_with_sidecars(&path) {
            total -= len;
            removed += 1;
            freed += len;
        }
    }
    tracing::info!(
        "Cache above cache_max_size: removed {} least recently used archives ({:.1} MB)",
        removed,
        freed as f64 / 1_048_576.0
    );
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
            max_size: None,
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

    /// Keep the archives of all projects within `bytes`, deleting the least
    /// recently used first (`None` or 0: no limit)
    pub fn with_max_size(mut self, bytes: Option<u64>) -> Self {
        self.max_size = bytes.filter(|bytes| *bytes > 0);
        self
    }

    /// Apply the age limit to `dir` (a project's directory) and the size
    /// limit to the whole cache, off the async runtime; `keep` is the
    /// archive just written
    async fn evict(&self, dir: &Path, keep: &Path) {
        let (max_age, max_size) = (self.max_age, self.max_size);
        if max_age.is_none() && max_size.is_none() {
            return;
        }
        let (root, dir, keep) = (self.root.clone(), dir.to_path_buf(), keep.to_path_buf());
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(max_age) = max_age {
                evict_old(&dir, max_age);
            }
            if let Some(max_size) = max_size {
                evict_over_size(&root, max_size, &keep);
            }
        })
        .await;
    }

    /// Apply the age limit to every project's archives and the size limit
    /// to the whole cache, off the async runtime: the runner's periodic sweep,
    /// so projects without new jobs age out too
    pub async fn evict_all(&self) {
        let (max_age, max_size) = (self.max_age, self.max_size);
        if max_age.is_none() && max_size.is_none() {
            return;
        }
        let root = self.root.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let (Some(max_age), Ok(projects)) = (max_age, std::fs::read_dir(&root)) {
                for project in projects.flatten() {
                    if project.file_type().is_ok_and(|kind| kind.is_dir()) {
                        evict_old(&project.path(), max_age);
                    }
                }
            }
            if let Some(max_size) = max_size {
                // Nothing was just written: no archive needs keeping
                evict_over_size(&root, max_size, Path::new(""));
            }
        })
        .await;
    }

    /// The archive of `key` (format 2)
    fn archive_path(&self, project_id: u64, key: &str) -> PathBuf {
        self.root
            .join(format!("project-{}", project_id))
            .join(archive_name(key))
    }

    /// The archive an older version saved for `key` (format 1)
    fn legacy_path(&self, project_id: u64, key: &str) -> PathBuf {
        self.root
            .join(format!("project-{}", project_id))
            .join(legacy_name(key))
    }

    /// Number of cache archives and their total size in bytes
    pub fn stats(&self) -> (usize, u64) {
        walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| is_archive(entry.path()))
            .fold((0, 0), |(count, size), entry| {
                (count + 1, size + entry.metadata().map_or(0, |m| m.len()))
            })
    }

    /// The local archive of `key`: of the two formats, the one written or
    /// used last
    async fn local_archive(&self, project_id: u64, key: &str) -> Result<Option<PathBuf>> {
        let mut found: Option<(std::time::SystemTime, PathBuf)> = None;
        for path in [
            self.archive_path(project_id, key),
            self.legacy_path(project_id, key),
        ] {
            match tokio::fs::metadata(&path).await {
                Ok(meta) => {
                    let used = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    if found.as_ref().is_none_or(|(newest, _)| used > *newest) {
                        found = Some((used, path));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("Failed to read {}", path.display()))
                }
            }
        }
        Ok(found.map(|(_, path)| path))
    }

    /// Extract the first key that has an archive, in S3 or locally, until `stop`
    pub async fn restore(
        &self,
        project_id: u64,
        keys: &[String],
        workspace: &str,
        stop: &Stop,
    ) -> Result<Restore> {
        let mut outcome = Restore::default();
        for key in keys {
            stop.check()?;
            let mut current = None;
            if let Some(remote) = &self.remote {
                match self.fetch(remote, project_id, key, workspace, stop).await? {
                    Ok(Fetched::Extracted(bytes)) => {
                        outcome.hit = Some((key.clone(), Source::S3Downloaded(bytes)));
                        return Ok(outcome);
                    }
                    Ok(Fetched::Local(path, source)) => current = Some((path, source)),
                    Ok(Fetched::Missing) => {}
                    Err(e) => outcome
                        .warnings
                        .push(format!("S3 unavailable for cache {}: {:#}", key, e)),
                }
            }
            let (path, source) = match current {
                Some(current) => current,
                None => match self.local_archive(project_id, key).await? {
                    Some(path) => (path, Source::Local),
                    None => continue,
                },
            };
            match tokio::fs::metadata(&path).await {
                Ok(_) => {
                    // Mark the archive as used, so eviction keeps it
                    let touched = path.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(file) = std::fs::File::options().append(true).open(&touched) {
                            let _ = file.set_modified(std::time::SystemTime::now());
                        }
                    })
                    .await;
                    artifacts::extract_cache_file(&path, workspace, stop).await?;
                    outcome.hit = Some((key.clone(), source));
                    return Ok(outcome);
                }
                // Evicted meanwhile by a concurrent job
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("Failed to read {}", path.display()))
                }
            }
        }
        Ok(outcome)
    }

    /// Bring the local archive of `key` up to date with S3: the archive
    /// (format 2), or else the ZIP an older version uploaded. A changed
    /// archive is extracted while it downloads. The inner error is an S3
    /// problem, after which the local copy is used; the outer one is an
    /// extraction that failed or was stopped.
    async fn fetch(
        &self,
        remote: &super::s3::Bucket,
        project_id: u64,
        key: &str,
        workspace: &str,
        stop: &Stop,
    ) -> Result<Result<Fetched>> {
        use super::s3::{Download, Get};

        let path = self.archive_path(project_id, key);
        let dir = path.parent().unwrap_or(Path::new("."));
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            return Ok(Err(e.into()));
        }
        let legacy = self.legacy_path(project_id, key);
        let etag = current_etag(&path).await;
        let object = remote.object_key(project_id, &archive_name(key));
        match remote.get(&object, etag.as_deref()).await {
            Ok(Get::NotModified) => return Ok(Ok(Fetched::Local(path, Source::S3NotModified))),
            Ok(Get::Found(object)) => {
                return self
                    .download_and_extract(object, (&path, &legacy), workspace, stop)
                    .await
            }
            Ok(Get::NotFound) => {}
            Err(e) => return Ok(Err(e)),
        }

        // No archive in S3 in this format: one an older runner uploaded
        let etag = current_etag(&legacy).await;
        let object = remote.object_key(project_id, &legacy_name(key));
        Ok(
            match remote.get_to_file(&object, etag.as_deref(), dir).await {
                Ok(Download::NotModified) => Ok(Fetched::Local(legacy, Source::S3NotModified)),
                Ok(Download::NotFound) => Ok(Fetched::Missing),
                Ok(Download::Downloaded {
                    file,
                    etag,
                    bytes,
                    sha256,
                }) => match file.persist(&legacy) {
                    Ok(_) => {
                        write_sidecars(&legacy, etag.as_deref(), &sha256).await;
                        // Hosts that only pull must keep the local layer bounded too
                        self.evict(dir, &legacy).await;
                        Ok(Fetched::Local(legacy, Source::S3Downloaded(bytes)))
                    }
                    Err(e) => Err(anyhow::Error::new(e.error))
                        .with_context(|| format!("Failed to save {}", legacy.display())),
                },
                Err(e) => Err(e),
            },
        )
    }

    /// Download `object` into the local layer at `path` and extract it into
    /// the workspace as it arrives: the download and the extraction overlap,
    /// and the archive is read from disk only when it is unchanged later.
    /// A download that fails midway leaves the files extracted so far; the
    /// local copy restored after it overwrites them.
    async fn download_and_extract(
        &self,
        mut object: super::s3::Object<'_>,
        (path, legacy): (&Path, &Path),
        workspace: &str,
        stop: &Stop,
    ) -> Result<Result<Fetched>> {
        let dir = path.parent().unwrap_or(Path::new("."));
        let staged = match tempfile::NamedTempFile::new_in(dir).and_then(|file| {
            let copy = file.reopen()?;
            Ok((file, copy))
        }) {
            Ok(staged) => staged,
            Err(e) => {
                return Ok(Err(anyhow::Error::new(e)
                    .context(format!("Failed to create a file in {}", dir.display()))))
            }
        };
        let (staged, copy) = staged;
        let (sender, chunks) = tokio::sync::mpsc::channel(STREAM_QUEUE);
        let extraction = {
            let (workspace, stop) = (workspace.to_string(), stop.clone());
            tokio::task::spawn_blocking(move || {
                let mut reader = ChannelReader::new(chunks, copy);
                artifacts::extract_tar_zst(
                    &mut reader,
                    &workspace,
                    artifacts::ExtractLimits::default(),
                    &stop,
                )
                // The rest of the stream is read (hashed and copied) to its
                // end, so the hash is the whole object's
                .map(|()| reader.finish())
            })
        };

        let mut size = 0u64;
        let received = async {
            while let Some(chunk) = object.chunk().await? {
                size += chunk.len() as u64;
                if sender.send(chunk).await.is_err() {
                    // The extraction ended: its error tells why
                    break;
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        drop(sender);
        let extracted = extraction.await.context("Extraction task panicked")?;
        let stream = match (received, extracted) {
            // The extraction saw a truncated stream: the download's error
            // is the one to report, and the local copy is used
            (Err(e), _) => return Ok(Err(e)),
            // A bad archive, or `stop`: as when extracting a local copy
            (Ok(()), Err(e)) => return Err(e),
            (Ok(()), Ok(stream)) if stream.extra > 0 => {
                return Ok(Err(anyhow::anyhow!(
                    "the cache archive ended before the download"
                )))
            }
            (Ok(()), Ok(stream)) => stream,
        };
        let sha256 = stream.sha256;
        // The files are extracted already; a mismatch keeps the archive out
        // of the local layer, and the local copy then overwrites them
        if let Err(e) = object.check(&sha256) {
            return Ok(Err(e));
        }
        if let Err(e) = stream.copy {
            // The workspace has the whole archive; only the next restore
            // downloads it again
            tracing::warn!("Failed to write {}: {}", path.display(), e);
            return Ok(Ok(Fetched::Extracted(size)));
        }
        match staged.persist(path) {
            Ok(_) => {
                write_sidecars(path, object.etag.as_deref(), &sha256).await;
                // It replaces the ZIP an older version saved for the key
                remove_archive(legacy).await;
                self.evict(dir, path).await;
            }
            // The cache is restored; only the next restore downloads again
            Err(e) => tracing::warn!("Failed to save {}: {}", path.display(), e.error),
        }
        Ok(Ok(Fetched::Extracted(size)))
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
        let Some(archive) = artifacts::create_cache_archive(workspace, paths, dir).await? else {
            return Ok(None);
        };
        let len = archive.len;
        let sha256 = archive
            .sha256
            .clone()
            .context("Cache archive written without its hash")?;
        // The handle uploads this archive even if a concurrent job saving
        // the same key renames its own into place meanwhile
        let file = archive
            .file
            .persist(&path)
            .with_context(|| format!("Failed to save {}", path.display()))?;
        // It replaces the ZIP an older version saved for the key
        remove_archive(&self.legacy_path(project_id, key)).await;
        let upload = match &self.remote {
            None => {
                // Without S3 the old ETag describes nothing any more
                write_sidecars(&path, None, &sha256).await;
                None
            }
            Some(remote) => {
                let object = remote.object_key(project_id, &archive_name(key));
                Some(upload(remote, &object, &path, &file, &sha256).await)
            }
        };
        drop(file);
        self.evict(dir, &path).await;
        Ok(Some(Saved { size: len, upload }))
    }
}

/// Upload the archive at `path` (open as `file`, hashing to `sha256`) as
/// `object`, unless S3 holds the same content already
async fn upload(
    remote: &super::s3::Bucket,
    object: &str,
    path: &Path,
    file: &std::fs::File,
    sha256: &str,
) -> std::result::Result<Upload, String> {
    // Archives are deterministic (sorted entries, fixed timestamps): the
    // same files give the same hash, typically a pull-push cache the job did
    // not change. One HEAD request then replaces the upload. The hash is
    // compared, not the ETag: a multipart upload's ETag is not the MD5 of
    // the content, and runners store the hash with the object. An object
    // without it matches when it is the one this host last saw with this
    // hash (`<archive>.sha256`).
    match remote.head(object).await {
        Ok(Some(info)) => {
            let same = info.sha256.as_deref() == Some(sha256)
                || read_hash_sidecar(path).await.is_some_and(|(known, etag)| {
                    known == sha256 && info.etag.as_deref() == Some(etag.as_str())
                });
            if same {
                // A restore then gets 304 for the object S3 holds
                write_sidecars(path, info.etag.as_deref(), sha256).await;
                return Ok(Upload::Unchanged);
            }
        }
        Ok(None) => {}
        // The upload below reports S3's state
        Err(e) => tracing::debug!("Cannot compare the cache with S3: {:#}", e),
    }
    // Until the upload succeeds the sidecar keeps the ETag of the object
    // this archive replaced: while S3 still holds that object, a restore
    // gets 304 and keeps this newer archive. A failed or interrupted upload
    // leaves it that way.
    match remote.put_file(object, file, sha256).await {
        Ok(etag) => {
            write_sidecars(path, etag.as_deref(), sha256).await;
            Ok(Upload::Sent)
        }
        Err(e) => Err(format!("{:#}", e)),
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
            reqwest::Client::new(),
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
            .and(path("/ci/project-5/main.tar.zst"))
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
        assert_eq!(saved.upload, Some(Ok(Upload::Sent)));
        assert_eq!(
            std::fs::read_to_string(host_a.path().join("project-5/main.tar.zst.etag")).unwrap(),
            "\"v1\""
        );

        // Host B has nothing locally: it downloads what host A uploaded
        let uploaded = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|request| request.method.as_str() == "PUT")
            .unwrap()
            .body;
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.tar.zst"))
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
            .restore(
                5,
                &["main".to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
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
            host_b.path().join("project-5/main.tar.zst").exists(),
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
            .restore(
                5,
                &["main".to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
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
        assert!(!root.path().join("project-5/main.tar.zst.etag").exists());

        let consumer = tempdir().unwrap();
        let restored = cache
            .restore(
                5,
                &["main".to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
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
    async fn a_failed_upload_keeps_the_previous_etag() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error><Code>AccessDenied</Code></Error>"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // S3 still holds "v1", the archive the local one replaced
        let old = workspace("old");
        let root = tempdir().unwrap();
        LocalCache::new(root.path())
            .save(
                5,
                "main",
                old.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();
        let archive = LocalCache::new(root.path()).archive_path(5, "main");
        std::fs::write(sidecar(&archive), "\"v1\"").unwrap();
        Mock::given(method("GET"))
            .and(wiremock::matchers::header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(std::fs::read(&archive).unwrap()),
            )
            .mount(&server)
            .await;
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let newer = workspace("new");

        let saved = cache
            .save(
                5,
                "main",
                newer.path().to_str().unwrap(),
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
        assert_eq!(
            std::fs::read_to_string(sidecar(&archive)).unwrap(),
            "\"v1\""
        );
        let consumer = tempdir().unwrap();
        let restored = cache
            .restore(
                5,
                &["main".to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(restored.hit, Some((_, Source::S3NotModified))),
            "{:?}",
            restored
        );
        assert_eq!(
            std::fs::read_to_string(consumer.path().join("vendor/lib.txt")).unwrap(),
            "new"
        );

        // An upload S3 takes without an ETag leaves no sidecar
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let saved = cache
            .save(
                5,
                "main",
                newer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.upload, Some(Ok(Upload::Sent)));
        assert!(!sidecar(&archive).exists());
    }

    #[tokio::test]
    async fn a_stopped_restore_does_not_call_s3() {
        let server = MockServer::start().await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let consumer = tempdir().unwrap();
        let stop = Stop::default();
        stop.stop();

        let stopped = cache
            .restore(
                5,
                &["main".to_string()],
                consumer.path().to_str().unwrap(),
                &stop,
            )
            .await
            .unwrap_err();

        assert!(stopped.to_string().starts_with("stopped"), "{:#}", stopped);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fallback_keys_are_tried_in_s3() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci/project-5/feature.tar.zst"))
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
        let archive = std::fs::read(staging.path().join("project-5/main.tar.zst")).unwrap();
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.tar.zst"))
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
                &Stop::default(),
            )
            .await
            .unwrap();

        assert!(
            matches!(restored.hit, Some((ref key, _)) if key == "main"),
            "{:?}",
            restored.hit
        );
        assert!(
            !root.path().join("project-5/main.tar.zst.etag").exists(),
            "no ETag header, no sidecar"
        );
    }

    #[tokio::test]
    async fn downloads_evict_old_archives_without_a_save() {
        let server = MockServer::start().await;
        let root = tempdir().unwrap();
        let producer = workspace("dep");
        let local = LocalCache::new(root.path()).with_max_age_days(1);
        local
            .save(
                5,
                "a",
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();
        let old = local.archive_path(5, "a");
        let archive = std::fs::read(&old).unwrap();
        let two_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
        std::fs::File::options()
            .append(true)
            .open(&old)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/ci/project-5/b.tar.zst"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
            .mount(&server)
            .await;
        let cache = LocalCache::new(root.path())
            .with_max_age_days(1)
            .with_remote(remote(&server));
        let consumer = tempdir().unwrap();

        let restored = cache
            .restore(
                5,
                &["b".to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();

        assert!(
            matches!(restored.hit, Some((_, Source::S3Downloaded(_)))),
            "{:?}",
            restored
        );
        assert!(!old.exists(), "archive unused for 2 days kept");
        assert!(cache.archive_path(5, "b").exists());
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
    async fn caches_saved_by_older_versions_still_restore() {
        let cache_root = tempdir().unwrap();
        let cache = LocalCache::new(cache_root.path());
        // The first versions wrote ZIPs with deflate entries
        let path = cache.legacy_path(42, "main");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        zip.start_file(
            "vendor/lib.txt",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        std::io::Write::write_all(&mut zip, b"old").unwrap();
        zip.finish().unwrap();

        let consumer = tempdir().unwrap();
        let restored = cache
            .restore(
                42,
                &["main".to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(restored.hit.is_some());
        assert_eq!(
            std::fs::read(consumer.path().join("vendor/lib.txt")).unwrap(),
            b"old"
        );

        // A save writes the new format and drops the old archive
        let producer = workspace("new");
        cache
            .save(
                42,
                "main",
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert!(!path.exists());
        let archive = std::fs::read(cache.archive_path(42, "main")).unwrap();
        assert_eq!(
            artifacts::cache_format(&archive),
            Some(artifacts::CacheFormat::TarZst)
        );
        assert_eq!(cache.stats().0, 1);
    }

    /// A ZIP (format 1) of vendor/lib.txt = `content`, as versions up to
    /// 0.10 saved it
    async fn legacy_archive(content: &str) -> Vec<u8> {
        let ws = workspace(content);
        let staging = tempdir().unwrap();
        let archive = artifacts::create_archive(
            ws.path().to_str().unwrap(),
            &["vendor".to_string()],
            &[],
            "zip-zstd",
            staging.path(),
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::read(archive.file.path()).unwrap()
    }

    /// In a fleet where older runners still save ZIPs, a key without a
    /// tar.zst archive in S3 restores from the ZIP; the next save writes
    /// the new format under its own name, which older runners never read
    #[tokio::test]
    async fn s3_archives_of_older_runners_are_restored() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.zip"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"old\"")
                    .set_body_bytes(legacy_archive("from-zip").await),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.zip"))
            .and(wiremock::matchers::header("if-none-match", "\"old\""))
            .respond_with(ResponseTemplate::new(304))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/ci/project-5/main.tar.zst"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"new\""))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let job = tempdir().unwrap();
        let job_path = job.path().to_str().unwrap();

        let first = cache
            .restore(5, &["main".to_string()], job_path, &Stop::default())
            .await
            .unwrap();
        assert!(
            matches!(first.hit, Some((_, Source::S3Downloaded(_)))),
            "{:?}",
            first
        );
        assert_eq!(
            std::fs::read_to_string(job.path().join("vendor/lib.txt")).unwrap(),
            "from-zip"
        );
        assert!(cache.legacy_path(5, "main").exists());
        // Unchanged: the local ZIP is used
        let second = cache
            .restore(5, &["main".to_string()], job_path, &Stop::default())
            .await
            .unwrap();
        assert!(
            matches!(second.hit, Some((_, Source::S3NotModified))),
            "{:?}",
            second
        );

        let saved = cache
            .save(5, "main", job_path, &["vendor".to_string()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.upload, Some(Ok(Upload::Sent)));
        assert!(!cache.legacy_path(5, "main").exists());
        let gets: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "GET")
            .map(|request| request.url.path().to_string())
            .collect();
        // The new name first, every time
        assert_eq!(
            gets,
            [
                "/ci/project-5/main.tar.zst",
                "/ci/project-5/main.zip",
                "/ci/project-5/main.tar.zst",
                "/ci/project-5/main.zip"
            ]
        );
    }

    /// A download is extracted as it arrives; one that breaks off midway is
    /// a warning, and the local copy is restored over what it extracted
    #[tokio::test]
    async fn a_download_cut_short_falls_back_to_the_local_copy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempdir().unwrap();
        let ws = workspace("local");
        let local = LocalCache::new(root.path());
        local
            .save(
                5,
                "main",
                ws.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();
        // A larger archive in S3, of which the server sends half
        let big = tempdir().unwrap();
        std::fs::create_dir(big.path().join("vendor")).unwrap();
        let mut seed: u64 = 3;
        let noise: Vec<u8> = (0..1 << 20)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect();
        std::fs::write(big.path().join("vendor/blob.bin"), &noise).unwrap();
        let staging = tempdir().unwrap();
        let remote_archive = artifacts::create_cache_archive(
            big.path().to_str().unwrap(),
            &["vendor".to_string()],
            staging.path(),
        )
        .await
        .unwrap()
        .unwrap();
        let body = std::fs::read(remote_archive.file.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0u8; 4096]).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"cut\"\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body[..body.len() / 2]).await.unwrap();
        });
        let cache = LocalCache::new(root.path()).with_remote(
            crate::runner_daemon::s3::Bucket::new(
                &crate::runner_daemon::config::S3CacheConfig {
                    url: format!("http://{}/ci", address),
                    access_key: "AK".to_string(),
                    secret_key: "SK".to_string(),
                    region: None,
                },
                reqwest::Client::new(),
            )
            .unwrap(),
        );
        let job = tempdir().unwrap();

        let restored = cache
            .restore(
                5,
                &["main".to_string()],
                job.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        server.abort();

        assert!(
            matches!(restored.hit, Some((_, Source::Local))),
            "{:?}",
            restored
        );
        assert_eq!(restored.warnings.len(), 1, "{:?}", restored.warnings);
        assert_eq!(
            std::fs::read_to_string(job.path().join("vendor/lib.txt")).unwrap(),
            "local"
        );
        // The broken download did not replace the local copy
        assert!(!sidecar(&cache.archive_path(5, "main")).exists());
    }

    /// An archive from S3 is untrusted: one escaping the workspace fails the
    /// restore as a local one would, and is not kept
    #[tokio::test]
    async fn a_hostile_archive_from_s3_is_refused_and_not_kept() {
        let mut header = tar::Header::new_gnu();
        header.as_old_mut().name[..14].copy_from_slice(b"../escaped.txt");
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(5);
        header.set_cksum();
        let mut tar = tar::Builder::new(Vec::new());
        tar.append(&header, &b"pwned"[..]).unwrap();
        let hostile = zstd::encode_all(&tar.into_inner().unwrap()[..], 1).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.tar.zst"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(hostile))
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let parent = tempdir().unwrap();
        let job = parent.path().join("job");

        let error = cache
            .restore(
                5,
                &["main".to_string()],
                job.to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Unsafe path"), "{:#}", error);
        assert!(!parent.path().join("escaped.txt").exists());
        assert!(!cache.archive_path(5, "main").exists());
    }

    /// The hash the uploader stored with the object must match what arrived
    #[tokio::test]
    async fn a_download_that_does_not_match_its_hash_is_not_kept() {
        let server = MockServer::start().await;
        let producer = workspace("dep");
        let elsewhere = tempdir().unwrap();
        let other = LocalCache::new(elsewhere.path());
        other
            .save(
                5,
                "main",
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.tar.zst"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .insert_header(crate::runner_daemon::s3::META_SHA256, "0".repeat(64))
                    .set_body_bytes(std::fs::read(other.archive_path(5, "main")).unwrap()),
            )
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let job = tempdir().unwrap();

        let restored = cache
            .restore(
                5,
                &["main".to_string()],
                job.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();

        assert!(restored.hit.is_none(), "{:?}", restored);
        assert!(
            restored.warnings[0].contains("does not match its hash"),
            "{:?}",
            restored.warnings
        );
        assert!(!cache.archive_path(5, "main").exists());
    }

    /// cache_max_size counts archives of both formats
    #[tokio::test]
    async fn the_size_limit_counts_archives_of_both_formats() {
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path());
        let ws = workspace("dep");
        let ws_path = ws.path().to_str().unwrap();
        let paths = ["vendor".to_string()];
        let legacy = cache.legacy_path(1, "old");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, legacy_archive("old").await).unwrap();
        used_days_ago(&legacy, 3);
        cache.save(2, "main", ws_path, &paths).await.unwrap();
        used_days_ago(&cache.archive_path(2, "main"), 2);
        assert_eq!(cache.stats().0, 2);
        let size = std::fs::metadata(cache.archive_path(2, "main"))
            .unwrap()
            .len();

        // Room for two new-format archives: the old ZIP goes first
        LocalCache::new(root.path())
            .with_max_size(Some(size * 2))
            .save(3, "main", ws_path, &paths)
            .await
            .unwrap();

        assert!(!legacy.exists());
        assert!(cache.archive_path(2, "main").exists() && cache.archive_path(3, "main").exists());
        assert_eq!(cache.stats().0, 2);
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
                &Stop::default(),
            )
            .await
            .unwrap();

        assert_eq!(restored.hit.as_ref().map(|(k, _)| k.as_str()), Some("main"));
        assert_eq!(
            std::fs::read(consumer.path().join("vendor/lib.txt")).unwrap(),
            b"dep"
        );
        let miss = cache
            .restore(
                7,
                &["main".to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
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
        assert_eq!(
            entries,
            vec!["%2E%2E%2F%2E%2E%2Fescaped.tar.zst".to_string()]
        );
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

    /// Set the last use of `file` to `days` days ago
    fn used_days_ago(file: &Path, days: u64) {
        std::fs::File::options()
            .append(true)
            .open(file)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86400),
            )
            .unwrap();
    }

    /// pull-push: a job that restored a cache and did not change it does not
    /// upload it again; a changed one is uploaded
    #[tokio::test]
    async fn an_unchanged_cache_is_not_uploaded_again() {
        let server = MockServer::start().await;
        // S3 holds, as "v1", an archive of the files another host saved
        let producer = workspace("dep");
        let elsewhere = tempdir().unwrap();
        LocalCache::new(elsewhere.path())
            .save(
                5,
                "main",
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap();
        let archive = std::fs::read(elsewhere.path().join("project-5/main.tar.zst")).unwrap();
        Mock::given(method("GET"))
            .and(path("/ci/project-5/main.tar.zst"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(archive),
            )
            .mount(&server)
            .await;
        // An object an older runner uploaded: no content hash in metadata
        Mock::given(method("HEAD"))
            .and(path("/ci/project-5/main.tar.zst"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"v1\""))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/ci/project-5/main.tar.zst"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"v2\""))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let job = tempdir().unwrap();
        let job_path = job.path().to_str().unwrap();
        let restored = cache
            .restore(5, &["main".to_string()], job_path, &Stop::default())
            .await
            .unwrap();
        assert!(
            matches!(restored.hit, Some((_, Source::S3Downloaded(_)))),
            "{:?}",
            restored
        );

        let unchanged = cache
            .save(5, "main", job_path, &["vendor".to_string()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.upload, Some(Ok(Upload::Unchanged)));

        std::fs::write(job.path().join("vendor/lib.txt"), "changed").unwrap();
        let changed = cache
            .save(5, "main", job_path, &["vendor".to_string()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed.upload, Some(Ok(Upload::Sent)));
        let local = cache.archive_path(5, "main");
        assert_eq!(std::fs::read_to_string(sidecar(&local)).unwrap(), "\"v2\"");
        let hash = {
            use sha2::Digest;
            format!("{:x}", sha2::Sha256::digest(std::fs::read(&local).unwrap()))
        };
        assert_eq!(
            std::fs::read_to_string(hash_sidecar(&local)).unwrap(),
            format!("{} \"v2\"", hash)
        );
    }

    /// The same content is uploaded again once S3 lost the object (expired
    /// by a lifecycle rule, say)
    #[tokio::test]
    async fn an_unchanged_cache_s3_lost_is_uploaded_again() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"v1\""))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .expect(2)
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));
        let ws = workspace("dep");
        let ws_path = ws.path().to_str().unwrap();

        for _ in 0..2 {
            let saved = cache
                .save(5, "main", ws_path, &["vendor".to_string()])
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved.upload, Some(Ok(Upload::Sent)));
        }
    }

    /// Another host uploaded the same content (in parts, so its ETag is no
    /// MD5): the hash stored with the object tells, without a sidecar
    #[tokio::test]
    async fn the_same_content_from_another_host_is_not_uploaded() {
        let server = MockServer::start().await;
        let ws = workspace("dep");
        let ws_path = ws.path().to_str().unwrap();
        let elsewhere = tempdir().unwrap();
        let other = LocalCache::new(elsewhere.path());
        other
            .save(5, "main", ws_path, &["vendor".to_string()])
            .await
            .unwrap();
        let hash = {
            use sha2::Digest;
            let archive = std::fs::read(other.archive_path(5, "main")).unwrap();
            format!("{:x}", sha2::Sha256::digest(archive))
        };
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"0123-3\"")
                    .insert_header(crate::runner_daemon::s3::META_SHA256, hash.as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let root = tempdir().unwrap();
        let cache = LocalCache::new(root.path()).with_remote(remote(&server));

        let saved = cache
            .save(5, "main", ws_path, &["vendor".to_string()])
            .await
            .unwrap()
            .unwrap();

        assert_eq!(saved.upload, Some(Ok(Upload::Unchanged)));
        // A restore asks S3 with that object's ETag
        let local = cache.archive_path(5, "main");
        assert_eq!(
            std::fs::read_to_string(sidecar(&local)).unwrap(),
            "\"0123-3\""
        );
    }

    #[tokio::test]
    async fn the_size_limit_removes_the_least_recently_used_archives_of_all_projects() {
        let root = tempdir().unwrap();
        let ws = workspace("dep");
        let ws_path = ws.path().to_str().unwrap();
        let paths = ["vendor".to_string()];
        let unlimited = LocalCache::new(root.path());
        for project in 1..=3 {
            unlimited
                .save(project, "main", ws_path, &paths)
                .await
                .unwrap();
        }
        let archive = |project| unlimited.archive_path(project, "main");
        for (project, days) in [(1, 3), (2, 2), (3, 1)] {
            used_days_ago(&archive(project), days);
        }
        std::fs::write(sidecar(&archive(1)), "\"e\"").unwrap();
        std::fs::write(hash_sidecar(&archive(1)), "h \"e\"").unwrap();
        let size = std::fs::metadata(archive(1)).unwrap().len();

        // Four archives, room for two and a half
        LocalCache::new(root.path())
            .with_max_size(Some(size * 5 / 2))
            .save(4, "main", ws_path, &paths)
            .await
            .unwrap();

        assert!(!archive(1).exists() && !archive(2).exists());
        assert!(!sidecar(&archive(1)).exists() && !hash_sidecar(&archive(1)).exists());
        assert!(archive(3).exists() && archive(4).exists());

        // The archive just saved stays, even alone above the limit
        LocalCache::new(root.path())
            .with_max_size(Some(1))
            .save(5, "main", ws_path, &paths)
            .await
            .unwrap();
        assert!(archive(5).exists());
        assert!(!archive(3).exists() && !archive(4).exists());
        assert_eq!(unlimited.stats().0, 1);
    }

    /// Runs `docker` with `args`, returning stdout
    fn docker(args: &[&str]) -> String {
        let output = std::process::Command::new("docker")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "docker {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Removes the container when the test ends, even after a failed assertion
    struct Container(String);

    impl Drop for Container {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.0])
                .output();
        }
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test -- --ignored"]
    async fn minio_shares_the_cache_between_hosts() {
        // minio/minio is no longer published; pgsty/minio is a maintained
        // community fork, pinned to a release and its multi-arch index digest
        // so the test runs the same server every time
        let container = Container(format!("turboci-minio-test-{}", std::process::id()));
        let name = container.0.as_str();
        docker(&[
            "run",
            "-d",
            "--rm",
            "--name",
            name,
            "-p",
            "127.0.0.1::9000",
            "-e",
            "MINIO_ROOT_USER=turboci",
            "-e",
            "MINIO_ROOT_PASSWORD=turboci-secret",
            "pgsty/minio:RELEASE.2026-08-04T00-00-00Z@sha256:\
             b6bfe7239bfc83fb90d31612d9704d86039dd714f7904b3f1ad68f211e602372",
            "server",
            "/data",
        ]);
        let port = docker(&["port", name, "9000/tcp"]);
        let endpoint = format!("http://{}", port.lines().next().unwrap());
        let bucket = || {
            crate::runner_daemon::s3::Bucket::new(
                &crate::runner_daemon::config::S3CacheConfig {
                    url: format!("{}/turboci-cache/team a", endpoint),
                    access_key: "turboci".to_string(),
                    secret_key: "turboci-secret".to_string(),
                    region: None,
                },
                reqwest::Client::new(),
            )
            .unwrap()
            .with_retry_delay(std::time::Duration::from_millis(200))
        };
        let mut ready = false;
        for _ in 0..50 {
            if bucket().create_bucket().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert!(ready, "MinIO did not start");
        assert_eq!(bucket().probe().await, Ok(()));

        let key = "feature/x y%";
        let host_a = tempdir().unwrap();
        let host_b = tempdir().unwrap();
        let cache_a = LocalCache::new(host_a.path()).with_remote(bucket());
        let cache_b = LocalCache::new(host_b.path()).with_remote(bucket());
        let producer = workspace("shared");
        let saved = cache_a
            .save(
                9,
                key,
                producer.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.upload, Some(Ok(Upload::Sent)));

        let consumer = tempdir().unwrap();
        let first = cache_b
            .restore(
                9,
                &[key.to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(first.hit, Some((_, Source::S3Downloaded(_)))),
            "{:?}",
            first
        );
        assert_eq!(
            std::fs::read_to_string(consumer.path().join("vendor/lib.txt")).unwrap(),
            "shared"
        );

        let second = cache_b
            .restore(
                9,
                &[key.to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(second.hit, Some((_, Source::S3NotModified))),
            "{:?}",
            second
        );

        // An archive above the multipart threshold goes in parts of 5 MiB,
        // S3's smallest, each signed with its hash over plain HTTP; another
        // host restores it and, saving the same files, finds them in S3 by
        // the stored hash
        let big = tempdir().unwrap();
        std::fs::create_dir(big.path().join("vendor")).unwrap();
        let mut seed: u64 = 7;
        let noise: Vec<u8> = (0..12 << 20)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect();
        std::fs::write(big.path().join("vendor/blob.bin"), &noise).unwrap();
        let parts = || bucket().with_multipart(6 << 20, 5 << 20);
        let uploader = tempdir().unwrap();
        let saved = LocalCache::new(uploader.path())
            .with_remote(parts())
            .save(
                9,
                "big",
                big.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert!(saved.size > 10 << 20, "{}", saved.size);
        assert_eq!(saved.upload, Some(Ok(Upload::Sent)));
        let host_c = tempdir().unwrap();
        let cache_c = LocalCache::new(host_c.path()).with_remote(parts());
        let restored_big = tempdir().unwrap();
        let big_hit = cache_c
            .restore(
                9,
                &["big".to_string()],
                restored_big.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(big_hit.hit, Some((_, Source::S3Downloaded(_)))),
            "{:?}",
            big_hit
        );
        assert!(std::fs::read(restored_big.path().join("vendor/blob.bin")).unwrap() == noise);
        let again = LocalCache::new(tempdir().unwrap().path())
            .with_remote(parts())
            .save(
                9,
                "big",
                big.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.upload, Some(Ok(Upload::Unchanged)));

        // Over HTTPS parts go unsigned (UNSIGNED-PAYLOAD), streamed from
        // the file: MinIO takes them the same way over plain HTTP
        let unsigned = || parts().with_unsigned_parts(true);
        let saved = LocalCache::new(tempdir().unwrap().path())
            .with_remote(unsigned())
            .save(
                9,
                "big-unsigned",
                big.path().to_str().unwrap(),
                &["vendor".to_string()],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.upload, Some(Ok(Upload::Sent)));
        let restored_unsigned = tempdir().unwrap();
        let unsigned_hit = LocalCache::new(tempdir().unwrap().path())
            .with_remote(unsigned())
            .restore(
                9,
                &["big-unsigned".to_string()],
                restored_unsigned.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(unsigned_hit.hit, Some((_, Source::S3Downloaded(_)))),
            "{:?}",
            unsigned_hit
        );
        assert!(std::fs::read(restored_unsigned.path().join("vendor/blob.bin")).unwrap() == noise);

        // A ZIP an older runner uploaded restores when the key has no
        // tar.zst archive
        let legacy = legacy_archive("from-0.10").await;
        let staged = tempdir().unwrap();
        let legacy_file = staged.path().join("old.zip");
        std::fs::write(&legacy_file, &legacy).unwrap();
        let hash = {
            use sha2::Digest;
            format!("{:x}", sha2::Sha256::digest(&legacy))
        };
        let b = bucket();
        b.put_file(
            &b.object_key(9, "old.zip"),
            &std::fs::File::open(&legacy_file).unwrap(),
            &hash,
        )
        .await
        .unwrap();
        let restored_old = tempdir().unwrap();
        let old = cache_b
            .restore(
                9,
                &["old".to_string()],
                restored_old.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(old.hit, Some((_, Source::S3Downloaded(_)))),
            "{:?}",
            old
        );
        assert_eq!(
            std::fs::read_to_string(restored_old.path().join("vendor/lib.txt")).unwrap(),
            "from-0.10"
        );

        // S3 gone: the restore falls back to the local copy
        docker(&["stop", name]);
        let offline = cache_b
            .restore(
                9,
                &[key.to_string()],
                consumer.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(offline.hit, Some((_, Source::Local))),
            "{:?}",
            offline
        );
        assert_eq!(offline.warnings.len(), 1, "{:?}", offline.warnings);
    }

    #[test]
    fn keys_are_expanded_and_default_when_empty() {
        let vars = HashMap::from([("CI_COMMIT_REF_SLUG".to_string(), "main".to_string())]);

        assert_eq!(resolve_key("deps-$CI_COMMIT_REF_SLUG", &vars), "deps-main");
        assert_eq!(resolve_key("$UNSET", &vars), "default");
    }
}
