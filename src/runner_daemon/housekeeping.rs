//! Workspace quota (`executor.workspace_max_size`): the kept workspaces of all
//! projects together stay within a size, the least recently used going first.
//! A workspace in use is never touched. Evicted workspaces are first renamed
//! (quick, done while jobs cannot claim workspaces), then deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// File in a workspace whose mtime says when a job last used it; the
/// executor writes it when a job starts (see `executor::LAST_USED`)
const LAST_USED: &str = ".turboci-last-used";
/// Name prefix of evicted workspaces waiting to be deleted
const EVICTED: &str = ".turboci-evicted-";

/// A workspace below the builds directory
#[derive(Debug, Clone)]
pub struct Workspace {
    pub path: PathBuf,
    /// Bytes of its files
    pub size: u64,
    pub last_used: SystemTime,
}

/// The workspaces in `root` with their sizes. Walks every file: run it off
/// the async runtime, and without the eviction lock.
pub fn scan(root: &Path) -> Vec<Workspace> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        // Symbolic links are not followed: only real directories count
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with(EVICTED))
        .map(|entry| {
            let path = entry.path();
            // Directories of older versions (job-<id>) have no marker
            let last_used = std::fs::metadata(path.join(LAST_USED))
                .or_else(|_| entry.metadata())
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            let size = walkdir::WalkDir::new(&path)
                .into_iter()
                .flatten()
                .filter(|file| file.file_type().is_file())
                .filter_map(|file| file.metadata().ok())
                .map(|meta| meta.len())
                .sum();
            Workspace {
                path,
                size,
                last_used,
            }
        })
        .collect()
}

/// Workspaces to delete so that all of them take at most `max_size`: the
/// least recently used first, never one in `in_use`. When the workspaces in
/// use alone are larger, every other one is picked.
pub fn over_quota<'a>(
    workspaces: &'a [Workspace],
    max_size: u64,
    in_use: &HashSet<PathBuf>,
) -> Vec<&'a Workspace> {
    let mut total: u64 = workspaces.iter().map(|w| w.size).sum();
    let mut candidates: Vec<&Workspace> = workspaces
        .iter()
        .filter(|w| !in_use.contains(&w.path))
        .collect();
    candidates.sort_by_key(|w| w.last_used);
    let mut picked = Vec::new();
    for workspace in candidates {
        if total <= max_size {
            break;
        }
        total -= workspace.size;
        picked.push(workspace);
    }
    picked
}

/// Move `workspace` out of the way: it is renamed to an evicted name in the
/// same directory (instant, whatever its size). False when it is gone or
/// cannot be moved.
pub fn detach(workspace: &Path) -> bool {
    let (Some(root), Some(name)) = (workspace.parent(), workspace.file_name()) else {
        return false;
    };
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let target = root.join(format!("{}{}-{}", EVICTED, name.to_string_lossy(), nanos));
    match std::fs::rename(workspace, &target) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!("Failed to evict workspace {}: {}", workspace.display(), e);
            false
        }
    }
}

/// Delete the evicted workspaces in `root`, including those an interrupted
/// run left behind
pub fn remove_detached(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let evicted = entry.file_name().to_string_lossy().starts_with(EVICTED);
        if evicted && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!("Failed to remove {}: {}", entry.path().display(), e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn workspace(name: &str, size: u64, age_hours: u64) -> Workspace {
        Workspace {
            path: PathBuf::from("/builds").join(name),
            size,
            last_used: SystemTime::now() - Duration::from_secs(age_hours * 3600),
        }
    }

    fn names(picked: &[&Workspace]) -> Vec<String> {
        picked
            .iter()
            .map(|w| w.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn picks_the_least_recently_used_until_the_rest_fits() {
        let workspaces = [
            workspace("project-1-0", 40, 1),
            workspace("project-2-0", 30, 50),
            workspace("project-3-0", 20, 10),
            workspace("project-4-0", 10, 100),
        ];
        let none = HashSet::new();

        assert!(over_quota(&workspaces, 100, &none).is_empty(), "fits");
        // 100 bytes, at most 75: the oldest (10) and the next oldest (30) go
        assert_eq!(
            names(&over_quota(&workspaces, 75, &none)),
            ["project-4-0", "project-2-0"]
        );
        assert_eq!(
            names(&over_quota(&workspaces, 0, &none)),
            ["project-4-0", "project-2-0", "project-3-0", "project-1-0"]
        );
    }

    #[test]
    fn never_picks_a_workspace_in_use() {
        let workspaces = [
            workspace("project-1-0", 40, 1),
            workspace("project-2-0", 30, 50),
            workspace("project-3-0", 20, 10),
            workspace("project-4-0", 10, 100),
        ];
        // The oldest and the largest old one are running jobs
        let in_use: HashSet<PathBuf> = [
            PathBuf::from("/builds/project-4-0"),
            PathBuf::from("/builds/project-2-0"),
        ]
        .into();

        assert_eq!(
            names(&over_quota(&workspaces, 75, &in_use)),
            ["project-3-0", "project-1-0"]
        );
        // In-use workspaces alone exceed the quota: all others go, they stay
        let picked = over_quota(&workspaces, 5, &in_use);
        assert_eq!(names(&picked), ["project-3-0", "project-1-0"]);
        assert!(picked.iter().all(|w| !in_use.contains(&w.path)));
    }

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn mark_used(workspace: &Path, at: SystemTime) {
        let marker = workspace.join(LAST_USED);
        write(&marker, 0);
        std::fs::File::options()
            .write(true)
            .open(marker)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[test]
    fn scans_sizes_and_last_use() {
        let root = tempfile::tempdir().unwrap();
        let day_ago = SystemTime::now() - Duration::from_secs(86400);
        let a = root.path().join("project-1-0");
        write(&a.join("project/src/main.rs"), 1000);
        write(&a.join("project/target/bin"), 4000);
        mark_used(&a, day_ago);
        let b = root.path().join("job-7");
        write(&b.join("project/x"), 10);
        // Not workspaces: a file, an evicted directory, a link
        write(&root.path().join("stray.zip"), 99);
        write(&root.path().join(format!("{}x-1/f", EVICTED)), 99);
        std::os::unix::fs::symlink(&a, root.path().join("link")).unwrap();

        let mut scanned = scan(root.path());
        scanned.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(scanned.len(), 2, "{:?}", scanned);
        let (job, project) = (&scanned[0], &scanned[1]);
        assert_eq!(project.path, a);
        assert_eq!(project.size, 5000);
        // From the marker (file systems may round the time a little)
        let off = project
            .last_used
            .duration_since(day_ago)
            .unwrap_or_else(|e| e.duration());
        assert!(off < Duration::from_secs(2), "{:?}", project.last_used);
        // Without a marker, the directory's own time
        assert_eq!(job.path, b);
        assert_eq!(job.size, 10);
        assert!(job.last_used > day_ago);
        assert!(scan(&root.path().join("missing")).is_empty());
    }

    #[test]
    fn detached_workspaces_are_moved_then_removed() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("project-1-0");
        write(&a.join("project/file"), 10);
        let kept = root.path().join("project-2-0");
        write(&kept.join("project/file"), 10);

        assert!(detach(&a));
        assert!(!a.exists());
        assert!(!detach(&a), "already gone");
        assert_eq!(scan(root.path()).len(), 1, "an evicted one is no workspace");

        remove_detached(root.path());
        let left: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["project-2-0"]);
    }
}
