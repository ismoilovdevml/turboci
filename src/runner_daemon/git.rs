//! Source checkout for a job: builds the git commands as argv (never through a shell)
//! so the job is built from exactly `git_info.sha`, following gitlab-runner's
//! init + fetch <refspecs> + checkout <sha> flow.

use anyhow::{bail, Result};

use crate::gitlab::{GitInfo, Variable};

/// How sources are prepared, from `GIT_STRATEGY`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitStrategy {
    /// Fresh workspace: init + fetch + checkout (`clone` and `fetch` behave the same here)
    Fetch,
    /// Do not touch sources at all
    None,
    /// Leave an empty project directory
    Empty,
}

/// Submodule handling, from `GIT_SUBMODULE_STRATEGY`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleStrategy {
    None,
    Normal,
    Recursive,
}

fn variable<'a>(variables: &'a [Variable], key: &str) -> Option<&'a str> {
    variables
        .iter()
        .rev()
        .find(|v| v.key == key)
        .and_then(|v| v.value.as_deref())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

pub fn strategy(variables: &[Variable]) -> GitStrategy {
    match variable(variables, "GIT_STRATEGY") {
        Some("none") => GitStrategy::None,
        Some("empty") => GitStrategy::Empty,
        _ => GitStrategy::Fetch,
    }
}

fn submodule_strategy(variables: &[Variable]) -> SubmoduleStrategy {
    match variable(variables, "GIT_SUBMODULE_STRATEGY") {
        Some("normal") => SubmoduleStrategy::Normal,
        Some("recursive") => SubmoduleStrategy::Recursive,
        _ => SubmoduleStrategy::None,
    }
}

/// `GIT_DEPTH` overrides the project setting sent in `git_info.depth`; 0 means full history
fn depth(git_info: &GitInfo, variables: &[Variable]) -> u32 {
    variable(variables, "GIT_DEPTH")
        .and_then(|v| v.parse().ok())
        .or(git_info.depth)
        .unwrap_or(0)
}

fn is_commit_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Argv lists that check out `git_info.sha` into `dest`, run in order.
///
/// Values coming from the job payload are validated so none of them can be parsed
/// as a git option (e.g. a refspec of `--upload-pack=...`).
pub fn checkout_commands(
    git_info: &GitInfo,
    variables: &[Variable],
    dest: &str,
) -> Result<Vec<Vec<String>>> {
    if !is_commit_sha(&git_info.sha) {
        bail!("Invalid commit SHA from GitLab: {:?}", git_info.sha);
    }
    if git_info.repo_url.starts_with('-') {
        bail!("Invalid repository URL from GitLab");
    }
    if let Some(bad) = git_info.refspecs.iter().find(|r| r.starts_with('-')) {
        bail!("Invalid refspec from GitLab: {:?}", bad);
    }

    // The workspace may be owned by another uid than the one running git (host vs container)
    let safe_directory = format!("safe.directory={}", dest);
    let git = |args: &[&str]| -> Vec<String> {
        ["git", "-c", &safe_directory, "-C", dest]
            .iter()
            .chain(args)
            .map(|s| s.to_string())
            .collect()
    };

    let mut init = vec!["git".to_string(), "init".to_string(), "-q".to_string()];
    if let Some(format) = git_info.repo_object_format.as_deref() {
        if format == "sha1" || format == "sha256" {
            init.push(format!("--object-format={}", format));
        }
    }
    init.push(dest.to_string());

    let mut fetch = git(&["fetch", "-q", "--prune", "--no-tags"]);
    let depth = depth(git_info, variables);
    if depth > 0 {
        fetch.push(format!("--depth={}", depth));
    }
    fetch.push("origin".to_string());
    if git_info.refspecs.is_empty() {
        // Older GitLab: fetch the commit itself
        fetch.push(git_info.sha.clone());
    } else {
        fetch.extend(git_info.refspecs.iter().cloned());
    }

    let mut commands = vec![
        init,
        git(&["remote", "add", "origin", &git_info.repo_url]),
        fetch,
        git(&["checkout", "-q", "-f", &git_info.sha]),
    ];

    match submodule_strategy(variables) {
        SubmoduleStrategy::None => {}
        SubmoduleStrategy::Normal => {
            commands.push(git(&["submodule", "sync"]));
            commands.push(git(&["submodule", "update", "--init"]));
        }
        SubmoduleStrategy::Recursive => {
            commands.push(git(&["submodule", "sync", "--recursive"]));
            commands.push(git(&["submodule", "update", "--init", "--recursive"]));
        }
    }

    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn var(key: &str, value: &str) -> Variable {
        serde_json::from_value(serde_json::json!({"key": key, "value": value})).unwrap()
    }

    fn git_info(url: &str, sha: &str, refspecs: &[&str], depth: Option<u32>) -> GitInfo {
        serde_json::from_value(serde_json::json!({
            "repo_url": url,
            "ref": "main",
            "ref_type": "branch",
            "sha": sha,
            "before_sha": "0000000000000000000000000000000000000000",
            "depth": depth,
            "refspecs": refspecs,
        }))
        .unwrap()
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Origin repo with commit A (the pipeline's commit) followed by B (current branch head)
    fn origin_with_two_commits(root: &Path) -> (String, String, String) {
        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        run(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("f"), "A").unwrap();
        run(&origin, &["add", "f"]);
        run(&origin, &["commit", "-q", "-m", "A"]);
        let a = run(&origin, &["rev-parse", "HEAD"]);
        run(&origin, &["update-ref", "refs/pipelines/1", &a]);
        std::fs::write(origin.join("f"), "B").unwrap();
        run(&origin, &["commit", "-q", "-am", "B"]);
        let b = run(&origin, &["rev-parse", "HEAD"]);
        (format!("file://{}", origin.display()), a, b)
    }

    fn checkout(info: &GitInfo, variables: &[Variable], dest: &Path) {
        for argv in checkout_commands(info, variables, dest.to_str().unwrap()).unwrap() {
            let out = Command::new(&argv[0])
                .args(&argv[1..])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{:?}: {}",
                argv,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn checks_out_job_sha_not_branch_head() {
        let root = tempdir().unwrap();
        let (url, a, b) = origin_with_two_commits(root.path());
        assert_ne!(a, b);
        let dest = root.path().join("project");
        let info = git_info(
            &url,
            &a,
            &["+refs/heads/main:refs/remotes/origin/main"],
            None,
        );

        checkout(&info, &[], &dest);

        assert_eq!(run(&dest, &["rev-parse", "HEAD"]), a);
        assert_eq!(std::fs::read_to_string(dest.join("f")).unwrap(), "A");
    }

    #[test]
    fn shallow_fetch_of_pipeline_ref_gets_exact_commit() {
        let root = tempdir().unwrap();
        let (url, a, _) = origin_with_two_commits(root.path());
        let dest = root.path().join("project");
        let info = git_info(
            &url,
            &a,
            &[
                "+refs/pipelines/1:refs/pipelines/1",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
            Some(1),
        );

        checkout(&info, &[], &dest);

        assert_eq!(run(&dest, &["rev-parse", "HEAD"]), a);
        assert_eq!(run(&dest, &["rev-list", "--count", "HEAD"]), "1");
    }

    #[test]
    fn fetches_sha_directly_without_refspecs() {
        let root = tempdir().unwrap();
        let (url, a, _) = origin_with_two_commits(root.path());
        let dest = root.path().join("project");
        let info = git_info(&url, &a, &[], Some(1));

        checkout(&info, &[], &dest);

        assert_eq!(run(&dest, &["rev-parse", "HEAD"]), a);
    }

    #[test]
    fn ref_with_shell_metacharacters_is_never_executed() {
        let root = tempdir().unwrap();
        let (url, a, _) = origin_with_two_commits(root.path());
        let marker = root.path().join("pwned");
        let mut info = git_info(&url, &a, &[], None);
        info.ref_name = format!("x;touch {}", marker.display());
        let dest = root.path().join("project");

        checkout(&info, &[], &dest);

        assert!(!marker.exists());
    }

    #[test]
    fn rejects_option_like_refspec_and_bad_sha() {
        let sha = "a".repeat(40);
        let info = git_info(
            "https://x/r.git",
            &sha,
            &["--upload-pack=touch /tmp/p"],
            None,
        );
        assert!(checkout_commands(&info, &[], "/w").is_err());

        let info = git_info("--upload-pack=x", &sha, &[], None);
        assert!(checkout_commands(&info, &[], "/w").is_err());

        let info = git_info("https://x/r.git", "main; rm -rf /", &[], None);
        assert!(checkout_commands(&info, &[], "/w").is_err());
    }

    #[test]
    fn depth_and_submodules_follow_variables() {
        let sha = "b".repeat(40);
        let info = git_info(
            "https://x/r.git",
            &sha,
            &["+refs/heads/main:refs/remotes/origin/main"],
            Some(20),
        );

        let cmds = checkout_commands(&info, &[], "/w").unwrap();
        assert!(cmds[2].contains(&"--depth=20".to_string()));
        assert_eq!(cmds.len(), 4);

        let vars = [
            var("GIT_DEPTH", "0"),
            var("GIT_SUBMODULE_STRATEGY", "recursive"),
        ];
        let cmds = checkout_commands(&info, &vars, "/w").unwrap();
        assert!(!cmds[2].iter().any(|a| a.starts_with("--depth")));
        assert_eq!(
            cmds.last().unwrap(),
            &[
                "git",
                "-c",
                "safe.directory=/w",
                "-C",
                "/w",
                "submodule",
                "update",
                "--init",
                "--recursive"
            ]
        );
    }

    #[test]
    fn strategy_from_variables() {
        assert_eq!(strategy(&[]), GitStrategy::Fetch);
        assert_eq!(
            strategy(&[var("GIT_STRATEGY", "clone")]),
            GitStrategy::Fetch
        );
        assert_eq!(strategy(&[var("GIT_STRATEGY", "none")]), GitStrategy::None);
        assert_eq!(
            strategy(&[var("GIT_STRATEGY", "empty")]),
            GitStrategy::Empty
        );
    }
}
