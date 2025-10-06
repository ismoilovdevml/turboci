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

use crate::gitlab::Job;

/// Executor type enum
#[derive(Debug, Clone)]
pub enum ExecutorType {
    Docker(DockerExecutor),
    Shell(ShellExecutor),
}

impl ExecutorType {
    pub async fn execute(&self, job: &Job) -> Result<String> {
        // Default timeout: 1 hour per job
        let job_timeout = if job.timeout > 0 {
            Duration::from_secs(job.timeout as u64)
        } else {
            Duration::from_secs(3600) // 1 hour default
        };

        // Execute with timeout
        match timeout(job_timeout, async {
            match self {
                ExecutorType::Docker(executor) => executor.execute(job).await,
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
    pub async fn execute(&self, job: &Job) -> Result<String> {
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

        // Clone repository
        output.push_str(&self.clone_repository(job, &container_id).await?);

        // Execute job steps with before_script/after_script
        for step in &job.steps {
            info!("  ▶️  Step: {}", step.name);

            // Execute before_script
            if !step.before_script.is_empty() {
                info!("    📋 Running before_script...");
                for script_line in &step.before_script {
                    let step_output = self.exec_in_container(&container_id, script_line).await?;
                    output.push_str(&step_output);
                    output.push('\n');
                }
            }

            // Execute main script
            for script_line in &step.script {
                let step_output = self.exec_in_container(&container_id, script_line).await?;
                output.push_str(&step_output);
                output.push('\n');
            }

            // Execute after_script (always run, even on failure)
            if !step.after_script.is_empty() {
                info!("    📋 Running after_script...");
                for script_line in &step.after_script {
                    if let Ok(step_output) =
                        self.exec_in_container(&container_id, script_line).await
                    {
                        output.push_str(&step_output);
                        output.push('\n');
                    }
                }
            }
        }

        // Cleanup
        self.cleanup_container(&container_id).await?;

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
        use bollard::models::ContainerCreateBody;

        let options = CreateContainerOptions {
            name: format!("turboci-job-{}", job.id),
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(image.to_string()),
            working_dir: Some("/builds".to_string()),
            cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
            ..Default::default()
        };

        let response = self
            .docker
            .create_container(Some(options), config)
            .await
            .context("Failed to create container")?;

        Ok(response.id)
    }

    /// Clone Git repository inside container
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
                        working_dir: Some("/builds"),  // Start from /builds, not /builds/project
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
                    return Err(anyhow::anyhow!("Git clone failed with exit code {}", exit_code));
                }
            }

            Ok(output)
        } else {
            Ok("No git repository to clone".to_string())
        }
    }

    /// Execute command in container
    async fn exec_in_container(&self, container_id: &str, command: &str) -> Result<String> {
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

    /// Cleanup container
    async fn cleanup_container(&self, container_id: &str) -> Result<()> {
        use bollard::container::RemoveContainerOptions;

        self.docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .context("Failed to remove container")?;

        info!("🗑️  Cleaned up container: {}", container_id);
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
