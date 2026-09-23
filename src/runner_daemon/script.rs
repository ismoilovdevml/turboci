//! Building what a job step runs: the shell script for a step, and the job's
//! environment from its CI/CD variables. Pure functions, shared by all executors.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::gitlab::Job;

/// Quote a string for POSIX sh (single quotes; `'` becomes `'\''`)
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn shell_prelude() -> &'static str {
    // pipefail is not POSIX; enable it only where the shell supports it
    "set -e\n(set -o pipefail) 2>/dev/null && set -o pipefail\n"
}

/// One script for a whole step: every line runs in the same shell (so `cd`,
/// `export` and functions carry over) and stops at the first failing command.
/// Each command is echoed like GitLab does, as a green `$ command` line.
pub fn step_script(lines: &[String]) -> String {
    let mut script = String::from(shell_prelude());
    for line in lines {
        script.push_str(&format!(
            "printf '\\033[32;1m%s\\033[0;m\\n' {}\n",
            quote(&format!("$ {}", line))
        ));
        script.push_str(line);
        script.push('\n');
    }
    script
}

/// A script running argv commands in order, without echoing them (they may hold
/// credentials, e.g. the repository URL). Arguments are quoted, never interpreted.
pub fn argv_script(commands: &[Vec<String>]) -> String {
    let mut script = String::from(shell_prelude());
    for argv in commands {
        let line: Vec<String> = argv.iter().map(|arg| quote(arg)).collect();
        script.push_str(&line.join(" "));
        script.push('\n');
    }
    script
}

/// Environment of a job, plus the files backing `file`-type variables
#[derive(Debug, Default)]
pub struct JobEnv {
    pub vars: Vec<(String, String)>,
    /// (host path, content) to write before the job runs
    pub files: Vec<(PathBuf, String)>,
}

impl JobEnv {
    /// Set a variable, replacing an earlier value of the same name
    fn set(&mut self, key: &str, value: String) {
        self.vars.retain(|(k, _)| k != key);
        self.vars.push((key.to_string(), value));
    }

    /// `KEY=value` list, as Docker expects it
    pub fn to_docker(&self) -> Vec<String> {
        self.vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect()
    }
}

fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Build the job environment.
///
/// `builds_dir` is where the job sees its builds directory (e.g. `/builds` in a
/// container); `host_job_dir` is the same directory on the runner host. File
/// variables are written to `<project>.tmp/<KEY>` like gitlab-runner does.
pub fn job_env(job: &Job, builds_dir: &str, host_job_dir: &Path) -> JobEnv {
    let project_dir = format!("{}/project", builds_dir);
    let tmp_dir = format!("{}/project.tmp", builds_dir);

    let mut env = JobEnv::default();
    env.set("CI_BUILDS_DIR", builds_dir.to_string());
    env.set("CI_PROJECT_DIR", project_dir);

    // Every variable is visible to expansion, whatever its position
    let valid: Vec<_> = job
        .variables
        .iter()
        .filter(|var| {
            let ok = is_valid_key(&var.key);
            if !ok {
                tracing::warn!("Skipping variable with invalid name {:?}", var.key);
            }
            ok
        })
        .collect();
    let mut values: HashMap<String, String> = env.vars.iter().cloned().collect();
    for var in &valid {
        values.insert(var.key.clone(), var.value.clone().unwrap_or_default());
    }

    for var in valid {
        let value = var.value.clone().unwrap_or_default();
        let value = if var.raw {
            value
        } else {
            expand(&value, &values)
        };
        if var.file {
            env.files.retain(|(path, _)| !path.ends_with(&var.key));
            env.files
                .push((host_job_dir.join("project.tmp").join(&var.key), value));
            env.set(&var.key, format!("{}/{}", tmp_dir, var.key));
        } else {
            env.set(&var.key, value);
        }
    }
    env
}

/// Environment of a service container: like gitlab-runner, only public and
/// internal job variables (never masked or protected secrets), plus the
/// service's own `variables:`, which take precedence
pub fn service_env(
    job: &Job,
    service_variables: &[crate::gitlab::Variable],
    values: &HashMap<String, String>,
) -> Vec<String> {
    let mut env = JobEnv::default();
    let visible = job
        .variables
        .iter()
        .filter(|v| (v.public || v.internal) && !v.masked && !v.file);
    for var in visible.chain(service_variables) {
        if !is_valid_key(&var.key) {
            continue;
        }
        let value = var.value.clone().unwrap_or_default();
        let value = if var.raw {
            value
        } else {
            expand(&value, values)
        };
        env.set(&var.key, value);
    }
    env.to_docker()
}

/// Expand `$VAR` and `${VAR}` from `values` (unknown names expand to empty);
/// `$$` is a literal `$`, as in GitLab.
pub fn expand(value: &str, values: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
        } else if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(end) if is_valid_key(&braced[..end]) => {
                    out.push_str(values.get(&braced[..end]).map_or("", String::as_str));
                    rest = &braced[end + 1..];
                }
                _ => {
                    out.push('$');
                    rest = after;
                }
            }
        } else {
            let len = after
                .char_indices()
                .find(|&(i, c)| {
                    !(c.is_ascii_alphanumeric() || c == '_') || (i == 0 && c.is_ascii_digit())
                })
                .map_or(after.len(), |(i, _)| i);
            if len == 0 {
                out.push('$');
            } else {
                out.push_str(values.get(&after[..len]).map_or("", String::as_str));
            }
            rest = &after[len..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(script: &str, dir: &Path) -> (i32, String) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn step_script_keeps_shell_state_between_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        let script = step_script(&lines(&[
            "cd app",
            "export X=42",
            "echo \"$X in $(basename $PWD)\"",
        ]));

        let (code, out) = sh(&script, dir.path());

        assert_eq!(code, 0);
        assert!(out.contains("42 in app"), "{}", out);
        assert!(out.contains("$ cd app"), "{}", out);
    }

    #[test]
    fn step_script_stops_at_first_failure_with_its_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let script = step_script(&lines(&["echo one", "sh -c 'exit 7'", "echo never"]));

        let (code, out) = sh(&script, dir.path());

        assert_eq!(code, 7);
        assert!(out.contains("one"));
        assert!(!out.contains("\nnever"), "{}", out);
    }

    #[test]
    fn argv_script_does_not_interpret_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        let evil = format!("x'; touch {} #", marker.display());
        let script = argv_script(&[vec!["echo".to_string(), evil.clone(), "$(id)".to_string()]]);

        let (code, out) = sh(&script, dir.path());

        assert_eq!(code, 0);
        assert!(!marker.exists());
        assert_eq!(out, format!("{} $(id)\n", evil));
    }

    fn job(vars: serde_json::Value) -> Job {
        serde_json::from_value(serde_json::json!({"id": 1, "token": "t", "variables": vars}))
            .unwrap()
    }

    fn get<'a>(env: &'a JobEnv, key: &str) -> Option<&'a str> {
        env.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn job_env_sets_dirs_expands_and_writes_file_variables() {
        let j = job(serde_json::json!([
            {"key": "HOST", "value": "db"},
            {"key": "URL", "value": "postgres://${HOST}:5432/$DB_NAME"},
            {"key": "DB_NAME", "value": "app"},
            {"key": "LITERAL", "value": "a$HOST", "raw": true},
            {"key": "PRICE", "value": "$$5"},
            {"key": "KUBECONFIG", "value": "apiVersion: v1", "file": true},
            {"key": "../../etc/evil", "value": "x", "file": true}
        ]));

        let env = job_env(&j, "/builds", Path::new("/host/job-1"));

        assert_eq!(get(&env, "CI_PROJECT_DIR"), Some("/builds/project"));
        assert_eq!(get(&env, "URL"), Some("postgres://db:5432/app"));
        assert_eq!(get(&env, "LITERAL"), Some("a$HOST"));
        assert_eq!(get(&env, "PRICE"), Some("$5"));
        assert_eq!(
            get(&env, "KUBECONFIG"),
            Some("/builds/project.tmp/KUBECONFIG")
        );
        assert_eq!(
            env.files,
            vec![(
                PathBuf::from("/host/job-1/project.tmp/KUBECONFIG"),
                "apiVersion: v1".to_string()
            )]
        );
        assert!(get(&env, "../../etc/evil").is_none());
    }

    #[test]
    fn later_variables_override_earlier_ones() {
        let j = job(serde_json::json!([
            {"key": "MODE", "value": "instance"},
            {"key": "MODE", "value": "job"}
        ]));

        let env = job_env(&j, "/builds", Path::new("/h"));

        assert_eq!(get(&env, "MODE"), Some("job"));
        assert_eq!(env.vars.iter().filter(|(k, _)| k == "MODE").count(), 1);
    }
}
