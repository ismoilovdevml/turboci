use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::gitlab::{ArtifactUpload, FailureReason, GitLabClient, Job, JobState, RemoteState};
use crate::security::secret_scrubber::SecretScrubber;
use cancel::CancelSignal;
use executor::{JobFailure, JobOutcome};
use trace::TraceWriter;

pub mod artifacts;
pub mod cancel;
pub mod config;
pub mod executor;
pub mod git;
pub mod image;
pub mod job_cache;
pub mod script;
pub mod system_id;
pub mod trace;

/// Restores into the workspace once the executor has checked out sources
struct WorkspaceRestore<'a>(&'a RunnerDaemon);

#[async_trait::async_trait]
impl executor::Restore for WorkspaceRestore<'_> {
    async fn restore(&self, job: &Job, trace: &mut TraceWriter<'_>) -> JobOutcome {
        self.0.restore_workspace(job, trace).await
    }
}

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
#[cfg(test)]
async fn claim_job(
    semaphore: &Arc<Semaphore>,
    gitlab: &GitLabClient,
    runner_token: &str,
) -> Result<Option<(Job, OwnedSemaphorePermit)>> {
    let permit = semaphore.clone().acquire_owned().await?;
    claim_with_permit(permit, gitlab, runner_token).await
}

/// Ask GitLab for a job for an already reserved slot; the slot is released
/// when no job is returned
async fn claim_with_permit(
    permit: OwnedSemaphorePermit,
    gitlab: &GitLabClient,
    runner_token: &str,
) -> Result<Option<(Job, OwnedSemaphorePermit)>> {
    Ok(gitlab
        .request_job(runner_token)
        .await?
        .map(|job| (job, permit)))
}

/// How the runner is stopping
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shutdown {
    Running,
    /// Take no new jobs, let running jobs finish (SIGQUIT)
    Graceful,
    /// Take no new jobs, stop running jobs and report them failed (SIGTERM/SIGINT)
    Abort,
}

/// Requests a shutdown of a running daemon
#[derive(Clone)]
pub struct ShutdownHandle(Arc<watch::Sender<Shutdown>>);

impl ShutdownHandle {
    /// Escalate the shutdown mode (a graceful request never undoes an abort)
    pub fn request(&self, mode: Shutdown) {
        self.0.send_if_modified(|current| {
            let escalates = mode > *current;
            if escalates {
                *current = mode;
            }
            escalates
        });
    }
}

/// Resolve once an abort shutdown is requested
async fn abort_requested(shutdown: &mut watch::Receiver<Shutdown>) {
    // Drop the borrowed value right away: it must not be held across awaits
    let _ = shutdown
        .wait_for(|mode| *mode == Shutdown::Abort)
        .await
        .map(|_| ());
}

/// Interval between keep-alive updates that also detect remote cancellation
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct RunnerDaemon {
    cache: job_cache::LocalCache,
    config: Arc<config::RunnerConfig>,
    gitlab: Arc<GitLabClient>,
    executor: Arc<executor::ExecutorType>,
    semaphore: Arc<Semaphore>,
    scrubber: Arc<SecretScrubber>,
    shutdown: Arc<watch::Sender<Shutdown>>,
    heartbeat_interval: Duration,
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
            shutdown: Arc::new(watch::Sender::new(Shutdown::Running)),
            heartbeat_interval: HEARTBEAT_INTERVAL,
        }
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle(self.shutdown.clone())
    }

    async fn shutdown_requested(&self) {
        let mut rx = self.shutdown.subscribe();
        let _ = rx.wait_for(|mode| *mode != Shutdown::Running).await;
    }

    /// Start the runner daemon
    pub async fn start(&self) -> Result<()> {
        info!("🚀 TurboCI Runner Daemon starting...");
        info!("   Concurrent jobs: {}", self.config.concurrent);
        info!("   Check interval: {}s", self.config.check_interval);
        info!("   Cache enabled: {}", self.config.cache_enabled);

        self.executor.sweep_orphans().await;

        let mut connected = false;
        loop {
            // Wait for a free slot; a shutdown interrupts only this wait and the
            // sleeps, never a job request GitLab may already have answered
            let permit = tokio::select! {
                permit = self.semaphore.clone().acquire_owned() => permit?,
                _ = self.shutdown_requested() => break,
            };
            let claimed = claim_with_permit(permit, &self.gitlab, &self.config.runner_token).await;
            if claimed.is_ok() && !connected {
                connected = true;
                info!(
                    "✅ Connected to GitLab at {}, waiting for jobs",
                    self.config.gitlab_url
                );
            }
            let idle = match claimed {
                Ok(Some((job, permit))) => {
                    let job_name = job
                        .job_info
                        .as_ref()
                        .map(|ji| ji.name.as_str())
                        .unwrap_or("unknown");
                    info!("📦 Received job #{} ({})", job.id, job_name);

                    let daemon = self.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = daemon.execute_job(job).await {
                            error!("Job execution failed: {}", e);
                        }
                    });
                    false
                }
                Ok(None) => true,
                Err(e) => {
                    warn!("Failed to request job: {:#}", e);
                    true
                }
            };
            if idle {
                tokio::select! {
                    _ = sleep(Duration::from_secs(self.config.check_interval)) => {}
                    _ = self.shutdown_requested() => break,
                }
            }
        }

        info!("⏳ Waiting for running jobs to finish...");
        let _ = self.semaphore.acquire_many(self.config.concurrent).await;
        info!("✅ Runner stopped");
        Ok(())
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
        let cancel = CancelSignal::default();
        let mut trace = TraceWriter::new(Some(&self.gitlab), job.id, &job.token, &scrubber)
            .with_cancel(cancel.clone());
        let heartbeat = self.spawn_heartbeat(&job, cancel.clone());
        trace
            .write(&format!(
                "Running with TurboCI {}\n",
                env!("CARGO_PKG_VERSION")
            ))
            .await;

        let project_dir = self.executor.job_dir(job.id).join("project");
        let mut outcome = tokio::fs::create_dir_all(&project_dir)
            .await
            .map_err(|e| JobFailure::system(format!("Failed to create workspace: {}", e)));
        if outcome.is_ok() {
            outcome = self
                .executor
                .execute(&job, &mut trace, &WorkspaceRestore(self))
                .await;
        }
        // Aborted jobs (runner shutdown, or gone from GitLab) upload nothing
        if cancel.state() != RemoteState::Aborted {
            // An upload failure fails a job that otherwise succeeded (the next
            // stage would miss its inputs); the job's own failure takes precedence
            let uploaded = self
                .upload_artifacts(&job, &mut trace, outcome.is_ok())
                .await;
            if outcome.is_ok() {
                outcome = uploaded;
            }
            self.upload_cache(&job, &mut trace, outcome.is_ok()).await;
        }
        self.executor.cleanup(job.id).await;
        heartbeat.abort();

        // Jobs stopped by a runner shutdown are the runner's failure, not the user's
        let stopped_by_shutdown = *self.shutdown.borrow() == Shutdown::Abort;
        let outcome = outcome.map_err(|mut failure| {
            if stopped_by_shutdown && failure.reason == FailureReason::JobCanceled {
                failure.reason = FailureReason::RunnerSystemFailure;
                failure.message = "the runner is shutting down".to_string();
            }
            failure
        });

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

        if cancel.state() == RemoteState::Aborted && !stopped_by_shutdown {
            // Canceled in GitLab: it already has the final state and rejects updates
            info!("🛑 Job #{} was canceled", job.id);
            return Ok(());
        }
        self.gitlab
            .update_job(job.id, &job.token, state, reason, exit_code)
            .await
    }

    /// Keep-alive for the job: learns about remote cancellation even when the job
    /// prints nothing, and aborts the job when the runner is told to abort
    fn spawn_heartbeat(&self, job: &Job, cancel: CancelSignal) -> tokio::task::JoinHandle<()> {
        let gitlab = self.gitlab.clone();
        let mut shutdown = self.shutdown.subscribe();
        let interval = self.heartbeat_interval;
        let (job_id, token) = (job.id, job.token.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sleep(interval) => {
                        if let Ok(state) = gitlab.touch_job(job_id, &token).await {
                            cancel.update(state);
                        }
                    }
                    _ = abort_requested(&mut shutdown) => {
                        cancel.update(RemoteState::Aborted);
                        return;
                    }
                }
            }
        })
    }

    /// Restore cache and dependency artifacts into the checked-out project
    async fn restore_workspace(&self, job: &Job, trace: &mut TraceWriter<'_>) -> JobOutcome {
        let project_dir = self.executor.job_dir(job.id).join("project");
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

        // Jobs without artifacts have nothing to download; a failed download
        // fails the job, as the script would run without its inputs
        for dependency in job
            .dependencies
            .iter()
            .filter(|d| d.artifacts_file.is_some())
        {
            trace
                .write(&format!(
                    "Downloading artifacts from job #{} ({})...\n",
                    dependency.id, dependency.name
                ))
                .await;
            let cancel = trace.cancel();
            let download = artifacts::download_and_extract_artifacts(
                &self.gitlab,
                dependency.id,
                &dependency.token,
                &workspace,
            );
            let result = tokio::select! {
                result = download => result,
                _ = cancel.reached(RemoteState::Aborted) => {
                    return Err(JobFailure {
                        reason: FailureReason::JobCanceled,
                        exit_code: None,
                        message: "canceled".to_string(),
                    });
                }
            };
            result.map_err(|e| JobFailure {
                reason: FailureReason::ScriptFailure,
                exit_code: None,
                message: format!(
                    "failed to download artifacts from job #{} ({}): {:#}",
                    dependency.id, dependency.name, e
                ),
            })?;
        }
        Ok(())
    }

    /// Upload artifacts whose `when` matches the job result (default: on_success)
    async fn upload_artifacts(
        &self,
        job: &Job,
        trace: &mut TraceWriter<'_>,
        succeeded: bool,
    ) -> JobOutcome {
        let Some(ref job_artifacts) = job.artifacts else {
            return Ok(());
        };
        let mut failure = None;
        // Paths, exclude patterns and names may use variables
        let variables = job_variables(job);
        let expand_all = |items: &[String]| -> Vec<String> {
            items
                .iter()
                .map(|item| script::expand(item, &variables))
                .collect()
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
            let name = artifact
                .name
                .as_deref()
                .map(|name| script::expand(name, &variables))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "artifacts".to_string());
            let name = name.as_str();
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
                &expand_all(&artifact.paths),
                &expand_all(&artifact.exclude),
                format,
            )
            .await
            {
                Ok(None) => {
                    trace
                        .write("WARNING: No files matched the artifact paths, nothing uploaded\n")
                        .await;
                    continue;
                }
                Ok(Some(data)) => {
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
                    warn!("Failed to upload artifact {}: {:#}", name, e);
                    trace
                        .write(&format!("ERROR: Uploading artifacts failed: {:#}\n", e))
                        .await;
                    failure.get_or_insert(JobFailure {
                        reason: FailureReason::ScriptFailure,
                        exit_code: None,
                        message: format!("uploading artifacts ({}) failed: {:#}", name, e),
                    });
                }
            }
        }
        failure.map_or(Ok(()), Err)
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
                    &cache_entry
                        .paths
                        .iter()
                        .map(|path| script::expand(path, &variables))
                        .collect::<Vec<_>>(),
                )
                .await
            {
                Ok(Some(size)) => {
                    trace
                        .write(&format!("Created cache {} ({} bytes)\n", key, size))
                        .await
                }
                Ok(None) => {
                    trace
                        .write(&format!(
                            "No files matched the paths of cache {}, not saved\n",
                            key
                        ))
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
    async fn artifact_paths_and_names_use_variables() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 81, "token": "t",
            "variables": [{"key": "OUT", "value": "build/release", "public": true},
                          {"key": "FLAVOR", "value": "linux", "public": true}],
            "steps": [{"name": "script", "when": "on_success",
                       "script": ["mkdir -p build/release", "echo bin > build/release/app", "echo map > build/release/app.map"]}],
            "artifacts": [{"name": "app-$FLAVOR", "paths": ["$OUT/"], "exclude": ["$OUT/*.map"]}]
        }))
        .unwrap();

        daemon.execute_job(job).await.unwrap();

        assert_eq!(final_update(&server, 81).await["state"], "success");
        let uploads = requests(&server, "POST", 81).await;
        assert_eq!(uploads.len(), 1);
        let body = String::from_utf8_lossy(&uploads[0].body);
        assert!(
            body.contains("filename=\"app-linux.zip\""),
            "{}",
            &body[..200.min(body.len())]
        );
        assert!(body.contains("build/release/app"));
        assert!(!body.contains("app.map"));
    }

    #[tokio::test]
    async fn failed_artifact_upload_fails_the_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(
                r"^/api/v4/jobs/\d+/artifacts$",
            ))
            .respond_with(ResponseTemplate::new(413))
            .with_priority(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;

        daemon
            .execute_job(lifecycle_job(
                61,
                &["mkdir -p out", "echo x > out/a.txt"],
                "on_success",
            ))
            .await
            .unwrap();

        let update = final_update(&server, 61).await;
        assert_eq!(update["state"], "failed");
        assert_eq!(update["failure_reason"], "script_failure");
        assert!(trace_of(&server, 61)
            .await
            .contains("ERROR: Uploading artifacts failed"));
    }

    #[tokio::test]
    async fn dependency_artifacts_are_required_only_when_they_exist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v4/jobs/5/artifacts"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let with_deps = |id: u64, deps: serde_json::Value| -> Job {
            serde_json::from_value(serde_json::json!({
                "id": id, "token": "t",
                "steps": [{"name": "script", "script": ["echo script-ran"], "when": "on_success"}],
                "dependencies": deps
            }))
            .unwrap()
        };

        // A dependency without artifacts is skipped silently
        daemon
            .execute_job(with_deps(
                71,
                serde_json::json!([{"id": 4, "name": "lint", "token": "d"}]),
            ))
            .await
            .unwrap();
        assert_eq!(final_update(&server, 71).await["state"], "success");
        assert!(requests(&server, "GET", 4).await.is_empty());

        // A failed download of existing artifacts fails the job before its script
        daemon
            .execute_job(with_deps(
                72,
                serde_json::json!([{"id": 5, "name": "build", "token": "d",
                "artifacts_file": {"filename": "artifacts.zip", "size": 10}}]),
            ))
            .await
            .unwrap();
        assert_eq!(final_update(&server, 72).await["state"], "failed");
        let trace = trace_of(&server, 72).await;
        assert!(
            trace.contains("failed to download artifacts from job #5"),
            "{}",
            trace
        );
        assert!(!trace.contains("\nscript-ran"), "{}", trace);
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
    async fn job_canceled_in_gitlab_stops_and_runs_after_script() {
        let server = MockServer::start().await;
        // Keep-alive updates learn that the job is being canceled
        Mock::given(method("PUT"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"state": "running"}),
            ))
            .respond_with(ResponseTemplate::new(200).insert_header("Job-Status", "canceling"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = daemon(&server, dir.path()).await;
        daemon.heartbeat_interval = Duration::from_millis(200);
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 41, "token": "job-token-41",
            "steps": [
                {"name": "script", "script": ["sleep 30"], "when": "on_success", "timeout": 60},
                {"name": "after_script", "script": ["echo cleanup-ran"], "when": "always",
                 "allow_failure": true, "timeout": 10}
            ]
        }))
        .unwrap();

        let started = std::time::Instant::now();
        daemon.execute_job(job).await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        let trace = trace_of(&server, 41).await;
        assert!(trace.contains("cleanup-ran"), "{}", trace);
        assert!(trace.contains("ERROR: Job failed: canceled"), "{}", trace);
    }

    #[tokio::test]
    async fn abort_shutdown_fails_running_job_as_runner_failure() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let job = lifecycle_job(51, &["sleep 30"], "always");
        let handle = daemon.shutdown_handle();
        tokio::spawn(async move {
            sleep(Duration::from_millis(500)).await;
            handle.request(Shutdown::Abort);
        });

        let started = std::time::Instant::now();
        daemon.execute_job(job).await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        let update = final_update(&server, 51).await;
        assert_eq!(update["state"], "failed");
        assert_eq!(update["failure_reason"], "runner_system_failure");
        assert!(trace_of(&server, 51)
            .await
            .contains("the runner is shutting down"));
        assert!(
            requests(&server, "POST", 51).await.is_empty(),
            "aborted jobs upload nothing, even artifacts with when: always"
        );
    }

    #[tokio::test]
    async fn start_returns_after_shutdown_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let daemon = daemon(&server, dir.path()).await;
        let handle = daemon.shutdown_handle();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            handle.request(Shutdown::Graceful);
        });

        tokio::time::timeout(Duration::from_secs(5), daemon.start())
            .await
            .expect("daemon stopped")
            .unwrap();
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
