//! Whether the uploads of a Docker job (cache, artifacts) can read what they
//! need in its workspace as the runner's user while the files the job's
//! containers made still belong to root, so handing the workspace back can
//! wait until the job has been reported.

use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// What a job's uploads read: what `patterns` (the paths of the cache
/// entries and artifacts to upload, relative to the project) match in
/// `project`, and with `untracked` any file of the project
#[derive(Debug, Clone)]
pub struct UploadReads {
    pub(crate) project: PathBuf,
    pub(crate) patterns: Vec<String>,
    pub(crate) untracked: bool,
    /// The job's file variables (`<project>.tmp`), removed after the uploads
    pub(crate) variables: PathBuf,
    /// Who reads: the runner's process
    pub(crate) reader: Reader,
}

impl UploadReads {
    pub fn new(project: PathBuf, patterns: Vec<String>, untracked: bool) -> Self {
        let mut variables = project.clone().into_os_string();
        variables.push(".tmp");
        Self {
            project,
            patterns,
            untracked,
            variables: variables.into(),
            reader: Reader::current(),
        }
    }
}

/// A user with its groups, whose access to a file its permission bits decide
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reader {
    pub(crate) uid: u32,
    /// Primary and supplementary groups
    pub(crate) groups: Vec<u32>,
}

impl Reader {
    /// The runner's process: its effective user and group, and its
    /// supplementary groups (none when they cannot be read, which only
    /// makes fewer files count as readable)
    fn current() -> Self {
        let mut groups = vec![rustix::process::getegid().as_raw()];
        if let Ok(more) = rustix::process::getgroups() {
            groups.extend(more.into_iter().map(|gid| gid.as_raw()));
        }
        Self {
            uid: rustix::process::geteuid().as_raw(),
            groups,
        }
    }

    fn may(&self, meta: &std::fs::Metadata, wanted: u32) -> bool {
        allows(meta.mode(), meta.uid(), meta.gid(), self, wanted)
    }
}

/// Whether each of `readers` may do `wanted` to the file of `meta`
fn all_may(readers: &[&Reader], meta: &std::fs::Metadata, wanted: u32) -> bool {
    readers.iter().all(|reader| reader.may(meta, wanted))
}

const READ: u32 = 0o4;
const WRITE: u32 = 0o2;
const SEARCH: u32 = 0o1;

/// Whether a file of `mode` owned by `uid:gid` grants `reader` all of
/// `wanted` (`READ`, `WRITE`, `SEARCH`), as the kernel decides: root may
/// (asked nothing but reading files, and reading, writing and searching
/// directories); otherwise only the owner's bits count for the owner, the
/// group's for its members, the others' for everyone else. POSIX ACLs are
/// not read: a job would have to set one naming the runner's user for the
/// bits to be wrong, and the upload would then fail on the file.
fn allows(mode: u32, uid: u32, gid: u32, reader: &Reader, wanted: u32) -> bool {
    if reader.uid == 0 {
        return true;
    }
    let bits = if uid == reader.uid {
        mode >> 6
    } else if reader.groups.contains(&gid) {
        mode >> 3
    } else {
        mode
    };
    bits & wanted == wanted
}

/// Where the matches of a pattern are: at or below its leading components
/// without glob characters
#[derive(Debug, PartialEq, Eq)]
struct Prefix {
    /// Relative to the project; empty for the whole project
    path: PathBuf,
    /// The pattern is `path` itself, without glob characters
    exact: bool,
    /// Matching may go through a symlinked directory below `path`: globs
    /// follow one where more components follow it, or with `**`. A last
    /// component (`dist/*`) matches a symlink as such, which is archived
    /// as a link and not followed.
    follows_links: bool,
}

/// The prefix of `pattern`; `None` for one the uploads skip, absolute or
/// with `..` (see `artifacts::collect_paths`)
fn prefix(pattern: &str) -> Option<Prefix> {
    let mut path = PathBuf::new();
    let (mut globbed, mut recursive) = (0, false);
    for component in Path::new(pattern).components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
            Component::CurDir => {}
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                if globbed > 0 || part.contains(['*', '?', '[']) {
                    globbed += 1;
                    recursive |= part.contains("**");
                } else {
                    path.push(part.as_ref());
                }
            }
        }
    }
    Some(Prefix {
        path,
        exact: globbed == 0,
        follows_links: globbed > 1 || recursive,
    })
}

/// What checking a part of the project found
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Readable,
    Unreadable,
    /// A symlink that matching may follow
    Link,
}

/// Whether the uploads can read everything they need in `job_dir` as
/// `reads.reader`, symbolic links not followed: the directories that hold
/// the project, the files and directories at and below the prefixes of
/// the patterns (with `untracked`, the whole project), `job_dir` to stage
/// artifact archives in, and the job's file variables to remove them
/// before the job is reported. Where a pattern may lead through a symlink,
/// the whole project is checked instead (matches stay inside it), and so
/// must every directory a symlink in it leads to. `git` lists the untracked
/// files, and must be able to read the project as well.
pub(crate) fn readable(job_dir: &Path, reads: &UploadReads, git: Option<&Reader>) -> bool {
    let reader = &reads.reader;
    // Artifact archives are staged in the job's directory
    match std::fs::symlink_metadata(job_dir) {
        Ok(meta) if meta.is_dir() && reader.may(&meta, WRITE | SEARCH) => {}
        _ => return false,
    }
    if !removable(&reads.variables, reader) {
        return false;
    }
    if reads.patterns.is_empty() && !reads.untracked {
        return true;
    }
    let mut readers = vec![reader];
    if reads.untracked {
        readers.extend(git);
    }
    // Root reads anything
    readers.retain(|reader| reader.uid != 0);
    readers.dedup();
    if readers.is_empty() {
        return true;
    }
    if !project_reachable(job_dir, &reads.project, &readers) {
        return false;
    }
    let mut whole = reads.untracked;
    for prefix in reads.patterns.iter().filter_map(|pattern| prefix(pattern)) {
        if whole {
            break;
        }
        match check_prefix(&reads.project, &prefix, &readers) {
            Found::Readable => {}
            Found::Unreadable => return false,
            Found::Link => whole = true,
        }
    }
    !whole || whole_project(&reads.project, &readers)
}

/// Whether `reader` can remove `dir` and everything in it (listing,
/// searching and writing each directory), or there is no such directory
fn removable(dir: &Path, reader: &Reader) -> bool {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        // Not followed: removing a symlink only takes its directory
        Ok(_) => return true,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    }
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .all(|entry| match entry {
            Ok(entry) if entry.file_type().is_dir() => entry
                .metadata()
                .is_ok_and(|meta| reader.may(&meta, READ | WRITE | SEARCH)),
            Ok(_) => true,
            Err(_) => false,
        })
}

/// Whether `readers` can search the directories from `job_dir` down to
/// `project` and list `project`, none of them a symlink
fn project_reachable(job_dir: &Path, project: &Path, readers: &[&Reader]) -> bool {
    let Ok(below) = project.strip_prefix(job_dir) else {
        return false;
    };
    let dir_allows = |dir: &Path, wanted| {
        matches!(std::fs::symlink_metadata(dir),
            Ok(meta) if meta.is_dir() && all_may(readers, &meta, wanted))
    };
    let mut dir = job_dir.to_path_buf();
    for component in below.components() {
        if !dir_allows(&dir, SEARCH) {
            return false;
        }
        dir.push(component);
    }
    dir_allows(&dir, READ | SEARCH)
}

/// Check what the matches of `prefix` in `project` need: searching the
/// directories down to it, reading what is at and below it
fn check_prefix(project: &Path, prefix: &Prefix, readers: &[&Reader]) -> Found {
    let mut path = project.to_path_buf();
    let components: Vec<_> = prefix.path.components().collect();
    for (index, component) in components.iter().enumerate() {
        path.push(component);
        let last = index + 1 == components.len();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // Nothing matches
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Found::Readable,
            Err(_) => return Found::Unreadable,
        };
        if meta.file_type().is_symlink() {
            // The match itself is archived as a link; anything else follows it
            return if last && prefix.exact {
                Found::Readable
            } else {
                Found::Link
            };
        }
        if !last {
            if !meta.is_dir() {
                return Found::Readable;
            }
            if !all_may(readers, &meta, SEARCH) {
                return Found::Unreadable;
            }
        }
    }
    check_tree(&path, readers, |_| {
        if prefix.follows_links {
            Found::Link
        } else {
            Found::Readable
        }
    })
}

/// Whether `readers` can read the whole project, and every symlink in it
/// that leads to a directory leads to one inside it
fn whole_project(project: &Path, readers: &[&Reader]) -> bool {
    let Ok(inside) = project.canonicalize() else {
        return false;
    };
    let found = check_tree(project, readers, |link| {
        match std::fs::metadata(link) {
            Ok(meta) if meta.is_dir() => {}
            // Never followed: a symlink to a file is archived as a link
            _ => return Found::Readable,
        }
        match link.canonicalize() {
            Ok(target) if target.starts_with(&inside) => Found::Readable,
            _ => Found::Unreadable,
        }
    });
    found == Found::Readable
}

/// Check everything at and below `root` (not followed when it is a
/// symlink): directories must be readable and searchable, regular files
/// readable; `link` decides about each symlink. Other files (sockets,
/// FIFOs, devices) are never archived.
fn check_tree(root: &Path, readers: &[&Reader], mut link: impl FnMut(&Path) -> Found) -> Found {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let Ok(entry) = entry else {
            return Found::Unreadable;
        };
        let kind = entry.file_type();
        let wanted = if kind.is_symlink() {
            match link(entry.path()) {
                Found::Readable => continue,
                found => return found,
            }
        } else if kind.is_dir() {
            READ | SEARCH
        } else if kind.is_file() {
            READ
        } else {
            continue;
        };
        match entry.metadata() {
            Ok(meta) if all_may(readers, &meta, wanted) => {}
            _ => return Found::Unreadable,
        }
    }
    Found::Readable
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn only_one_class_of_bits_counts() {
        let runner = Reader {
            uid: 4242,
            groups: vec![4242, 777],
        };
        // Owner
        assert!(allows(0o400, 4242, 0, &runner, READ));
        assert!(!allows(0o044, 4242, 0, &runner, READ), "owner bits rule");
        // Primary and supplementary group
        assert!(allows(0o040, 0, 4242, &runner, READ));
        assert!(allows(0o040, 0, 777, &runner, READ));
        assert!(!allows(0o404, 0, 777, &runner, READ), "group bits rule");
        // Others
        assert!(allows(0o004, 0, 0, &runner, READ));
        assert!(!allows(0o640, 0, 0, &runner, READ));
        // A directory needs every bit asked for
        assert!(allows(0o755, 0, 0, &runner, READ | SEARCH));
        assert!(!allows(0o744, 0, 0, &runner, READ | SEARCH), "no x");
        assert!(!allows(0o711, 0, 0, &runner, READ | SEARCH), "no r");
        assert!(!allows(0o755, 0, 0, &runner, WRITE | SEARCH));
        assert!(allows(0o300, 4242, 0, &runner, WRITE | SEARCH));
        // Root reads anything
        let root = Reader {
            uid: 0,
            groups: vec![0],
        };
        assert!(allows(0o000, 1000, 1000, &root, READ | SEARCH));
    }

    #[test]
    fn prefixes_stop_at_the_first_glob() {
        let p = |path: &str, exact, follows_links| {
            Some(Prefix {
                path: PathBuf::from(path),
                exact,
                follows_links,
            })
        };
        assert_eq!(prefix("dist"), p("dist", true, false));
        assert_eq!(prefix("./dist/"), p("dist", true, false));
        assert_eq!(prefix("dist//app.jar"), p("dist/app.jar", true, false));
        assert_eq!(prefix("dist/."), p("dist", true, false));
        assert_eq!(prefix("dist/*.js"), p("dist", false, false));
        assert_eq!(prefix("out/[ab]"), p("out", false, false));
        assert_eq!(prefix("build/*/reports"), p("build", false, true));
        assert_eq!(prefix("**/junit.xml"), p("", false, true));
        assert_eq!(prefix("target/**"), p("target", false, true));
        assert_eq!(prefix("."), p("", true, false));
        // Skipped by the uploads
        assert_eq!(prefix("../secret"), None);
        assert_eq!(prefix("dist/../../secret"), None);
        assert_eq!(prefix("/etc/passwd"), None);
    }

    /// A job directory (0700) with an empty project (0755)
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        chmod(&project, 0o755);
        (dir, project)
    }

    /// The owner of everything in `dir`, who plays the runner's user. As
    /// root, whom no bits stop, the files are given to uid 4242 first.
    fn owner(dir: &Path) -> Reader {
        if rustix::process::geteuid().is_root() {
            for entry in walkdir::WalkDir::new(dir) {
                std::os::unix::fs::lchown(entry.unwrap().path(), Some(4242), Some(4242)).unwrap();
            }
        }
        let meta = std::fs::metadata(dir).unwrap();
        Reader {
            uid: meta.uid(),
            groups: vec![meta.gid()],
        }
    }

    /// Someone in none of the owner's groups: the others' bits apply
    fn stranger(owner: &Reader) -> Reader {
        Reader {
            uid: owner.uid + 1,
            groups: vec![owner.groups[0] + 1],
        }
    }

    fn reads(project: &Path, patterns: &[&str], untracked: bool, reader: &Reader) -> UploadReads {
        let mut reads = UploadReads::new(
            project.to_path_buf(),
            patterns.iter().map(|p| p.to_string()).collect(),
            untracked,
        );
        reads.reader = reader.clone();
        reads
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn files_of_someone_else_that_anyone_may_read_need_no_hand_back() {
        let (dir, project) = workspace();
        std::fs::create_dir_all(project.join("dist/js")).unwrap();
        std::fs::write(project.join("dist/js/app.js"), "js").unwrap();
        std::fs::write(project.join("report.xml"), "xml").unwrap();
        for path in ["dist", "dist/js"] {
            chmod(&project.join(path), 0o755);
        }
        for path in ["dist/js/app.js", "report.xml"] {
            chmod(&project.join(path), 0o644);
        }
        let owner = owner(dir.path());
        let other = stranger(&owner);
        let patterns = ["dist/", "*.xml", "missing/file"];
        let ok = |untracked, reader: &Reader| {
            readable(
                dir.path(),
                &reads(&project, &patterns, untracked, reader),
                None,
            )
        };

        assert!(ok(false, &owner));
        assert!(ok(true, &owner));
        // 0644 files in 0755 directories, the job directory writable for
        // staging archives
        chmod(dir.path(), 0o777);
        assert!(ok(false, &other));
        assert!(ok(true, &other));
        chmod(dir.path(), 0o755);
        assert!(!ok(false, &other), "staging needs a writable job directory");
    }

    #[test]
    fn an_unreadable_file_or_directory_needs_a_hand_back() {
        let (dir, project) = workspace();
        std::fs::create_dir_all(project.join("dist/sub")).unwrap();
        std::fs::write(project.join("dist/sub/secret"), "s").unwrap();
        std::fs::write(project.join("elsewhere"), "e").unwrap();
        let owner = owner(dir.path());
        let ok = |patterns: &[&str], untracked| {
            readable(
                dir.path(),
                &reads(&project, patterns, untracked, &owner),
                None,
            )
        };
        // What the patterns cannot match does not count
        chmod(&project.join("elsewhere"), 0o000);
        assert!(ok(&["dist"], false));
        assert!(!ok(&["dist"], true), "untracked reads the whole project");

        chmod(&project.join("dist/sub/secret"), 0o200);
        assert!(!ok(&["dist"], false));
        assert!(!ok(&["dist/sub/secret"], false));
        assert!(!ok(&["dist/sub/*"], false));
        assert!(ok(&["dist/other", "dist/sub/other"], false));
        chmod(&project.join("dist/sub/secret"), 0o600);

        // A directory without x cannot be searched, without r not listed
        chmod(&project.join("dist/sub"), 0o600);
        assert!(!ok(&["dist/"], false));
        assert!(!ok(&["dist/sub/secret"], false), "searched on the way");
        chmod(&project.join("dist/sub"), 0o300);
        assert!(!ok(&["dist/*"], false));
        assert!(ok(&["dist/sub/secret"], false), "searching is enough");
        chmod(&project.join("dist/sub"), 0o700);

        // The job directory, where archives are staged, must be writable
        chmod(dir.path(), 0o500);
        assert!(!ok(&["dist"], false));
        chmod(dir.path(), 0o700);
        assert!(ok(&["dist"], false));
        assert!(ok(&["../elsewhere", "/etc/shadow"], false), "never read");
    }

    #[test]
    fn symlinks_are_not_followed_unless_matching_may() {
        let (dir, project) = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("unreadable"), "u").unwrap();
        chmod(&outside.path().join("unreadable"), 0o000);
        std::fs::create_dir_all(project.join("dist")).unwrap();
        std::fs::create_dir_all(project.join("real")).unwrap();
        std::fs::write(project.join("real/file"), "f").unwrap();
        symlink(outside.path(), project.join("dist/out")).unwrap();
        symlink(outside.path().join("unreadable"), project.join("dist/file")).unwrap();
        let owner = owner(dir.path());
        let ok = |patterns: &[&str]| {
            readable(dir.path(), &reads(&project, patterns, false, &owner), None)
        };
        // Archived as links
        assert!(ok(&[
            "dist",
            "dist/out",
            "dist/out/",
            "dist/*",
            "dist/file"
        ]));
        // `dist/out/x`, `dist/*/x` and `**` look through the link into a
        // directory outside the project
        assert!(!ok(&["dist/out/unreadable"]));
        assert!(!ok(&["dist/*/x"]));
        assert!(!ok(&["**/x"]));

        // A link to a directory in the project is fine: that directory is
        // checked with the rest of the project
        std::fs::remove_file(project.join("dist/out")).unwrap();
        symlink("../real", project.join("dist/out")).unwrap();
        assert!(ok(&["dist/out/file"]));
        assert!(ok(&["dist/*/file", "**/file"]));
        chmod(&project.join("real/file"), 0o000);
        assert!(!ok(&["dist/*/file"]));
    }

    #[test]
    fn a_symlinked_project_or_parent_needs_a_hand_back() {
        let (dir, project) = workspace();
        std::fs::create_dir_all(dir.path().join("real/project")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("group")).unwrap();
        let owner = owner(dir.path());
        let nested = dir.path().join("group/project");
        let ok = |project: &Path, patterns: &[&str]| {
            readable(dir.path(), &reads(project, patterns, false, &owner), None)
        };
        assert!(!ok(&nested, &["x"]));
        // Nothing to read: only staging counts
        assert!(ok(&nested, &[]));
        assert!(ok(&project, &["x"]));
    }

    #[test]
    fn untracked_files_are_listed_by_git_s_user_too() {
        let (dir, project) = workspace();
        std::fs::write(project.join("file"), "f").unwrap();
        chmod(&project.join("file"), 0o600);
        chmod(dir.path(), 0o755);
        let owner = owner(dir.path());
        let git = stranger(&owner);
        let ok = |untracked, git: &Reader| {
            readable(
                dir.path(),
                &reads(&project, &["file"], untracked, &owner),
                Some(git),
            )
        };
        assert!(ok(false, &git), "git lists nothing");
        assert!(!ok(true, &git));
        assert!(ok(true, &owner));
        chmod(&project.join("file"), 0o644);
        assert!(ok(true, &git));
    }

    #[test]
    fn file_variables_must_be_removable_before_the_report() {
        let (dir, project) = workspace();
        let variables = dir.path().join("project.tmp");
        std::fs::create_dir_all(variables.join("tool")).unwrap();
        std::fs::write(variables.join("SECRET"), "s").unwrap();
        std::fs::write(variables.join("tool/cache"), "c").unwrap();
        let owner = owner(dir.path());
        let ok = || readable(dir.path(), &reads(&project, &[], false, &owner), None);
        // Files of others in the owner's directories go with them
        chmod(&variables.join("SECRET"), 0o000);
        assert!(ok());
        // A directory of someone else the owner cannot empty
        chmod(&variables.join("tool"), 0o555);
        assert!(!ok());
        chmod(&variables.join("tool"), 0o755);
        assert!(ok());
        std::fs::remove_dir_all(&variables).unwrap();
        assert!(ok(), "no file variables");
    }
}
