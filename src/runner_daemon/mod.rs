use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::gitlab::{GitLabClient, Job, JobState};
use crate::security::secret_scrubber::SecretScrubber;
use crate::storage::{HybridStorage, StorageBackend};
use executor::{JobFailure, JobOutcome};
use trace::TraceWriter;

pub mod artifacts;
pub mod config;
pub mod executor;
pub mod git;
pub mod script;
pub mod system_id;
pub mod trace;

/// Wait for a free job slot, then ask GitLab for a job.
///
/// The permit is taken before the request, so the runner never accepts more jobs
/// than `concurrent`; it is returned with the job and released when the job ends.
async fn claim_job(
    semaphore: &Arc<Semaphore>,
    gitlab: &GitLabClient,
    runner_token: &str,
) -> Result<Option<(Job, OwnedSemaphorePermit)>> {
    let permit = semaphore.clone().acquire_owned().await?;
    Ok(gitlab
        .request_job(runner_token)
        .await?
        .map(|job| (job, permit)))
}

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
            // Request a job only when a slot is free
            match claim_job(&self.semaphore, &self.gitlab, &self.config.runner_token).await {
                Ok(Some((job, permit))) => {
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
                        let _permit = permit;
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

    /// Run a job end to end and always report a final state to GitLab
    async fn execute_job(&self, job: Job) -> Result<()> {
        let job_name = job
            .job_info
            .as_ref()
            .map(|ji| ji.name.as_str())
            .unwrap_or("unknown");
        info!("▶️  Starting job #{}: {}", job.id, job_name);

        // Mask the job token, dependency tokens and masked variables
        let scrubber = self.scrubber.with_job_secrets(&job);
        let mut trace = TraceWriter::new(Some(&self.gitlab), job.id, &job.token, &scrubber);
        trace
            .write(&format!(
                "Running with TurboCI {}\n",
                env!("CARGO_PKG_VERSION")
            ))
            .await;

        let mut outcome = self.prepare_workspace(&job, &mut trace).await;
        if outcome.is_ok() {
            outcome = self.executor.execute(&job, &mut trace).await;
        }
        self.upload_artifacts(&job, &mut trace, outcome.is_ok())
            .await;
        if outcome.is_ok() {
            self.upload_cache(&job, &mut trace).await;
        }
        self.executor.cleanup(job.id).await;

        let (state, reason, exit_code) = match &outcome {
            Ok(()) => {
                info!("✅ Job #{} succeeded", job.id);
                trace.write("\nJob succeeded\n").await;
                (JobState::Success, None, None)
            }
            Err(failure) => {
                let message = scrubber.scrub(&failure.message);
                error!("❌ Job #{} failed: {}", job.id, message);
                trace
                    .write(&format!("\nERROR: Job failed: {}\n", message))
                    .await;
                (JobState::Failed, Some(failure.reason), failure.exit_code)
            }
        };
        trace.finish().await;

        self.gitlab
            .update_job(job.id, &job.token, state, reason, exit_code)
            .await
    }

    /// Create the workspace and restore cache and dependency artifacts into it
    async fn prepare_workspace(&self, job: &Job, trace: &mut TraceWriter<'_>) -> JobOutcome {
        let project_dir = self.executor.job_dir(job.id).join("project");
        tokio::fs::create_dir_all(&project_dir)
            .await
            .map_err(|e| JobFailure::system(format!("Failed to create workspace: {}", e)))?;
        let workspace = project_dir.to_string_lossy();

        for cache_entry in &job.cache {
            if cache_entry.policy == "pull" || cache_entry.policy == "pull-push" {
                trace
                    .write(&format!("Restoring cache {}...\n", cache_entry.key))
                    .await;
                if let Err(e) = artifacts::download_and_extract_cache(
                    &self.config.gitlab_url,
                    job.id,
                    &job.token,
                    &workspace,
                    &cache_entry.key,
                )
                .await
                {
                    warn!("Failed to restore cache {}: {}", cache_entry.key, e);
                    trace
                        .write(&format!("WARNING: Failed to restore cache: {}\n", e))
                        .await;
                }
            }
        }

        for dependency in &job.dependencies {
            trace
                .write(&format!(
                    "Downloading artifacts from job #{} ({})...\n",
                    dependency.id, dependency.name
                ))
                .await;
            if let Err(e) = artifacts::download_and_extract_artifacts(
                &self.config.gitlab_url,
                dependency.id,
                &dependency.token,
                &workspace,
                &["artifact".to_string()],
            )
            .await
            {
                warn!(
                    "Failed to download artifacts from job #{}: {}",
                    dependency.id, e
                );
                trace
                    .write(&format!("WARNING: Failed to download artifacts: {}\n", e))
                    .await;
            }
        }
        Ok(())
    }

    /// Upload artifacts whose `when` matches the job result (default: on_success)
    async fn upload_artifacts(&self, job: &Job, trace: &mut TraceWriter<'_>, succeeded: bool) {
        let Some(ref job_artifacts) = job.artifacts else {
            return;
        };
        let workspace = self.executor.job_dir(job.id).join("project");

        for artifact in job_artifacts {
            let wanted = match artifact.when.as_deref() {
                Some("always") => true,
                Some("on_failure") => !succeeded,
                _ => succeeded,
            };
            if !wanted || artifact.paths.is_empty() {
                continue;
            }
            let name = artifact.name.as_deref().unwrap_or("artifact");
            trace
                .write(&format!("Uploading artifacts ({})...\n", name))
                .await;

            let result = match artifacts::create_zip_from_paths(
                &workspace.to_string_lossy(),
                &artifact.paths,
            )
            .await
            {
                Ok(data) => {
                    self.gitlab
                        .upload_artifacts(
                            job.id,
                            &job.token,
                            data,
                            name,
                            artifact.expire_in.as_deref(),
                        )
                        .await
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => info!("✅ Uploaded artifact: {}", name),
                Err(e) => {
                    warn!("Failed to upload artifact {}: {}", name, e);
                    trace
                        .write(&format!("WARNING: Uploading artifacts failed: {}\n", e))
                        .await;
                }
            }
        }
    }

    /// Save cache entries whose policy pushes (after a successful job)
    async fn upload_cache(&self, job: &Job, trace: &mut TraceWriter<'_>) {
        let workspace = self.executor.job_dir(job.id).join("project");

        for cache_entry in &job.cache {
            if cache_entry.policy != "push" && cache_entry.policy != "pull-push" {
                continue;
            }
            trace
                .write(&format!("Saving cache {}...\n", cache_entry.key))
                .await;
            let result = match artifacts::create_zip_from_paths(
                &workspace.to_string_lossy(),
                &cache_entry.paths,
            )
            .await
            {
                Ok(data) => {
                    self.gitlab
                        .upload_cache(job.id, &job.token, &cache_entry.key, data)
                        .await
                }
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                warn!("Failed to save cache {}: {}", cache_entry.key, e);
                trace
                    .write(&format!("WARNING: Failed to save cache: {}\n", e))
                    .await;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn claim_job_does_not_request_while_all_slots_are_busy() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"id": 1, "token": "job-token"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let gitlab = GitLabClient::new(server.uri(), "runner-token".to_string());
        let semaphore = Arc::new(Semaphore::new(1));

        let (_job, permit) = claim_job(&semaphore, &gitlab, "runner-token")
            .await
            .unwrap()
            .expect("job expected");

        // The only slot is taken by the running job: no second request may be sent
        let second = tokio::time::timeout(
            Duration::from_millis(200),
            claim_job(&semaphore, &gitlab, "runner-token"),
        )
        .await;
        assert!(second.is_err(), "requested a job with no free slot");

        drop(permit);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn claim_job_releases_slot_when_no_job_is_available() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let gitlab = GitLabClient::new(server.uri(), "runner-token".to_string());
        let semaphore = Arc::new(Semaphore::new(1));

        assert!(claim_job(&semaphore, &gitlab, "runner-token")
            .await
            .unwrap()
            .is_none());
        assert_eq!(semaphore.available_permits(), 1);
    }
}
