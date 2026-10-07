//! Executors run a job's sources checkout and steps: Docker (one container per job)
//! or shell (directly on the host). The step semantics are shared; executors only
//! differ in how a script is run.

// Bollard 0.19 has deprecated old API, but new API is complex
// We'll migrate to new API in future version
#![allow(deprecated)]

use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::Docker;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;
use tracing::{info, warn};

use super::cancel::{CancelSignal, Stop};
use super::config::DockerConfig;
use super::git::{self, GitStrategy};
use super::image::{self, PullPolicy};
use super::script::{self, JobEnv};
use super::trace::TraceWriter;
use crate::gitlab::{FailureReason, Job, RemoteState};

mod docker;
mod shell;

pub use docker::DockerExecutor;
pub use shell::ShellExecutor;

/// Used when GitLab sends no job timeout
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(3600);
/// GitLab's default after_script timeout
const DEFAULT_AFTER_SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Runs the script (passed as `$1`) with bash when the image or host has it, else
/// sh, like gitlab-runner: on Debian-based images `sh` is dash, which lacks
/// `[[ ]]`, `source` and arrays
const SHELL_DETECT: &str =
    r#"if command -v bash >/dev/null 2>&1; then exec bash -c "$1"; fi; exec sh -c "$1""#;

/// How often a quiet script's pending output is checked for sending
const TRACE_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Upper bound for resetting workspace ownership after a job
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a restore stopped by the job timeout or a cancel may take to
/// end; a download stuck in the network is dropped after it (shorter in tests)
const RESTORE_STOP_GRACE: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(10)
};
/// Host directory holding Docker job workspaces (bind-mounted as /builds)
const DOCKER_BUILDS_ROOT: &str = "/tmp/turboci-builds";

/// Why a job did not succeed
#[derive(Debug, Clone)]
pub struct JobFailure {
    pub reason: FailureReason,
    pub exit_code: Option<i32>,
    pub message: String,
}

impl JobFailure {
    pub fn system(message: impl std::fmt::Display) -> Self {
        Self {
            reason: FailureReason::RunnerSystemFailure,
            exit_code: None,
            message: message.to_string(),
        }
    }
}

pub type JobOutcome = std::result::Result<(), JobFailure>;

// One executor exists per runner process, so the size of the Docker variant
// does not matter
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ExecutorType {
    Docker(DockerExecutor),
    Shell(ShellExecutor),
}

impl ExecutorType {
    /// Host directory of the job; the project is checked out in `<job_dir>/project`
    pub fn job_dir(&self, job_id: u64) -> PathBuf {
        match self {
            ExecutorType::Docker(_) => {
                Path::new(DOCKER_BUILDS_ROOT).join(format!("job-{}", job_id))
            }
            ExecutorType::Shell(executor) => executor.job_dir(job_id),
        }
    }

    /// Check out sources and run the job's steps, writing output to `trace`
    pub async fn execute(
        &self,
        job: &Job,
        trace: &mut TraceWriter<'_>,
        restore: &dyn Restore,
    ) -> JobOutcome {
        use futures_util::FutureExt;

        let run = async {
            match self {
                ExecutorType::Docker(executor) => {
                    executor
                        .execute(job, &self.job_dir(job.id), trace, restore)
                        .await
                }
                ExecutorType::Shell(executor) => executor.execute(job, trace, restore).await,
            }
        };
        // A bug must fail the job, not skip its cleanup and final report
        std::panic::AssertUnwindSafe(run)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(JobFailure::system("internal runner error")))
    }

    /// Give jobs the custom CA the GitLab server's certificate is signed with
    pub fn with_ca_pem(mut self, pem: Option<String>) -> Self {
        match &mut self {
            ExecutorType::Docker(executor) => executor.ca_pem = pem,
            ExecutorType::Shell(executor) => executor.ca_pem = pem,
        }
        self
    }

    /// Files in the checked-out project that git does not track (including
    /// ignored ones), relative to the project, for `artifacts:untracked` and
    /// `cache:untracked`
    pub async fn untracked_files(&self, job: &Job) -> Result<Vec<String>> {
        let subdir = script::project_subdir(job).unwrap_or_else(|_| "project".to_string());
        let job_dir = self.job_dir(job.id);
        let output = match self {
            ExecutorType::Docker(executor) => {
                executor.untracked_files(job.id, &job_dir, &subdir).await?
            }
            ExecutorType::Shell(executor) => {
                executor.untracked_files(&job_dir.join(&subdir)).await?
            }
        };
        Ok(output
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Remove what a previous run of this runner left behind
    pub async fn sweep_orphans(&self) {
        if let ExecutorType::Docker(executor) = self {
            executor.sweep_orphans().await;
        }
    }

    /// Remove the job's host directory
    pub async fn cleanup(&self, job_id: u64) {
        let dir = self.job_dir(job_id);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => info!("💾 Removed workspace {}", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to remove workspace {}: {}", dir.display(), e),
        }
    }
}

/// Restores cache and dependency artifacts into the project, after sources are
/// checked out and before the scripts run (gitlab-runner's order: otherwise
/// `git checkout -f` would revert restored tracked files). Extractions end
/// once `stop` is set.
#[async_trait]
pub trait Restore: Send + Sync {
    async fn restore(&self, job: &Job, trace: &mut TraceWriter<'_>, stop: &Stop) -> JobOutcome;
}

/// Nothing to restore
#[cfg(test)]
pub struct NoRestore;

#[cfg(test)]
#[async_trait]
impl Restore for NoRestore {
    async fn restore(&self, _job: &Job, _trace: &mut TraceWriter<'_>, _stop: &Stop) -> JobOutcome {
        Ok(())
    }
}

/// Git options for commands the runner runs in a job's repository after the job
/// has ended: the repository's config must not run commands (core.fsmonitor, hooks)
const NO_REPO_COMMANDS: [&str; 4] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// Result of running one script
#[derive(Debug, PartialEq, Eq)]
enum RunStatus {
    Exited(i32),
    TimedOut,
    Canceled,
}

/// A script run stops at its deadline or once cancellation reaches `stop_at`
struct Limits<'a> {
    deadline: Instant,
    cancel: &'a CancelSignal,
    stop_at: RemoteState,
}

/// How an executor runs a shell script in a directory, streaming output to the trace
#[async_trait]
trait ScriptRunner: Send + Sync {
    async fn run(
        &self,
        script: &str,
        workdir: &str,
        trace: &mut TraceWriter<'_>,
        limits: &Limits<'_>,
    ) -> Result<RunStatus>;
}

/// Directories as the job sees them
struct JobDirs {
    builds: String,
    project: String,
}

/// Get sources, then run the steps GitLab sent, honouring `when`, `allow_failure`
/// and timeouts. The job timeout covers sources and script steps; `after_script`
/// has its own timeout and never changes the job result.
async fn run_job_steps(
    job: &Job,
    sources: &dyn ScriptRunner,
    runner: &dyn ScriptRunner,
    trace: &mut TraceWriter<'_>,
    dirs: &JobDirs,
    restore: &dyn Restore,
) -> JobOutcome {
    let timeout = job
        .timeout_secs()
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_JOB_TIMEOUT);
    let deadline = Instant::now() + timeout;
    let cancel = trace.cancel();
    let canceled = || JobFailure {
        reason: FailureReason::JobCanceled,
        exit_code: None,
        message: "canceled".to_string(),
    };
    let timed_out = || JobFailure {
        reason: FailureReason::JobExecutionTimeout,
        exit_code: None,
        message: format!("execution took longer than {}s", timeout.as_secs()),
    };

    let script_limits = Limits {
        deadline,
        cancel: &cancel,
        stop_at: RemoteState::Canceling,
    };
    trace
        .section_start("get_sources", "Getting source from Git repository")
        .await;
    let fetched = get_sources(job, sources, trace, dirs, &script_limits).await;
    trace.section_end("get_sources").await;
    let mut failure = match fetched {
        Ok(()) => None,
        Err(RunError::TimedOut) => Some(timed_out()),
        Err(RunError::Canceled) => Some(canceled()),
        Err(RunError::Failed(f)) => Some(f),
    };
    if failure.is_none() {
        // Bound like the scripts: a slow S3 transfer must not outlive the job
        // timeout or a cancel. The restore is then stopped and awaited, so no
        // extraction still writes to the workspace while after_script, the
        // saves and the cleanup run.
        let stop = Stop::default();
        let mut restoring = restore.restore(job, trace, &stop);
        let interrupted = tokio::select! {
            restored = &mut restoring => {
                failure = restored.err();
                None
            }
            _ = tokio::time::sleep_until(script_limits.deadline) => Some(timed_out()),
            _ = script_limits.cancel.reached(script_limits.stop_at) => Some(canceled()),
        };
        if interrupted.is_some() {
            stop.stop();
            // Dropping a download removes its temporary file
            let _ = tokio::time::timeout(RESTORE_STOP_GRACE, restoring).await;
            failure = interrupted;
        }
    }

    // RUNNER_SCRIPT_TIMEOUT caps the script steps within the job timeout
    let script_timeout = stage_timeout(job, "RUNNER_SCRIPT_TIMEOUT", trace).await;
    // checked_add: a job may ask for any duration, which must never overflow
    let script_deadline = script_timeout.map_or(deadline, |t| {
        Instant::now()
            .checked_add(t)
            .map_or(deadline, |d| deadline.min(d))
    });
    let after_script_timeout = stage_timeout(job, "RUNNER_AFTER_SCRIPT_TIMEOUT", trace).await;

    for step in &job.steps {
        if cancel.state() == RemoteState::Aborted {
            failure.get_or_insert_with(canceled);
            break;
        }
        let is_after_script = step.name == "after_script";
        if !is_after_script && cancel.state() != RemoteState::Running {
            failure.get_or_insert_with(canceled);
        }
        let should_run = match step.when.as_str() {
            "always" => true,
            "on_failure" => failure.is_some(),
            _ => failure.is_none(),
        };
        let lines: Vec<String> = step
            .before_script
            .iter()
            .chain(&step.script)
            .cloned()
            .collect();
        if !should_run || lines.is_empty() {
            continue;
        }

        // after_script still runs when the job is being canceled (canceling), and
        // only stops when it is aborted
        let section = if is_after_script {
            "after_script".to_string()
        } else {
            format!("step_{}", step.name)
        };
        let limits = if is_after_script {
            trace.section_start(&section, "Running after_script").await;
            let secs = u64::from(step.timeout);
            let wanted = match after_script_timeout {
                Some(timeout) => timeout,
                None if secs > 0 => Duration::from_secs(secs),
                None => DEFAULT_AFTER_SCRIPT_TIMEOUT,
            };
            // after_script may not outlive the job timeout (it keeps a runner
            // slot), but always gets the default grace, even after a timeout
            let now = Instant::now();
            let cap = deadline.max(now + DEFAULT_AFTER_SCRIPT_TIMEOUT);
            Limits {
                deadline: now.checked_add(wanted).map_or(cap, |d| d.min(cap)),
                cancel: &cancel,
                stop_at: RemoteState::Aborted,
            }
        } else {
            trace
                .section_start(
                    &section,
                    &format!("Executing \"step_{}\" stage of the job script", step.name),
                )
                .await;
            Limits {
                deadline: script_deadline,
                cancel: &cancel,
                stop_at: RemoteState::Canceling,
            }
        };

        let mut script = script::step_script(&lines);
        // CI_DEBUG_TRACE shows every command as it runs (masking still applies)
        if variable_value(job, "CI_DEBUG_TRACE") == Some("true") {
            script = format!("set -x\n{}", script);
        }
        if is_after_script {
            // Like gitlab-runner, after_script can tell how the job went
            let status = match &failure {
                None => "success",
                Some(f) if f.reason == FailureReason::JobCanceled => "canceled",
                Some(_) => "failed",
            };
            script = format!("export CI_JOB_STATUS={}\n{}", status, script);
        }
        let result = runner.run(&script, &dirs.project, trace, &limits).await;
        match result {
            Ok(RunStatus::Exited(0)) => {}
            Ok(RunStatus::Exited(code)) if step.allow_failure || is_after_script => {
                trace
                    .write(&format!(
                        "WARNING: {} failed with exit code {}\n",
                        step.name, code
                    ))
                    .await;
            }
            Ok(RunStatus::Exited(code)) => {
                failure.get_or_insert(JobFailure {
                    reason: FailureReason::ScriptFailure,
                    exit_code: Some(code),
                    message: format!("exit code {}", code),
                });
            }
            Ok(RunStatus::TimedOut) if is_after_script => {
                trace.write("WARNING: after_script timed out\n").await;
            }
            Ok(RunStatus::TimedOut) => {
                failure.get_or_insert_with(timed_out);
            }
            Ok(RunStatus::Canceled) if is_after_script => {
                trace.write("WARNING: after_script canceled\n").await;
            }
            Ok(RunStatus::Canceled) => {
                failure.get_or_insert_with(canceled);
            }
            Err(e) if is_after_script => {
                trace
                    .write(&format!("WARNING: after_script could not run: {}\n", e))
                    .await;
            }
            Err(e) => {
                failure.get_or_insert(JobFailure::system(format!("{:#}", e)));
            }
        }
        trace.section_end(&section).await;
    }

    failure.map_or(Ok(()), Err)
}

/// A stage timeout from a job variable (RUNNER_SCRIPT_TIMEOUT or
/// RUNNER_AFTER_SCRIPT_TIMEOUT), in Go duration syntax like "10m" or "1h30m"
async fn stage_timeout(job: &Job, key: &str, trace: &mut TraceWriter<'_>) -> Option<Duration> {
    let raw = variable_value(job, key)?.trim();
    if raw.is_empty() {
        return None;
    }
    match script::parse_duration(raw) {
        Some(timeout) if !timeout.is_zero() => Some(timeout),
        Some(_) => None,
        None => {
            trace
                .write(&format!("WARNING: Ignoring malformed {}: {:?}\n", key, raw))
                .await;
            None
        }
    }
}

enum RunError {
    TimedOut,
    Canceled,
    Failed(JobFailure),
}

async fn get_sources(
    job: &Job,
    runner: &dyn ScriptRunner,
    trace: &mut TraceWriter<'_>,
    dirs: &JobDirs,
    limits: &Limits<'_>,
) -> std::result::Result<(), RunError> {
    // The runner's and the job's pre_get_sources_script run before the
    // checkout, post_get_sources_script after it
    run_hook(
        job,
        "pre_get_sources_script",
        &dirs.builds,
        runner,
        trace,
        limits,
    )
    .await?;
    checkout(job, runner, trace, dirs, limits).await?;
    run_hook(
        job,
        "post_get_sources_script",
        &dirs.project,
        runner,
        trace,
        limits,
    )
    .await
}

/// Run the lines of hook `name` (see `hook_lines`) in `dir`; a failing hook fails the job
async fn run_hook(
    job: &Job,
    name: &str,
    dir: &str,
    runner: &dyn ScriptRunner,
    trace: &mut TraceWriter<'_>,
    limits: &Limits<'_>,
) -> std::result::Result<(), RunError> {
    let lines = hook_lines(job, name);
    if lines.is_empty() {
        return Ok(());
    }
    trace.write(&format!("Running {}\n", name)).await;
    match runner
        .run(&script::step_script(&lines), dir, trace, limits)
        .await
    {
        Ok(RunStatus::Exited(0)) => Ok(()),
        Ok(RunStatus::Exited(code)) => Err(RunError::Failed(JobFailure {
            reason: FailureReason::ScriptFailure,
            exit_code: Some(code),
            message: format!("{} failed with exit code {}", name, code),
        })),
        Ok(RunStatus::TimedOut) => Err(RunError::TimedOut),
        Ok(RunStatus::Canceled) => Err(RunError::Canceled),
        Err(e) => Err(RunError::Failed(JobFailure::system(format!("{:#}", e)))),
    }
}

async fn checkout(
    job: &Job,
    runner: &dyn ScriptRunner,
    trace: &mut TraceWriter<'_>,
    dirs: &JobDirs,
    limits: &Limits<'_>,
) -> std::result::Result<(), RunError> {
    let Some(ref git_info) = job.git_info else {
        return Ok(());
    };

    let strategy = git::strategy(&job.variables);
    let commands = match strategy {
        GitStrategy::None => {
            trace.write("Skipping Git repository setup\n").await;
            return Ok(());
        }
        GitStrategy::Empty => vec![vec![
            "mkdir".to_string(),
            "-p".to_string(),
            dirs.project.clone(),
        ]],
        GitStrategy::Fetch => git::checkout_commands(git_info, &job.variables, &dirs.project)
            .map_err(|e| RunError::Failed(JobFailure::system(e)))?,
    };

    // repo_url embeds the job token, so only the ref and SHA are shown
    trace
        .write(&format!(
            "Fetching changes...\nChecking out {} as detached HEAD (ref is {})...\n",
            git_info.sha.chars().take(8).collect::<String>(),
            git_info.ref_name
        ))
        .await;

    // Like gitlab-runner: skip LFS objects during checkout, then pull them if
    // git-lfs is available and the pipeline did not opt out
    let lfs = if matches!(strategy, GitStrategy::Fetch)
        && variable_value(job, "GIT_LFS_SKIP_SMUDGE") != Some("1")
    {
        let dest = script::quote(&dirs.project);
        let safe = script::quote(&format!("safe.directory={}", dirs.project));
        format!(
            "if git lfs version >/dev/null 2>&1 && [ -d {dest}/.git ]; then \
             git -c {safe} -C {dest} lfs pull; fi\n"
        )
    } else {
        String::new()
    };
    let script = format!(
        "umask 0000\nexport GIT_LFS_SKIP_SMUDGE=1\n{}{}",
        script::argv_script(&commands),
        lfs
    );
    // GET_SOURCES_ATTEMPTS retries flaky fetches (1 to 10, default 1)
    let attempts = variable_value(job, "GET_SOURCES_ATTEMPTS")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, 10);
    for attempt in 1..=attempts {
        // Checked-out files must stay writable for the runner (restore) and
        // for images with a non-root USER, as with gitlab-runner
        let result = runner.run(&script, &dirs.builds, trace, limits).await;
        match result {
            Ok(RunStatus::Exited(0)) => return Ok(()),
            Ok(RunStatus::Exited(code)) if attempt < attempts => {
                trace
                    .write(&format!(
                        "WARNING: getting sources failed with exit code {} (attempt {}/{}), retrying\n",
                        code, attempt, attempts
                    ))
                    .await;
                let reset = script::argv_script(&[vec![
                    "rm".to_string(),
                    "-rf".to_string(),
                    format!("{}/.git", dirs.project),
                ]]);
                let _ = runner.run(&reset, &dirs.builds, trace, limits).await;
            }
            Ok(RunStatus::Exited(code)) => {
                return Err(RunError::Failed(JobFailure {
                    reason: FailureReason::ScriptFailure,
                    exit_code: Some(code),
                    message: format!("getting sources failed with exit code {}", code),
                }))
            }
            Ok(RunStatus::TimedOut) => return Err(RunError::TimedOut),
            Ok(RunStatus::Canceled) => return Err(RunError::Canceled),
            Err(e) => return Err(RunError::Failed(JobFailure::system(format!("{:#}", e)))),
        }
    }
    Ok(())
}

/// Lines of hook `name` (e.g. `pre_get_sources_script`), from the job's
/// `hooks:` and the runner's config (added to `hooks` by the daemon)
fn hook_lines(job: &Job, name: &str) -> Vec<String> {
    job.hooks
        .iter()
        .filter(|hook| hook["name"] == name)
        .filter_map(|hook| hook["script"].as_array())
        .flatten()
        .filter_map(|line| line.as_str().map(str::to_string))
        .collect()
}

/// Value of a job variable (last definition wins)
fn variable_value<'a>(job: &'a Job, key: &str) -> Option<&'a str> {
    job.variables
        .iter()
        .rev()
        .find(|v| v.key == key)
        .and_then(|v| v.value.as_deref())
}

/// Write the files backing `file`-type variables
async fn write_variable_files(env: &JobEnv) -> Result<()> {
    for (path, content) in &env.files {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, content)
            .await
            .with_context(|| format!("Failed to write variable file {}", path.display()))?;
    }
    Ok(())
}

/// Decodes a byte stream to UTF-8 without breaking characters split across chunks
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            // Incomplete character at the end: keep it for the next chunk
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => {
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                return text;
            }
        };
        let rest = self.pending.split_off(valid);
        String::from_utf8(std::mem::replace(&mut self.pending, rest)).unwrap_or_default()
    }

    fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::secret_scrubber::SecretScrubber;

    pub(super) fn job(value: serde_json::Value) -> Job {
        serde_json::from_value(value).unwrap()
    }

    async fn run_shell(job: &Job, work_dir: &Path) -> (JobOutcome, String) {
        let executor = ShellExecutor::new(Some(work_dir.to_string_lossy().into_owned()));
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(job);
        let mut trace = TraceWriter::new(None, job.id, &job.token, &scrubber);
        let outcome = executor.execute(job, &mut trace, &NoRestore).await;
        trace.finish().await;
        (outcome, trace.text())
    }

    pub(super) fn steps(script: &[&str], after: &[&str]) -> serde_json::Value {
        serde_json::json!([
            {"name": "script", "script": script, "when": "on_success", "timeout": 3600},
            {"name": "after_script", "script": after, "when": "always",
             "allow_failure": true, "timeout": 5}
        ])
    }

    #[tokio::test]
    async fn runs_steps_with_variables_and_masks_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 1, "token": "t",
            "variables": [
                {"key": "GREETING", "value": "hello"},
                {"key": "API_KEY", "value": "top-secret-key", "masked": true},
                {"key": "CONFIG", "value": "file-content", "file": true}
            ],
            "steps": steps(
                &["cd /", "echo \"$GREETING from $PWD\"", "echo key=$API_KEY", "cat \"$CONFIG\""],
                &["echo cleanup"]
            )
        }));

        let (outcome, log) = run_shell(&j, dir.path()).await;

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("hello from /"), "{}", log);
        assert!(log.contains("key=[MASKED]"), "{}", log);
        assert!(!log.contains("top-secret-key"));
        assert!(log.contains("file-content"), "{}", log);
        assert!(log.contains("cleanup"));
    }

    #[tokio::test]
    async fn script_failure_reports_exit_code_and_still_runs_after_script() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 2, "token": "t",
            "steps": steps(
                &["echo before", "exit 3", "echo never-printed"],
                &["echo after-ran", "exit 9"]
            )
        }));

        let (outcome, log) = run_shell(&j, dir.path()).await;

        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, FailureReason::ScriptFailure);
        assert_eq!(failure.exit_code, Some(3));
        assert!(log.contains("after-ran"), "{}", log);
        assert!(!log.contains("\nnever-printed"), "{}", log);
    }

    #[tokio::test]
    async fn timeout_kills_script_and_reports_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let j = job(serde_json::json!({
            "id": 3, "token": "t",
            "runner_info": {"timeout": 1},
            "steps": steps(
                &[&format!("(sleep 3; touch {}) &", marker.display()), "sleep 30"],
                &["echo after-timeout"]
            )
        }));

        let started = std::time::Instant::now();
        let (outcome, log) = run_shell(&j, dir.path()).await;

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::JobExecutionTimeout
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(log.contains("after-timeout"), "{}", log);
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "background process outlived the timeout");
    }

    async fn run_shell_canceled(
        job: &Job,
        work_dir: &Path,
        state: RemoteState,
        restore: &dyn Restore,
    ) -> (JobOutcome, String) {
        let executor = ShellExecutor::new(Some(work_dir.to_string_lossy().into_owned()));
        let scrubber = SecretScrubber::new(vec![]);
        let cancel = CancelSignal::default();
        let mut trace =
            TraceWriter::new(None, job.id, &job.token, &scrubber).with_cancel(cancel.clone());
        let trigger = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel.update(state);
        });
        let outcome = executor.execute(job, &mut trace, restore).await;
        trigger.await.unwrap();
        trace.finish().await;
        (outcome, trace.text())
    }

    #[tokio::test]
    async fn canceling_stops_script_but_runs_after_script() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 5, "token": "t",
            "steps": steps(&["echo started", "sleep 30", "echo not-reached"], &["echo cleanup-ran"])
        }));

        let started = std::time::Instant::now();
        let (outcome, log) =
            run_shell_canceled(&j, dir.path(), RemoteState::Canceling, &NoRestore).await;

        assert_eq!(outcome.unwrap_err().reason, FailureReason::JobCanceled);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(log.contains("started"), "{}", log);
        assert!(!log.contains("\nnot-reached"), "{}", log);
        assert!(log.contains("cleanup-ran"), "{}", log);
    }

    #[tokio::test]
    async fn abort_stops_everything_including_after_script() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 6, "token": "t",
            "steps": steps(&["sleep 30"], &["echo cleanup-ran"])
        }));

        let (outcome, log) =
            run_shell_canceled(&j, dir.path(), RemoteState::Aborted, &NoRestore).await;

        assert_eq!(outcome.unwrap_err().reason, FailureReason::JobCanceled);
        assert!(!log.contains("cleanup-ran"), "{}", log);
    }

    /// A restore that never finishes and ignores the stop, like a download
    /// stuck in the network: only the grace period ends it
    struct EndlessRestore;

    #[async_trait]
    impl Restore for EndlessRestore {
        async fn restore(
            &self,
            _job: &Job,
            _trace: &mut TraceWriter<'_>,
            _stop: &Stop,
        ) -> JobOutcome {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn the_job_timeout_stops_a_restore() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ShellExecutor::new(Some(dir.path().to_string_lossy().into_owned()));
        let j = job(serde_json::json!({
            "id": 9, "token": "t",
            "runner_info": {"timeout": 1},
            "steps": steps(&["echo script-ran"], &["echo after-ran"])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, j.id, &j.token, &scrubber);

        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            executor.execute(&j, &mut trace, &EndlessRestore),
        )
        .await
        .expect("the restore outlived the 1 s job timeout and the grace period");
        trace.finish().await;
        let log = trace.text();

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::JobExecutionTimeout
        );
        assert!(!log.contains("\nscript-ran"), "{}", log);
        assert!(log.contains("after-ran"), "{}", log);
    }

    #[tokio::test]
    async fn canceling_stops_a_restore() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 10, "token": "t",
            "steps": steps(&["echo script-ran"], &["echo cleanup-ran"])
        }));

        // Canceled after 500 ms
        let (outcome, log) = tokio::time::timeout(
            Duration::from_secs(3),
            run_shell_canceled(&j, dir.path(), RemoteState::Canceling, &EndlessRestore),
        )
        .await
        .expect("the restore outlived the cancel and the grace period");

        assert_eq!(outcome.unwrap_err().reason, FailureReason::JobCanceled);
        assert!(!log.contains("\nscript-ran"), "{}", log);
        assert!(log.contains("cleanup-ran"), "{}", log);
    }

    /// Extracts a cache archive, like the S3 cache restore
    struct ExtractRestore {
        archive: PathBuf,
        into: PathBuf,
    }

    #[async_trait]
    impl Restore for ExtractRestore {
        async fn restore(
            &self,
            _job: &Job,
            _trace: &mut TraceWriter<'_>,
            stop: &Stop,
        ) -> JobOutcome {
            super::super::artifacts::extract_archive_file(
                &self.archive,
                &self.into.to_string_lossy(),
                stop,
            )
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)))
        }
    }

    fn files_in(dir: &Path) -> usize {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .count()
    }

    #[tokio::test]
    async fn a_canceled_restore_stops_writing_before_the_job_moves_on() {
        const ENTRIES: usize = 20_000;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("cache.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for i in 0..ENTRIES {
            zip.start_file(format!("vendor/{}/{}.txt", i % 100, i), stored)
                .unwrap();
            std::io::Write::write_all(&mut zip, b"cached").unwrap();
        }
        zip.finish().unwrap();
        let into = dir.path().join("restored");
        let restore = ExtractRestore {
            archive,
            into: into.clone(),
        };
        let executor = ShellExecutor::new(Some(dir.path().to_string_lossy().into_owned()));
        let j = job(serde_json::json!({
            "id": 11, "token": "t",
            "steps": steps(&["echo script-ran"], &["echo cleanup-ran"])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let cancel = CancelSignal::default();
        let mut trace =
            TraceWriter::new(None, j.id, &j.token, &scrubber).with_cancel(cancel.clone());
        // Canceled once the extraction has started
        let trigger = async {
            while files_in(&into) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel.update(RemoteState::Canceling);
        };

        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(executor.execute(&j, &mut trace, &restore), trigger)
        })
        .await
        .expect("the job did not end");

        assert_eq!(outcome.unwrap_err().reason, FailureReason::JobCanceled);
        let written = files_in(&into);
        assert!(written < ENTRIES, "the extraction was not stopped");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(files_in(&into), written, "written after the job moved on");
        std::fs::remove_dir_all(&into).unwrap();
    }

    #[tokio::test]
    async fn after_script_sees_job_status() {
        for (script, expected) in [("true", "status=success"), ("exit 1", "status=failed")] {
            let dir = tempfile::tempdir().unwrap();
            let j = job(serde_json::json!({
                "id": 7, "token": "t",
                "steps": steps(&[script], &["echo status=$CI_JOB_STATUS"])
            }));

            let (_, log) = run_shell(&j, dir.path()).await;

            assert!(log.contains(expected), "{}", log);
        }
    }

    /// Writes a new VERSION into the project, like an artifact from an earlier job
    struct BumpVersion(PathBuf);

    #[async_trait]
    impl Restore for BumpVersion {
        async fn restore(
            &self,
            _job: &Job,
            _trace: &mut TraceWriter<'_>,
            _stop: &Stop,
        ) -> JobOutcome {
            std::fs::write(self.0.join("project/VERSION"), "2.0.0\n").map_err(JobFailure::system)
        }
    }

    #[tokio::test]
    async fn restore_runs_after_checkout_and_is_not_reverted() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&origin)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("VERSION"), "1.0.0\n").unwrap();
        git(&["add", "VERSION"]);
        git(&["commit", "-q", "-m", "v1"]);
        let sha = git(&["rev-parse", "HEAD"]);

        let work = dir.path().join("work");
        let executor = ShellExecutor::new(Some(work.to_string_lossy().into_owned()));
        let j = job(serde_json::json!({
            "id": 8, "token": "t",
            "git_info": {
                "repo_url": format!("file://{}", origin.display()), "ref": "main",
                "ref_type": "branch", "sha": sha, "before_sha": "",
                "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]
            },
            "steps": steps(&["echo version=$(cat VERSION)"], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, 8, "t", &scrubber);

        let outcome = executor
            .execute(&j, &mut trace, &BumpVersion(executor.job_dir(8)))
            .await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("version=2.0.0"), "{}", log);
    }

    #[tokio::test]
    async fn get_sources_attempts_retry_a_failing_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 9, "token": "t",
            "git_info": {
                "repo_url": format!("file://{}/missing.git", dir.path().display()),
                "ref": "main", "ref_type": "branch", "sha": "d".repeat(40),
                "before_sha": "", "refspecs": []
            },
            "variables": [{"key": "GET_SOURCES_ATTEMPTS", "value": "3"}],
            "steps": steps(&["echo never"], &[])
        }));

        let (outcome, log) = run_shell(&j, dir.path()).await;

        assert_eq!(outcome.unwrap_err().reason, FailureReason::ScriptFailure);
        assert!(log.contains("(attempt 1/3), retrying"), "{}", log);
        assert!(log.contains("(attempt 2/3), retrying"), "{}", log);
        assert!(!log.contains("attempt 3/3"), "{}", log);
    }

    #[tokio::test]
    async fn runner_script_timeout_limits_the_script() {
        let dir = tempfile::tempdir().unwrap();
        let job = job(serde_json::json!({
            "id": 1, "token": "t",
            "variables": [
                {"key": "RUNNER_SCRIPT_TIMEOUT", "value": "1s"},
                {"key": "RUNNER_AFTER_SCRIPT_TIMEOUT", "value": "bogus"}
            ],
            "steps": [
                {"name": "script", "script": ["sleep 5"], "when": "on_success", "timeout": 3600},
                {"name": "after_script", "script": ["echo after=$CI_JOB_STATUS"], "when": "always"}
            ]
        }));
        let (outcome, text) = run_shell(&job, dir.path()).await;

        assert_eq!(
            outcome.unwrap_err().reason,
            FailureReason::JobExecutionTimeout
        );
        assert!(text.contains("after=failed"), "{}", text);
        assert!(text.contains("Ignoring malformed RUNNER_AFTER_SCRIPT_TIMEOUT"));
    }

    #[tokio::test]
    async fn pre_get_sources_hook_and_debug_trace() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 12, "token": "t",
            "hooks": [{"name": "pre_get_sources_script", "script": ["echo hook-ran-first"]}],
            "variables": [{"key": "CI_DEBUG_TRACE", "value": "true"}],
            "steps": steps(&["X=traced-value"], &[])
        }));

        let (outcome, log) = run_shell(&j, dir.path()).await;

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        let hook = log.find("hook-ran-first").expect("hook output");
        let step = log.find("step_script").expect("step output");
        assert!(hook < step, "{}", log);
        assert!(log.contains("+ X=traced-value"), "{}", log);
    }

    #[tokio::test]
    async fn huge_stage_timeouts_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 16, "token": "t",
            "variables": [
                {"key": "RUNNER_SCRIPT_TIMEOUT", "value": "10000000000000000000s"},
                {"key": "RUNNER_AFTER_SCRIPT_TIMEOUT", "value": "10000000000000000000s"}
            ],
            "steps": steps(&["echo script-ran"], &["echo after-ran"])
        }));
        let (outcome, log) = run_shell(&j, dir.path()).await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("after-ran"), "{}", log);
    }

    #[tokio::test]
    async fn untracked_listing_ignores_the_jobs_git_config() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ExecutorType::Shell(ShellExecutor::new(Some(
            dir.path().to_string_lossy().into_owned(),
        )));
        let project = executor.job_dir(17).join("project");
        std::fs::create_dir_all(&project).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&project)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        let marker = dir.path().join("fsmonitor-ran");
        git(&[
            "config",
            "core.fsmonitor",
            &format!("touch {}", marker.display()),
        ]);
        std::fs::write(project.join("new.txt"), "n").unwrap();

        let j = job(serde_json::json!({"id": 17, "token": "t"}));
        let files = executor.untracked_files(&j).await.unwrap();

        assert!(files.contains(&"new.txt".to_string()), "{:?}", files);
        assert!(!marker.exists(), "the job's core.fsmonitor command ran");
    }

    #[tokio::test]
    async fn git_clone_path_moves_the_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 14, "token": "t",
            "variables": [
                {"key": "CI_PROJECT_PATH", "value": "group/app"},
                {"key": "GIT_CLONE_PATH", "value": "$CI_BUILDS_DIR/$CI_PROJECT_PATH"}
            ],
            "steps": steps(&["echo \"dir=$PWD project=${CI_PROJECT_DIR#$CI_BUILDS_DIR/}\""], &[])
        }));
        let (outcome, log) = run_shell(&j, dir.path()).await;
        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(
            log.contains("/job-14/group/app project=group/app"),
            "{}",
            log
        );

        let bad = job(serde_json::json!({
            "id": 15, "token": "t",
            "variables": [{"key": "GIT_CLONE_PATH", "value": "/etc"}],
            "steps": steps(&["echo never"], &[])
        }));
        let (outcome, log) = run_shell(&bad, dir.path()).await;
        assert!(outcome.is_err(), "{}", log);
        assert!(!log.contains("\nnever"), "{}", log);
    }

    #[tokio::test]
    async fn allow_failure_step_does_not_fail_job() {
        let dir = tempfile::tempdir().unwrap();
        let j = job(serde_json::json!({
            "id": 4, "token": "t",
            "steps": [{"name": "script", "script": ["exit 1"], "when": "on_success", "allow_failure": true}]
        }));

        let (outcome, _) = run_shell(&j, dir.path()).await;

        assert!(outcome.is_ok());
    }

    #[test]
    fn utf8_decoder_keeps_characters_split_across_chunks() {
        let text = "ok ✓ done";
        let bytes = text.as_bytes();
        let split = text.find('✓').unwrap() + 1;
        let mut decoder = Utf8Decoder::default();

        let mut out = decoder.decode(&bytes[..split]);
        out.push_str(&decoder.decode(&bytes[split..]));
        out.push_str(&decoder.finish());

        assert_eq!(out, text);
    }
}
