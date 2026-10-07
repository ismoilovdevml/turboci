//! Source checkout for a job: builds the git commands as argv (never through a shell)
//! so the job is built from exactly `git_info.sha`, following gitlab-runner's
//! init + fetch <refspecs> + checkout <sha> flow.

use anyhow::{bail, Result};

use super::script;
use crate::gitlab::{GitInfo, Variable};

/// How sources are prepared, from `GIT_STRATEGY`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitStrategy {
    /// Update the checkout an earlier job left in the workspace, or clone when
    /// there is none (the default)
    Fetch,
    /// Always clone into an emptied project directory
    Clone,
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

/// `GIT_STRATEGY`, else the project's setting (`allow_git_fetch`), as in
/// gitlab-runner
pub fn strategy(variables: &[Variable], allow_git_fetch: bool) -> GitStrategy {
    match variable(variables, "GIT_STRATEGY") {
        Some("none") => GitStrategy::None,
        Some("empty") => GitStrategy::Empty,
        Some("clone") => GitStrategy::Clone,
        Some("fetch") => GitStrategy::Fetch,
        _ if allow_git_fetch => GitStrategy::Fetch,
        _ => GitStrategy::Clone,
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

/// Argv lists that check out `git_info.sha` into a new repository at `dest`,
/// run in order.
///
/// Values coming from the job payload are validated so none of them can be parsed
/// as a git option (e.g. a refspec of `--upload-pack=...`).
pub fn checkout_commands(
    git_info: &GitInfo,
    variables: &[Variable],
    dest: &str,
) -> Result<Vec<Vec<String>>> {
    sync_commands(git_info, variables, dest, false)
}

/// Like `checkout_commands`, for `dest` holding the checkout of an earlier job
/// of the same project. That job may have changed `.git`, so its config and
/// hooks are replaced (a `core.fsmonitor` or hook would run commands during
/// the checkout and the job's own git calls); untracked and ignored files are
/// removed like gitlab-runner does (`GIT_CLEAN_FLAGS`, default `-ffdx`).
pub fn update_commands(
    git_info: &GitInfo,
    variables: &[Variable],
    dest: &str,
) -> Result<Vec<Vec<String>>> {
    sync_commands(git_info, variables, dest, true)
}

fn sync_commands(
    git_info: &GitInfo,
    variables: &[Variable],
    dest: &str,
    existing: bool,
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
    // Extra flags chosen by the pipeline (e.g. --filter=blob:none)
    if let Some(extra) = variable(variables, "GIT_FETCH_EXTRA_FLAGS") {
        fetch.extend(extra.split_whitespace().map(str::to_string));
    }
    fetch.push("origin".to_string());
    if git_info.refspecs.is_empty() {
        // Older GitLab: fetch the commit itself
        fetch.push(git_info.sha.clone());
    } else {
        fetch.extend(git_info.refspecs.iter().cloned());
    }

    let mut commands = Vec::new();
    if existing {
        let git_dir = format!("{}/.git", dest);
        // Locks left by a job that was killed, and everything of the earlier
        // job's repository that can run commands; `git init` writes a new config
        let mut remove = vec!["rm".to_string(), "-f".to_string()];
        for name in [
            "index.lock",
            "shallow.lock",
            "HEAD.lock",
            "config.lock",
            "config",
        ] {
            remove.push(format!("{}/{}", git_dir, name));
        }
        commands.push(remove);
        commands.push(vec![
            "rm".to_string(),
            "-rf".to_string(),
            format!("{}/hooks", git_dir),
        ]);
    }
    commands.extend([
        init,
        git(&["remote", "add", "origin", &git_info.repo_url]),
        fetch,
    ]);
    // GIT_CHECKOUT=false fetches without checking out, as in gitlab-runner
    if variable(variables, "GIT_CHECKOUT") != Some("false") {
        commands.push(git(&["checkout", "-q", "-f", &git_info.sha]));
        let flags = variable(variables, "GIT_CLEAN_FLAGS").unwrap_or("-ffdx");
        if existing && flags != "none" {
            let mut clean = git(&["clean", "-q"]);
            clean.extend(flags.split_whitespace().map(str::to_string));
            commands.push(clean);
        }
    }

    let recursive = match submodule_strategy(variables) {
        SubmoduleStrategy::None => None,
        SubmoduleStrategy::Normal => Some(false),
        SubmoduleStrategy::Recursive => Some(true),
    };
    if let Some(recursive) = recursive {
        // GIT_SUBMODULE_PATHS limits which submodules are updated; being
        // pathspecs after "--" they can never be read as options
        let paths: Vec<String> = variable(variables, "GIT_SUBMODULE_PATHS")
            .map(|paths| paths.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        if paths.iter().any(|path| path == ":(exclude)") {
            bail!(
                "GIT_SUBMODULE_PATHS: invalid submodule pathspec {:?}",
                paths
            );
        }
        let with_paths = |mut args: Vec<String>| {
            if !paths.is_empty() {
                args.push("--".to_string());
                args.extend(paths.iter().cloned());
            }
            args
        };

        commands.push(git(&["submodule", "init"]));
        let mut sync = git(&["submodule", "sync"]);
        let mut update = git(&["submodule", "update", "--init"]);
        if recursive {
            sync.push("--recursive".to_string());
            update.push("--recursive".to_string());
        }
        // GIT_SUBMODULE_DEPTH defaults to the depth of the main repository
        let submodule_depth = variable(variables, "GIT_SUBMODULE_DEPTH")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(depth);
        if submodule_depth > 0 {
            update.push(format!("--depth={}", submodule_depth));
        }
        if let Some(flags) = variable(variables, "GIT_SUBMODULE_UPDATE_FLAGS") {
            update.extend(flags.split_whitespace().map(str::to_string));
        }
        commands.push(with_paths(sync));
        commands.push(with_paths(update));
    }

    Ok(commands)
}

/// The script that prepares the sources in `dest` for `GitStrategy::Fetch` or
/// `Clone`. Fetch updates a checkout an earlier job left there and clones
/// again when there is none or updating it fails (a killed job may leave a
/// broken repository). Repositories with submodules are always cloned: their
/// `.git/modules` hold repositories with configs and hooks of their own.
pub fn checkout_script(
    git_info: &GitInfo,
    variables: &[Variable],
    dest: &str,
    strategy: GitStrategy,
) -> Result<String> {
    let fresh = checkout_commands(git_info, variables, dest)?;
    let wipe = vec!["rm".to_string(), "-rf".to_string(), dest.to_string()];
    let mut out = String::new();
    let reuse =
        strategy == GitStrategy::Fetch && submodule_strategy(variables) == SubmoduleStrategy::None;
    if reuse {
        let update: Vec<String> = update_commands(git_info, variables, dest)?
            .iter()
            .map(|argv| script::argv_line(argv))
            .collect();
        let git_dir = script::quote(&format!("{}/.git", dest));
        out.push_str(&format!(
            "updated=0\n\
             if [ -d {git_dir} ] && [ ! -e {git_dir}/modules ]; then\n\
             echo 'Reusing the checkout of an earlier job'\n\
             if {}; then updated=1; else echo 'WARNING: Updating the earlier checkout failed, cloning again'; fi\n\
             fi\n\
             if [ \"$updated\" = 0 ]; then\n",
            update.join(" && ")
        ));
    }
    out.push_str(&script::argv_line(&wipe));
    out.push('\n');
    for argv in &fresh {
        out.push_str(&script::argv_line(argv));
        out.push('\n');
    }
    if reuse {
        out.push_str("fi\n");
    }
    Ok(out)
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
    fn submodule_depth_flags_and_paths_follow_variables() {
        let sha = "d".repeat(40);
        let info = git_info("https://x/r.git", &sha, &[], Some(10));
        let vars = [
            var("GIT_SUBMODULE_STRATEGY", "normal"),
            var("GIT_SUBMODULE_DEPTH", "1"),
            var("GIT_SUBMODULE_UPDATE_FLAGS", "--remote --jobs 4"),
            var("GIT_SUBMODULE_PATHS", "libs/a :(exclude)libs/b"),
        ];

        let cmds = checkout_commands(&info, &vars, "/w").unwrap();
        let tail: Vec<String> = cmds.last().unwrap()[5..].to_vec();
        assert_eq!(
            tail,
            [
                "submodule",
                "update",
                "--init",
                "--depth=1",
                "--remote",
                "--jobs",
                "4",
                "--",
                "libs/a",
                ":(exclude)libs/b"
            ]
        );
        assert_eq!(cmds[cmds.len() - 3][5..], ["submodule", "init"]);

        // Without GIT_SUBMODULE_DEPTH submodules use the repository depth
        let cmds = checkout_commands(&info, &vars[..1], "/w").unwrap();
        assert!(cmds.last().unwrap().contains(&"--depth=10".to_string()));

        let bad = [
            var("GIT_SUBMODULE_STRATEGY", "normal"),
            var("GIT_SUBMODULE_PATHS", ":(exclude) libs/b"),
        ];
        assert!(checkout_commands(&info, &bad, "/w").is_err());
    }

    #[test]
    fn checkout_and_fetch_flags_follow_variables() {
        let sha = "c".repeat(40);
        let info = git_info("https://x/r.git", &sha, &[], None);
        let vars = [
            var("GIT_CHECKOUT", "false"),
            var("GIT_FETCH_EXTRA_FLAGS", "--filter=blob:none --prune-tags"),
        ];

        let cmds = checkout_commands(&info, &vars, "/w").unwrap();

        assert!(!cmds
            .iter()
            .any(|argv| argv.contains(&"checkout".to_string())));
        assert!(cmds[2].contains(&"--filter=blob:none".to_string()));
        assert!(cmds[2].contains(&"--prune-tags".to_string()));
    }

    /// Run the checkout script like the executor does; its output
    fn run_script(
        info: &GitInfo,
        variables: &[Variable],
        dest: &Path,
        strategy: GitStrategy,
    ) -> String {
        let body = checkout_script(info, variables, dest.to_str().unwrap(), strategy).unwrap();
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("set -e\n{}", body))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{}", log);
        log
    }

    /// Origin with commit A at refs/pipelines/1; then commit B at refs/pipelines/2
    fn pipeline_info(url: &str, sha: &str, pipeline: u32) -> GitInfo {
        let refspec = format!("+refs/pipelines/{0}:refs/pipelines/{0}", pipeline);
        git_info(url, sha, &[refspec.as_str()], Some(20))
    }

    #[test]
    fn fetch_updates_the_checkout_of_an_earlier_job_without_running_its_config() {
        let root = tempdir().unwrap();
        let (url, a, b) = origin_with_two_commits(root.path());
        let origin = root.path().join("origin");
        run(&origin, &["update-ref", "refs/pipelines/2", &b]);
        let dest = root.path().join("project");

        let first = run_script(&pipeline_info(&url, &a, 1), &[], &dest, GitStrategy::Fetch);
        assert!(!first.contains("Reusing"), "{}", first);
        assert_eq!(run(&dest, &["rev-parse", "HEAD"]), a);

        // What the earlier job leaves behind: files, a config that runs
        // commands and a hook
        let marker = root.path().join("ran");
        std::fs::write(dest.join("leftover"), "x").unwrap();
        run(
            &dest,
            &[
                "config",
                "core.fsmonitor",
                &format!("touch {}", marker.display()),
            ],
        );
        let hook = dest.join(".git/hooks/post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let second = run_script(&pipeline_info(&url, &b, 2), &[], &dest, GitStrategy::Fetch);
        assert!(second.contains("Reusing the checkout"), "{}", second);
        assert!(!second.contains("WARNING"), "{}", second);
        assert_eq!(run(&dest, &["rev-parse", "HEAD"]), b);
        assert_eq!(std::fs::read_to_string(dest.join("f")).unwrap(), "B");
        assert!(!dest.join("leftover").exists());
        assert!(!marker.exists(), "the earlier job's config or hook ran");
        // The job's own git calls run with a config of the runner's
        run(&dest, &["status"]);
        assert!(!marker.exists(), "the earlier job's config survived");
    }

    #[test]
    fn a_broken_checkout_is_cloned_again() {
        let root = tempdir().unwrap();
        let (url, a, b) = origin_with_two_commits(root.path());
        run(
            &root.path().join("origin"),
            &["update-ref", "refs/pipelines/2", &b],
        );
        let dest = root.path().join("project");
        run_script(&pipeline_info(&url, &a, 1), &[], &dest, GitStrategy::Fetch);
        // A job killed in the middle of a git command
        std::fs::write(dest.join(".git/HEAD"), "garbage").unwrap();

        let log = run_script(&pipeline_info(&url, &b, 2), &[], &dest, GitStrategy::Fetch);
        assert!(log.contains("cloning again"), "{}", log);
        assert_eq!(run(&dest, &["rev-parse", "HEAD"]), b);
    }

    #[test]
    fn clone_and_submodules_never_reuse_the_checkout() {
        let root = tempdir().unwrap();
        let (url, a, _) = origin_with_two_commits(root.path());
        let dest = root.path().join("project");
        let info = pipeline_info(&url, &a, 1);
        run_script(&info, &[], &dest, GitStrategy::Fetch);
        std::fs::write(dest.join("leftover"), "x").unwrap();

        let log = run_script(&info, &[], &dest, GitStrategy::Clone);
        assert!(!log.contains("Reusing"), "{}", log);
        assert!(!dest.join("leftover").exists());

        let submodules = [var("GIT_SUBMODULE_STRATEGY", "normal")];
        let body = checkout_script(&info, &submodules, "/w", GitStrategy::Fetch).unwrap();
        assert!(!body.contains("Reusing"), "{}", body);
    }

    #[test]
    fn clean_flags_follow_the_variable() {
        let info = git_info("https://g/r.git", &"a".repeat(40), &[], None);
        let cmds = update_commands(&info, &[], "/w").unwrap();
        assert!(cmds
            .iter()
            .any(|c| c.ends_with(&["clean".into(), "-q".into(), "-ffdx".into()])));
        let custom = [var("GIT_CLEAN_FLAGS", "-ffd -e node_modules/")];
        let cmds = update_commands(&info, &custom, "/w").unwrap();
        assert!(cmds.iter().any(|c| c.ends_with(&[
            "-ffd".into(),
            "-e".into(),
            "node_modules/".into()
        ])));
        let none = [var("GIT_CLEAN_FLAGS", "none")];
        let cmds = update_commands(&info, &none, "/w").unwrap();
        assert!(!cmds.iter().any(|c| c.contains(&"clean".to_string())));
    }

    #[test]
    fn strategy_from_variables() {
        assert_eq!(strategy(&[], true), GitStrategy::Fetch);
        assert_eq!(strategy(&[], false), GitStrategy::Clone);
        assert_eq!(
            strategy(&[var("GIT_STRATEGY", "clone")], true),
            GitStrategy::Clone
        );
        assert_eq!(
            strategy(&[var("GIT_STRATEGY", "fetch")], false),
            GitStrategy::Fetch
        );
        assert_eq!(
            strategy(&[var("GIT_STRATEGY", "none")], true),
            GitStrategy::None
        );
        assert_eq!(
            strategy(&[var("GIT_STRATEGY", "empty")], true),
            GitStrategy::Empty
        );
    }
}
