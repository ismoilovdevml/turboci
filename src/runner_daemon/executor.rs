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

use super::cancel::CancelSignal;
use super::config::DockerConfig;
use super::git::{self, GitStrategy};
use super::image::{self, PullPolicy};
use super::script::{self, JobEnv};
use super::trace::TraceWriter;
use crate::gitlab::{FailureReason, Job, RemoteState};

/// Used when GitLab sends no job timeout
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(3600);
/// GitLab's default after_script timeout
const DEFAULT_AFTER_SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Runs the script (passed as `$1`) with bash when the image or host has it, else
/// sh, like gitlab-runner: on Debian-based images `sh` is dash, which lacks
/// `[[ ]]`, `source` and arrays
const SHELL_DETECT: &str =
    r#"if command -v bash >/dev/null 2>&1; then exec bash -c "$1"; fi; exec sh -c "$1""#;

/// Upper bound for resetting workspace ownership after a job
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(120);
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
    let mut failure = match get_sources(job, sources, trace, dirs, &script_limits).await {
        Ok(()) => None,
        Err(RunError::TimedOut) => Some(timed_out()),
        Err(RunError::Canceled) => Some(canceled()),
        Err(RunError::Failed(f)) => Some(f),
    };

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
        let limits = if is_after_script {
            trace.write("\nRunning after_script\n").await;
            let secs = u64::from(step.timeout);
            Limits {
                deadline: Instant::now()
                    + if secs > 0 {
                        Duration::from_secs(secs)
                    } else {
                        DEFAULT_AFTER_SCRIPT_TIMEOUT
                    },
                cancel: &cancel,
                stop_at: RemoteState::Aborted,
            }
        } else {
            trace
                .write(&format!(
                    "\nExecuting \"step_{}\" stage of the job script\n",
                    step.name
                ))
                .await;
            Limits {
                deadline,
                cancel: &cancel,
                stop_at: RemoteState::Canceling,
            }
        };

        let result = runner
            .run(&script::step_script(&lines), &dirs.project, trace, &limits)
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
    }

    failure.map_or(Ok(()), Err)
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
        .run(&script::argv_script(&commands), &dirs.builds, trace, limits)
        .await
    {
        Ok(RunStatus::Exited(0)) => Ok(()),
        Ok(RunStatus::Exited(code)) => Err(RunError::Failed(JobFailure {
            reason: FailureReason::ScriptFailure,
            exit_code: Some(code),
            message: format!("getting sources failed with exit code {}", code),
        })),
        Ok(RunStatus::TimedOut) => Err(RunError::TimedOut),
        Ok(RunStatus::Canceled) => Err(RunError::Canceled),
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
    config: DockerConfig,
}

impl std::fmt::Debug for DockerExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerExecutor")
            .field("config", &self.config)
            .finish()
    }
}

/// Containers and network created for one job, removed when it ends
#[derive(Default)]
struct JobContainers {
    network: Option<String>,
    /// Checks out sources, so job images do not need git (like gitlab-runner's helper)
    helper: Option<String>,
    services: Vec<String>,
    job: Option<String>,
}

impl DockerExecutor {
    pub fn new(config: DockerConfig) -> Result<Self> {
        let docker =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;
        config.memory_bytes()?;

        Ok(Self {
            docker: Arc::new(docker),
            config,
        })
    }

    async fn execute(&self, job: &Job, job_dir: &Path, trace: &mut TraceWriter<'_>) -> JobOutcome {
        let mut containers = JobContainers::default();
        let outcome = self
            .start_and_run(job, job_dir, trace, &mut containers)
            .await;
        // Always runs, including after failures, timeouts and cancellation
        self.release(job.id, &containers, job_dir).await;
        outcome
    }

    async fn start_and_run(
        &self,
        job: &Job,
        job_dir: &Path,
        trace: &mut TraceWriter<'_>,
        containers: &mut JobContainers,
    ) -> JobOutcome {
        let env = script::job_env(job, "/builds", job_dir);
        // Image and service names may use variables, e.g. $CI_REGISTRY_IMAGE/ci
        let values: std::collections::HashMap<String, String> = env.vars.iter().cloned().collect();
        let expand = |name: &str| script::expand(name, &values);

        let (image, policies) = match &job.image {
            Some(img) => (expand(&img.name), img.pull_policy.clone()),
            None => (self.config.default_image.clone(), Vec::new()),
        };
        trace
            .write(&format!("Using Docker executor with image {} ...\n", image))
            .await;
        info!("🐳 Using Docker image: {}", image);
        self.ensure_image(&image, Some(&policies), job, trace)
            .await?;

        write_variable_files(&env)
            .await
            .map_err(JobFailure::system)?;
        tokio::fs::create_dir_all(job_dir.join("project"))
            .await
            .map_err(|e| JobFailure::system(format!("Failed to create host workspace: {}", e)))?;

        if job.git_info.is_some() && git::strategy(&job.variables) != GitStrategy::None {
            let helper = self.config.helper_image.clone();
            self.ensure_image(&helper, None, job, trace).await?;
            let id = self
                .start_helper(job, job_dir, &env)
                .await
                .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
            containers.helper = Some(id);
        }

        // Services need a network of their own to be reachable by alias
        if !job.services.is_empty() {
            let network = format!("turboci-job-{}", job.id);
            self.create_network(&network)
                .await
                .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
            containers.network = Some(network);
        }
        for (index, service) in job.services.iter().enumerate() {
            let service_image = expand(&service.name);
            trace
                .write(&format!("Starting service {} ...\n", service_image))
                .await;
            self.ensure_image(&service_image, Some(&service.pull_policy), job, trace)
                .await?;
            let id = self
                .start_service(
                    job,
                    index,
                    &service_image,
                    service,
                    &env,
                    containers.network.as_deref(),
                )
                .await
                .map_err(|e| JobFailure::system(format!("service {}: {:#}", service.name, e)))?;
            containers.services.push(id);
        }

        let name = format!("turboci-job-{}", job.id);
        let config = ContainerCreateBody {
            image: Some(image.clone()),
            working_dir: Some("/builds".to_string()),
            env: Some(env.to_docker()),
            // `image: {entrypoint: [""]}` clears an entrypoint that is not a shell
            entrypoint: job.image.as_ref().and_then(|img| img.entrypoint.clone()),
            // Keep the container alive for the whole job; steps run as execs
            cmd: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "while :; do sleep 3600; done".to_string(),
            ]),
            host_config: Some(
                self.host_config(
                    std::iter::once(format!("{}:/builds", job_dir.display()))
                        .chain(self.config.volumes.iter().cloned())
                        .collect(),
                    containers.network.as_deref(),
                ),
            ),
            ..Default::default()
        };
        let id = self
            .create_named(&name, config)
            .await
            .map_err(|e| JobFailure::system(format!("{:#}", e)))?;
        containers.job = Some(id.clone());
        info!("📦 Created container: {}", id);

        use bollard::container::StartContainerOptions;
        self.docker
            .start_container(&id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| JobFailure::system(format!("Failed to start container: {}", e)))?;

        let runner = DockerRunner {
            docker: &self.docker,
            container_id: &id,
            env: env.to_docker(),
        };
        let helper_id = containers.helper.clone().unwrap_or_else(|| id.clone());
        let sources = DockerRunner {
            docker: &self.docker,
            container_id: &helper_id,
            env: env.to_docker(),
        };
        let dirs = JobDirs {
            builds: "/builds".to_string(),
            project: "/builds/project".to_string(),
        };
        run_job_steps(job, &sources, &runner, trace, &dirs).await
    }

    /// Start the container that checks out sources, with the workspace mounted
    async fn start_helper(&self, job: &Job, job_dir: &Path, env: &JobEnv) -> Result<String> {
        let config = ContainerCreateBody {
            image: Some(self.config.helper_image.clone()),
            working_dir: Some("/builds".to_string()),
            env: Some(env.to_docker()),
            // Helper images may have their own entrypoint (alpine/git's is `git`)
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            cmd: Some(vec!["while :; do sleep 3600; done".to_string()]),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{}:/builds", job_dir.display())]),
                network_mode: Some(self.config.network_mode.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let id = self
            .create_named(&format!("turboci-job-{}-sources", job.id), config)
            .await?;
        use bollard::container::StartContainerOptions;
        self.docker
            .start_container(&id, None::<StartContainerOptions<String>>)
            .await
            .context("Failed to start helper container")?;
        Ok(id)
    }

    /// Host settings shared by job and service containers
    fn host_config(&self, binds: Vec<String>, network: Option<&str>) -> HostConfig {
        HostConfig {
            binds: (!binds.is_empty()).then_some(binds),
            network_mode: Some(
                network
                    .map(str::to_string)
                    .unwrap_or_else(|| self.config.network_mode.clone()),
            ),
            privileged: Some(self.config.privileged),
            // Validated when the executor is created
            memory: self.config.memory_bytes().ok().flatten(),
            nano_cpus: self.config.cpus.map(|cpus| (cpus * 1e9) as i64),
            ..Default::default()
        }
    }

    /// Make `image` available according to the pull policy, using the registry
    /// credentials GitLab sent with the job
    /// `job_policies` is `None` for the runner's own helper image, which is pulled
    /// only when missing
    async fn ensure_image(
        &self,
        image: &str,
        job_policies: Option<&[String]>,
        job: &Job,
        trace: &mut TraceWriter<'_>,
    ) -> JobOutcome {
        let pull_failure = |message: String| JobFailure {
            reason: FailureReason::ImagePullFailure,
            exit_code: None,
            message,
        };
        let reference = image::with_default_tag(image);
        let present = self.docker.inspect_image(&reference).await.is_ok();
        let policy = match job_policies {
            None => PullPolicy::IfNotPresent,
            Some(policies) => PullPolicy::resolve(
                policies,
                &self.config.pull_policy,
                &self.config.allowed_pull_policies,
            )
            .map_err(pull_failure)?,
        };
        match policy {
            PullPolicy::Never if present => return Ok(()),
            PullPolicy::Never => {
                return Err(pull_failure(format!(
                    "image {} is not present and pull_policy is never",
                    image
                )))
            }
            PullPolicy::IfNotPresent if present => {
                trace
                    .write(&format!("Using locally found image {}\n", image))
                    .await;
                return Ok(());
            }
            _ => {}
        }

        trace.write(&format!("Pulling image {} ...\n", image)).await;
        let credentials = image::credentials_for(image, &job.credentials).map(|c| {
            bollard::auth::DockerCredentials {
                username: Some(c.username.clone()),
                password: Some(c.password.clone()),
                serveraddress: Some(image::registry(image).to_string()),
                ..Default::default()
            }
        });
        use bollard::image::CreateImageOptions;
        let options = Some(CreateImageOptions {
            from_image: reference.as_str(),
            ..Default::default()
        });
        let mut stream = self.docker.create_image(options, None, credentials);
        while let Some(progress) = stream.next().await {
            if let Err(e) = progress {
                return Err(pull_failure(format!(
                    "failed to pull image {}: {}",
                    image, e
                )));
            }
        }
        Ok(())
    }

    /// Create a bridge network for the job (replacing a leftover one)
    async fn create_network(&self, name: &str) -> Result<()> {
        let _ = self.docker.remove_network(name).await;
        self.docker
            .create_network(bollard::models::NetworkCreateRequest {
                name: name.to_string(),
                driver: Some("bridge".to_string()),
                ..Default::default()
            })
            .await
            .with_context(|| format!("Failed to create network {}", name))?;
        Ok(())
    }

    /// Start a service container reachable by its aliases on the job network
    async fn start_service(
        &self,
        job: &Job,
        index: usize,
        image: &str,
        service: &crate::gitlab::Service,
        env: &JobEnv,
        network: Option<&str>,
    ) -> Result<String> {
        use bollard::models::{EndpointSettings, NetworkingConfig};

        let aliases = image::service_aliases(image, service.alias.as_deref());
        let networking_config = network.map(|network| NetworkingConfig {
            endpoints_config: Some(std::collections::HashMap::from([(
                network.to_string(),
                EndpointSettings {
                    aliases: Some(aliases),
                    ..Default::default()
                },
            )])),
        });
        let config = ContainerCreateBody {
            image: Some(image.to_string()),
            env: Some(env.to_docker()),
            entrypoint: service.entrypoint.clone(),
            cmd: service.command.clone(),
            host_config: Some(self.host_config(Vec::new(), network)),
            networking_config,
            ..Default::default()
        };
        let id = self
            .create_named(&format!("turboci-job-{}-svc-{}", job.id, index), config)
            .await?;

        use bollard::container::StartContainerOptions;
        self.docker
            .start_container(&id, None::<StartContainerOptions<String>>)
            .await
            .context("Failed to start service container")?;
        Ok(id)
    }

    /// Create a container under a fixed name, replacing a leftover one
    async fn create_named(&self, name: &str, config: ContainerCreateBody) -> Result<String> {
        use bollard::container::{CreateContainerOptions, RemoveContainerOptions};

        let _ = self
            .docker
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await;
        let response = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: name.to_string(),
                    ..Default::default()
                }),
                config,
            )
            .await
            .context("Failed to create container")?;
        Ok(response.id)
    }

    /// Remove the job's containers and network, then hand the workspace back to
    /// the runner's user (files created in containers belong to root)
    async fn release(&self, job_id: u64, containers: &JobContainers, job_dir: &Path) {
        // Removing the containers first stops everything the job started, so
        // nothing the job controls runs during cleanup
        for id in containers
            .job
            .iter()
            .chain(&containers.helper)
            .chain(&containers.services)
        {
            match self.force_remove(id).await {
                Ok(()) => info!("🗑️  Removed container: {}", id),
                Err(e) => warn!("Failed to remove container {}: {}", id, e),
            }
        }
        if let Some(network) = &containers.network {
            if let Err(e) = self.docker.remove_network(network).await {
                warn!("Failed to remove network {}: {}", network, e);
            }
        }

        if containers.job.is_some() || containers.helper.is_some() {
            if let Err(e) = self.reset_ownership(job_id, job_dir).await {
                warn!("Failed to reset workspace ownership: {:#}", e);
            }
        }
    }

    async fn force_remove(&self, id: &str) -> Result<()> {
        use bollard::container::RemoveContainerOptions;

        self.docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await?;
        Ok(())
    }

    /// `chown -R` the workspace to the runner's uid/gid from a fresh container of
    /// the (trusted) helper image, never from the job's own image
    async fn reset_ownership(&self, job_id: u64, job_dir: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use bollard::container::{StartContainerOptions, WaitContainerOptions};
            use std::os::unix::fs::MetadataExt;

            let meta = std::fs::metadata(job_dir)?;
            let config = ContainerCreateBody {
                image: Some(self.config.helper_image.clone()),
                user: Some("0".to_string()),
                entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
                cmd: Some(vec![format!(
                    "chown -R {}:{} /builds",
                    meta.uid(),
                    meta.gid()
                )]),
                network_disabled: Some(true),
                host_config: Some(HostConfig {
                    binds: Some(vec![format!("{}:/builds", job_dir.display())]),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let id = self
                .create_named(&format!("turboci-job-{}-cleanup", job_id), config)
                .await?;
            let result = async {
                self.docker
                    .start_container(&id, None::<StartContainerOptions<String>>)
                    .await?;
                let mut wait = self
                    .docker
                    .wait_container(&id, None::<WaitContainerOptions<String>>);
                tokio::time::timeout(CLEANUP_TIMEOUT, async {
                    while let Some(status) = wait.next().await {
                        status?;
                    }
                    anyhow::Ok(())
                })
                .await
                .context("chown timed out")?
            }
            .await;
            let _ = self.force_remove(&id).await;
            result?;
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
        limits: &Limits<'_>,
    ) -> Result<RunStatus> {
        let exec = self
            .docker
            .create_exec(
                self.container_id,
                CreateExecOptions {
                    cmd: Some(vec!["sh", "-c", SHELL_DETECT, "sh", script]),
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
                // On timeout or cancel the exec keeps running until the container is removed
                let next = tokio::select! {
                    next = tokio::time::timeout_at(limits.deadline, output.next()) => next,
                    _ = limits.cancel.reached(limits.stop_at) => return Ok(RunStatus::Canceled),
                };
                let chunk = match next {
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
        run_job_steps(job, &runner, &runner, trace, &dirs).await
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
        limits: &Limits<'_>,
    ) -> Result<RunStatus> {
        let mut command = Command::new("sh");
        command
            .args(["-c", SHELL_DETECT, "sh", script])
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
                _ = tokio::time::sleep_until(limits.deadline) => {
                    kill_process_group(pid);
                    let _ = child.wait().await;
                    return Ok(RunStatus::TimedOut);
                }
                _ = limits.cancel.reached(limits.stop_at) => {
                    kill_process_group(pid);
                    let _ = child.wait().await;
                    return Ok(RunStatus::Canceled);
                }
            }
        }
        trace.write(&stdout.finish()).await;
        trace.write(&stderr.finish()).await;

        let status = tokio::select! {
            status = child.wait() => status?,
            _ = tokio::time::sleep_until(limits.deadline) => {
                kill_process_group(pid);
                let _ = child.wait().await;
                return Ok(RunStatus::TimedOut);
            }
            _ = limits.cancel.reached(limits.stop_at) => {
                kill_process_group(pid);
                let _ = child.wait().await;
                return Ok(RunStatus::Canceled);
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

    async fn run_shell_canceled(
        job: &Job,
        work_dir: &Path,
        state: RemoteState,
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
        let outcome = executor.execute(job, &mut trace).await;
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
        let (outcome, log) = run_shell_canceled(&j, dir.path(), RemoteState::Canceling).await;

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

        let (outcome, log) = run_shell_canceled(&j, dir.path(), RemoteState::Aborted).await;

        assert_eq!(outcome.unwrap_err().reason, FailureReason::JobCanceled);
        assert!(!log.contains("cleanup-ran"), "{}", log);
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
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                default_image: "alpine:3.20".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
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

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test --all-features -- --ignored"]
    async fn docker_executor_runs_services_with_limits() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                memory: Some("256m".to_string()),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 910_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "redis:7-alpine", "pull_policy": ["if-not-present"]},
            "services": [{"name": "redis:7-alpine", "alias": "cache"}],
            "steps": steps(
                &[
                    "for i in $(seq 1 20); do redis-cli -h redis ping >/dev/null 2>&1 && break; sleep 0.5; done",
                    "echo \"by-image $(redis-cli -h redis ping)\"",
                    "echo \"by-alias $(redis-cli -h cache ping)\"",
                    "echo \"memory $(cat /sys/fs/cgroup/memory.max 2>/dev/null || cat /sys/fs/cgroup/memory/memory.limit_in_bytes)\""
                ],
                &[]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("by-image PONG"), "{}", log);
        assert!(log.contains("by-alias PONG"), "{}", log);
        assert!(log.contains("memory 268435456"), "{}", log);

        // Job container, service container and job network are all gone
        let docker = Docker::connect_with_local_defaults().unwrap();
        for name in [
            format!("turboci-job-{}", job_id),
            format!("turboci-job-{}-svc-0", job_id),
        ] {
            assert!(docker
                .inspect_container(
                    &name,
                    None::<bollard::query_parameters::InspectContainerOptions>
                )
                .await
                .is_err());
        }
        assert!(docker
            .inspect_network(
                &format!("turboci-job-{}", job_id),
                None::<bollard::query_parameters::InspectNetworkOptions>
            )
            .await
            .is_err());
        executor.cleanup(job_id).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test --all-features -- --ignored"]
    async fn docker_executor_checks_out_sources_without_git_in_job_image() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 920_000_000 + u64::from(std::process::id());
        let job_dir = executor.job_dir(job_id);

        // Origin repository inside the workspace, visible to containers as /builds/origin.git
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
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
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let work = job_dir.join("origin-work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "hello from git\n").unwrap();
        git(&work, &["add", "file.txt"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(
            &job_dir,
            &["clone", "-q", "--bare", "origin-work", "origin.git"],
        );

        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "git_info": {
                "repo_url": "file:///builds/origin.git", "ref": "main", "ref_type": "branch",
                "sha": sha, "before_sha": "", "refspecs": ["+refs/heads/main:refs/remotes/origin/main"]
            },
            // The origin repo belongs to the host user, not root in the helper
            "variables": [
                {"key": "GIT_CONFIG_COUNT", "value": "1"},
                {"key": "GIT_CONFIG_KEY_0", "value": "safe.directory"},
                {"key": "GIT_CONFIG_VALUE_0", "value": "*"}
            ],
            "steps": steps(&["cat file.txt", "command -v git || echo no-git-in-job-image"], &[])
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("hello from git"), "{}", log);
        assert!(log.contains("no-git-in-job-image"), "{}", log);
        let docker = Docker::connect_with_local_defaults().unwrap();
        assert!(docker
            .inspect_container(
                &format!("turboci-job-{}-sources", job_id),
                None::<bollard::query_parameters::InspectContainerOptions>
            )
            .await
            .is_err());
        executor.cleanup(job_id).await;
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test --all-features -- --ignored"]
    async fn docker_job_replacing_sh_cannot_hang_cleanup() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 930_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "alpine:3.20"},
            "steps": [{"name": "script", "when": "on_success", "timeout": 60, "script": [
                "mkdir -p out && echo x > out/root-owned",
                "rm /bin/sh && printf '#!/bin/busybox ash\\nexec sleep 100000\\n' > /bin/sh && chmod +x /bin/sh"
            ]}]
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome =
            tokio::time::timeout(Duration::from_secs(90), executor.execute(&j, &mut trace))
                .await
                .expect("cleanup hung on the job's /bin/sh");
        trace.finish().await;

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, trace.text());
        executor.cleanup(job_id).await;
        assert!(
            !executor.job_dir(job_id).exists(),
            "workspace not removable"
        );
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: cargo test --all-features -- --ignored"]
    async fn docker_uses_bash_and_expands_image_variables() {
        let executor = ExecutorType::Docker(
            DockerExecutor::new(DockerConfig {
                pull_policy: "if-not-present".to_string(),
                ..DockerConfig::default()
            })
            .unwrap(),
        );
        let job_id = 940_000_000 + u64::from(std::process::id());
        let j = job(serde_json::json!({
            "id": job_id, "token": "t",
            "image": {"name": "${BASE_IMAGE}"},
            "variables": [{"key": "BASE_IMAGE", "value": "debian:bookworm-slim"}],
            "steps": steps(
                &["[[ 1 == 1 ]] && echo \"bash-syntax-ok\"", "arr=(a b); echo \"array=${arr[1]}\""],
                &[]
            )
        }));
        let scrubber = SecretScrubber::new(vec![]);
        let mut trace = TraceWriter::new(None, job_id, "t", &scrubber);

        let outcome = executor.execute(&j, &mut trace).await;
        trace.finish().await;
        let log = trace.text();

        assert!(outcome.is_ok(), "{:?}\n{}", outcome, log);
        assert!(log.contains("image debian:bookworm-slim"), "{}", log);
        assert!(log.contains("bash-syntax-ok"), "{}", log);
        assert!(log.contains("array=b"), "{}", log);
        executor.cleanup(job_id).await;
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
