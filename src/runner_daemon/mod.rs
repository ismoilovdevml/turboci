use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::gitlab::{GitLabClient, Job, JobState};
use crate::security::secret_scrubber::SecretScrubber;
use crate::storage::{HybridStorage, StorageBackend};

pub mod artifacts;
pub mod config;
pub mod executor;
pub mod git;
pub mod system_id;

#[derive(Clone)]
pub struct RunnerDaemon {
    config: Arc<config::RunnerConfig>,
    gitlab: Arc<GitLabClient>,
    storage: Arc<HybridStorage>,
    executor: Arc<executor::ExecutorType>,
    semaphore: Arc<Semaphore>,
    scrubber: Arc<SecretScrubber>,
}

impl RunnerDaemon {
    pub fn new(
        config: config::RunnerConfig,
        gitlab: GitLabClient,
        storage: HybridStorage,
        executor: executor::ExecutorType,
    ) -> Self {
        let concurrent = config.concurrent as usize;

        // Create secret scrubber with runner token
        let mut scrubber = SecretScrubber::new(vec![config.runner_token.clone()]);

        // Add GitLab URL as potential secret location
        if config.gitlab_url.contains('@') {
            scrubber.add_secret(config.gitlab_url.clone());
        }

        Self {
            config: Arc::new(config),
            gitlab: Arc::new(gitlab),
            storage: Arc::new(storage),
            executor: Arc::new(executor),
            semaphore: Arc::new(Semaphore::new(concurrent)),
            scrubber: Arc::new(scrubber),
        }
    }

    /// Start the runner daemon
    pub async fn start(&self) -> Result<()> {
        info!("🚀 TurboCI Runner Daemon starting...");
        info!("   Concurrent jobs: {}", self.config.concurrent);
        info!("   Check interval: {}s", self.config.check_interval);
        info!("   Cache enabled: {}", self.config.cache_enabled);

        // Log cache statistics every 100 jobs
        let mut job_counter = 0;

        loop {
            // Print cache stats every 100 jobs
            if job_counter > 0 && job_counter % 100 == 0 {
                if let Ok(stats) = self.stats().await {
                    info!("📊 Cache Statistics (after {} jobs):", job_counter);
                    info!("   Hit rate: {:.1}%", stats.cache_hit_rate);
                    info!("   Cached items: {}", stats.cached_items);
                    info!(
                        "   Total size: {:.2} MB",
                        stats.total_cached_size as f64 / 1_048_576.0
                    );
                }
            }
            // Request a job from GitLab
            match self.gitlab.request_job(&self.config.runner_token).await {
                Ok(Some(job)) => {
                    let job_name = job
                        .job_info
                        .as_ref()
                        .map(|ji| ji.name.as_str())
                        .unwrap_or("unknown");
                    info!("📦 Received job #{} ({})", job.id, job_name);

                    job_counter += 1;

                    // Execute job concurrently
                    let daemon = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = daemon.execute_job(job).await {
                            error!("Job execution failed: {}", e);
                        }
                    });
                }
                Ok(None) => {
                    // No jobs available, wait
                    sleep(Duration::from_secs(self.config.check_interval)).await;
                }
                Err(e) => {
                    warn!("Failed to request job: {}", e);
                    sleep(Duration::from_secs(self.config.check_interval)).await;
                }
            }
        }
    }

    /// Execute a single job
    async fn execute_job(&self, job: Job) -> Result<()> {
        // Acquire semaphore permit (limit concurrency)
        let _permit = self.semaphore.acquire().await?;

        let job_name = job
            .job_info
            .as_ref()
            .map(|ji| ji.name.as_str())
            .unwrap_or("unknown");
        info!("▶️  Starting job #{}: {}", job.id, job_name);

        // Update job state to running
        self.gitlab
            .update_job(job.id, &job.token, JobState::Running, None)
            .await?;

        // STAGE 1: Prepare execution environment
        info!("📋 Stage: Preparing execution environment");
        let prepare_trace = "Preparing execution environment...\n";
        let mut trace_offset = 0;
        trace_offset = self
            .gitlab
            .patch_trace(job.id, &job.token, prepare_trace, trace_offset)
            .await
            .unwrap_or(0);

        // STAGE 2: Get sources (git clone/fetch)
        if let Some(ref git_info) = job.git_info {
            // repo_url embeds the job token, so it is never logged or traced
            info!(
                "📥 Stage: Getting sources for {} ({})",
                git_info.ref_name, git_info.sha
            );
            let git_trace = format!(
                "Fetching changes...\nRef: {}\nSHA: {}\n",
                git_info.ref_name, git_info.sha
            );
            trace_offset = self
                .gitlab
                .patch_trace(job.id, &job.token, &git_trace, trace_offset)
                .await
                .unwrap_or(trace_offset);
        }

        // STAGE 3: Restore cache
        info!("📦 Stage: Restoring cache");
        for cache_entry in &job.cache {
            if cache_entry.policy == "pull" || cache_entry.policy == "pull-push" {
                if let Ok(Some(cache_data)) = self
                    .gitlab
                    .download_cache(job.id, &job.token, &cache_entry.key)
                    .await
                {
                    let cache_trace = format!(
                        "✓ Restored cache: {} ({} bytes)\n",
                        cache_entry.key,
                        cache_data.len()
                    );
                    trace_offset = self
                        .gitlab
                        .patch_trace(job.id, &job.token, &cache_trace, trace_offset)
                        .await
                        .unwrap_or(trace_offset);

                    // Extract cache to workspace
                    let workspace_path = match &*self.executor {
                        executor::ExecutorType::Docker(_) => {
                            format!("/tmp/turboci-builds/job-{}/project", job.id)
                        }
                        executor::ExecutorType::Shell(_) => {
                            format!("/tmp/turboci/job-{}/project", job.id)
                        }
                    };

                    if let Err(e) = artifacts::download_and_extract_cache(
                        &self.config.gitlab_url,
                        job.id,
                        &job.token,
                        &workspace_path,
                        &cache_entry.key,
                    )
                    .await
                    {
                        warn!("Failed to extract cache {}: {}", cache_entry.key, e);
                    }
                }
            }
        }

        // STAGE 4: Download artifacts from dependencies
        info!("📥 Stage: Downloading artifacts");

        // Determine workspace path based on executor type
        let workspace_path = match &*self.executor {
            executor::ExecutorType::Docker(_) => {
                format!("/tmp/turboci-builds/job-{}/project", job.id)
            }
            executor::ExecutorType::Shell(_) => {
                format!("/tmp/turboci/job-{}/project", job.id)
            }
        };

        for dependency in &job.dependencies {
            let dep_trace = format!(
                "Downloading artifacts from job #{} ({})\n",
                dependency.id, dependency.name
            );
            trace_offset = self
                .gitlab
                .patch_trace(job.id, &job.token, &dep_trace, trace_offset)
                .await
                .unwrap_or(trace_offset);

            // Download and extract artifacts
            let artifact_names = vec!["artifact".to_string()]; // Default name, should come from job config
            if let Err(e) = artifacts::download_and_extract_artifacts(
                &self.config.gitlab_url,
                dependency.id,
                &dependency.token,
                &workspace_path,
                &artifact_names,
            )
            .await
            {
                warn!(
                    "Failed to download artifacts from job #{}: {}",
                    dependency.id, e
                );
            }
        }

        // Mask the job token, dependency tokens and masked variables
        let scrubber = self.scrubber.with_job_secrets(&job);

        // Execute job with real-time trace streaming to GitLab
        let result = self
            .executor
            .execute_with_streaming(&job, Some(&self.gitlab), &scrubber)
            .await;

        match result {
            Ok(trace) => {
                info!("✅ Job #{} completed successfully", job.id);

                // Scrub secrets from trace
                let scrubbed_trace = scrubber.scrub(&trace);

                // Stream trace to GitLab (GitLab 17.x)
                let _final_offset = match self
                    .gitlab
                    .patch_trace(job.id, &job.token, &scrubbed_trace, 0)
                    .await
                {
                    Ok(offset) => offset,
                    Err(e) => {
                        warn!("Failed to stream trace: {}", e);
                        0
                    }
                };

                // Upload artifacts in parallel if available (GitLab 17.x)
                if let Some(ref artifacts) = job.artifacts {
                    let mut upload_futures = Vec::new();

                    for artifact in artifacts {
                        let artifact_name = artifact.name.as_deref().unwrap_or("artifact");

                        // Collect and ZIP artifacts from workspace
                        let workspace_path = format!("/tmp/turboci-builds/job-{}/project", job.id);
                        let artifact_data = match artifacts::create_zip_from_paths(
                            &workspace_path,
                            &artifact.paths,
                        )
                        .await
                        {
                            Ok(data) => data,
                            Err(e) => {
                                warn!("Failed to create artifact ZIP: {}", e);
                                continue;
                            }
                        };

                        info!(
                            "📦 Created artifact ZIP: {} ({} bytes)",
                            artifact_name,
                            artifact_data.len()
                        );

                        // Spawn parallel upload task
                        let gitlab = self.gitlab.clone();
                        let job_id = job.id;
                        let job_token = job.token.clone();
                        let artifact_name_owned = artifact_name.to_string();
                        let expire_in_owned = artifact.expire_in.clone();

                        let upload_task = tokio::spawn(async move {
                            gitlab
                                .upload_artifacts(
                                    job_id,
                                    &job_token,
                                    artifact_data,
                                    &artifact_name_owned,
                                    expire_in_owned.as_deref(),
                                )
                                .await
                        });

                        upload_futures.push((artifact_name.to_string(), upload_task));
                    }

                    // Wait for all uploads to complete in parallel
                    for (artifact_name, upload_task) in upload_futures {
                        match upload_task.await {
                            Ok(Ok(())) => {
                                info!("✅ Uploaded artifact: {}", artifact_name);
                            }
                            Ok(Err(e)) => {
                                warn!("Failed to upload artifact {}: {}", artifact_name, e);
                            }
                            Err(e) => {
                                warn!("Upload task panicked for {}: {}", artifact_name, e);
                            }
                        }
                    }
                }

                // Upload cache after execution (GitLab 17.x)
                for cache_entry in &job.cache {
                    if cache_entry.policy == "push" || cache_entry.policy == "pull-push" {
                        // Create ZIP from cache paths
                        let workspace_path = format!("/tmp/turboci-builds/job-{}/project", job.id);
                        let cache_data = match artifacts::create_zip_from_paths(
                            &workspace_path,
                            &cache_entry.paths,
                        )
                        .await
                        {
                            Ok(data) => data,
                            Err(e) => {
                                warn!("Failed to create cache ZIP: {}", e);
                                continue;
                            }
                        };

                        info!(
                            "📦 Created cache ZIP: {} ({} bytes)",
                            cache_entry.key,
                            cache_data.len()
                        );

                        if let Err(e) = self
                            .gitlab
                            .upload_cache(job.id, &job.token, &cache_entry.key, cache_data)
                            .await
                        {
                            warn!("Failed to upload cache {}: {}", cache_entry.key, e);
                        }
                    }
                }

                self.gitlab
                    .update_job(job.id, &job.token, JobState::Success, Some(&scrubbed_trace))
                    .await?;
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                let scrubbed_error = scrubber.scrub(&error_msg);
                error!("❌ Job #{} failed: {}", job.id, scrubbed_error);

                let trace = format!("Job failed: {}", scrubbed_error);

                // Stream failure trace to GitLab
                let _ = self
                    .gitlab
                    .patch_trace(job.id, &job.token, &trace, 0)
                    .await
                    .map_err(|e| warn!("Failed to stream failure trace: {}", e));

                self.gitlab
                    .update_job(job.id, &job.token, JobState::Failed, Some(&trace))
                    .await?;
            }
        }

        Ok(())
    }

    /// Get runner statistics
    pub async fn stats(&self) -> Result<RunnerStats> {
        let storage_stats = self.storage.stats().await?;

        Ok(RunnerStats {
            cache_hit_rate: if storage_stats.hit_count + storage_stats.miss_count > 0 {
                (storage_stats.hit_count as f64
                    / (storage_stats.hit_count + storage_stats.miss_count) as f64)
                    * 100.0
            } else {
                0.0
            },
            total_cached_size: storage_stats.total_size,
            cached_items: storage_stats.item_count,
        })
    }
}

#[derive(Debug)]
pub struct RunnerStats {
    pub cache_hit_rate: f64,
    pub total_cached_size: u64,
    pub cached_items: u64,
}
