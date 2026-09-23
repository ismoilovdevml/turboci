use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::gitlab::{ArtifactUpload, GitLabClient, Job, JobState};
use crate::security::secret_scrubber::SecretScrubber;
use executor::{JobFailure, JobOutcome};
use trace::TraceWriter;

pub mod artifacts;
pub mod config;
pub mod executor;
pub mod git;
pub mod job_cache;
pub mod script;
pub mod system_id;
pub mod trace;

/// Values of the job's variables, for expanding cache keys
fn job_variables(job: &Job) -> std::collections::HashMap<String, String> {
    script::job_env(job, "", std::path::Path::new(""))
        .vars
        .into_iter()
        .collect()
}

fn project_id(job: &Job) -> u64 {
    job.job_info.as_ref().map_or(0, |info| info.project_id)
}

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
    cache: job_cache::LocalCache,
    config: Arc<config::RunnerConfig>,
    gitlab: Arc<GitLabClient>,
    executor: Arc<executor::ExecutorType>,
    semaphore: Arc<Semaphore>,
    scrubber: Arc<SecretScrubber>,
}

impl RunnerDaemon {
    pub fn new(
        config: config::RunnerConfig,
        gitlab: GitLabClient,
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
            cache: job_cache::LocalCache::new(&config.cache_dir),
            config: Arc::new(config),
            gitlab: Arc::new(gitlab),
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

        loop {
            // Request a job only when a slot is free
            match claim_job(&self.semaphore, &self.gitlab, &self.config.runner_token).await {
                Ok(Some((job, permit))) => {
                    let job_name = job
                        .job_info
                        .as_ref()
                        .map(|ji| ji.name.as_str())
                        .unwrap_or("unknown");
                    info!("📦 Received job #{} ({})", job.id, job_name);

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
        self.upload_cache(&job, &mut trace, outcome.is_ok()).await;
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

        if self.config.cache_enabled {
            let variables = job_variables(job);
            for cache_entry in &job.cache {
                if cache_entry.policy != "pull" && cache_entry.policy != "pull-push" {
                    continue;
                }
                let keys: Vec<String> = std::iter::once(&cache_entry.key)
                    .chain(&cache_entry.fallback_keys)
                    .map(|key| job_cache::resolve_key(key, &variables))
                    .collect();
                trace
                    .write(&format!("Restoring cache {}...\n", keys[0]))
                    .await;
                match self.cache.restore(project_id(job), &keys, &workspace).await {
                    Ok(Some(key)) => {
                        trace
                            .write(&format!("Successfully restored cache {}\n", key))
                            .await
                    }
                    Ok(None) => trace.write("No cache found\n").await,
                    Err(e) => {
                        warn!("Failed to restore cache {}: {}", keys[0], e);
                        trace
                            .write(&format!("WARNING: Failed to restore cache: {}\n", e))
                            .await;
                    }
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
                &self.gitlab,
                dependency.id,
                &dependency.token,
                &workspace,
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
            let name = artifact.name.as_deref().unwrap_or("artifacts");
            let artifact_type = artifact.artifact_type.as_deref().unwrap_or("archive");
            let format = artifact.artifact_format.as_deref().unwrap_or("zip");
            trace
                .write(&format!(
                    "Uploading artifacts ({}, {})...\n",
                    name, artifact_type
                ))
                .await;

            let result = match artifacts::create_archive(
                &workspace.to_string_lossy(),
                &artifact.paths,
                format,
            )
            .await
            {
                Ok(data) => {
                    let extension = match format {
                        "gzip" => ".gz",
                        "zip" => ".zip",
                        _ => "",
                    };
                    self.gitlab
                        .upload_artifacts(
                            job.id,
                            &job.token,
                            ArtifactUpload {
                                data,
                                file_name: format!("{}{}", name, extension),
                                format: format.to_string(),
                                artifact_type: artifact_type.to_string(),
                                expire_in: artifact.expire_in.clone(),
                            },
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

    /// Save cache entries whose policy pushes and whose `when` matches the result
    async fn upload_cache(&self, job: &Job, trace: &mut TraceWriter<'_>, succeeded: bool) {
        if !self.config.cache_enabled {
            return;
        }
        let workspace = self.executor.job_dir(job.id).join("project");
        let variables = job_variables(job);

        for cache_entry in &job.cache {
            let wanted = match cache_entry.when.as_deref() {
                Some("always") => true,
                Some("on_failure") => !succeeded,
                _ => succeeded,
            };
            if !wanted
                || cache_entry.paths.is_empty()
                || (cache_entry.policy != "push" && cache_entry.policy != "pull-push")
            {
                continue;
            }
            let key = job_cache::resolve_key(&cache_entry.key, &variables);
            trace.write(&format!("Saving cache {}...\n", key)).await;
            match self
                .cache
                .save(
                    project_id(job),
                    &key,
                    &workspace.to_string_lossy(),
                    &cache_entry.paths,
                )
                .await
            {
                Ok(size) => {
                    trace
                        .write(&format!("Created cache {} ({} bytes)\n", key, size))
                        .await
                }
                Err(e) => {
                    warn!("Failed to save cache {}: {}", key, e);
                    trace
                        .write(&format!("WARNING: Failed to save cache: {}\n", e))
                        .await;
                }
            }
        }
    }
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

    /// A daemon with the shell executor talking to a mock GitLab
    async fn daemon(server: &MockServer, dir: &std::path::Path) -> RunnerDaemon {
        for (verb, route, status) in [
            ("PATCH", r"^/api/v4/jobs/\d+/trace$", 202),
            ("PUT", r"^/api/v4/jobs/\d+$", 200),
            ("POST", r"^/api/v4/jobs/\d+/artifacts$", 201),
        ] {
            Mock::given(method(verb))
                .and(wiremock::matchers::path_regex(route))
                .respond_with(ResponseTemplate::new(status))
                .mount(server)
                .await;
        }
        let config = config::RunnerConfig {
            runner_token: "glrt-test".to_string(),
            gitlab_url: server.uri(),
            cache_dir: dir.join("cache").to_string_lossy().into_owned(),
            ..config::RunnerConfig::default()
        };
        let executor = executor::ExecutorType::Shell(executor::ShellExecutor::new(Some(
            dir.join("builds").to_string_lossy().into_owned(),
        )));
        RunnerDaemon::new(
            config,
            GitLabClient::new(server.uri(), "glrt-test".to_string()),
            executor,
        )
    }

    fn lifecycle_job(id: u64, script: &[&str], artifacts_when: &str) -> Job {
        serde_json::from_value(serde_json::json!({
            "id": id, "token": format!("job-token-{}", id),
            "job_info": {"name": "build", "stage": "test", "project_id": 5, "project_name": "app"},
            "variables": [{"key": "SECRET", "value": "s3cr3t-value", "masked": true}],
            "steps": [{"name": "script", "script": script, "when": "on_success", "timeout": 60}],
            "artifacts": [{"name": "dist", "paths": ["out/"], "when": artifacts_when}],
            "cache": [{"key": "deps", "paths": ["vendor/"], "policy": "pull-push"}]
        }))
        .unwrap()
    }

    async fn requests(server: &MockServer, verb: &str, job_id: u64) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| {
                r.method.as_str() == verb && r.url.path().contains(&format!("/jobs/{}", job_id))
            })
            .collect()
    }

    async fn trace_of(server: &MockServer, job_id: u64) -> String {
        requests(server, "PATCH", job_id)
            .await
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect()
    }

    async fn final_update(server: &MockServer, job_id: u64) -> serde_json::Value {
        let updates = requests(server, "PUT", job_id).await;
        assert_eq!(updates.len(), 1, "exactly one final update expected");
        serde_json::from_slice(&updates[0].body).unwrap()
    }

    #[tokio::test]
    async fn successful_job_streams_trace_uploads_artifacts_saves_cache_and_cleans_up() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job = lifecycle_job(
            11,
            &[
                "mkdir -p out vendor",
                "echo built > out/app.txt",
                "echo dep > vendor/lib.txt",
                "echo secret=$SECRET",
            ],
            "on_success",
        );

        daemon.execute_job(job).await.unwrap();

        let update = final_update(&server, 11).await;
        assert_eq!(update["state"], "success");
        assert!(update.get("failure_reason").is_none());

        let trace = trace_of(&server, 11).await;
        assert!(trace.contains("$ echo built > out/app.txt"), "{}", trace);
        assert!(trace.contains("secret=[MASKED]"), "{}", trace);
        assert!(!trace.contains("s3cr3t-value"));
        assert!(trace.ends_with("Job succeeded\n"), "{}", trace);

        let uploads = requests(&server, "POST", 11).await;
        assert_eq!(uploads.len(), 1);
        assert!(String::from_utf8_lossy(&uploads[0].body).contains("filename=\"dist.zip\""));

        assert_eq!(daemon.cache.stats().0, 1, "cache archive saved");
        assert!(
            !dir.path().join("builds/job-11").exists(),
            "workspace removed"
        );
    }

    #[tokio::test]
    async fn failed_job_reports_reason_and_only_uploads_on_failure_artifacts() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;

        daemon
            .execute_job(lifecycle_job(
                21,
                &["mkdir -p out", "echo log > out/log.txt", "exit 2"],
                "on_success",
            ))
            .await
            .unwrap();
        daemon
            .execute_job(lifecycle_job(
                22,
                &["mkdir -p out", "echo log > out/log.txt", "exit 2"],
                "on_failure",
            ))
            .await
            .unwrap();

        for id in [21, 22] {
            let update = final_update(&server, id).await;
            assert_eq!(update["state"], "failed");
            assert_eq!(update["failure_reason"], "script_failure");
            assert_eq!(update["exit_code"], 2);
            assert!(trace_of(&server, id)
                .await
                .contains("ERROR: Job failed: exit code 2"));
        }
        assert!(
            requests(&server, "POST", 21).await.is_empty(),
            "on_success artifacts skipped"
        );
        assert_eq!(
            requests(&server, "POST", 22).await.len(),
            1,
            "on_failure artifacts uploaded"
        );
        assert_eq!(daemon.cache.stats().0, 0, "no cache saved for failed jobs");
    }

    #[tokio::test]
    async fn next_job_restores_cache_of_previous_job() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;

        daemon
            .execute_job(lifecycle_job(
                31,
                &["mkdir -p vendor", "echo cached-dep > vendor/lib.txt"],
                "on_success",
            ))
            .await
            .unwrap();
        daemon
            .execute_job(lifecycle_job(32, &["cat vendor/lib.txt"], "on_success"))
            .await
            .unwrap();

        assert_eq!(final_update(&server, 32).await["state"], "success");
        let trace = trace_of(&server, 32).await;
        assert!(
            trace.contains("Successfully restored cache deps"),
            "{}",
            trace
        );
        assert!(trace.contains("cached-dep"), "{}", trace);
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
