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

/// zstd level of the entries of a ZIP cache archive (written by older
/// versions; tests write them to check they still restore)
const CACHE_ZSTD_LEVEL: i64 = 1;

/// zstd level of a cache archive (format 2, tar.zst)
const CACHE_TAR_ZSTD_LEVEL: i32 = 1;

/// Largest zstd window (as a power of two) a cache archive may ask for:
/// 16 MiB. It bounds what a crafted archive makes the runner allocate; the
/// levels used write with 2 MiB at most.
const CACHE_ZSTD_WINDOW_LOG_MAX: u32 = 24;

/// Longest name in a GNU long name or long link entry
const MAX_LONG_NAME: u64 = 64 * 1024;

/// Bytes that may follow the end of the tar inside the zstd stream (the
/// end-of-archive blocks and padding)
const MAX_TAR_TRAILER: u64 = 64 * 1024;

/// First bytes of a zstd frame
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// The two formats of cache archives
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheFormat {
    /// Format 1 (up to 0.10): a ZIP with zstd-compressed entries
    Zip,
    /// Format 2: a tar in one zstd stream
    TarZst,
}

/// The kinds of entries a cache archive holds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Dir = 0,
    File = 1,
    Symlink = 2,
}

/// A workspace-relative path without `.` components or trailing slashes,
/// as tar entries are extracted and as cached paths are found
fn normalized(rel: &Path) -> PathBuf {
    rel.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect()
}

/// Feed a path, length first, so that no two sequences of fields hash alike
fn hash_path(hasher: &mut blake3::Hasher, path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// The set of entries of a cache (workspace-relative paths and their kinds),
/// as a hash: what an extraction wrote, or what the cache's paths match in
/// the workspace. Equal sets mean the workspace holds no cached file the
/// archive did not write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntrySet(blake3::Hash);

impl EntrySet {
    fn of<'a>(entries: impl Iterator<Item = (&'a PathBuf, &'a EntryKind)>) -> Self {
        let mut hasher = blake3::Hasher::new();
        for (path, kind) in entries {
            hasher.update(&[*kind as u8]);
            hash_path(&mut hasher, path);
        }
        Self(hasher.finalize())
    }
}

/// The state of a cache's files in the workspace: which there are
/// (`entries`), and a hash of their metadata (`files`): kind, mode, and for
/// files and symlinks size, mtime, ctime (to the nanosecond), inode and
/// device, and a symlink's target. Content is not read.
///
/// Writing to a file, or replacing it, changes its ctime (which no system
/// call sets back) or its inode, so a content change changes the state,
/// unless it happens within the same tick of the file system's clock as the
/// state was taken: see `LocalCache`, which lets that tick pass after a
/// restore. mtime and size alone would miss changes that keep both, which
/// tools that preserve timestamps make (npm extracts every package file with
/// the same fixed mtime, `cp -p`, `rsync -t`, `tar -x`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheState {
    pub entries: EntrySet,
    files: blake3::Hash,
}

/// The state of the files `patterns` match in `workspace_path`, the ones a
/// cache archive of them would hold, off the async runtime. `None` when the
/// file system keeps whole seconds only (no file has a ctime with
/// nanoseconds): a change within the same second would go unseen.
pub async fn cache_state(workspace_path: &str, patterns: &[String]) -> Result<Option<CacheState>> {
    let workspace = workspace_path.to_string();
    let patterns = patterns.to_vec();
    tokio::task::spawn_blocking(move || scan_cache_state(&workspace, &patterns))
        .await
        .context("Scanning task panicked")?
}

fn scan_cache_state(workspace_path: &str, patterns: &[String]) -> Result<Option<CacheState>> {
    use std::collections::BTreeMap;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let root = Path::new(workspace_path);
    let mut found: BTreeMap<PathBuf, (EntryKind, std::fs::Metadata, PathBuf)> = BTreeMap::new();
    for path in collect_paths(workspace_path, patterns, &[], true)? {
        let rel = normalized(path.strip_prefix(root).unwrap_or(&path));
        if rel.as_os_str().is_empty() {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let kind = if meta.file_type().is_symlink() {
            EntryKind::Symlink
        } else if meta.is_dir() {
            EntryKind::Dir
        } else if meta.is_file() {
            EntryKind::File
        } else {
            // Sockets, FIFOs and devices are not archived
            continue;
        };
        found.insert(rel, (kind, meta, path));
    }

    let mut files = blake3::Hasher::new();
    let (mut timed, mut fine) = (false, false);
    for (rel, (kind, meta, path)) in &found {
        files.update(&[*kind as u8]);
        hash_path(&mut files, rel);
        files.update(&meta.mode().to_le_bytes());
        if *kind == EntryKind::Dir {
            // Its entries are in the set; its times change with them
            continue;
        }
        for value in [
            meta.size(),
            meta.mtime() as u64,
            meta.mtime_nsec() as u64,
            meta.ctime() as u64,
            meta.ctime_nsec() as u64,
            meta.ino(),
            meta.dev(),
        ] {
            files.update(&value.to_le_bytes());
        }
        timed = true;
        fine |= meta.ctime_nsec() != 0;
        if *kind == EntryKind::Symlink {
            let target = std::fs::read_link(path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let target = target.as_os_str().as_bytes();
            files.update(&(target.len() as u64).to_le_bytes());
            files.update(target);
        }
    }
    if timed && !fine {
        return Ok(None);
    }
    Ok(Some(CacheState {
        entries: EntrySet::of(found.iter().map(|(rel, (kind, _, _))| (rel, kind))),
        files: files.finalize(),
    }))
}

/// The format of an archive, from its first bytes
pub fn cache_format(head: &[u8]) -> Option<CacheFormat> {
    if head.starts_with(&ZSTD_MAGIC) {
        Some(CacheFormat::TarZst)
    } else if head.starts_with(b"PK") {
        Some(CacheFormat::Zip)
    } else {
        None
    }
}

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

/// Extract an untrusted cache archive of either format (told by its first
/// bytes) into the workspace, off the async runtime, until `stop`. Returns
/// the entries a tar.zst archive wrote (`None` for a ZIP).
pub async fn extract_cache_file(
    path: &Path,
    workspace_path: &str,
    stop: &Stop,
) -> Result<Option<EntrySet>> {
    let path = path.to_path_buf();
    let workspace = workspace_path.to_string();
    let stop = stop.clone();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        let mut head = Vec::with_capacity(ZSTD_MAGIC.len());
        (&mut file)
            .take(ZSTD_MAGIC.len() as u64)
            .read_to_end(&mut head)?;
        file.rewind()?;
        match cache_format(&head) {
            Some(CacheFormat::TarZst) => extract_tar_zst(
                std::io::BufReader::with_capacity(IO_BUF, file),
                &workspace,
                ExtractLimits::default(),
                &stop,
            )
            .map(Some),
            Some(CacheFormat::Zip) => extract_zip_reader(
                SeekBufReader::new(file)?,
                &workspace,
                ExtractLimits::default(),
                &stop,
            )
            .map(|()| None),
            None => anyhow::bail!("{} is not a cache archive", path.display()),
        }
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
                safe_fs::create_symlink(&dir, name, Path::new(&target))?;
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

/// Extract an untrusted tar.zst cache archive read from `reader` (a file,
/// or a download as it arrives) into `workspace_path`, until `stop`. The
/// zstd stream is read to its end, which checks its checksum: a truncated
/// or corrupted archive is an error, even after its last entry. Returns the
/// entries written.
pub fn extract_tar_zst<R: Read>(
    reader: R,
    workspace_path: &str,
    limits: ExtractLimits,
    stop: &Stop,
) -> Result<EntrySet> {
    let mut decoder =
        zstd::stream::read::Decoder::new(reader).context("Failed to read the cache archive")?;
    decoder.window_log_max(CACHE_ZSTD_WINDOW_LOG_MAX)?;
    let written = extract_tar_reader(&mut decoder, workspace_path, limits, stop)?;
    let rest = std::io::copy(
        &mut StopReader {
            inner: (&mut decoder).take(MAX_TAR_TRAILER + 1),
            stop,
        },
        &mut std::io::sink(),
    )
    .context("Failed to read the end of the cache archive")?;
    if rest > MAX_TAR_TRAILER {
        anyhow::bail!("Unexpected data after the end of the cache archive");
    }
    Ok(written)
}

/// The workspace-relative path of a tar entry. Absolute names and `..`
/// reject the whole archive, as for ZIP; `.` components are dropped.
fn tar_entry_path(name: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let unsafe_path = || {
        anyhow::anyhow!(
            "Unsafe path in archive: {:?}",
            String::from_utf8_lossy(name)
        )
    };
    if is_rooted_name(name) {
        return Err(unsafe_path());
    }
    let mut path = PathBuf::new();
    for component in Path::new(std::ffi::OsStr::from_bytes(name)).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            _ => return Err(unsafe_path()),
        }
    }
    Ok(path)
}

/// Extract an untrusted tar stream into `workspace_path` with the same
/// guarantees as `extract_zip_reader`: writes go through directory handles
/// that never follow a symlink, paths outside the workspace reject the
/// archive, symlinks pointing outside are skipped, setuid/setgid/sticky bits
/// are dropped, and the entry count and total size are limited. Hard links,
/// devices, FIFOs and other entry types reject the archive: the runner
/// never writes them. Directories get the default mode, as with ZIP.
/// Returns the entries written: of two entries with one path, the later.
fn extract_tar_reader<R: Read>(
    reader: R,
    workspace_path: &str,
    limits: ExtractLimits,
    stop: &Stop,
) -> Result<EntrySet> {
    use std::os::unix::ffi::OsStrExt;
    use tar::EntryType;

    let root = Path::new(workspace_path);
    std::fs::create_dir_all(root)?;
    let root_dir = safe_fs::open_root(root)?;
    let mut dirs = safe_fs::DirCache::default();
    let mut buf = vec![0u8; IO_BUF];
    let mut total_bytes: u64 = 0;
    let mut count: usize = 0;
    // At most `limits.max_entries` paths
    let mut extracted: std::collections::BTreeMap<PathBuf, EntryKind> = Default::default();
    // GNU long names and link targets, for the next entry
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;

    let mut archive = tar::Archive::new(reader);
    // Raw: the tar crate would read long names of any size into memory
    let entries = archive
        .entries()
        .context("Failed to read the cache archive")?
        .raw(true);
    for entry in entries {
        stop.check()?;
        let mut entry = entry.context("Failed to read the cache archive")?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() || kind.is_gnu_longlink() {
            let size = entry.header().entry_size()?;
            if size > MAX_LONG_NAME {
                anyhow::bail!("Archive entry name of {} bytes", size);
            }
            let mut value = Vec::with_capacity(size as usize);
            (&mut entry).take(size).read_to_end(&mut value)?;
            // NUL-terminated
            while value.last() == Some(&0) {
                value.pop();
            }
            if kind.is_gnu_longname() {
                long_name = Some(value);
            } else {
                long_link = Some(value);
            }
            continue;
        }

        count += 1;
        if count > limits.max_entries {
            anyhow::bail!("Archive has more than {} entries", limits.max_entries);
        }
        let name = match long_name.take() {
            Some(name) => name,
            None => entry.header().path_bytes().into_owned(),
        };
        let link = match long_link.take() {
            Some(link) => Some(link),
            None => entry
                .header()
                .link_name_bytes()
                .map(|link| link.into_owned()),
        };
        let rel_path = tar_entry_path(&name)?;

        match kind {
            EntryType::Directory => {
                if rel_path.as_os_str().is_empty() {
                    continue;
                }
                dirs.open(&root_dir, &rel_path)?;
                extracted.insert(rel_path, EntryKind::Dir);
            }
            EntryType::Symlink => {
                let target =
                    link.with_context(|| format!("Symlink {:?} has no target", rel_path))?;
                let target = Path::new(std::ffi::OsStr::from_bytes(&target));
                let (parent, file_name) = safe_fs::split_leaf(&rel_path)?;
                if symlink_stays_inside(&rel_path, target) {
                    let dir = dirs.open(&root_dir, parent)?;
                    safe_fs::create_symlink(&dir, file_name, target)?;
                    extracted.insert(rel_path, EntryKind::Symlink);
                } else {
                    warn!(
                        "Skipping symlink {:?} -> {:?}: points outside workspace",
                        rel_path, target
                    );
                }
            }
            EntryType::Regular => {
                let (parent, file_name) = safe_fs::split_leaf(&rel_path)?;
                let dir = dirs.open(&root_dir, parent)?;
                let remaining = limits.max_total_bytes.saturating_sub(total_bytes);
                let mut out_file = safe_fs::create_file(&dir, file_name)?;
                let written = copy_with(
                    &mut buf,
                    &mut StopReader {
                        inner: (&mut entry).take(remaining.saturating_add(1)),
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
                // Without setuid/setgid/sticky bits
                safe_fs::set_mode(&out_file, entry.header().mode()? & 0o777)?;
                extracted.insert(rel_path, EntryKind::File);
            }
            other => anyhow::bail!(
                "Unsupported entry {:?} in cache archive ({:?})",
                rel_path,
                other
            ),
        }
    }
    Ok(EntrySet::of(extracted.iter()))
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

    pub fn create_symlink(dir: impl AsFd, name: &OsStr, target: &Path) -> Result<()> {
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

/// A cache archive (format 2): a tar of the files, symlinks and directories
/// in one zstd stream. Caches never leave the runners, so they need not be
/// readable by GitLab. One stream compresses across files, which per-entry
/// compression (the ZIPs of format 1) cannot, and is read as it downloads.
///
/// It is written as a stream, so its SHA-256 is computed on the way.
/// Entries are in path order with a fixed timestamp, owner and group: the
/// same files give the same archive, and the same hash. Modes are kept
/// (without setuid/setgid/sticky bits), timestamps are not, as in format 1.
pub async fn create_cache_archive(
    workspace_path: &str,
    paths: &[String],
    staging_dir: &Path,
) -> Result<Option<Archive>> {
    create_archive(workspace_path, paths, &[], "tar-zstd", staging_dir).await
}

/// Archive the files matching `paths` in the format GitLab expects for the
/// artifact: `zip` (archives), `gzip` (reports; one gzip member per file) or
/// `raw` (a single file as is), or `tar-zstd` for caches (`zip-zstd`, the
/// format before, remains for tests), written to a temporary file in
/// `staging_dir`.
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
        // A cache keeps directories, empty ones included
        let files = collect_paths(&workspace, &patterns, &exclude, format == "tar-zstd")?;
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
            "tar-zstd" => {
                let out = HashingWriter::new(std::io::BufWriter::with_capacity(
                    IO_BUF,
                    file.as_file_mut(),
                ));
                let mut out = build_tar_zst(out, root, &files)?;
                out.flush()?;
                Some(out.hex())
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

/// Files matching `patterns` inside the workspace, in path order, with the
/// directories they match or are under when `with_dirs`.
///
/// Patterns that are absolute or contain `..` are skipped, matches reached through a
/// symlinked directory are skipped, and symlinks are returned as-is (never followed).
fn collect_paths(
    workspace_path: &str,
    patterns: &[String],
    exclude: &[String],
    with_dirs: bool,
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
                    if with_dirs || !item.file_type().is_dir() {
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

/// Write `entries` (files, symlinks, directories) as a tar into one zstd
/// stream on `out`, and finish both. Returns the writer.
fn build_tar_zst<W: Write>(out: W, root: &Path, entries: &[PathBuf]) -> Result<W> {
    let mut encoder = zstd::stream::write::Encoder::new(out, CACHE_TAR_ZSTD_LEVEL)?;
    encoder.include_checksum(true)?;
    let mut tar = tar::Builder::new(encoder);
    for path in entries {
        add_path_to_tar(&mut tar, path, root)?;
    }
    // Writes the end-of-archive blocks
    let encoder = tar.into_inner()?;
    Ok(encoder.finish()?)
}

/// Reads exactly `left` bytes from `inner`: a file that shrinks while it is
/// archived is an error, not a tar entry shorter than its header says
struct ExactReader<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "a file shrank while it was archived",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Add a regular file, symlink or directory to the tar under its
/// workspace-relative name, with a fixed timestamp and owner (sockets,
/// FIFOs and devices are left out)
fn add_path_to_tar<W: Write>(tar: &mut tar::Builder<W>, path: &Path, root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let relative_path = path
        .strip_prefix(root)
        .with_context(|| format!("{:?} is not inside the workspace", path))?;
    let meta = std::fs::symlink_metadata(path)?;
    let mut header = tar::Header::new_gnu();
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(0);
    if meta.file_type().is_symlink() {
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        tar.append_link(&mut header, relative_path, std::fs::read_link(path)?)?;
    } else if meta.is_dir() {
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(meta.permissions().mode() & 0o777);
        tar.append_data(&mut header, relative_path, std::io::empty())?;
    } else if meta.is_file() {
        let file = open_found_file(path)?;
        // The size and mode of what was opened, which the header must match
        let opened = file.metadata()?;
        if !opened.is_file() {
            return Ok(());
        }
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(opened.permissions().mode() & 0o777);
        header.set_size(opened.len());
        let data = ExactReader {
            inner: file,
            left: opened.len(),
        };
        tar.append_data(&mut header, relative_path, data)?;
    }
    Ok(())
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

        let paths = [
            "dist".to_string(),
            "*.xml".to_string(),
            "dist/js/*".to_string(),
        ];
        let ws_path = ws.path().to_str().unwrap();
        let artifact = create_archive(ws_path, &paths, &[], "zip", staging().path())
            .await
            .unwrap()
            .map(bytes)
            .expect("files matched");
        let cache = create_cache_archive(ws_path, &paths, staging().path())
            .await
            .unwrap()
            .map(bytes)
            .expect("files matched");

        assert_eq!(zip_names(&artifact), vec!["dist/js/app.js", "report.xml"]);
        // A cache keeps directories too, each entry once, in path order
        let names: Vec<String> = tar_entries(&cache).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["dist", "dist/js", "dist/js/app.js", "report.xml"]);
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
    async fn caches_use_tar_zstd_and_artifacts_deflate_that_gitlab_reads() {
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
        assert_eq!(cache_format(&cache), Some(CacheFormat::TarZst));
        assert!(cache.len() < 200, "{}", cache.len());
        let out = tempdir().unwrap();
        extract_tar_zst_to_workspace(&cache, out.path().to_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("a.txt")).unwrap(),
            "x".repeat(4096)
        );
    }

    /// Format 1 caches (ZIPs, written by versions up to 0.10)
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
        let first = create_archive(root, &paths, &[], "zip-zstd", stage.path())
            .await
            .unwrap()
            .unwrap();
        let second = create_archive(root, &paths, &[], "zip-zstd", stage.path())
            .await
            .unwrap()
            .unwrap();

        // Same files, same archive: its hash is the file's
        let hash = first.sha256.clone().unwrap();
        assert_eq!(second.sha256.as_deref(), Some(hash.as_str()));
        let data = bytes(first);
        assert_eq!(format!("{:x}", sha2::Sha256::digest(&data)), hash);
        assert_eq!(data, bytes(second));
        assert_eq!(cache_format(&data), Some(CacheFormat::Zip));

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

        let entries = tar_entries(&data);
        assert_eq!(entries.len(), 1);
        let leak = &entries[0];
        assert_eq!(leak.name, "leak");
        assert_eq!(leak.kind, tar::EntryType::Symlink);
        assert_eq!(leak.link.as_deref(), secret.to_str());
        assert!(leak.content.is_empty());
        assert!(!String::from_utf8_lossy(&data).contains("runner-token"));
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
        // Format 1 (ZIP, zstd per entry) against format 2 (tar.zst)
        for format in ["zip-zstd", "tar-zstd"] {
            let mut best = (f64::MAX, f64::MAX, 0u64);
            for _ in 0..7 {
                let paths = ["node_modules".to_string()];
                let started = std::time::Instant::now();
                let archive = create_archive(root, &paths, &[], format, stage.path())
                    .await
                    .unwrap()
                    .unwrap();
                let create = started.elapsed().as_secs_f64();
                let out = tempdir().unwrap();
                let started = std::time::Instant::now();
                extract_cache_file(
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
                "BENCH {} files={} input={:.1}MB archive={:.2}MB create={:.3}s extract={:.3}s",
                format,
                files,
                bytes as f64 / 1048576.0,
                best.2 as f64 / 1048576.0,
                best.0,
                best.1
            );
        }
    }

    // --- cache archives, format 2 (tar.zst) ---

    fn extract_tar_zst_to_workspace(data: &[u8], workspace_path: &str) -> Result<()> {
        extract_tar_zst_with_limits(data, workspace_path, ExtractLimits::default())
    }

    fn extract_tar_zst_with_limits(
        data: &[u8],
        workspace_path: &str,
        limits: ExtractLimits,
    ) -> Result<()> {
        extract_tar_zst(data, workspace_path, limits, &Stop::default()).map(|_| ())
    }

    /// An entry of a tar.zst archive, for assertions
    #[derive(Debug)]
    struct TarEntry {
        name: String,
        kind: tar::EntryType,
        mtime: u64,
        uid: u64,
        link: Option<String>,
        content: Vec<u8>,
    }

    fn tar_entries(data: &[u8]) -> Vec<TarEntry> {
        let decoder = zstd::stream::read::Decoder::new(data).unwrap();
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let mut content = Vec::new();
                entry.read_to_end(&mut content).unwrap();
                let header = entry.header();
                TarEntry {
                    name: entry.path().unwrap().to_string_lossy().into_owned(),
                    kind: header.entry_type(),
                    mtime: header.mtime().unwrap(),
                    uid: header.uid().unwrap(),
                    link: entry
                        .link_name()
                        .unwrap()
                        .map(|link| link.to_string_lossy().into_owned()),
                    content,
                }
            })
            .collect()
    }

    /// A crafted entry: the name and link are written verbatim into the
    /// header, as an attacker would
    struct Crafted<'a> {
        name: &'a [u8],
        kind: tar::EntryType,
        mode: u32,
        link: &'a [u8],
        content: &'a [u8],
    }

    fn crafted<'a>(name: &'a str, kind: tar::EntryType, content: &'a [u8]) -> Crafted<'a> {
        Crafted {
            name: name.as_bytes(),
            kind,
            mode: 0o644,
            link: b"",
            content,
        }
    }

    /// A tar.zst archive of `entries`
    fn tar_zst_of(entries: &[Crafted]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            let old = header.as_old_mut();
            old.name[..entry.name.len()].copy_from_slice(entry.name);
            old.linkname[..entry.link.len()].copy_from_slice(entry.link);
            header.set_entry_type(entry.kind);
            header.set_mode(entry.mode);
            header.set_size(entry.content.len() as u64);
            header.set_cksum();
            tar.append(&header, entry.content).unwrap();
        }
        zstd::encode_all(&tar.into_inner().unwrap()[..], 1).unwrap()
    }

    /// Round trip of everything a cache keeps: modes (setuid dropped),
    /// symlinks inside the workspace, empty directories, a file larger than
    /// the I/O buffers, and names and link targets longer than a tar header
    /// holds. Timestamps and owners are fixed, so the archive is the same
    /// every time.
    #[cfg(unix)]
    #[tokio::test]
    async fn cache_archives_round_trip_and_are_deterministic() {
        use sha2::Digest;
        use std::os::unix::fs::PermissionsExt;

        let ws = tempdir().unwrap();
        let p = |rel: &str| ws.path().join("vendor").join(rel);
        let long_dir = "d".repeat(60) + "/" + &"e".repeat(60);
        std::fs::create_dir_all(p(&long_dir)).unwrap();
        std::fs::create_dir_all(p("empty/nested")).unwrap();
        std::fs::create_dir_all(p("sub")).unwrap();
        std::fs::write(p("sub/file"), b"inside").unwrap();
        std::fs::write(p("tool.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(p("tool.sh"), std::fs::Permissions::from_mode(0o4755)).unwrap();
        std::fs::write(p("secret"), b"private").unwrap();
        std::fs::set_permissions(p("secret"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let large: Vec<u8> = (0..3 * IO_BUF + 17).map(|i| (i * 31 % 253) as u8).collect();
        let long_file = format!("{}/{}", long_dir, "f".repeat(40));
        std::fs::write(p(&long_file), &large).unwrap();
        std::os::unix::fs::symlink("sub/file", p("link")).unwrap();
        let long_target = format!("{}/{}", long_dir, "f".repeat(40));
        std::os::unix::fs::symlink(&long_target, p("long-link")).unwrap();
        let root = ws.path().to_str().unwrap();
        let paths = ["vendor".to_string()];
        let stage = staging();

        let first = create_cache_archive(root, &paths, stage.path())
            .await
            .unwrap()
            .unwrap();
        // Later timestamps change nothing in the archive
        std::fs::File::options()
            .append(true)
            .open(p("sub/file"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(86400))
            .unwrap();
        let second = create_cache_archive(root, &paths, stage.path())
            .await
            .unwrap()
            .unwrap();

        let hash = first.sha256.clone().unwrap();
        assert_eq!(second.sha256.as_deref(), Some(hash.as_str()));
        let data = bytes(first);
        assert_eq!(format!("{:x}", sha2::Sha256::digest(&data)), hash);
        assert_eq!(data, bytes(second));
        assert!(tar_entries(&data)
            .iter()
            .all(|entry| entry.mtime == 0 && entry.uid == 0));

        let out = tempdir().unwrap();
        let archive = stage.path().join("cache.tar.zst");
        std::fs::write(&archive, &data).unwrap();
        extract_cache_file(&archive, out.path().to_str().unwrap(), &Stop::default())
            .await
            .unwrap();

        let q = |rel: &str| out.path().join("vendor").join(rel);
        let mode = |rel: &str| std::fs::metadata(q(rel)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            std::fs::read(q("tool.sh")).unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        assert_eq!(mode("tool.sh"), 0o755);
        assert_eq!(mode("secret"), 0o600);
        assert_eq!(std::fs::read(q(&long_file)).unwrap(), large);
        assert_eq!(
            std::fs::read_link(q("link")).unwrap(),
            Path::new("sub/file")
        );
        assert_eq!(std::fs::read(q("link")).unwrap(), b"inside");
        assert_eq!(
            std::fs::read_link(q("long-link")).unwrap(),
            Path::new(&long_target)
        );
        assert!(q("empty/nested").is_dir());
        assert_eq!(std::fs::read_dir(q("empty/nested")).unwrap().count(), 0);
    }

    /// A tree with a file, a nested file, a symlink and an empty directory
    fn cache_tree() -> tempfile::TempDir {
        let ws = tempdir().unwrap();
        let vendor = ws.path().join("vendor");
        std::fs::create_dir_all(vendor.join("lib/empty")).unwrap();
        std::fs::write(vendor.join("a.txt"), b"aaaa").unwrap();
        std::fs::write(vendor.join("lib/b.txt"), b"bbbb").unwrap();
        std::os::unix::fs::symlink("a.txt", vendor.join("link")).unwrap();
        ws
    }

    async fn state_of(ws: &Path) -> CacheState {
        cache_state(ws.to_str().unwrap(), &["vendor/".to_string()])
            .await
            .unwrap()
            .expect("timestamps with nanoseconds")
    }

    /// What an extraction wrote is what the cache's paths then match, for
    /// the patterns spelled either way
    #[tokio::test]
    async fn extracted_entries_are_what_the_paths_match() {
        let src = cache_tree();
        let stage = staging();
        let paths = ["./vendor".to_string()];
        let archive = create_cache_archive(src.path().to_str().unwrap(), &paths, stage.path())
            .await
            .unwrap()
            .unwrap();
        let out = tempdir().unwrap();

        let written = extract_cache_file(
            archive.file.path(),
            out.path().to_str().unwrap(),
            &Stop::default(),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(state_of(out.path()).await.entries, written);
        // One more file under the paths: not what the archive wrote
        std::fs::write(out.path().join("vendor/other.txt"), b"x").unwrap();
        assert_ne!(state_of(out.path()).await.entries, written);
    }

    /// Any change to what an archive of the files would hold changes the
    /// state; looking twice does not
    #[tokio::test]
    async fn the_cache_state_follows_every_change() {
        use std::os::unix::fs::PermissionsExt;
        let ws = cache_tree();
        let vendor = ws.path().join("vendor");
        let mut seen = vec![state_of(ws.path()).await];
        assert_eq!(state_of(ws.path()).await, seen[0]);
        let changes: Vec<Box<dyn Fn() + '_>> = vec![
            // Same size, mtime set back
            Box::new(|| {
                let file = vendor.join("a.txt");
                let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
                std::fs::write(&file, b"AAAA").unwrap();
                std::fs::File::options()
                    .append(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
            }),
            // Replaced by a file of the same size and mtime
            Box::new(|| {
                let file = vendor.join("lib/b.txt");
                let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
                let new = vendor.join("lib/.b.new");
                std::fs::write(&new, b"BBBB").unwrap();
                std::fs::File::options()
                    .append(true)
                    .open(&new)
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
                std::fs::rename(&new, &file).unwrap();
            }),
            Box::new(|| {
                std::fs::set_permissions(
                    vendor.join("a.txt"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap()
            }),
            Box::new(|| {
                std::fs::remove_file(vendor.join("link")).unwrap();
                std::os::unix::fs::symlink("lib/b.txt", vendor.join("link")).unwrap();
            }),
            Box::new(|| std::fs::remove_dir(vendor.join("lib/empty")).unwrap()),
            Box::new(|| std::fs::write(vendor.join("new.txt"), b"").unwrap()),
            Box::new(|| std::fs::remove_file(vendor.join("a.txt")).unwrap()),
        ];
        for (n, change) in changes.iter().enumerate() {
            // Past the tick of the file system's clock
            std::thread::sleep(std::time::Duration::from_millis(20));
            change();
            let state = state_of(ws.path()).await;
            assert!(!seen.contains(&state), "change {} not seen", n);
            seen.push(state);
        }
    }

    #[test]
    fn formats_are_told_by_their_first_bytes() {
        assert_eq!(
            cache_format(&[0x28, 0xB5, 0x2F, 0xFD, 0]),
            Some(CacheFormat::TarZst)
        );
        assert_eq!(cache_format(b"PK\x03\x04"), Some(CacheFormat::Zip));
        assert_eq!(cache_format(b"PK\x05\x06"), Some(CacheFormat::Zip));
        for other in [&b""[..], b"\x28\xB5", b"\x1f\x8b\x08", b"ustar"] {
            assert_eq!(cache_format(other), None, "{:?}", other);
        }
    }

    #[tokio::test]
    async fn cache_files_of_both_formats_extract_and_others_are_refused() {
        let ws = tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), b"a").unwrap();
        let root = ws.path().to_str().unwrap();
        let stage = staging();
        for format in ["zip-zstd", "tar-zstd"] {
            let archive = create_archive(root, &["a.txt".to_string()], &[], format, stage.path())
                .await
                .unwrap()
                .unwrap();
            let out = tempdir().unwrap();
            extract_cache_file(
                archive.file.path(),
                out.path().to_str().unwrap(),
                &Stop::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read(out.path().join("a.txt")).unwrap(),
                b"a",
                "{}",
                format
            );
        }
        let junk = stage.path().join("junk");
        std::fs::write(&junk, b"not an archive").unwrap();
        let error = extract_cache_file(&junk, root, &Stop::default())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("is not a cache archive"),
            "{:#}",
            error
        );
    }

    #[test]
    fn tar_extract_rejects_parent_traversal() {
        let root = tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        for name in ["../escaped.txt", "a/../../escaped.txt"] {
            let data = tar_zst_of(&[crafted(name, tar::EntryType::Regular, b"pwned")]);
            let result = extract_tar_zst_to_workspace(&data, ws.to_str().unwrap());
            assert!(result.is_err(), "{}", name);
        }
        assert!(!root.path().join("escaped.txt").exists());
    }

    #[test]
    fn tar_extract_rejects_absolute_path() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("abs.txt");
        let data = tar_zst_of(&[crafted(
            target.to_str().unwrap(),
            tar::EntryType::Regular,
            b"pwned",
        )]);

        let result = extract_tar_zst_to_workspace(&data, ws.path().to_str().unwrap());

        assert!(result.is_err());
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn tar_extract_strips_setuid_bits() {
        use std::os::unix::fs::PermissionsExt;
        let ws = tempdir().unwrap();
        let mut tool = crafted("tool", tar::EntryType::Regular, b"#!/bin/sh");
        tool.mode = 0o6755;

        extract_tar_zst_to_workspace(&tar_zst_of(&[tool]), ws.path().to_str().unwrap()).unwrap();

        let mode = std::fs::metadata(ws.path().join("tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn tar_extract_does_not_write_through_existing_symlink() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();
        let data = tar_zst_of(&[crafted("link/pwned.txt", tar::EntryType::Regular, b"pwned")]);

        let result = extract_tar_zst_to_workspace(&data, ws.path().to_str().unwrap());

        assert!(result.is_err());
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn tar_extract_skips_symlinks_pointing_outside() {
        let ws = tempdir().unwrap();
        let link = |name: &'static str, target: &'static str| Crafted {
            name: name.as_bytes(),
            kind: tar::EntryType::Symlink,
            mode: 0o777,
            link: target.as_bytes(),
            content: b"",
        };
        let data = tar_zst_of(&[
            link("evil", "/etc"),
            link("up", "../../outside"),
            link("sub/up", "../../x"),
            link("ok", "sub/file"),
            link("sub/sibling", "../ok"),
        ]);

        extract_tar_zst_to_workspace(&data, ws.path().to_str().unwrap()).unwrap();

        for skipped in ["evil", "up", "sub/up"] {
            assert!(
                std::fs::symlink_metadata(ws.path().join(skipped)).is_err(),
                "{}",
                skipped
            );
        }
        assert_eq!(
            std::fs::read_link(ws.path().join("ok")).unwrap(),
            Path::new("sub/file")
        );
        assert_eq!(
            std::fs::read_link(ws.path().join("sub/sibling")).unwrap(),
            Path::new("../ok")
        );
    }

    /// A later entry cannot turn a directory the cache holds into a symlink
    #[cfg(unix)]
    #[test]
    fn tar_symlink_entry_cannot_replace_a_cached_directory() {
        let ws = tempdir().unwrap();
        let data = tar_zst_of(&[
            crafted("d/x", tar::EntryType::Regular, b"x"),
            Crafted {
                name: b"d",
                kind: tar::EntryType::Symlink,
                mode: 0o777,
                link: b"e",
                content: b"",
            },
            crafted("d/y", tar::EntryType::Regular, b"y"),
        ]);

        let result = extract_tar_zst_to_workspace(&data, ws.path().to_str().unwrap());

        assert!(result.is_err());
        assert!(std::fs::symlink_metadata(ws.path().join("d"))
            .unwrap()
            .is_dir());
        assert!(!ws.path().join("e").exists());
    }

    /// Hard links, devices and FIFOs: the runner never writes them, so an
    /// archive holding one is refused
    #[test]
    fn tar_extract_refuses_links_devices_and_fifos() {
        use tar::EntryType;
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"secret").unwrap();
        let hard_link = Crafted {
            name: b"stolen",
            kind: EntryType::Link,
            mode: 0o644,
            link: secret.to_str().unwrap().as_bytes(),
            content: b"",
        };
        let relative_link = Crafted {
            name: b"copy",
            kind: EntryType::Link,
            mode: 0o644,
            link: b"a",
            content: b"",
        };
        for (entry, label) in [
            (hard_link, "hard link outside"),
            (relative_link, "hard link inside"),
            (crafted("null", EntryType::Char, b""), "char device"),
            (crafted("disk", EntryType::Block, b""), "block device"),
            (crafted("pipe", EntryType::Fifo, b""), "fifo"),
            (crafted("pax", EntryType::XHeader, b""), "pax header"),
        ] {
            let data = tar_zst_of(&[crafted("a", EntryType::Regular, b"a"), entry]);
            let result = extract_tar_zst_to_workspace(&data, ws.path().to_str().unwrap());
            assert!(result.is_err(), "{}", label);
        }
        for name in ["stolen", "copy", "null", "disk", "pipe"] {
            assert!(
                std::fs::symlink_metadata(ws.path().join(name)).is_err(),
                "{}",
                name
            );
        }
    }

    #[test]
    fn tar_extract_enforces_size_entry_and_name_limits() {
        let ws = tempdir().unwrap();
        let big = vec![0u8; 4096];
        let data = tar_zst_of(&[
            crafted("a", tar::EntryType::Regular, &big),
            crafted("b", tar::EntryType::Regular, &big),
        ]);
        let ws_path = ws.path().to_str().unwrap();

        let too_big = ExtractLimits {
            max_entries: 10,
            max_total_bytes: 5000,
        };
        assert!(extract_tar_zst_with_limits(&data, ws_path, too_big).is_err());
        let too_many = ExtractLimits {
            max_entries: 1,
            max_total_bytes: u64::MAX,
        };
        assert!(extract_tar_zst_with_limits(&data, ws_path, too_many).is_err());

        // A GNU long name is read into memory: its size is bounded
        let long = vec![b'n'; MAX_LONG_NAME as usize + 1];
        let data = tar_zst_of(&[Crafted {
            name: b"././@LongLink",
            kind: tar::EntryType::GNULongName,
            mode: 0o644,
            link: b"",
            content: &long,
        }]);
        let error = extract_tar_zst_to_workspace(&data, ws_path).unwrap_err();
        assert!(error.to_string().contains("name of"), "{:#}", error);
    }

    #[test]
    fn tar_extract_refuses_truncated_and_trailing_data() {
        let ws = tempdir().unwrap();
        let ws_path = ws.path().to_str().unwrap();
        let data = tar_zst_of(&[crafted("a", tar::EntryType::Regular, &[7u8; 10_000])]);
        extract_tar_zst_to_workspace(&data, ws_path).unwrap();

        assert!(extract_tar_zst_to_workspace(&data[..data.len() - 4], ws_path).is_err());
        let mut trailing = data.clone();
        trailing.extend_from_slice(b"garbage");
        assert!(extract_tar_zst_to_workspace(&trailing, ws_path).is_err());
    }

    #[test]
    fn tar_extraction_stops_within_a_file() {
        let mut seed: u64 = 11;
        let noise: Vec<u8> = (0..4 << 20)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect();
        let data = tar_zst_of(&[
            crafted("big.bin", tar::EntryType::Regular, &noise),
            crafted("next.txt", tar::EntryType::Regular, b"next"),
        ]);
        let ws = tempdir().unwrap();
        let stop = Stop::default();
        let reader = StopAfter {
            inner: std::io::Cursor::new(data),
            read: 0,
            after: 1 << 20,
            stop: stop.clone(),
        };

        let error = extract_tar_zst(
            reader,
            ws.path().to_str().unwrap(),
            ExtractLimits::default(),
            &stop,
        )
        .unwrap_err();

        assert!(format!("{:#}", error).contains("stopped"), "{:#}", error);
        let written = std::fs::metadata(ws.path().join("big.bin")).unwrap().len();
        assert!(written < 4 << 20, "{}", written);
        assert!(!ws.path().join("next.txt").exists());
    }
}
