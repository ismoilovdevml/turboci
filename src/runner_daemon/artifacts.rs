use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tracing::warn;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::cancel::Stop;
use crate::gitlab::GitLabClient;

/// An archive staged in a temporary file (archives are never held in memory)
pub struct Archive {
    pub file: tempfile::NamedTempFile,
    pub len: u64,
    /// SHA-256 of the archive (hex), computed while it was written; set for
    /// cache archives
    pub sha256: Option<String>,
}

/// Buffer size for reading and writing archives
const IO_BUF: usize = 256 * 1024;

/// zstd level of cache entries: the fastest. On a node_modules-like tree
/// (15,000 files, 57.6 MB) the archive was 20.56 MB, against 20.54 MB with
/// the default level 3.
const CACHE_ZSTD_LEVEL: i64 = 1;

/// Cache entries smaller than this are stored: compressing them gains
/// nothing and costs a compressor per entry
const STORE_BELOW: u64 = 128;

/// Extensions of files that are compressed already: cache entries with them
/// are stored rather than compressed again
const COMPRESSED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "gz", "tgz", "zst", "zip", "whl", "jar", "woff2", "mp4",
    "xz", "bz2", "7z",
];

/// Download a dependency's artifacts archive to a file in `staging_dir` and
/// extract it into the workspace, until `stop`. Returns the archive size.
pub async fn download_and_extract_artifacts(
    gitlab: &GitLabClient,
    job_id: u64,
    token: &str,
    workspace_path: &str,
    staging_dir: &Path,
    stop: &Stop,
) -> Result<u64> {
    stop.check()?;
    let staged = tempfile::NamedTempFile::new_in(staging_dir)?;
    let size = gitlab
        .download_artifacts_to(job_id, token, staged.path())
        .await?;
    extract_archive_file(staged.path(), workspace_path, stop).await?;
    Ok(size)
}

/// Extract an untrusted ZIP file into the workspace, off the async runtime.
/// Dropping the future does not end the extraction: `stop` does, between
/// files and between the chunks of a file.
pub async fn extract_archive_file(path: &Path, workspace_path: &str, stop: &Stop) -> Result<()> {
    let path = path.to_path_buf();
    let workspace = workspace_path.to_string();
    let stop = stop.clone();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        extract_zip_reader(
            SeekBufReader::new(file)?,
            &workspace,
            ExtractLimits::default(),
            &stop,
        )
    })
    .await
    .context("Extraction task panicked")?
}

/// A buffered reader that keeps its buffer across seeks that land inside it.
/// `std::io::BufReader` empties its buffer on every seek, and the ZIP reader
/// seeks for every central directory record and every entry (mostly to
/// where it already is): small entries would cost a full refill each.
struct SeekBufReader<R> {
    inner: std::io::BufReader<R>,
    /// Position of the next byte `read` returns
    pos: u64,
}

impl<R: Read + Seek> SeekBufReader<R> {
    fn new(mut inner: R) -> std::io::Result<Self> {
        let pos = inner.stream_position()?;
        Ok(Self {
            inner: std::io::BufReader::with_capacity(IO_BUF, inner),
            pos,
        })
    }
}

impl<R: Read> Read for SeekBufReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for SeekBufReader<R> {
    fn seek(&mut self, target: SeekFrom) -> std::io::Result<u64> {
        let offset = match target {
            SeekFrom::Start(to) => i64::try_from(i128::from(to) - i128::from(self.pos)).ok(),
            SeekFrom::Current(offset) => Some(offset),
            SeekFrom::End(_) => None,
        };
        match offset.and_then(|offset| Some((offset, self.pos.checked_add_signed(offset)?))) {
            // Moves within the buffer when it can, seeks the file otherwise
            Some((offset, pos)) => {
                self.inner.seek_relative(offset)?;
                self.pos = pos;
            }
            None => self.pos = self.inner.seek(target)?,
        }
        Ok(self.pos)
    }
}

/// Copy `reader` to `writer` through `buf`, one buffer reused for every
/// file of an archive
fn copy_with(buf: &mut [u8], reader: &mut impl Read, writer: &mut impl Write) -> Result<u64> {
    let mut copied = 0u64;
    loop {
        let n = match reader.read(buf) {
            Ok(0) => return Ok(copied),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        writer.write_all(&buf[..n])?;
        copied += n as u64;
    }
}

/// Reads from `inner` until `stop` is set
struct StopReader<'a, R> {
    inner: R,
    stop: &'a Stop,
}

impl<R: std::io::Read> std::io::Read for StopReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stop.check().map_err(std::io::Error::other)?;
        self.inner.read(buf)
    }
}

/// Limits applied when extracting untrusted archives (zip bomb protection)
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_total_bytes: 10 * 1024 * 1024 * 1024, // 10 GiB uncompressed
        }
    }
}

/// Extract ZIP archive to workspace directory
#[cfg(test)]
fn extract_zip_to_workspace(zip_data: &[u8], workspace_path: &str) -> Result<()> {
    extract_zip_with_limits(zip_data, workspace_path, ExtractLimits::default())
}

#[cfg(test)]
fn extract_zip_with_limits(
    zip_data: &[u8],
    workspace_path: &str,
    limits: ExtractLimits,
) -> Result<()> {
    extract_zip_reader(
        std::io::Cursor::new(zip_data),
        workspace_path,
        limits,
        &Stop::default(),
    )
}

/// Whether an archive entry name is absolute: a leading `/` or `\\`, or a
/// Windows drive prefix like `C:`
fn is_rooted_name(name: &[u8]) -> bool {
    match name {
        [b'/' | b'\\', ..] => true,
        [drive, b':', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

/// Extract an untrusted ZIP archive into `workspace_path`.
///
/// The whole archive is rejected if any entry would land outside the workspace
/// (`..`, absolute names, or writing through a symlink), or if limits are exceeded.
fn extract_zip_reader<R: std::io::Read + std::io::Seek>(
    reader: R,
    workspace_path: &str,
    limits: ExtractLimits,
    stop: &Stop,
) -> Result<()> {
    let mut archive = ZipArchive::new(reader).context("Failed to read ZIP archive")?;

    if archive.len() > limits.max_entries {
        anyhow::bail!(
            "Archive has {} entries, limit is {}",
            archive.len(),
            limits.max_entries
        );
    }

    let root = Path::new(workspace_path);
    std::fs::create_dir_all(root)?;
    // Every operation below is relative to directory handles opened without
    // following symlinks. The workspace may be changed while we extract (a
    // job container is running), so a path checked once could be swapped
    // for a symlink before it is used; a handle cannot.
    let root_dir = safe_fs::open_root(root)?;
    // Entries come grouped by directory: the handles of the last one are
    // kept, so a file next to the previous one opens no directory at all
    let mut dirs = safe_fs::DirCache::default();
    let mut buf = vec![0u8; IO_BUF];
    let mut total_bytes: u64 = 0;

    for i in 0..archive.len() {
        stop.check()?;
        let mut file = archive.by_index(i)?;
        // zip's enclosed_name() turns "/etc/x" or "C:\x" into a relative path;
        // an archive carrying such names is rejected instead
        if is_rooted_name(file.name_raw()) {
            anyhow::bail!(
                "Unsafe path in archive: {:?}",
                String::from_utf8_lossy(file.name_raw())
            );
        }
        let rel_path = file.enclosed_name().with_context(|| {
            format!(
                "Unsafe path in archive: {:?}",
                String::from_utf8_lossy(file.name_raw())
            )
        })?;

        if file.is_dir() {
            dirs.open(&root_dir, &rel_path)?;
            continue;
        }

        let (parent, name) = safe_fs::split_leaf(&rel_path)?;
        let dir = dirs.open(&root_dir, parent)?;

        if file.is_symlink() {
            let mut target = String::new();
            std::io::Read::read_to_string(&mut file, &mut target)?;
            if symlink_stays_inside(&rel_path, Path::new(&target)) {
                safe_fs::create_symlink(&dir, name, &target)?;
            } else {
                warn!(
                    "Skipping symlink {:?} -> {:?}: points outside workspace",
                    rel_path, target
                );
            }
            continue;
        }

        // Bound the copy by the remaining budget; `size()` comes from the archive and can lie
        let remaining = limits.max_total_bytes.saturating_sub(total_bytes);
        let mut out_file = safe_fs::create_file(&dir, name)?;
        let written = copy_with(
            &mut buf,
            &mut StopReader {
                inner: std::io::Read::take(&mut file, remaining.saturating_add(1)),
                stop,
            },
            &mut out_file,
        )?;
        if written > remaining {
            anyhow::bail!(
                "Archive exceeds uncompressed size limit of {} bytes",
                limits.max_total_bytes
            );
        }
        total_bytes += written;

        // Permissions on the open file, without setuid/setgid/sticky bits
        if let Some(mode) = file.unix_mode() {
            safe_fs::set_mode(&out_file, mode & 0o777)?;
        }
    }

    Ok(())
}

/// Filesystem operations relative to directory handles that never follow a
/// symlink, so a concurrent writer cannot redirect them outside the workspace
mod safe_fs {
    use anyhow::{Context, Result};
    use rustix::fs::{self as rfs, Mode, OFlags};
    use rustix::io::Errno;
    use std::ffi::{OsStr, OsString};
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::path::{Component, Path};

    const DIR_FLAGS: OFlags = OFlags::DIRECTORY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::RDONLY)
        .union(OFlags::CLOEXEC);

    pub fn open_root(root: &Path) -> Result<OwnedFd> {
        rfs::open(root, DIR_FLAGS, Mode::empty())
            .with_context(|| format!("Failed to open workspace {:?}", root))
    }

    /// Open (creating when missing) each directory of `rel` below `root`
    #[cfg(test)]
    pub fn open_dirs(root: &OwnedFd, rel: &Path) -> Result<OwnedFd> {
        let mut dir = rfs::openat(root, ".", DIR_FLAGS, Mode::empty())?;
        for name in normal_components(rel)? {
            dir = open_or_create_dir(dir.as_fd(), name)
                .with_context(|| format!("Refusing to extract through {:?}", rel))?;
        }
        Ok(dir)
    }

    fn normal_components(rel: &Path) -> Result<Vec<&OsStr>> {
        rel.components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name),
                _ => anyhow::bail!("Unsafe path component in {:?}", rel),
            })
            .collect()
    }

    /// Deeper directories are opened for each entry instead of being kept:
    /// a crafted archive must not run the runner out of file descriptors
    const MAX_CACHED_DEPTH: usize = 64;

    /// The directory handles of the last path opened, each opened from the
    /// one before it (the first from the root) with `open_or_create_dir`,
    /// exactly as an uncached walk would. Reusing them gives the same
    /// guarantee as one walk does between opening a directory and writing
    /// into it: a handle stays bound to the directory it opened, so swapping
    /// a path component for a symlink later cannot redirect writes. Entries
    /// of the archive cannot replace a cached directory either: files and
    /// symlinks are created after `unlinkat` without `AT_REMOVEDIR`, which
    /// fails on a directory, and nothing here removes directories.
    #[derive(Default)]
    pub struct DirCache {
        open: Vec<(OsString, OwnedFd)>,
    }

    /// A directory handle, kept by the cache or opened for one entry
    pub enum Dir<'a> {
        Cached(BorrowedFd<'a>),
        Opened(OwnedFd),
    }

    impl AsFd for Dir<'_> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            match self {
                Dir::Cached(fd) => fd.as_fd(),
                Dir::Opened(fd) => fd.as_fd(),
            }
        }
    }

    impl DirCache {
        /// Open (creating when missing) each directory of `rel` below
        /// `root`, starting from the deepest directory it shares with the
        /// last path
        pub fn open<'a>(&'a mut self, root: &'a OwnedFd, rel: &Path) -> Result<Dir<'a>> {
            let names = normal_components(rel)?;
            let shared = self
                .open
                .iter()
                .zip(&names)
                .take_while(|((open, _), name)| open.as_os_str() == **name)
                .count();
            self.open.truncate(shared);
            let cached = names.len().min(MAX_CACHED_DEPTH);
            for name in &names[shared.min(cached)..cached] {
                let parent = self.open.last().map_or(root.as_fd(), |(_, fd)| fd.as_fd());
                let dir = open_or_create_dir(parent, name)
                    .with_context(|| format!("Refusing to extract through {:?}", rel))?;
                self.open.push((name.to_os_string(), dir));
            }
            let deepest = self.open.last().map_or(root.as_fd(), |(_, fd)| fd.as_fd());
            if names.len() <= cached {
                return Ok(Dir::Cached(deepest));
            }
            let mut dir = open_or_create_dir(deepest, names[cached])
                .with_context(|| format!("Refusing to extract through {:?}", rel))?;
            for name in &names[cached + 1..] {
                dir = open_or_create_dir(dir.as_fd(), name)
                    .with_context(|| format!("Refusing to extract through {:?}", rel))?;
            }
            Ok(Dir::Opened(dir))
        }
    }

    fn open_or_create_dir(dir: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd> {
        match rfs::openat(dir, name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(fd),
            Err(Errno::NOENT) => {
                match rfs::mkdirat(dir, name, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
                Ok(rfs::openat(dir, name, DIR_FLAGS, Mode::empty())?)
            }
            // ELOOP (a symlink) or ENOTDIR (a file): never walk through it
            Err(e) => anyhow::bail!("{:?} is not a plain directory ({})", name, e),
        }
    }

    pub fn split_leaf(rel: &Path) -> Result<(&Path, &OsStr)> {
        let name = rel
            .file_name()
            .with_context(|| format!("Archive entry {:?} has no file name", rel))?;
        Ok((rel.parent().unwrap_or(Path::new("")), name))
    }

    /// Remove whatever non-directory is at `name` (a leftover file or a
    /// symlink someone planted); the new entry is then created exclusively
    fn clear_leaf(dir: BorrowedFd<'_>, name: &OsStr) -> Result<()> {
        match rfs::unlinkat(dir, name, rfs::AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Cannot replace {:?}", name)),
        }
    }

    pub fn create_file(dir: impl AsFd, name: &OsStr) -> Result<std::fs::File> {
        let dir = dir.as_fd();
        clear_leaf(dir, name)?;
        // O_EXCL|O_NOFOLLOW: fails instead of following anything created in between
        let fd = rfs::openat(
            dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .with_context(|| format!("Failed to create {:?}", name))?;
        Ok(std::fs::File::from(fd))
    }

    pub fn create_symlink(dir: impl AsFd, name: &OsStr, target: &str) -> Result<()> {
        let dir = dir.as_fd();
        clear_leaf(dir, name)?;
        rfs::symlinkat(target, dir, name)
            .with_context(|| format!("Failed to create symlink {:?}", name))
    }

    pub fn set_mode(file: &std::fs::File, mode: u32) -> Result<()> {
        Ok(rfs::fchmod(file, Mode::from_raw_mode(mode as _))?)
    }
}

/// Whether a symlink at `link` (relative to the workspace) pointing at `target` resolves inside it
fn symlink_stays_inside(link: &Path, target: &Path) -> bool {
    use std::path::Component;

    if target.is_absolute() {
        return false;
    }
    let mut depth: usize = link.parent().map_or(0, |p| p.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// A cache archive: a ZIP whose entries are zstd-compressed (or stored, when
/// compressing gains nothing). Caches never leave the runners, so they need
/// not be readable by GitLab; zstd saves a cache about 2.5x faster than
/// deflate at the same size.
///
/// It is written as a stream (sizes in data descriptors after each entry,
/// no seeking back), so its SHA-256 is computed on the way. Entries are in
/// path order with a fixed timestamp: the same files give the same archive,
/// and the same hash.
pub async fn create_cache_archive(
    workspace_path: &str,
    paths: &[String],
    staging_dir: &Path,
) -> Result<Option<Archive>> {
    create_archive(workspace_path, paths, &[], "zip-zstd", staging_dir).await
}

/// Archive the files matching `paths` in the format GitLab expects for the
/// artifact: `zip` (archives), `gzip` (reports; one gzip member per file) or
/// `raw` (a single file as is), or `zip-zstd` for caches, written to a
/// temporary file in `staging_dir`.
/// `None` when no file matches.
pub async fn create_archive(
    workspace_path: &str,
    paths: &[String],
    exclude: &[String],
    format: &str,
    staging_dir: &Path,
) -> Result<Option<Archive>> {
    let workspace = workspace_path.to_string();
    let patterns = paths.to_vec();
    let exclude = exclude.to_vec();
    let format = format.to_string();
    let staging_dir = staging_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let files = collect_paths(&workspace, &patterns, &exclude)?;
        if files.is_empty() {
            return Ok(None);
        }
        let mut file = tempfile::NamedTempFile::new_in(&staging_dir)
            .with_context(|| format!("Failed to create a file in {}", staging_dir.display()))?;
        let root = Path::new(&workspace);
        let sha256 = match format.as_str() {
            "zip" => {
                let mut out = std::io::BufWriter::with_capacity(IO_BUF, file.as_file_mut());
                build_zip(ZipWriter::new(&mut out), root, &files, false)?;
                out.flush()?;
                None
            }
            "zip-zstd" => {
                let mut out = HashingWriter::new(std::io::BufWriter::with_capacity(
                    IO_BUF,
                    file.as_file_mut(),
                ));
                build_zip(ZipWriter::new_stream(&mut out), root, &files, true)?;
                out.flush()?;
                Some(out.hex())
            }
            "gzip" => {
                let mut out = std::io::BufWriter::with_capacity(IO_BUF, file.as_file_mut());
                build_gzip(&files, &mut out)?;
                out.flush()?;
                None
            }
            "raw" => match files.as_slice() {
                [path] => {
                    copy_regular_file(path, MAX_RAW_ARTIFACT_BYTES, file.as_file_mut())?;
                    None
                }
                _ => anyhow::bail!("raw artifacts need exactly one file, got {}", files.len()),
            },
            other => anyhow::bail!("Unsupported artifact format {:?}", other),
        };
        let len = file.as_file().metadata()?.len();
        Ok(Some(Archive { file, len, sha256 }))
    })
    .await
    .context("Archive task panicked")?
}

/// Passes writes on to `inner` and hashes them
struct HashingWriter<W> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        use sha2::Digest;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
        }
    }

    /// SHA-256 of everything written, in hex
    fn hex(self) -> String {
        use sha2::Digest;
        format!("{:x}", self.hasher.finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Largest `raw` report artifact
const MAX_RAW_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Copy a regular file (never a symlink, FIFO or device) of at most `max` bytes
fn copy_regular_file(path: &Path, max: u64, out: &mut impl Write) -> Result<()> {
    let before = std::fs::symlink_metadata(path)?;
    if !before.is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    let file = std::fs::File::open(path)?;
    // The path must still be the file checked above, not something swapped in
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if opened.dev() != before.dev() || opened.ino() != before.ino() {
            anyhow::bail!("{} changed while it was being read", path.display());
        }
    }
    let copied = std::io::copy(&mut file.take(max + 1), out)?;
    if copied > max {
        anyhow::bail!("{} is larger than {} bytes", path.display(), max);
    }
    Ok(())
}

/// Open a file found to be a regular file for reading, failing instead of
/// following a symlink swapped in since (and not waiting on a FIFO)
fn open_found_file(path: &Path) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("Failed to open {}", path.display()))?;
    Ok(std::fs::File::from(fd))
}

/// Files matching `patterns` inside the workspace.
///
/// Patterns that are absolute or contain `..` are skipped, matches reached through a
/// symlinked directory are skipped, and symlinks are returned as-is (never followed).
fn collect_paths(
    workspace_path: &str,
    patterns: &[String],
    exclude: &[String],
) -> Result<Vec<PathBuf>> {
    use std::collections::{BTreeSet, HashMap};
    use std::path::Component;

    let root = Path::new(workspace_path);
    let root_canon = root
        .canonicalize()
        .with_context(|| format!("Workspace {} not found", workspace_path))?;
    // Whether each parent directory of a match resolves inside the
    // workspace: matches share parents, and resolving one costs a system
    // call per path component
    let mut inside: HashMap<PathBuf, bool> = HashMap::new();

    let mut entries: BTreeSet<PathBuf> = BTreeSet::new();
    for pattern in patterns {
        let pattern_path = Path::new(pattern);
        if pattern_path.is_absolute()
            || pattern_path
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            warn!("Skipping artifact path {:?}: outside of workspace", pattern);
            continue;
        }

        let full_pattern = root.join(pattern_path);
        for entry in glob::glob(&full_pattern.to_string_lossy())? {
            let path = entry?;
            if !is_inside_workspace(&path, &root_canon, &mut inside) {
                warn!("Skipping {:?}: resolves outside of workspace", path);
                continue;
            }
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                for item in walkdir::WalkDir::new(&path).follow_links(false) {
                    let item = item?;
                    if !item.file_type().is_dir() {
                        entries.insert(item.into_path());
                    }
                }
            } else {
                entries.insert(path);
            }
        }
    }

    // `artifacts:exclude`: glob patterns on workspace-relative paths (`**` spans directories)
    let exclude: Vec<glob::Pattern> = exclude
        .iter()
        .filter_map(|p| match glob::Pattern::new(p) {
            Ok(pattern) => Some(pattern),
            Err(e) => {
                warn!("Ignoring invalid exclude pattern {:?}: {}", p, e);
                None
            }
        })
        .collect();
    let options = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    Ok(entries
        .into_iter()
        .filter(|path| {
            let relative = path.strip_prefix(root).unwrap_or(path);
            !exclude
                .iter()
                .any(|pattern| pattern.matches_path_with(relative, options))
        })
        .collect())
}

/// Add `files` to `zip` and finish it; `cache` picks the cache's entry
/// compression (zstd or stored) over deflate. Returns the writer.
fn build_zip<W: Write + Seek>(
    mut zip: ZipWriter<W>,
    root: &Path,
    files: &[PathBuf],
    cache: bool,
) -> Result<W> {
    let mut buf = vec![0u8; IO_BUF];
    for path in files {
        add_path_to_zip(&mut zip, path, root, cache, &mut buf)?;
    }
    Ok(zip.finish()?)
}

/// Concatenated gzip members, one per regular file (symlinks are skipped)
fn build_gzip(files: &[PathBuf], out: &mut impl Write) -> Result<()> {
    use flate2::write::GzEncoder;

    for path in files {
        if !std::fs::symlink_metadata(path)?.is_file() {
            continue;
        }
        let mut encoder = GzEncoder::new(&mut *out, flate2::Compression::default());
        std::io::copy(&mut open_found_file(path)?, &mut encoder)?;
        encoder.finish()?;
    }
    Ok(())
}

/// A glob match is inside the workspace if its parent directory canonicalizes under the root
/// (the match itself may be a symlink, which is archived as a link and not followed).
/// `known` remembers the answer for each parent.
fn is_inside_workspace(
    path: &Path,
    root_canon: &Path,
    known: &mut std::collections::HashMap<PathBuf, bool>,
) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if let Some(&inside) = known.get(parent) {
        return inside;
    }
    let inside = parent
        .canonicalize()
        .is_ok_and(|parent| parent.starts_with(root_canon));
    known.insert(parent.to_path_buf(), inside);
    inside
}

/// Options of an entry of `size` bytes named `name`: deflate for artifacts;
/// for caches, zstd unless the entry is tiny or compressed already
fn entry_options(cache: bool, name: &Path, size: u64) -> SimpleFileOptions {
    let options = SimpleFileOptions::default();
    if !cache {
        return options.compression_method(CompressionMethod::Deflated);
    }
    let compressed = name
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            COMPRESSED_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ext))
        });
    if compressed || size < STORE_BELOW {
        options.compression_method(CompressionMethod::Stored)
    } else {
        options
            .compression_method(CompressionMethod::Zstd)
            .compression_level(Some(CACHE_ZSTD_LEVEL))
    }
}

/// Add a regular file or symlink to the ZIP archive under its workspace-relative name
fn add_path_to_zip<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    path: &Path,
    workspace_root: &Path,
    cache: bool,
    buf: &mut [u8],
) -> Result<()> {
    let relative_path = path
        .strip_prefix(workspace_root)
        .with_context(|| format!("{:?} is not inside the workspace", path))?
        .to_string_lossy()
        .to_string();

    let meta = std::fs::symlink_metadata(path)?;

    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        // The zip crate stores link targets uncompressed
        zip.add_symlink(
            relative_path,
            target.to_string_lossy(),
            SimpleFileOptions::default(),
        )?;
        return Ok(());
    }
    if !meta.is_file() {
        return Ok(());
    }

    let options = entry_options(cache, path, meta.len());
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::PermissionsExt;
        options.unix_permissions(meta.permissions().mode())
    };

    let mut file = open_found_file(path)?;
    zip.start_file(relative_path, options)?;
    copy_with(buf, &mut file, zip)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    /// Archive contents, for assertions
    fn bytes(archive: Archive) -> Vec<u8> {
        std::fs::read(archive.file.path()).unwrap()
    }

    fn staging() -> tempfile::TempDir {
        tempdir().unwrap()
    }

    /// Build a ZIP in memory from (name, content, unix mode) entries, names written verbatim
    fn zip_of(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut zip = ZipWriter::new(std::io::Cursor::new(&mut buf));
        for (name, content, mode) in entries {
            zip.start_file(*name, SimpleFileOptions::default().unix_permissions(*mode))
                .unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
        buf
    }

    fn zip_names(data: &[u8]) -> Vec<String> {
        let mut archive = ZipArchive::new(std::io::Cursor::new(data)).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    // --- extraction (#1) ---

    #[test]
    fn extract_writes_regular_nested_files() {
        let ws = tempdir().unwrap();
        let data = zip_of(&[("dir/sub/a.txt", b"hello", 0o644)]);

        extract_zip_to_workspace(&data, ws.path().to_str().unwrap()).unwrap();

        let got = std::fs::read(ws.path().join("dir/sub/a.txt")).unwrap();
        assert_eq!(got, b"hello");
    }

    #[test]
    fn extract_rejects_parent_traversal() {
        let root = tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let data = zip_of(&[("../escaped.txt", b"pwned", 0o644)]);

        let result = extract_zip_to_workspace(&data, ws.to_str().unwrap());

        assert!(result.is_err());
        assert!(!root.path().join("escaped.txt").exists());
    }

    #[test]
    fn rooted_entry_names_are_detected() {
        for name in ["/etc/passwd", "\\\\server\\x", "\\x", "C:\\x", "c:x"] {
            assert!(is_rooted_name(name.as_bytes()), "{name}");
        }
        for name in ["a/b", "./a", "..", "dir:x", "1:x", ""] {
            assert!(!is_rooted_name(name.as_bytes()), "{name}");
        }
    }

    #[test]
    fn extract_rejects_absolute_path() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("abs.txt");
        let data = zip_of(&[(target.to_str().unwrap(), b"pwned", 0o644)]);

        let result = extract_zip_to_workspace(&data, ws.path().to_str().unwrap());

        assert!(result.is_err());
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn extract_strips_setuid_bits() {
        use std::os::unix::fs::PermissionsExt;
        let ws = tempdir().unwrap();
        let data = zip_of(&[("tool", b"#!/bin/sh", 0o4755)]);

        extract_zip_to_workspace(&data, ws.path().to_str().unwrap()).unwrap();

        let mode = std::fs::metadata(ws.path().join("tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn extract_does_not_write_through_existing_symlink() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();
        let data = zip_of(&[("link/pwned.txt", b"pwned", 0o644)]);

        let result = extract_zip_to_workspace(&data, ws.path().to_str().unwrap());

        assert!(result.is_err());
        assert!(!outside.path().join("pwned.txt").exists());
    }

    /// A job container can change the workspace while it is being extracted
    /// into. Writes go through directory handles, so replacing a directory
    /// with a symlink after it was opened cannot redirect them, and a planted
    /// leaf symlink is replaced rather than followed.
    #[cfg(unix)]
    #[test]
    fn extraction_writes_stay_bound_to_opened_directories() {
        use std::ffi::OsStr;

        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let root = safe_fs::open_root(ws.path()).unwrap();
        let dir = safe_fs::open_dirs(&root, Path::new("d")).unwrap();

        // The job swaps d for a symlink after the runner opened it
        std::fs::rename(ws.path().join("d"), ws.path().join("d.moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("d")).unwrap();
        // ...and plants a symlink where the next file will go
        std::os::unix::fs::symlink(outside.path().join("leaf"), ws.path().join("d.moved/f"))
            .unwrap();

        let mut file = safe_fs::create_file(&dir, OsStr::new("f")).unwrap();
        std::io::Write::write_all(&mut file, b"x").unwrap();
        safe_fs::set_mode(&file, 0o600).unwrap();

        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        let written = std::fs::symlink_metadata(ws.path().join("d.moved/f")).unwrap();
        assert!(written.is_file());
        // A new walk refuses to pass through the symlink
        assert!(safe_fs::open_dirs(&root, Path::new("d/sub")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn extract_skips_symlink_entry_pointing_outside() {
        let ws = tempdir().unwrap();
        let mut buf = Vec::new();
        {
            let mut zip = ZipWriter::new(std::io::Cursor::new(&mut buf));
            zip.add_symlink("evil", "/etc", SimpleFileOptions::default())
                .unwrap();
            zip.add_symlink("ok", "sub/file", SimpleFileOptions::default())
                .unwrap();
            zip.finish().unwrap();
        }

        extract_zip_to_workspace(&buf, ws.path().to_str().unwrap()).unwrap();

        assert!(std::fs::symlink_metadata(ws.path().join("evil")).is_err());
        let ok = std::fs::read_link(ws.path().join("ok")).unwrap();
        assert_eq!(ok, Path::new("sub/file"));
    }

    #[test]
    fn extract_enforces_size_and_entry_limits() {
        let ws = tempdir().unwrap();
        let big = vec![0u8; 4096];
        let data = zip_of(&[("a", &big, 0o644), ("b", &big, 0o644)]);
        let ws_path = ws.path().to_str().unwrap();

        let too_big = ExtractLimits {
            max_entries: 10,
            max_total_bytes: 5000,
        };
        assert!(extract_zip_with_limits(&data, ws_path, too_big).is_err());

        let too_many = ExtractLimits {
            max_entries: 1,
            max_total_bytes: u64::MAX,
        };
        assert!(extract_zip_with_limits(&data, ws_path, too_many).is_err());
    }

    /// Reads a ZIP and sets `stop` once more than `after` bytes were read
    struct StopAfter {
        inner: std::io::Cursor<Vec<u8>>,
        read: u64,
        after: u64,
        stop: Stop,
    }

    impl Read for StopAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n as u64;
            if self.read > self.after {
                self.stop.stop();
            }
            Ok(n)
        }
    }

    impl std::io::Seek for StopAfter {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn extraction_stops_within_a_file() {
        let mut data = Vec::new();
        let mut zip = ZipWriter::new(std::io::Cursor::new(&mut data));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("big.bin", stored).unwrap();
        zip.write_all(&vec![7u8; 4 << 20]).unwrap();
        zip.start_file("next.txt", stored).unwrap();
        zip.finish().unwrap();
        let ws = tempdir().unwrap();
        let stop = Stop::default();
        let reader = StopAfter {
            inner: std::io::Cursor::new(data),
            read: 0,
            after: 1 << 20,
            stop: stop.clone(),
        };

        let error = extract_zip_reader(
            reader,
            ws.path().to_str().unwrap(),
            ExtractLimits::default(),
            &stop,
        )
        .unwrap_err();

        assert!(error.to_string().starts_with("stopped"), "{:#}", error);
        let written = std::fs::metadata(ws.path().join("big.bin")).unwrap().len();
        assert!(written < 4 << 20, "{}", written);
        assert!(!ws.path().join("next.txt").exists());
    }

    #[test]
    fn extraction_reuses_directories_for_nested_and_sibling_entries() {
        let deep = "deep/".repeat(70);
        let deep_file = format!("{}f", deep);
        let mut data = Vec::new();
        {
            let mut zip = ZipWriter::new(std::io::Cursor::new(&mut data));
            let options = SimpleFileOptions::default();
            for (name, content) in [
                ("a/b/c/1", "1"),
                ("a/b/c/2", "2"),
                ("a/b/d/3", "3"),
                ("a/e/4", "4"),
                ("f/5", "5"),
                ("a/b/c/6", "6"),
                ("7", "7"),
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(content.as_bytes()).unwrap();
            }
            zip.add_directory("g", options).unwrap();
            zip.start_file("g/h/8", options).unwrap();
            zip.write_all(b"8").unwrap();
            // Deeper than the cache keeps
            zip.start_file(deep_file.as_str(), options).unwrap();
            zip.write_all(b"deep").unwrap();
            zip.start_file("a/b/c/9", options).unwrap();
            zip.write_all(b"9").unwrap();
            zip.finish().unwrap();
        }
        let ws = tempdir().unwrap();

        extract_zip_to_workspace(&data, ws.path().to_str().unwrap()).unwrap();

        for (name, content) in [
            ("a/b/c/1", "1"),
            ("a/b/c/2", "2"),
            ("a/b/d/3", "3"),
            ("a/e/4", "4"),
            ("f/5", "5"),
            ("a/b/c/6", "6"),
            ("7", "7"),
            ("g/h/8", "8"),
            (deep_file.as_str(), "deep"),
            ("a/b/c/9", "9"),
        ] {
            assert_eq!(
                std::fs::read_to_string(ws.path().join(name)).unwrap(),
                content,
                "{}",
                name
            );
        }
    }

    /// A later entry cannot turn a directory the cache holds into a symlink
    #[cfg(unix)]
    #[test]
    fn a_symlink_entry_cannot_replace_a_cached_directory() {
        let ws = tempdir().unwrap();
        let mut data = Vec::new();
        {
            let mut zip = ZipWriter::new(std::io::Cursor::new(&mut data));
            let options = SimpleFileOptions::default();
            zip.start_file("d/x", options).unwrap();
            zip.write_all(b"x").unwrap();
            zip.add_symlink("d", "e", options).unwrap();
            zip.start_file("d/y", options).unwrap();
            zip.write_all(b"y").unwrap();
            zip.finish().unwrap();
        }

        let result = extract_zip_to_workspace(&data, ws.path().to_str().unwrap());

        assert!(result.is_err());
        assert!(std::fs::symlink_metadata(ws.path().join("d"))
            .unwrap()
            .is_dir());
        assert!(!ws.path().join("e").exists());
    }

    /// Cached handles keep the guarantee of a single walk: a directory
    /// swapped for a symlink after it was opened does not redirect writes
    #[cfg(unix)]
    #[test]
    fn cached_directories_stay_bound_after_a_swap() {
        use std::ffi::OsStr;

        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let root = safe_fs::open_root(ws.path()).unwrap();
        let mut dirs = safe_fs::DirCache::default();
        let _ = dirs.open(&root, Path::new("d")).unwrap();

        std::fs::rename(ws.path().join("d"), ws.path().join("d.moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("d")).unwrap();

        let sub = dirs.open(&root, Path::new("d/sub")).unwrap();
        let mut file = safe_fs::create_file(&sub, OsStr::new("f")).unwrap();
        std::io::Write::write_all(&mut file, b"x").unwrap();

        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(ws.path().join("d.moved/sub/f").is_file());
        // A walk that does not start from the cached handle refuses the symlink
        let mut fresh = safe_fs::DirCache::default();
        assert!(fresh.open(&root, Path::new("d/sub")).is_err());
    }

    // --- collection (#2) ---

    #[tokio::test]
    async fn collect_includes_files_and_directories() {
        let ws = tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("dist/js")).unwrap();
        std::fs::write(ws.path().join("dist/js/app.js"), b"js").unwrap();
        std::fs::write(ws.path().join("report.xml"), b"xml").unwrap();

        let data = create_cache_archive(
            ws.path().to_str().unwrap(),
            &[
                "dist".to_string(),
                "*.xml".to_string(),
                "dist/js/*".to_string(),
            ],
            staging().path(),
        )
        .await
        .unwrap()
        .map(bytes)
        .expect("files matched");

        assert_eq!(zip_names(&data), vec!["dist/js/app.js", "report.xml"]);
    }

    #[tokio::test]
    async fn collect_skips_traversal_and_absolute_patterns() {
        let root = tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        std::fs::write(root.path().join("secret.toml"), b"token").unwrap();

        let data = create_cache_archive(
            ws.to_str().unwrap(),
            &[
                "../secret.toml".to_string(),
                root.path()
                    .join("secret.toml")
                    .to_string_lossy()
                    .to_string(),
            ],
            staging().path(),
        )
        .await
        .unwrap();

        assert!(data.is_none());
    }

    #[tokio::test]
    async fn exclude_patterns_drop_matching_files() {
        let ws = tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("dist/js")).unwrap();
        std::fs::create_dir_all(ws.path().join("dist/tmp/deep")).unwrap();
        std::fs::write(ws.path().join("dist/js/app.js"), b"js").unwrap();
        std::fs::write(ws.path().join("dist/js/app.js.map"), b"map").unwrap();
        std::fs::write(ws.path().join("dist/tmp/deep/scratch"), b"x").unwrap();

        let data = create_archive(
            ws.path().to_str().unwrap(),
            &["dist".to_string()],
            &["dist/**/*.map".to_string(), "dist/tmp/**".to_string()],
            "zip",
            staging().path(),
        )
        .await
        .unwrap()
        .map(bytes)
        .expect("files matched");

        assert_eq!(zip_names(&data), vec!["dist/js/app.js"]);
    }

    /// Compression method of every entry of a ZIP
    fn methods(data: &[u8]) -> Vec<zip::CompressionMethod> {
        let mut archive = ZipArchive::new(std::io::Cursor::new(data)).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().compression())
            .collect()
    }

    #[tokio::test]
    async fn caches_use_zstd_and_artifacts_deflate_that_gitlab_reads() {
        let ws = tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "x".repeat(4096)).unwrap();
        let root = ws.path().to_str().unwrap();
        let paths = ["a.txt".to_string()];

        let artifact = create_archive(root, &paths, &[], "zip", staging().path())
            .await
            .unwrap()
            .map(bytes)
            .unwrap();
        assert_eq!(methods(&artifact), [zip::CompressionMethod::Deflated]);

        let cache = create_cache_archive(root, &paths, staging().path())
            .await
            .unwrap()
            .map(bytes)
            .unwrap();
        assert_eq!(methods(&cache), [zip::CompressionMethod::Zstd]);
        let out = tempdir().unwrap();
        extract_zip_to_workspace(&cache, out.path().to_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("a.txt")).unwrap(),
            "x".repeat(4096)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cache_stores_tiny_and_compressed_files_and_extracts_both() {
        use sha2::Digest;

        let ws = tempdir().unwrap();
        let text = "x".repeat(4096);
        let image: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(ws.path().join("big.txt"), &text).unwrap();
        std::fs::write(ws.path().join("tiny.txt"), b"tiny").unwrap();
        std::fs::write(ws.path().join("img.PNG"), &image).unwrap();
        std::os::unix::fs::symlink("big.txt", ws.path().join("link")).unwrap();
        let root = ws.path().to_str().unwrap();
        let paths = ["*".to_string()];

        let stage = staging();
        let first = create_cache_archive(root, &paths, stage.path())
            .await
            .unwrap()
            .unwrap();
        let second = create_cache_archive(root, &paths, stage.path())
            .await
            .unwrap()
            .unwrap();

        // Same files, same archive: its hash is the file's
        let hash = first.sha256.clone().unwrap();
        assert_eq!(second.sha256.as_deref(), Some(hash.as_str()));
        let data = bytes(first);
        assert_eq!(format!("{:x}", sha2::Sha256::digest(&data)), hash);
        assert_eq!(data, bytes(second));

        let mut archive = ZipArchive::new(std::io::Cursor::new(&data)).unwrap();
        for (name, method) in [
            ("big.txt", CompressionMethod::Zstd),
            ("tiny.txt", CompressionMethod::Stored),
            ("img.PNG", CompressionMethod::Stored),
            ("link", CompressionMethod::Stored),
        ] {
            assert_eq!(
                archive.by_name(name).unwrap().compression(),
                method,
                "{}",
                name
            );
        }
        let out = tempdir().unwrap();
        extract_zip_to_workspace(&data, out.path().to_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("big.txt")).unwrap(),
            text
        );
        assert_eq!(std::fs::read(out.path().join("tiny.txt")).unwrap(), b"tiny");
        assert_eq!(std::fs::read(out.path().join("img.PNG")).unwrap(), image);
        assert_eq!(
            std::fs::read_link(out.path().join("link")).unwrap(),
            Path::new("big.txt")
        );
    }

    #[tokio::test]
    async fn gzip_archive_holds_each_report_file() {
        use std::io::Read;
        let ws = tempdir().unwrap();
        std::fs::write(ws.path().join("a.xml"), b"<a/>").unwrap();
        std::fs::write(ws.path().join("b.xml"), b"<b/>").unwrap();

        let data = create_archive(
            ws.path().to_str().unwrap(),
            &["*.xml".to_string()],
            &[],
            "gzip",
            staging().path(),
        )
        .await
        .unwrap()
        .map(bytes)
        .expect("files matched");

        let mut decoded = String::new();
        flate2::read::MultiGzDecoder::new(&data[..])
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, "<a/><b/>");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn raw_archive_refuses_symlinks_and_special_files() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("turboci-runner.toml");
        std::fs::write(&secret, b"runner_token = \"glrt-SECRET\"").unwrap();
        std::os::unix::fs::symlink(&secret, ws.path().join("gl-sast-report.json")).unwrap();
        let ws_path = ws.path().to_str().unwrap();

        let leaked = create_archive(
            ws_path,
            &["gl-sast-report.json".to_string()],
            &[],
            "raw",
            staging().path(),
        )
        .await;
        assert!(leaked.is_err(), "symlink target was read");

        let fifo = ws.path().join("report.json");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let blocked = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            create_archive(
                ws_path,
                &["report.json".to_string()],
                &[],
                "raw",
                staging().path(),
            ),
        )
        .await
        .expect("reading a FIFO must not block");
        assert!(blocked.is_err());
    }

    #[tokio::test]
    async fn raw_archive_requires_exactly_one_file() {
        let ws = tempdir().unwrap();
        std::fs::write(ws.path().join("report.json"), b"{}").unwrap();
        std::fs::write(ws.path().join("other.json"), b"[]").unwrap();
        let ws_path = ws.path().to_str().unwrap();

        let stage = staging();
        let one = create_archive(
            ws_path,
            &["report.json".to_string()],
            &[],
            "raw",
            stage.path(),
        )
        .await;
        assert_eq!(bytes(one.unwrap().unwrap()), b"{}");
        assert!(create_archive(
            ws_path,
            &["*.json".to_string()],
            &[],
            "raw",
            staging().path()
        )
        .await
        .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn collect_stores_symlink_as_link_not_target_content() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("secret.toml");
        std::fs::write(&secret, b"runner-token").unwrap();
        std::os::unix::fs::symlink(&secret, ws.path().join("leak")).unwrap();

        let data = create_cache_archive(
            ws.path().to_str().unwrap(),
            &["leak".to_string()],
            staging().path(),
        )
        .await
        .unwrap()
        .map(bytes)
        .expect("files matched");

        let mut archive = ZipArchive::new(std::io::Cursor::new(&data)).unwrap();
        let mut entry = archive.by_name("leak").unwrap();
        assert!(entry.is_symlink());
        let mut content = String::new();
        entry.read_to_string(&mut content).unwrap();
        assert!(!content.contains("runner-token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn collect_does_not_traverse_symlinked_directory() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), b"root:x:0:0").unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("etc")).unwrap();

        let data = create_cache_archive(
            ws.path().to_str().unwrap(),
            &["etc/*".to_string()],
            staging().path(),
        )
        .await
        .unwrap();

        assert!(data.is_none());
    }

    /// Synthetic node_modules-like tree: ~15k small files in nested
    /// directories, a few incompressible images. Timing only, not a check.
    #[tokio::test]
    #[ignore = "benchmark: cargo test --release -- --ignored bench_cache_archive --nocapture"]
    async fn bench_cache_archive() {
        let ws = tempdir().unwrap();
        let mut seed: u64 = 42;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        let mut files = 0;
        let mut bytes = 0u64;
        for pkg in 0..600 {
            for sub in 0..5 {
                let dir = ws
                    .path()
                    .join(format!("node_modules/pkg-{pkg}/lib/sub-{sub}"));
                std::fs::create_dir_all(&dir).unwrap();
                for f in 0..5 {
                    let (name, content) = if next() % 20 == 0 {
                        let n = 2000 + (next() % 30000) as usize;
                        (
                            format!("img-{f}.png"),
                            (0..n).map(|_| next() as u8).collect::<Vec<u8>>(),
                        )
                    } else {
                        let lines = 1 + next() % 120;
                        let mut s = String::new();
                        for l in 0..lines {
                            s.push_str(&format!(
                                "export function fn_{}_{l}(a, b) {{ return a * {} + b; }}\n",
                                next() % 1000,
                                next() % 97
                            ));
                        }
                        (format!("file-{f}.js"), s.into_bytes())
                    };
                    bytes += content.len() as u64;
                    std::fs::write(dir.join(name), content).unwrap();
                    files += 1;
                }
            }
        }
        let root = ws.path().to_str().unwrap();
        let stage = staging();
        let mut best = (f64::MAX, f64::MAX, 0u64);
        for _ in 0..7 {
            let started = std::time::Instant::now();
            let archive = create_cache_archive(root, &["node_modules".to_string()], stage.path())
                .await
                .unwrap()
                .unwrap();
            let create = started.elapsed().as_secs_f64();
            let out = tempdir().unwrap();
            let started = std::time::Instant::now();
            extract_archive_file(
                archive.file.path(),
                out.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
            let extract = started.elapsed().as_secs_f64();
            best = (best.0.min(create), best.1.min(extract), archive.len);
        }
        println!(
            "BENCH files={} input={:.1}MB archive={:.2}MB create={:.3}s extract={:.3}s",
            files,
            bytes as f64 / 1048576.0,
            best.2 as f64 / 1048576.0,
            best.0,
            best.1
        );
    }
}
