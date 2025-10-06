use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::gitlab::{GitLabClient, Job, JobState};
use crate::security::SecretScrubber;
use crate::storage::{HybridStorage, StorageBackend};

pub mod config;
pub mod executor;

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

        loop {
            // Request a job from GitLab
            match self.gitlab.request_job(&self.config.runner_token).await {
                Ok(Some(job)) => {
                    info!("📦 Received job #{} ({})", job.id, job.job_info.name);

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

        info!("▶️  Starting job #{}: {}", job.id, job.job_info.name);

        // Update job state to running
        self.gitlab
            .update_job(job.id, &job.token, JobState::Running, None)
            .await?;

        // Download cache before execution (GitLab 17.x)
        for cache_entry in &job.cache {
            if cache_entry.policy == "pull" || cache_entry.policy == "pull-push" {
                if let Ok(Some(cache_data)) = self
                    .gitlab
                    .download_cache(job.id, &job.token, &cache_entry.key)
                    .await
                {
                    // Extract cache to job workspace
                    info!("📥 Downloaded cache: {}", cache_entry.key);
                    // TODO: Extract zip to workspace
                    let _ = cache_data; // Suppress unused warning
                }
            }
        }

        // Check internal cache before execution
        let cache_key = self.compute_cache_key(&job).await?;
        if let Some(_cached_result) = self.load_from_cache(&cache_key).await? {
            info!("✅ Job #{} completed from cache!", job.id);

            self.gitlab
                .update_job(
                    job.id,
                    &job.token,
                    JobState::Success,
                    Some("Completed from cache (TurboCI)"),
                )
                .await?;

            return Ok(());
        }

        // Add job secrets to scrubber
        let mut scrubber = (*self.scrubber).clone();
        for var in &job.variables {
            if var.masked {
                scrubber.add_secret(var.value.clone());
            }
        }

        // Execute job with trace streaming
        let result = self.executor.execute(&job).await;

        match result {
            Ok(trace) => {
                info!("✅ Job #{} completed successfully", job.id);

                // Scrub secrets from trace
                let scrubbed_trace = scrubber.scrub(&trace);

                // Stream trace to GitLab (GitLab 17.x)
                if let Err(e) = self
                    .gitlab
                    .patch_trace(job.id, &job.token, &scrubbed_trace, 0)
                    .await
                {
                    warn!("Failed to stream trace: {}", e);
                }

                // Save to internal cache for future use
                self.save_to_cache(&cache_key, &scrubbed_trace).await?;

                // Upload artifacts if available (GitLab 17.x)
                for artifact in &job.artifacts {
                    // TODO: Collect artifacts from job workspace
                    // For now, create empty zip as placeholder
                    let artifact_data = Vec::new();
                    if let Err(e) = self
                        .gitlab
                        .upload_artifacts(
                            job.id,
                            &job.token,
                            artifact_data,
                            &artifact.name,
                            artifact.expire_in.as_deref(),
                        )
                        .await
                    {
                        warn!("Failed to upload artifact {}: {}", artifact.name, e);
                    }
                }

                // Upload cache after execution (GitLab 17.x)
                for cache_entry in &job.cache {
                    if cache_entry.policy == "push" || cache_entry.policy == "pull-push" {
                        // TODO: Create zip from cache paths
                        let cache_data = Vec::new();
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
                if let Err(err) = self.gitlab.patch_trace(job.id, &job.token, &trace, 0).await {
                    warn!("Failed to stream failure trace: {}", err);
                }

                self.gitlab
                    .update_job(job.id, &job.token, JobState::Failed, Some(&trace))
                    .await?;
            }
        }

        Ok(())
    }

    /// Compute cache key for a job
    async fn compute_cache_key(&self, job: &Job) -> Result<String> {
        use blake3::Hasher;

        let mut hasher = Hasher::new();

        // Hash job script
        for step in &job.steps {
            for script_line in &step.script {
                hasher.update(script_line.as_bytes());
            }
        }

        // Hash git commit SHA
        hasher.update(job.git_info.sha.as_bytes());

        // Hash variables (for environment-specific cache)
        for var in &job.variables {
            if !var.masked {
                // Don't include secrets in cache key
                hasher.update(format!("{}={}", var.key, var.value).as_bytes());
            }
        }

        let hash = hasher.finalize().to_hex();
        Ok(format!("job:{}:{}", job.job_info.project_id, hash))
    }

    /// Load job result from cache
    async fn load_from_cache(&self, key: &str) -> Result<Option<String>> {
        if !self.config.cache_enabled {
            return Ok(None);
        }

        match self.storage.retrieve(key).await? {
            Some(data) => {
                let trace = String::from_utf8(data)?;
                info!("📦 Cache HIT: {}", key);
                Ok(Some(trace))
            }
            None => {
                info!("📭 Cache MISS: {}", key);
                Ok(None)
            }
        }
    }

    /// Save job result to cache
    async fn save_to_cache(&self, key: &str, trace: &str) -> Result<()> {
        if !self.config.cache_enabled {
            return Ok(());
        }

        self.storage.store(key, trace.as_bytes()).await?;
        info!("💾 Cached result: {}", key);
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
