use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;
use zip::{ZipArchive, ZipWriter};

use crate::gitlab::GitLabClient;

/// An archive staged in a temporary file (archives are never held in memory)
pub struct Archive {
    pub file: tempfile::NamedTempFile,
    pub len: u64,
}

/// Download a dependency's artifacts archive to a file in `staging_dir` and
/// extract it into the workspace. Returns the archive size.
pub async fn download_and_extract_artifacts(
    gitlab: &GitLabClient,
    job_id: u64,
    token: &str,
    workspace_path: &str,
    staging_dir: &Path,
) -> Result<u64> {
    let staged = tempfile::NamedTempFile::new_in(staging_dir)?;
    let size = gitlab
        .download_artifacts_to(job_id, token, staged.path())
        .await?;
    extract_archive_file(staged.path(), workspace_path).await?;
    Ok(size)
}

/// Extract an untrusted ZIP file into the workspace, off the async runtime
pub async fn extract_archive_file(path: &Path, workspace_path: &str) -> Result<()> {
    let path = path.to_path_buf();
    let workspace = workspace_path.to_string();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        extract_zip_reader(file, &workspace, ExtractLimits::default())
    })
    .await
    .context("Extraction task panicked")?
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
    extract_zip_reader(std::io::Cursor::new(zip_data), workspace_path, limits)
}

/// Extract an untrusted ZIP archive into `workspace_path`.
///
/// The whole archive is rejected if any entry would land outside the workspace
/// (`..`, absolute names, or writing through a symlink), or if limits are exceeded.
fn extract_zip_reader<R: std::io::Read + std::io::Seek>(
    reader: R,
    workspace_path: &str,
    limits: ExtractLimits,
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
    let mut total_bytes: u64 = 0;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let rel_path = file
            .enclosed_name()
            .with_context(|| format!("Unsafe path in archive: {:?}", file.name()))?;

        if file.is_dir() {
            safe_fs::open_dirs(&root_dir, &rel_path)?;
            continue;
        }

        let (parent, name) = safe_fs::split_leaf(&rel_path)?;
        let dir = safe_fs::open_dirs(&root_dir, parent)?;

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
        let written = std::io::copy(
            &mut std::io::Read::take(&mut file, remaining.saturating_add(1)),
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
    use std::ffi::OsStr;
    use std::os::fd::OwnedFd;
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
    pub fn open_dirs(root: &OwnedFd, rel: &Path) -> Result<OwnedFd> {
        let mut dir = rfs::openat(root, ".", DIR_FLAGS, Mode::empty())?;
        for component in rel.components() {
            let Component::Normal(name) = component else {
                anyhow::bail!("Unsafe path component in {:?}", rel);
            };
            dir = open_or_create_dir(&dir, name)
                .with_context(|| format!("Refusing to extract through {:?}", rel))?;
        }
        Ok(dir)
    }

    fn open_or_create_dir(dir: &OwnedFd, name: &OsStr) -> Result<OwnedFd> {
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
    fn clear_leaf(dir: &OwnedFd, name: &OsStr) -> Result<()> {
        match rfs::unlinkat(dir, name, rfs::AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Cannot replace {:?}", name)),
        }
    }

    pub fn create_file(dir: &OwnedFd, name: &OsStr) -> Result<std::fs::File> {
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

    pub fn create_symlink(dir: &OwnedFd, name: &OsStr, target: &str) -> Result<()> {
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

/// Create ZIP archive from paths (used for artifacts and cache upload)
pub async fn create_zip_from_paths(
    workspace_path: &str,
    paths: &[String],
    staging_dir: &Path,
) -> Result<Option<Archive>> {
    create_archive(workspace_path, paths, &[], "zip", staging_dir).await
}

/// Archive the files matching `paths` in the format GitLab expects for the
/// artifact: `zip` (archives), `gzip` (reports; one gzip member per file) or
/// `raw` (a single file as is), written to a temporary file in `staging_dir`.
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
        match format.as_str() {
            "zip" => build_zip(Path::new(&workspace), &files, file.as_file_mut())?,
            "gzip" => build_gzip(&files, file.as_file_mut())?,
            "raw" => match files.as_slice() {
                [path] => copy_regular_file(path, MAX_RAW_ARTIFACT_BYTES, file.as_file_mut())?,
                _ => anyhow::bail!("raw artifacts need exactly one file, got {}", files.len()),
            },
            other => anyhow::bail!("Unsupported artifact format {:?}", other),
        };
        file.as_file_mut().flush()?;
        let len = file.as_file().metadata()?.len();
        Ok(Some(Archive { file, len }))
    })
    .await
    .context("Archive task panicked")?
}

/// Largest `raw` report artifact
const MAX_RAW_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Copy a regular file (never a symlink, FIFO or device) of at most `max` bytes
fn copy_regular_file(path: &Path, max: u64, out: &mut impl Write) -> Result<()> {
    use std::io::Read;

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

/// Files matching `patterns` inside the workspace.
///
/// Patterns that are absolute or contain `..` are skipped, matches reached through a
/// symlinked directory are skipped, and symlinks are returned as-is (never followed).
fn collect_paths(
    workspace_path: &str,
    patterns: &[String],
    exclude: &[String],
) -> Result<Vec<PathBuf>> {
    use std::collections::BTreeSet;
    use std::path::Component;

    let root = Path::new(workspace_path);
    let root_canon = root
        .canonicalize()
        .with_context(|| format!("Workspace {} not found", workspace_path))?;

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
            if !is_inside_workspace(&path, &root_canon) {
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

fn build_zip<W: Write + std::io::Seek>(root: &Path, files: &[PathBuf], out: W) -> Result<()> {
    let mut zip = ZipWriter::new(out);
    for path in files {
        add_path_to_zip(&mut zip, path, root)?;
    }
    zip.finish()?;
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
        std::io::copy(&mut std::fs::File::open(path)?, &mut encoder)?;
        encoder.finish()?;
    }
    Ok(())
}

/// A glob match is inside the workspace if its parent directory canonicalizes under the root
/// (the match itself may be a symlink, which is archived as a link and not followed)
fn is_inside_workspace(path: &Path, root_canon: &Path) -> bool {
    path.parent()
        .and_then(|parent| parent.canonicalize().ok())
        .is_some_and(|parent| parent.starts_with(root_canon))
}

/// Add a regular file or symlink to the ZIP archive under its workspace-relative name
fn add_path_to_zip<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    path: &Path,
    workspace_root: &Path,
) -> Result<()> {
    use zip::write::FileOptions;
    use zip::CompressionMethod;

    let relative_path = path
        .strip_prefix(workspace_root)
        .with_context(|| format!("{:?} is not inside the workspace", path))?
        .to_string_lossy()
        .to_string();

    let meta = std::fs::symlink_metadata(path)?;
    let options: FileOptions<'_, ()> =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        zip.add_symlink(relative_path, target.to_string_lossy(), options)?;
        return Ok(());
    }
    if !meta.is_file() {
        return Ok(());
    }

    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::PermissionsExt;
        options.unix_permissions(meta.permissions().mode())
    };

    zip.start_file(relative_path, options)?;
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, zip)?;

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
            .map(|i| archive.by_index(i).unwrap().name().to_string())
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

    // --- collection (#2) ---

    #[tokio::test]
    async fn collect_includes_files_and_directories() {
        let ws = tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("dist/js")).unwrap();
        std::fs::write(ws.path().join("dist/js/app.js"), b"js").unwrap();
        std::fs::write(ws.path().join("report.xml"), b"xml").unwrap();

        let data = create_zip_from_paths(
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

        let data = create_zip_from_paths(
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

        let data = create_zip_from_paths(
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

        let data = create_zip_from_paths(
            ws.path().to_str().unwrap(),
            &["etc/*".to_string()],
            staging().path(),
        )
        .await
        .unwrap();

        assert!(data.is_none());
    }
}
