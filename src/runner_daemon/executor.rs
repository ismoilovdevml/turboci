use anyhow::{Context, Result};
use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::Docker;
use futures::StreamExt;
use std::sync::Arc;
use tracing::{info, debug};

use crate::gitlab::Job;

#[derive(Clone)]
pub struct DockerExecutor {
    docker: Arc<Docker>,
    default_image: String,
}

impl DockerExecutor {
    pub fn new(default_image: String) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("Failed to connect to Docker daemon")?;

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
        self.docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
            .context("Failed to start container")?;

        let mut output = String::new();

        // Clone repository
        output.push_str(&self.clone_repository(job, &container_id).await?);

        // Execute job steps
        for step in &job.steps {
            info!("  ▶️  Step: {}", step.name);

            for script_line in &step.script {
                let step_output = self.exec_in_container(&container_id, script_line).await?;
                output.push_str(&step_output);
                output.push('\n');
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
        let options = CreateContainerOptions {
            name: format!("turboci-job-{}", job.id),
            ..Default::default()
        };

        let config = Config {
            image: Some(image),
            working_dir: Some("/builds"),
            cmd: Some(vec!["sleep", "3600"]), // Keep alive
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
        let clone_cmd = format!(
            "git clone --depth 1 --branch {} {} /builds/project",
            job.git_info.ref_name, job.git_info.repo_url
        );

        self.exec_in_container(container_id, &clone_cmd).await
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

        if let StartExecResults::Attached { mut output: stream, .. } =
            self.docker.start_exec(&exec.id, None).await?
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

        Ok(output)
    }

    /// Cleanup container
    async fn cleanup_container(&self, container_id: &str) -> Result<()> {
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
