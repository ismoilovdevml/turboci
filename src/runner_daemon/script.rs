//! Building what a job step runs: the shell script for a step, and the job's
//! environment from its CI/CD variables. Pure functions, shared by all executors.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
        script.push_str(&argv_line(argv));
        script.push('\n');
    }
    script
}

/// One argv command as a shell line, every argument quoted
pub fn argv_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Environment of a job, plus the files backing `file`-type variables
#[derive(Debug, Default)]
pub struct JobEnv {
    pub vars: Vec<(String, String)>,
    /// (host path, content) to write before the job runs
    pub files: Vec<(PathBuf, String)>,
}

/// Variables in the order they were last set: setting a name again replaces
/// its value and moves it to the end. The replaced entry is left empty and
/// skipped, so a set costs O(1) however many variables a job has.
#[derive(Default)]
struct Vars {
    entries: Vec<Option<(String, String)>>,
    index: HashMap<String, usize>,
}

impl Vars {
    fn set(&mut self, key: &str, value: String) {
        if let Some(earlier) = self.index.insert(key.to_string(), self.entries.len()) {
            self.entries[earlier] = None;
        }
        self.entries.push(Some((key.to_string(), value)));
    }

    fn iter(&self) -> impl Iterator<Item = &(String, String)> {
        self.entries.iter().flatten()
    }

    fn get(&self, key: &str) -> Option<&str> {
        let at = *self.index.get(key)?;
        self.entries[at].as_ref().map(|(_, value)| value.as_str())
    }

    fn into_vec(self) -> Vec<(String, String)> {
        self.entries.into_iter().flatten().collect()
    }
}

impl JobEnv {
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

/// What the runner itself adds to a job's environment
#[derive(Debug, Default, Clone)]
pub struct RunnerVars {
    /// Docker (disposable) or shell (shared) environment
    pub disposable: bool,
    /// PEM of a custom CA for the GitLab server, exposed as CI_SERVER_TLS_CA_FILE
    pub ca_pem: Option<String>,
}

/// Parse a Go duration ("90s", "10m", "1h30m", "1.5h", "250ms")
pub fn parse_duration(text: &str) -> Option<Duration> {
    if text == "0" {
        return Some(Duration::ZERO);
    }
    if text.is_empty() {
        return None;
    }
    let mut total = 0f64;
    let mut rest = text;
    while !rest.is_empty() {
        let number_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..number_len].parse().ok()?;
        rest = &rest[number_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit_len] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            "us" | "µs" => 0.000_001,
            "ns" => 0.000_000_001,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += number * seconds;
    }
    Duration::try_from_secs_f64(total).ok()
}

/// Where the project is checked out, relative to the builds directory:
/// `project`, or the part of `GIT_CLONE_PATH` after `$CI_BUILDS_DIR/` (which
/// may use variables, e.g. `$CI_BUILDS_DIR/$CI_PROJECT_PATH`). A clone path
/// outside the builds directory is an error, as in gitlab-runner.
pub fn project_subdir(job: &Job) -> std::result::Result<String, String> {
    let Some(clone_path) = job
        .variables
        .iter()
        .rev()
        .find(|v| v.key == "GIT_CLONE_PATH")
        .and_then(|v| v.value.as_deref())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Ok("project".to_string());
    };
    let relative = clone_path
        .strip_prefix("$CI_BUILDS_DIR/")
        .or_else(|| clone_path.strip_prefix("${CI_BUILDS_DIR}/"))
        .ok_or_else(|| {
            format!(
                "GIT_CLONE_PATH {:?} must start with $CI_BUILDS_DIR/",
                clone_path
            )
        })?;
    let values: HashMap<String, String> = job
        .variables
        .iter()
        .filter_map(|v| Some((v.key.clone(), v.value.clone()?)))
        .collect();
    let relative = expand(relative, &values);
    let valid = !relative.is_empty()
        && relative.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        });
    if !valid {
        return Err(format!(
            "GIT_CLONE_PATH {:?} gives {:?}, not a directory inside the builds directory",
            clone_path, relative
        ));
    }
    Ok(relative)
}

/// Build the job environment.
///
/// `builds_dir` is where the job sees its builds directory (e.g. `/builds` in a
/// container); `host_job_dir` is the same directory on the runner host. File
/// variables are written to `<project>.tmp/<KEY>` like gitlab-runner does.
pub fn job_env(job: &Job, builds_dir: &str, host_job_dir: &Path, runner: &RunnerVars) -> JobEnv {
    let subdir = project_subdir(job).unwrap_or_else(|_| "project".to_string());
    let project_dir = format!("{}/{}", builds_dir, subdir);
    let tmp_dir = format!("{}/{}.tmp", builds_dir, subdir);

    let mut env = JobEnv::default();
    let mut vars = Vars::default();
    vars.set("CI_BUILDS_DIR", builds_dir.to_string());
    vars.set("CI_PROJECT_DIR", project_dir);
    // Variables gitlab-runner defines itself (common/build.go GetDefaultVariables)
    vars.set("CI_SERVER", "yes".to_string());
    vars.set("CI_JOB_STATUS", "running".to_string());
    vars.set(
        "CI_JOB_TIMEOUT",
        job.timeout_secs().unwrap_or(3600).to_string(),
    );
    if runner.disposable {
        vars.set("CI_DISPOSABLE_ENVIRONMENT", "true".to_string());
        if let Some(image) = &job.image {
            vars.set("CI_JOB_IMAGE", image.name.clone());
        }
    } else {
        vars.set("CI_SHARED_ENVIRONMENT", "true".to_string());
    }
    if let Some(pem) = &runner.ca_pem {
        let key = "CI_SERVER_TLS_CA_FILE";
        env.files.push((
            host_job_dir.join(format!("{}.tmp", subdir)).join(key),
            pem.clone(),
        ));
        let path = format!("{}/{}", tmp_dir, key);
        // git (sources, submodules, LFS) trusts the same CA
        vars.set("GIT_SSL_CAINFO", path.clone());
        vars.set(key, path);
    }

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
    // What a reference to each variable expands to, as gitlab-runner's
    // `Variables.Get`: the value as given, not itself expanded (one pass; GitLab
    // already expanded nested references before sending the job), and for a
    // file variable the path of its file, never its content
    let mut values: HashMap<String, String> = vars.iter().cloned().collect();
    for var in &valid {
        let value = if var.file {
            format!("{}/{}", tmp_dir, var.key)
        } else {
            var.value.clone().unwrap_or_default()
        };
        values.insert(var.key.clone(), value);
    }

    for var in valid {
        let value = var.value.clone().unwrap_or_default();
        let value = if var.raw {
            value
        } else {
            // A variable naming itself (`PATH=/opt/tool/bin:$PATH`) means the
            // value it replaces: an earlier definition (the runner's
            // `environment` under a job's PATH), or on the shell executor the
            // runner process's own environment. Its value as given would
            // name itself again and leave a literal `$PATH` in the job's PATH.
            let previous = vars.get(&var.key).map(str::to_string).or_else(|| {
                if runner.disposable {
                    None
                } else {
                    std::env::var(&var.key).ok()
                }
            });
            match previous {
                Some(previous) if names(&value, &var.key) => {
                    let mut scoped = values.clone();
                    scoped.insert(var.key.clone(), previous);
                    expand(&value, &scoped)
                }
                _ => expand(&value, &values),
            }
        };
        if var.file {
            env.files.retain(|(path, _)| !path.ends_with(&var.key));
            env.files.push((
                host_job_dir.join(format!("{}.tmp", subdir)).join(&var.key),
                value,
            ));
            vars.set(&var.key, format!("{}/{}", tmp_dir, var.key));
        } else {
            vars.set(&var.key, value);
        }
    }
    env.vars = vars.into_vec();
    env
}

/// Environment of a service container: like gitlab-runner, only public and
/// internal job variables (never masked or protected secrets), plus the
/// service's own `variables:`, which take precedence.
///
/// `values` is the job's environment (see `job_env`). Job variables keep the
/// value the job container sees: they are not expanded a second time. Like
/// gitlab-runner's `GetServiceVariables`, only the service's own variables are
/// expanded, against the job's environment and each other. Visibility goes by
/// the last definition of a name, so an earlier public value overridden by a
/// hidden one never shows up.
pub fn service_env(
    job: &Job,
    service_variables: &[crate::gitlab::Variable],
    values: &HashMap<String, String>,
) -> Vec<String> {
    let mut hidden: HashMap<&str, bool> = HashMap::new();
    for var in &job.variables {
        hidden.insert(
            &var.key,
            !(var.public || var.internal) || var.masked || var.file,
        );
    }
    let mut vars = Vars::default();
    for var in &job.variables {
        if !is_valid_key(&var.key) || hidden.get(var.key.as_str()) != Some(&false) {
            continue;
        }
        if let Some(value) = values.get(&var.key) {
            vars.set(&var.key, value.clone());
        }
    }
    let mut source = values.clone();
    for var in service_variables.iter().filter(|v| is_valid_key(&v.key)) {
        source.insert(var.key.clone(), var.value.clone().unwrap_or_default());
    }
    for var in service_variables {
        if !is_valid_key(&var.key) {
            continue;
        }
        let value = var.value.clone().unwrap_or_default();
        let value = if var.raw {
            value
        } else {
            expand(&value, &source)
        };
        vars.set(&var.key, value);
    }
    JobEnv {
        vars: vars.into_vec(),
        files: Vec::new(),
    }
    .to_docker()
}

/// Whether `value` refers to the variable `key` (`$KEY` or `${KEY}`)
fn names(value: &str, key: &str) -> bool {
    value.contains(&format!("${{{}}}", key))
        || value
            .match_indices(&format!("${}", key))
            .any(|(at, found)| {
                !value[at + found.len()..]
                    .bytes()
                    .next()
                    .is_some_and(|c| c == b'_' || c.is_ascii_alphanumeric())
            })
}

/// Expand `$VAR` and `${VAR}` from `values` exactly as gitlab-runner does
/// (Go's `os.Expand` with `Variables.Get`). One pass: a value put in is not
/// expanded again. Unknown names, `$0`-`$9`, `$*`, `$#`, `$@`, `$!`, `$?`
/// and `$-` give nothing; `$$` gives `$`; a `$` followed by anything else
/// stays; a `${` without `}` and `${}` are dropped; `${NAME:-x}` looks up the
/// whole text between the braces (no shell defaults), so it gives nothing.
pub fn expand(value: &str, values: &HashMap<String, String>) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    // Start of the text not copied to `out` yet
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'$' || at + 1 == bytes.len() {
            at += 1;
            continue;
        }
        out.push_str(&value[copied..at]);
        let (name, width) = shell_name(&value[at + 1..]);
        match name {
            Some(name) => out.push_str(lookup(name, values)),
            // Not followed by a name: the `$` stays
            None if width == 0 => out.push('$'),
            // Malformed `${`: dropped
            None => {}
        }
        at += 1 + width;
        copied = at;
    }
    out.push_str(&value[copied..]);
    out
}

/// Characters that form a one-character shell parameter (Go's `isShellSpecialVar`)
fn is_special(c: u8) -> bool {
    matches!(
        c,
        b'*' | b'#' | b'$' | b'@' | b'!' | b'?' | b'-' | b'0'..=b'9'
    )
}

/// The name after a `$` at the start of `text` and how many bytes it takes
/// (Go's `getShellName`): `None` with a width means bad syntax to drop,
/// `None` with width 0 that no name follows
fn shell_name(text: &str) -> (Option<&str>, usize) {
    let bytes = text.as_bytes();
    if bytes[0] == b'{' {
        if bytes.len() > 2 && is_special(bytes[1]) && bytes[2] == b'}' {
            return (Some(&text[1..2]), 3);
        }
        return match bytes.iter().position(|&c| c == b'}') {
            Some(1) => (None, 2),
            Some(end) => (Some(&text[1..end]), end + 1),
            None => (None, 1),
        };
    }
    if is_special(bytes[0]) {
        return (Some(&text[..1]), 1);
    }
    let len = bytes
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
        .count();
    ((len > 0).then(|| &text[..len]), len)
}

/// What a name expands to (gitlab-runner's `Variables.value`)
fn lookup<'a>(name: &str, values: &'a HashMap<String, String>) -> &'a str {
    match name {
        "$" => "$",
        "*" | "#" | "@" | "!" | "?" | "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
            ""
        }
        _ => values.get(name).map_or("", String::as_str),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn clone_path_stays_inside_the_builds_dir() {
        let with = |value: &str| -> Job {
            serde_json::from_value(serde_json::json!({
                "id": 1, "token": "t",
                "variables": [
                    {"key": "CI_PROJECT_PATH", "value": "group/app"},
                    {"key": "GIT_CLONE_PATH", "value": value}
                ]
            }))
            .unwrap()
        };
        assert_eq!(project_subdir(&with("")), Ok("project".to_string()));
        assert_eq!(
            project_subdir(&with("$CI_BUILDS_DIR/$CI_PROJECT_PATH")),
            Ok("group/app".to_string())
        );
        assert_eq!(
            project_subdir(&with("${CI_BUILDS_DIR}/src")),
            Ok("src".to_string())
        );
        for bad in [
            "/tmp/x",
            "$CI_BUILDS_DIR/../etc",
            "$CI_BUILDS_DIR/a//b",
            "$CI_BUILDS_DIR/a b",
        ] {
            assert!(project_subdir(&with(bad)).is_err(), "{}", bad);
        }
    }

    #[test]
    fn parses_go_durations() {
        let secs = |text| parse_duration(text).map(|d| d.as_secs_f64());
        assert_eq!(secs("90s"), Some(90.0));
        assert_eq!(secs("1h30m"), Some(5400.0));
        assert_eq!(secs("1.5h"), Some(5400.0));
        assert_eq!(secs("250ms"), Some(0.25));
        assert_eq!(secs("0"), Some(0.0));
        for bad in ["", "10", "5x", "m", "-1s", "1h 30m"] {
            assert_eq!(parse_duration(bad), None, "{:?}", bad);
        }
    }

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

        let env = job_env(
            &j,
            "/builds",
            Path::new("/host/job-1"),
            &RunnerVars::default(),
        );

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
    fn runner_adds_predefined_and_tls_variables() {
        let j = job(serde_json::json!([]));
        let runner = RunnerVars {
            disposable: true,
            ca_pem: Some("-----BEGIN CERTIFICATE-----".to_string()),
        };

        let env = job_env(&j, "/builds", Path::new("/h"), &runner);

        assert_eq!(get(&env, "CI_SERVER"), Some("yes"));
        assert_eq!(get(&env, "CI_JOB_TIMEOUT"), Some("3600"));
        assert_eq!(get(&env, "CI_DISPOSABLE_ENVIRONMENT"), Some("true"));
        assert_eq!(
            get(&env, "CI_SERVER_TLS_CA_FILE"),
            Some("/builds/project.tmp/CI_SERVER_TLS_CA_FILE")
        );
        assert_eq!(
            get(&env, "GIT_SSL_CAINFO"),
            get(&env, "CI_SERVER_TLS_CA_FILE")
        );
        assert_eq!(
            env.files,
            vec![(
                PathBuf::from("/h/project.tmp/CI_SERVER_TLS_CA_FILE"),
                "-----BEGIN CERTIFICATE-----".to_string()
            )]
        );
        let shell = job_env(&j, "/b", Path::new("/h"), &RunnerVars::default());
        assert_eq!(get(&shell, "CI_SHARED_ENVIRONMENT"), Some("true"));
    }

    #[test]
    fn later_variables_override_earlier_ones() {
        let j = job(serde_json::json!([
            {"key": "MODE", "value": "instance"},
            {"key": "MODE", "value": "job"}
        ]));

        let env = job_env(&j, "/builds", Path::new("/h"), &RunnerVars::default());

        assert_eq!(get(&env, "MODE"), Some("job"));
        assert_eq!(env.vars.iter().filter(|(k, _)| k == "MODE").count(), 1);
    }

    #[test]
    fn a_variable_set_again_moves_to_the_end() {
        let j = job(serde_json::json!([
            {"key": "A", "value": "1"},
            {"key": "B", "value": "2"},
            {"key": "CI_SERVER", "value": "job"},
            {"key": "A", "value": "3"},
            {"key": "C", "value": "$A"}
        ]));

        let env = job_env(&j, "/b", Path::new("/h"), &RunnerVars::default());

        let expected = [
            ("CI_BUILDS_DIR", "/b"),
            ("CI_PROJECT_DIR", "/b/project"),
            ("CI_JOB_STATUS", "running"),
            ("CI_JOB_TIMEOUT", "3600"),
            ("CI_SHARED_ENVIRONMENT", "true"),
            ("B", "2"),
            ("CI_SERVER", "job"),
            ("A", "3"),
            ("C", "3"),
        ];
        let expected: Vec<(String, String)> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(env.vars, expected);
    }

    #[test]
    fn service_variables_override_job_variables_in_place_of_last_set() {
        let j = job(serde_json::json!([
            {"key": "A", "value": "job", "public": true},
            {"key": "B", "value": "job", "public": true},
            {"key": "SECRET", "value": "x", "masked": true}
        ]));
        let service: Vec<crate::gitlab::Variable> =
            serde_json::from_value(serde_json::json!([{"key": "A", "value": "service"}])).unwrap();

        let values = job_env(&j, "/b", Path::new("/h"), &RunnerVars::default())
            .vars
            .into_iter()
            .collect();

        let env = service_env(&j, &service, &values);

        assert_eq!(env, ["B=job", "A=service"]);
    }

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn expansion_is_one_pass_like_gitlab_runner() {
        // gitlab-runner's Variables.Get returns the value as given: GitLab
        // expands nested references itself before it sends the job, the
        // runner adds one pass on top
        let j = job(serde_json::json!([
            {"key": "A", "value": "$B"},
            {"key": "B", "value": "$C"},
            {"key": "C", "value": "x"},
            {"key": "D", "value": "${A}-$CI_BUILDS_DIR"}
        ]));

        let env = job_env(&j, "/b", Path::new("/h"), &RunnerVars::default());

        assert_eq!(get(&env, "A"), Some("$C"));
        assert_eq!(get(&env, "B"), Some("x"));
        assert_eq!(get(&env, "D"), Some("$B-/b"));
    }

    #[test]
    fn a_file_variable_expands_to_its_path_in_other_variables() {
        let j = job(serde_json::json!([
            {"key": "KUBECONFIG_FILE", "value": "apiVersion: v1 $HOME", "file": true},
            {"key": "KUBECONFIG", "value": "$KUBECONFIG_FILE"},
            {"key": "ARGS", "value": "--config=${KUBECONFIG_FILE}"},
            {"key": "HOME", "value": "/root"}
        ]));

        let env = job_env(&j, "/builds", Path::new("/h"), &RunnerVars::default());

        let path = "/builds/project.tmp/KUBECONFIG_FILE";
        assert_eq!(get(&env, "KUBECONFIG_FILE"), Some(path));
        assert_eq!(get(&env, "KUBECONFIG"), Some(path));
        assert_eq!(get(&env, "ARGS"), Some(&*format!("--config={}", path)));
        // The file's own content is expanded once, like any other value
        assert_eq!(
            env.files,
            vec![(
                PathBuf::from("/h/project.tmp/KUBECONFIG_FILE"),
                "apiVersion: v1 /root".to_string()
            )]
        );
    }

    #[test]
    fn a_variable_naming_itself_extends_the_value_it_replaces() {
        let with = |vars: serde_json::Value| -> Job {
            serde_json::from_value(serde_json::json!({"id": 1, "token": "t", "variables": vars}))
                .unwrap()
        };
        let path_of = |j: &Job, disposable: bool| {
            let runner = RunnerVars {
                disposable,
                ..RunnerVars::default()
            };
            get(&job_env(j, "/b", Path::new("/h"), &runner), "PATH")
                .unwrap()
                .to_string()
        };

        // The runner's `environment` first, then the job's own PATH
        let both = with(serde_json::json!([
            {"key": "PATH", "value": "/env/bin:/usr/bin"},
            {"key": "PATH", "value": "$PATH:/job/bin"}
        ]));
        assert_eq!(path_of(&both, true), "/env/bin:/usr/bin:/job/bin");
        assert_eq!(path_of(&both, false), "/env/bin:/usr/bin:/job/bin");

        // Alone on the shell executor: the runner process's own PATH
        let alone = with(serde_json::json!([{"key": "PATH", "value": "/tools:${PATH}"}]));
        let host = std::env::var("PATH").unwrap();
        assert_eq!(path_of(&alone, false), format!("/tools:{}", host));
        // A container's PATH is not the runner's: unchanged there
        assert!(!path_of(&alone, true).contains(&host));

        assert!(names("a:$PATH:b", "PATH") && names("${PATH}", "PATH") && names("$PATH", "PATH"));
        assert!(
            !names("$PATHX", "PATH") && !names("$PATH_EXTRA", "PATH") && !names("PATH", "PATH")
        );
    }

    #[test]
    fn expands_like_go_os_expand() {
        let vars = values(&[("A", "a"), ("B_1", "b")]);
        for (input, expected) in [
            ("$A/${A}/$B_1.x", "a/a/b.x"),
            ("$UNSET|${UNSET}", "|"),
            ("$$A and $${A}", "$A and ${A}"),
            ("${$}", "$"),
            // Positional and special shell parameters give nothing
            ("awk '{print $1}'", "awk '{print }'"),
            ("$12", "2"),
            ("$* $# $@ $! $? $- ${*}", "      "),
            // A `$` not followed by a name stays
            ("$ $/ $. 100$", "$ $/ $. 100$"),
            ("$é", "$é"),
            // No shell operators: the whole text in braces is the name
            ("${A:-default}", ""),
            ("${A-x}y", "y"),
            // Malformed braces are dropped
            ("x${}y", "xy"),
            ("x${A", "xA"),
            ("${", ""),
            ("é${A}é", "éaé"),
        ] {
            assert_eq!(expand(input, &vars), expected, "{:?}", input);
        }
    }

    #[test]
    fn service_variables_are_not_expanded_twice() {
        let j = job(serde_json::json!([
            {"key": "A", "value": "$B", "public": true},
            {"key": "B", "value": "$C", "public": true},
            {"key": "C", "value": "x", "public": true},
            {"key": "TOKEN", "value": "secret", "masked": true},
            {"key": "MODE", "value": "yaml", "public": true},
            {"key": "MODE", "value": "project"}
        ]));
        let service: Vec<crate::gitlab::Variable> = serde_json::from_value(serde_json::json!([
            {"key": "S1", "value": "$A"},
            {"key": "S2", "value": "${S1}+$TOKEN"},
            {"key": "S3", "value": "$C", "raw": true}
        ]))
        .unwrap();
        let values = job_env(&j, "/b", Path::new("/h"), &RunnerVars::default())
            .vars
            .into_iter()
            .collect();

        let env = service_env(&j, &service, &values);

        // A is what the job container sees ("$C"), not expanded again;
        // service variables expand against the job's values and each other
        // (a hidden one included, the service asked for it by name); the
        // overridden public MODE does not resurface
        assert_eq!(
            env,
            ["A=$C", "B=x", "C=x", "S1=$C", "S2=$A+secret", "S3=$C"]
        );
    }
}
