// Bollard 0.19 has deprecated old API, but new API is complex
// We'll migrate to new API in future version
#![allow(deprecated)]

use anyhow::{Context, Result};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::Docker;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::gitlab::{GitLabClient, Job};

/// Executor type enum
#[derive(Debug, Clone)]
pub enum ExecutorType {
    Docker(DockerExecutor),
    Shell(ShellExecutor),
}

impl ExecutorType {
    #[allow(dead_code)]
    pub async fn execute(&self, job: &Job) -> Result<String> {
        self.execute_with_streaming(job, None).await
    }

    pub async fn execute_with_streaming(
        &self,
        job: &Job,
        gitlab_client: Option<&GitLabClient>,
    ) -> Result<String> {
        // Default timeout: 1 hour per job
        let job_timeout = if job.timeout > 0 {
            Duration::from_secs(job.timeout as u64)
        } else {
            Duration::from_secs(3600) // 1 hour default
        };

        // Execute with timeout
        match timeout(job_timeout, async {
            match self {
                ExecutorType::Docker(executor) => {
                    executor.execute_with_streaming(job, gitlab_client).await
                }
                ExecutorType::Shell(executor) => executor.execute(job).await,
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                warn!("Job #{} timed out after {:?}", job.id, job_timeout);
                Err(anyhow::anyhow!(
                    "Job execution timed out after {} seconds",
                    job_timeout.as_secs()
                ))
            }
        }
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

    /// Execute a GitLab job in Docker container
    #[allow(dead_code)]
    pub async fn execute(&self, job: &Job) -> Result<String> {
        self.execute_with_streaming(job, None).await
    }

    /// Execute a GitLab job in Docker container with real-time trace streaming
    pub async fn execute_with_streaming(
        &self,
        job: &Job,
        gitlab_client: Option<&GitLabClient>,
    ) -> Result<String> {
        let image = job
            .image
            .as_ref()
            .map(|img| img.name.clone())
            .unwrap_or_else(|| self.default_image.clone());

        info!("🐳 Using Docker image: {}", image);

        // Pull image if not exists
        self.pull_image(&image).await?;

        // Create container
        let container_id = self.create_container(job, &image).await?;
        info!("📦 Created container: {}", container_id);

        // Start container
        use bollard::container::StartContainerOptions;
        self.docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
            .context("Failed to start container")?;

        let mut output = String::new();
        let mut trace_offset = 0;

        // Prepare streaming parameters
        let streaming_params = gitlab_client.map(|client| (client, job.id, job.token.as_str()));

        // Clone repository with streaming
        let clone_output = self
            .clone_repository_with_streaming(job, &container_id, streaming_params, trace_offset)
            .await?;
        output.push_str(&clone_output);
        trace_offset += clone_output.len();

        // Execute job steps with before_script/after_script
        for step in &job.steps {
            info!("  ▶️  Step: {}", step.name);

            // Execute before_script
            if !step.before_script.is_empty() {
                info!("    📋 Running before_script...");
                for script_line in &step.before_script {
                    let step_output = self
                        .exec_in_container_with_streaming(
                            &container_id,
                            script_line,
                            streaming_params,
                            trace_offset,
                        )
                        .await?;
                    trace_offset += step_output.len();
                    output.push_str(&step_output);
                    output.push('\n');
                    trace_offset += 1;
                }
            }

            // Execute main script
            for script_line in &step.script {
                let step_output = self
                    .exec_in_container_with_streaming(
                        &container_id,
                        script_line,
                        streaming_params,
                        trace_offset,
                    )
                    .await?;
                trace_offset += step_output.len();
                output.push_str(&step_output);
                output.push('\n');
                trace_offset += 1;
            }

            // Execute after_script (always run, even on failure)
            if !step.after_script.is_empty() {
                info!("    📋 Running after_script...");
                for script_line in &step.after_script {
                    if let Ok(step_output) = self
                        .exec_in_container_with_streaming(
                            &container_id,
                            script_line,
                            streaming_params,
                            trace_offset,
                        )
                        .await
                    {
                        trace_offset += step_output.len();
                        output.push_str(&step_output);
                        output.push('\n');
                        trace_offset += 1;
                    }
                }
            }
        }

        // Cleanup
        self.cleanup_container(&container_id, job.id).await?;

        Ok(output)
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
            match info {
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    /// Create Docker container
    async fn create_container(&self, job: &Job, image: &str) -> Result<String> {
        use bollard::container::CreateContainerOptions;
        use bollard::models::{ContainerCreateBody, HostConfig};

        let options = CreateContainerOptions {
            name: format!("turboci-job-{}", job.id),
            ..Default::default()
        };

        // Create workspace directory on host for volume mount
        let host_workspace = format!("/tmp/turboci-builds/job-{}", job.id);
        tokio::fs::create_dir_all(&host_workspace)
            .await
            .context("Failed to create host workspace")?;

        let config = ContainerCreateBody {
            image: Some(image.to_string()),
            working_dir: Some("/builds".to_string()),
            cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
            host_config: Some(HostConfig {
                binds: Some(vec![
                    format!("{}:/builds", host_workspace), // ✅ Mount workspace
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let response = self
            .docker
            .create_container(Some(options), config)
            .await
            .context("Failed to create container")?;

        info!("📁 Mounted host workspace: {} -> /builds", host_workspace);
        Ok(response.id)
    }

    /// Clone Git repository inside container
    #[allow(dead_code)]
    async fn clone_repository(&self, job: &Job, container_id: &str) -> Result<String> {
        if let Some(ref git_info) = job.git_info {
            // First, create the builds directory and cd into it
            let clone_cmd = format!(
                "cd /builds && git clone --depth 1 --branch {} {} project",
                git_info.ref_name, git_info.repo_url
            );

            // Execute git clone command from /builds directory (not /builds/project)
            let exec = self
                .docker
                .create_exec(
                    container_id,
                    CreateExecOptions {
                        cmd: Some(vec!["sh", "-c", &clone_cmd]),
                        attach_stdout: Some(true),
                        attach_stderr: Some(true),
                        working_dir: Some("/builds"), // Start from /builds, not /builds/project
                        ..Default::default()
                    },
                )
                .await
                .context("Failed to create exec for git clone")?;

            let mut output = String::new();

            if let StartExecResults::Attached {
                output: mut stream, ..
            } = self.docker.start_exec(&exec.id, None).await?
            {
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(msg) => {
                            let text = msg.to_string();
                            print!("{}", text);
                            output.push_str(&text);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }

            // Check exit code
            let inspect = self.docker.inspect_exec(&exec.id).await?;
            if let Some(exit_code) = inspect.exit_code {
                if exit_code != 0 {
                    return Err(anyhow::anyhow!(
                        "Git clone failed with exit code {}",
                        exit_code
                    ));
                }
            }

            Ok(output)
        } else {
            Ok("No git repository to clone".to_string())
        }
    }

    /// Clone Git repository inside container with real-time streaming to GitLab
    #[allow(dead_code)]
    async fn clone_repository_with_streaming(
        &self,
        job: &Job,
        container_id: &str,
        gitlab_params: Option<(&GitLabClient, u64, &str)>,
        mut trace_offset: usize,
    ) -> Result<String> {
        if let Some(ref git_info) = job.git_info {
            let clone_cmd = format!(
                "cd /builds && git clone --depth 1 --branch {} {} project",
                git_info.ref_name, git_info.repo_url
            );

            let exec = self
                .docker
                .create_exec(
                    container_id,
                    CreateExecOptions {
                        cmd: Some(vec!["sh", "-c", &clone_cmd]),
                        attach_stdout: Some(true),
                        attach_stderr: Some(true),
                        working_dir: Some("/builds"),
                        ..Default::default()
                    },
                )
                .await
                .context("Failed to create exec for git clone")?;

            let mut output = String::new();
            let mut buffer = String::new();
            let mut last_flush = std::time::Instant::now();
            const BUFFER_SIZE: usize = 10 * 1024; // 10KB
            const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

            if let StartExecResults::Attached {
                output: mut stream, ..
            } = self.docker.start_exec(&exec.id, None).await?
            {
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(msg) => {
                            let text = msg.to_string();
                            print!("{}", text);
                            output.push_str(&text);
                            buffer.push_str(&text);

                            // Stream to GitLab with batching
                            if let Some((client, job_id, token)) = gitlab_params {
                                let should_flush = buffer.len() >= BUFFER_SIZE
                                    || last_flush.elapsed() >= FLUSH_INTERVAL;

                                if should_flush && !buffer.is_empty() {
                                    let new_offset = trace_offset + buffer.len();
                                    if let Err(e) = client
                                        .patch_trace(job_id, token, &buffer, trace_offset)
                                        .await
                                    {
                                        warn!("Failed to stream clone output: {}", e);
                                    }
                                    trace_offset = new_offset;
                                    buffer.clear();
                                    last_flush = std::time::Instant::now();
                                }
                            }
                        }
                        Err(e) => return Err(e.into()),
                    }
                }

                // Flush remaining buffer
                if let Some((client, job_id, token)) = gitlab_params {
                    if !buffer.is_empty() {
                        if let Err(e) = client
                            .patch_trace(job_id, token, &buffer, trace_offset)
                            .await
                        {
                            warn!("Failed to stream final clone output: {}", e);
                        }
                    }
                }
            }

            // Check exit code
            let inspect = self.docker.inspect_exec(&exec.id).await?;
            if let Some(exit_code) = inspect.exit_code {
                if exit_code != 0 {
                    return Err(anyhow::anyhow!(
                        "Git clone failed with exit code {}",
                        exit_code
                    ));
                }
            }

            Ok(output)
        } else {
            Ok("No git repository to clone".to_string())
        }
    }

    /// Execute command in container
    #[allow(dead_code)]
    async fn exec_in_container(&self, container_id: &str, command: &str) -> Result<String> {
        self.exec_in_container_with_streaming(container_id, command, None, 0)
            .await
    }

    /// Execute command in container with optional real-time streaming
    async fn exec_in_container_with_streaming(
        &self,
        container_id: &str,
        command: &str,
        gitlab_client: Option<(&GitLabClient, u64, &str)>, // (client, job_id, token)
        trace_offset: usize,
    ) -> Result<String> {
        debug!("Executing: {}", command);

        let exec = self
            .docker
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(vec!["sh", "-c", command]),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    working_dir: Some("/builds/project"),
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create exec")?;

        let mut output = String::new();
        let mut current_offset = trace_offset;
        let mut buffer = String::new();
        let mut last_flush = std::time::Instant::now();
        const BUFFER_SIZE: usize = 10 * 1024; // 10KB batching
        const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

        if let StartExecResults::Attached {
            output: mut stream, ..
        } = self.docker.start_exec(&exec.id, None).await?
        {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(msg) => {
                        let text = msg.to_string();
                        print!("{}", text);
                        output.push_str(&text);
                        buffer.push_str(&text);

                        // Batched streaming: flush when buffer >= 10KB or 1s elapsed
                        if let Some((client, job_id, token)) = gitlab_client {
                            let should_flush = buffer.len() >= BUFFER_SIZE
                                || last_flush.elapsed() >= FLUSH_INTERVAL;

                            if should_flush && !buffer.is_empty() {
                                let new_offset = current_offset + buffer.len();
                                if let Err(e) = client
                                    .patch_trace(job_id, token, &buffer, current_offset)
                                    .await
                                {
                                    warn!("Failed to stream trace batch: {}", e);
                                } else {
                                    current_offset = new_offset;
                                    buffer.clear();
                                    last_flush = std::time::Instant::now();
                                }
                            }
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // Flush remaining buffer before checking exit code
        if let Some((client, job_id, token)) = gitlab_client {
            if !buffer.is_empty() {
                if let Err(e) = client
                    .patch_trace(job_id, token, &buffer, current_offset)
                    .await
                {
                    warn!("Failed to flush final trace: {}", e);
                }
            }
        }

        // Check exec exit code
        let inspect = self.docker.inspect_exec(&exec.id).await?;
        if let Some(exit_code) = inspect.exit_code {
            if exit_code != 0 {
                return Err(anyhow::anyhow!(
                    "Command failed with exit code {}: {}",
                    exit_code,
                    command
                ));
            }
        }

        Ok(output)
    }

    /// Cleanup container and workspace
    async fn cleanup_container(&self, container_id: &str, job_id: u64) -> Result<()> {
        use bollard::container::RemoveContainerOptions;

        // Stop and remove container
        self.docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true, // Remove volumes
                    ..Default::default()
                }),
            )
            .await
            .context("Failed to remove container")?;

        info!("🗑️  Removed container: {}", container_id);

        // Clean up host workspace to free disk space
        let workspace = format!("/tmp/turboci-builds/job-{}", job_id);
        if let Err(e) = tokio::fs::remove_dir_all(&workspace).await {
            warn!("Failed to remove workspace {}: {}", workspace, e);
        } else {
            info!("💾 Freed disk space: {}", workspace);
        }

        Ok(())
    }
}

/// Shell Executor - Runs jobs directly on host (FAST!)
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

    /// Execute a GitLab job using shell (super fast!)
    pub async fn execute(&self, job: &Job) -> Result<String> {
        info!("⚡ Using Shell executor (direct execution)");

        let job_dir = format!("{}/job-{}", self.work_dir, job.id);

        // Create work directory
        tokio::fs::create_dir_all(&job_dir).await?;

        let mut output = String::new();

        // Clone repository
        output.push_str(&self.clone_repository(job, &job_dir).await?);

        // Execute job steps with before_script/after_script
        for step in &job.steps {
            info!("  ⚡ Step: {}", step.name);

            // Execute before_script
            if !step.before_script.is_empty() {
                info!("    📋 Running before_script...");
                for script_line in &step.before_script {
                    let step_output = self.exec_command(script_line, &job_dir).await?;
                    output.push_str(&step_output);
                    output.push('\n');
                }
            }

            // Execute main script
            for script_line in &step.script {
                let step_output = self.exec_command(script_line, &job_dir).await?;
                output.push_str(&step_output);
                output.push('\n');
            }

            // Execute after_script (always run)
            if !step.after_script.is_empty() {
                info!("    📋 Running after_script...");
                for script_line in &step.after_script {
                    if let Ok(step_output) = self.exec_command(script_line, &job_dir).await {
                        output.push_str(&step_output);
                        output.push('\n');
                    }
                }
            }
        }

        // Cleanup (optional - keep for debugging)
        // tokio::fs::remove_dir_all(&job_dir).await?;

        Ok(output)
    }

    /// Clone Git repository
    async fn clone_repository(&self, job: &Job, job_dir: &str) -> Result<String> {
        if let Some(ref git_info) = job.git_info {
            info!("📥 Cloning repository...");

            let clone_output = Command::new("git")
                .arg("clone")
                .arg("--depth")
                .arg("1")
                .arg("--branch")
                .arg(&git_info.ref_name)
                .arg(&git_info.repo_url)
                .arg(format!("{}/project", job_dir))
                .output()
                .await
                .context("Failed to clone repository")?;

            let output = format!(
                "{}\n{}",
                String::from_utf8_lossy(&clone_output.stdout),
                String::from_utf8_lossy(&clone_output.stderr)
            );

            if !clone_output.status.success() {
                return Err(anyhow::anyhow!("Git clone failed: {}", output));
            }

            Ok(output)
        } else {
            info!("No git repository configured, skipping clone");
            Ok("No git repository to clone".to_string())
        }
    }

    /// Execute command via shell
    async fn exec_command(&self, command: &str, job_dir: &str) -> Result<String> {
        debug!("⚡ Executing: {}", command);

        let project_dir = format!("{}/project", job_dir);

        let exec_output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&project_dir)
            .output()
            .await
            .context("Failed to execute command")?;

        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&exec_output.stdout),
            String::from_utf8_lossy(&exec_output.stderr)
        );

        print!("{}", output);

        if !exec_output.status.success() {
            return Err(anyhow::anyhow!("Command failed: {}", command));
        }

        Ok(output)
    }
}
