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
use bollard::Docker;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;
use tracing::{info, warn};

use super::git::{self, GitStrategy};
use super::script::{self, JobEnv};
use super::trace::TraceWriter;
use crate::gitlab::{FailureReason, Job};

/// Used when GitLab sends no job timeout
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(3600);
/// GitLab's default after_script timeout
const DEFAULT_AFTER_SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
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
    pub async fn execute(&self, job: &Job, trace: &mut TraceWriter<'_>) -> JobOutcome {
        match self {
            ExecutorType::Docker(executor) => {
                executor.execute(job, &self.job_dir(job.id), trace).await
            }
            ExecutorType::Shell(executor) => executor.execute(job, trace).await,
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

/// Result of running one script
#[derive(Debug, PartialEq, Eq)]
enum RunStatus {
    Exited(i32),
    TimedOut,
}

/// How an executor runs a shell script in a directory, streaming output to the trace
#[async_trait]
trait ScriptRunner: Send + Sync {
    async fn run(
        &self,
        script: &str,
        workdir: &str,
        trace: &mut TraceWriter<'_>,
        deadline: Instant,
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
    runner: &dyn ScriptRunner,
    trace: &mut TraceWriter<'_>,
    dirs: &JobDirs,
) -> JobOutcome {
    let timeout = job
        .timeout_secs()
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_JOB_TIMEOUT);
    let deadline = Instant::now() + timeout;
    let timed_out = || JobFailure {
        reason: FailureReason::JobExecutionTimeout,
        exit_code: None,
        message: format!("execution took longer than {}s", timeout.as_secs()),
    };

    let mut failure = match get_sources(job, runner, trace, dirs, deadline).await {
        Ok(()) => None,
        Err(RunError::TimedOut) => Some(timed_out()),
        Err(RunError::Failed(f)) => Some(f),
    };

    for step in &job.steps {
        let is_after_script = step.name == "after_script";
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

        let step_deadline = if is_after_script {
            trace.write("\nRunning after_script\n").await;
            let secs = u64::from(step.timeout);
            Instant::now()
                + if secs > 0 {
                    Duration::from_secs(secs)
                } else {
                    DEFAULT_AFTER_SCRIPT_TIMEOUT
                }
        } else {
            trace
                .write(&format!(
                    "\nExecuting \"step_{}\" stage of the job script\n",
                    step.name
                ))
                .await;
            deadline
        };

        let result = runner
            .run(
                &script::step_script(&lines),
                &dirs.project,
                trace,
                step_deadline,
            )
            .await;
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
            Err(e) if is_after_script => {
                trace
                    .write(&format!("WARNING: after_script could not run: {}\n", e))
                    .await;
            }
            Err(e) => {
                failure.get_or_insert(JobFailure::system(format!("{:#}", e)));
            }
        }
    }

    failure.map_or(Ok(()), Err)
}

enum RunError {
    TimedOut,
    Failed(JobFailure),
}

async fn get_sources(
    job: &Job,
    runner: &dyn ScriptRunner,
    trace: &mut TraceWriter<'_>,
    dirs: &JobDirs,
    deadline: Instant,
) -> std::result::Result<(), RunError> {
    let Some(ref git_info) = job.git_info else {
        return Ok(());
    };

    let commands = match git::strategy(&job.variables) {
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
            &git_info.sha[..git_info.sha.len().min(8)],
            git_info.ref_name
        ))
        .await;

    match runner
        .run(
            &script::argv_script(&commands),
            &dirs.builds,
            trace,
            deadline,
        )
        .await
    {
        Ok(RunStatus::Exited(0)) => Ok(()),
        Ok(RunStatus::Exited(code)) => Err(RunError::Failed(JobFailure {
            reason: FailureReason::ScriptFailure,
            exit_code: Some(code),
            message: format!("getting sources failed with exit code {}", code),
        })),
        Ok(RunStatus::TimedOut) => Err(RunError::TimedOut),
        Err(e) => Err(RunError::Failed(JobFailure::system(format!("{:#}", e)))),
    }
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

#[derive(Clone)]
pub struct DockerExecutor {
    docker: Arc<Docker>,
    default_image: String,
}

impl std::fmt::Debug for DockerExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerExecutor")
            .field("default_image", &self.default_image)
            .finish()
    }
}

impl DockerExecutor {
    pub fn new(default_image: String) -> Result<Self> {
        let docker =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;

        Ok(Self {
            docker: Arc::new(docker),
            default_image,
        })
    }

    async fn execute(&self, job: &Job, job_dir: &Path, trace: &mut TraceWriter<'_>) -> JobOutcome {
        let image = job
            .image
            .as_ref()
            .map(|img| img.name.clone())
            .unwrap_or_else(|| self.default_image.clone());

        trace
            .write(&format!("Using Docker executor with image {} ...\n", image))
            .await;
        info!("🐳 Using Docker image: {}", image);

        if let Err(e) = self.pull_image(&image).await {
            return Err(JobFailure {
                reason: FailureReason::ImagePullFailure,
                exit_code: None,
                message: format!("failed to pull image {}: {}", image, e),
            });
        }

        let env = script::job_env(job, "/builds", job_dir);
        write_variable_files(&env)
            .await
            .map_err(JobFailure::system)?;

        let container_id = self
            .create_container(job, &image, job_dir, &env)
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
        info!("📦 Created container: {}", container_id);

        use bollard::container::StartContainerOptions;
        let outcome = match self
            .docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
        {
            Ok(()) => {
                let runner = DockerRunner {
                    docker: &self.docker,
                    container_id: &container_id,
                    env: env.to_docker(),
                };
                let dirs = JobDirs {
                    builds: "/builds".to_string(),
                    project: "/builds/project".to_string(),
                };
                run_job_steps(job, &runner, trace, &dirs).await
            }
            Err(e) => Err(JobFailure::system(format!(
                "Failed to start container: {}",
                e
            ))),
        };

        // Always runs, including after failures and timeouts
        self.release_container(&container_id, job_dir).await;
        outcome
    }

    /// Pull Docker image
    async fn pull_image(&self, image: &str) -> Result<()> {
        use bollard::image::CreateImageOptions;

        let options = Some(CreateImageOptions {
            from_image: image,
            ..Default::default()
        });

        let mut stream = self.docker.create_image(options, None, None);

        while let Some(info) = stream.next().await {
            info?;
        }

        Ok(())
    }

    /// Create the job container with the workspace mounted at /builds
    async fn create_container(
        &self,
        job: &Job,
        image: &str,
        job_dir: &Path,
        env: &JobEnv,
    ) -> Result<String> {
        use bollard::container::CreateContainerOptions;
        use bollard::models::{ContainerCreateBody, HostConfig};

        let options = CreateContainerOptions {
            name: format!("turboci-job-{}", job.id),
            ..Default::default()
        };

        tokio::fs::create_dir_all(job_dir.join("project"))
            .await
            .context("Failed to create host workspace")?;

        let config = ContainerCreateBody {
            image: Some(image.to_string()),
            working_dir: Some("/builds".to_string()),
            env: Some(env.to_docker()),
            // Keep the container alive for the whole job; steps run as execs
            cmd: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "while :; do sleep 3600; done".to_string(),
            ]),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{}:/builds", job_dir.display())]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let response = self
            .docker
            .create_container(Some(options), config)
            .await
            .context("Failed to create container")?;

        Ok(response.id)
    }

    /// Hand the workspace back to the runner's user (files created in the
    /// container belong to root) and remove the container
    async fn release_container(&self, container_id: &str, job_dir: &Path) {
        use bollard::container::RemoveContainerOptions;

        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(job_dir) {
            use std::os::unix::fs::MetadataExt;
            let chown = format!("chown -R {}:{} /builds", meta.uid(), meta.gid());
            if let Err(e) = self.exec_as_root(container_id, &chown).await {
                warn!("Failed to reset workspace ownership: {}", e);
            }
        }

        if let Err(e) = self
            .docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await
        {
            warn!("Failed to remove container {}: {}", container_id, e);
        } else {
            info!("🗑️  Removed container: {}", container_id);
        }
    }

    async fn exec_as_root(&self, container_id: &str, command: &str) -> Result<()> {
        let exec = self
            .docker
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(vec!["sh", "-c", command]),
                    user: Some("0"),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await?;
        if let StartExecResults::Attached { mut output, .. } =
            self.docker.start_exec(&exec.id, None).await?
        {
            while output.next().await.is_some() {}
        }
        Ok(())
    }
}

struct DockerRunner<'a> {
    docker: &'a Docker,
    container_id: &'a str,
    env: Vec<String>,
}

#[async_trait]
impl ScriptRunner for DockerRunner<'_> {
    async fn run(
        &self,
        script: &str,
        workdir: &str,
        trace: &mut TraceWriter<'_>,
        deadline: Instant,
    ) -> Result<RunStatus> {
        let exec = self
            .docker
            .create_exec(
                self.container_id,
                CreateExecOptions {
                    cmd: Some(vec!["sh", "-c", script]),
                    env: Some(self.env.iter().map(String::as_str).collect()),
                    working_dir: Some(workdir),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create exec")?;

        let mut stdout = Utf8Decoder::default();
        let mut stderr = Utf8Decoder::default();
        if let StartExecResults::Attached { mut output, .. } =
            self.docker.start_exec(&exec.id, None).await?
        {
            loop {
                let chunk = match tokio::time::timeout_at(deadline, output.next()).await {
                    // The exec keeps running until the container is removed
                    Err(_) => return Ok(RunStatus::TimedOut),
                    Ok(None) => break,
                    Ok(Some(chunk)) => chunk?,
                };
                let text = match chunk {
                    LogOutput::StdErr { message } => stderr.decode(&message),
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        stdout.decode(&message)
                    }
                    LogOutput::StdIn { .. } => continue,
                };
                trace.write(&text).await;
            }
        }
        trace.write(&stdout.finish()).await;
        trace.write(&stderr.finish()).await;

        let inspect = self.docker.inspect_exec(&exec.id).await?;
        let code = inspect.exit_code.unwrap_or(-1);
        Ok(RunStatus::Exited(i32::try_from(code).unwrap_or(-1)))
    }
}

/// Shell Executor - Runs jobs directly on host
#[derive(Clone, Debug)]
pub struct ShellExecutor {
    work_dir: String,
}

impl ShellExecutor {
    pub fn new(work_dir: Option<String>) -> Self {
        Self {
            work_dir: work_dir.unwrap_or_else(|| "/tmp/turboci".to_string()),
        }
    }

    fn job_dir(&self, job_id: u64) -> PathBuf {
        Path::new(&self.work_dir).join(format!("job-{}", job_id))
    }

    async fn execute(&self, job: &Job, trace: &mut TraceWriter<'_>) -> JobOutcome {
        trace.write("Using Shell executor...\n").await;

        let job_dir = self.job_dir(job.id);
        let dirs = JobDirs {
            builds: job_dir.to_string_lossy().into_owned(),
            project: job_dir.join("project").to_string_lossy().into_owned(),
        };
        tokio::fs::create_dir_all(&dirs.project)
            .await
            .map_err(JobFailure::system)?;

        let env = script::job_env(job, &dirs.builds, &job_dir);
        write_variable_files(&env)
            .await
            .map_err(JobFailure::system)?;

        let runner = ShellRunner { env: env.vars };
        run_job_steps(job, &runner, trace, &dirs).await
    }
}

struct ShellRunner {
    env: Vec<(String, String)>,
}

#[async_trait]
impl ScriptRunner for ShellRunner {
    async fn run(
        &self,
        script: &str,
        workdir: &str,
        trace: &mut TraceWriter<'_>,
        deadline: Instant,
    ) -> Result<RunStatus> {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .current_dir(workdir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group, so a timeout kills everything the script started
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().context("Failed to start sh")?;
        let pid = child.id();
        let mut out = child.stdout.take().context("stdout not captured")?;
        let mut err = child.stderr.take().context("stderr not captured")?;
        let (mut out_buf, mut err_buf) = ([0u8; 8192], [0u8; 8192]);
        let (mut out_open, mut err_open) = (true, true);
        let (mut stdout, mut stderr) = (Utf8Decoder::default(), Utf8Decoder::default());

        while out_open || err_open {
            tokio::select! {
                n = out.read(&mut out_buf), if out_open => match n? {
                    0 => out_open = false,
                    n => trace.write(&stdout.decode(&out_buf[..n])).await,
                },
                n = err.read(&mut err_buf), if err_open => match n? {
                    0 => err_open = false,
                    n => trace.write(&stderr.decode(&err_buf[..n])).await,
                },
                _ = tokio::time::sleep_until(deadline) => {
                    kill_process_group(pid);
                    let _ = child.wait().await;
                    return Ok(RunStatus::TimedOut);
                }
            }
        }
        trace.write(&stdout.finish()).await;
        trace.write(&stderr.finish()).await;

        let status = tokio::select! {
            status = child.wait() => status?,
            _ = tokio::time::sleep_until(deadline) => {
                kill_process_group(pid);
                let _ = child.wait().await;
                return Ok(RunStatus::TimedOut);
            }
        };
        Ok(RunStatus::Exited(status.code().unwrap_or(-1)))
    }
}

fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", pid)])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::secret_scrubber::SecretScrubber;

    fn job(value: serde_json::Value) -> Job {
        serde_json::from_value(value).unwrap()
    }

    async fn run_shell(job: &Job, work_dir: &Path) -> (JobOutcome, String) {
        let executor = ShellExecutor::new(Some(work_dir.to_string_lossy().into_owned()));
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(job);
        let mut trace = TraceWriter::new(None, job.id, &job.token, &scrubber);
        let outcome = executor.execute(job, &mut trace).await;
        trace.finish().await;
        (outcome, trace.text())
    }

    fn steps(script: &[&str], after: &[&str]) -> serde_json::Value {
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

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test --all-features -- --ignored"]
    async fn docker_executor_runs_job_and_cleans_up() {
        let executor =
            ExecutorType::Docker(DockerExecutor::new("alpine:3.20".to_string()).unwrap());
        let job_id = 900_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "variables": [
                {"key": "GREETING", "value": "hello"},
                {"key": "API_KEY", "value": "top-secret-key", "masked": true},
                {"key": "CONFIG", "value": "file-content", "file": true}
            ],
            "steps": steps(
                &[
                    "cd /tmp",
                    "echo \"$GREETING from $PWD as $(id -u)\"",
                    "echo key=$API_KEY",
                    "cat \"$CONFIG\"",
                    "mkdir -p \"$CI_PROJECT_DIR/out\" && echo artifact > \"$CI_PROJECT_DIR/out/a.txt\"",
                    "exit 4"
                ],
                &["echo after-ran"]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]).with_job_secrets(&j);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace).await;
        trace.finish().await;
        let log = trace.text();

        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, FailureReason::ScriptFailure, "{}", log);
        assert_eq!(failure.exit_code, Some(4), "{}", log);
        assert!(log.contains("hello from /tmp as 0"), "{}", log);
        assert!(log.contains("key=[MASKED]"), "{}", log);
        assert!(!log.contains("top-secret-key"));
        assert!(log.contains("file-content"), "{}", log);
        assert!(log.contains("after-ran"), "{}", log);

        // Files the container created as root are left for artifact upload...
        let job_dir = executor.job_dir(job_id);
        assert_eq!(
            std::fs::read_to_string(job_dir.join("project/out/a.txt")).unwrap(),
            "artifact\n"
        );
        // ...the container is gone...
        let docker = Docker::connect_with_local_defaults().unwrap();
        assert!(docker
            .inspect_container(
                &format!("turboci-job-{}", job_id),
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .is_err());
        // ...and the runner's user can delete the workspace
        executor.cleanup(job_id).await;
        assert!(!job_dir.exists());
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
