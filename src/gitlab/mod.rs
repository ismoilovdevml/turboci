use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Deserializer, Serialize};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Deserialize null JSON values as None for Option<String>
fn deserialize_null_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<serde_json::Value>::deserialize(deserializer)?;
    match opt {
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(other) => Ok(Some(other.to_string())),
        None => Ok(None),
    }
}

use uuid::Uuid;

/// Deserialize `null` as the type's default (GitLab sends null for some string fields)
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Accept either `"always"` or `["always", ...]` (GitLab sends a list)
fn string_or_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(OneOrMany::One(value)) => vec![value],
        Some(OneOrMany::Many(values)) => values,
    })
}

fn default_cache_policy() -> String {
    "pull-push".to_string()
}

/// Describe a JSON parse error without echoing payload values (they may be secrets)
fn describe_parse_error(e: &serde_json::Error) -> String {
    format!(
        "{:?} error at line {} column {}",
        e.classify(),
        e.line(),
        e.column()
    )
}

/// The minimum needed to report a job we could not parse as failed
#[derive(Deserialize)]
struct JobIdentity {
    id: u64,
    token: String,
}

#[derive(Debug, Clone)]
pub struct GitLabClient {
    client: Client,
    url: String,
    #[allow(dead_code)]
    token: String,
    /// Identifies this runner manager to GitLab; must stay the same across requests
    system_id: String,
    /// Executor name reported to GitLab ("docker" or "shell")
    executor: String,
}

impl GitLabClient {
    pub fn new(url: String, token: String) -> Self {
        Self {
            client: crate::net::client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client with the default TLS backend"),
            url,
            token,
            system_id: format!("r_{}", &Uuid::new_v4().simple().to_string()[..12]),
            executor: "docker".to_string(),
        }
    }

    /// Report the configured executor and its capabilities to GitLab
    pub fn with_executor(mut self, executor: &str) -> Self {
        self.executor = executor.to_string();
        self
    }

    /// Also trust `pem` (the CA of a GitLab server with a private certificate)
    pub fn with_root_certificate(mut self, pem: &[u8]) -> Result<Self> {
        let certificates =
            reqwest::Certificate::from_pem_bundle(pem).context("Invalid CA certificate (PEM)")?;
        if certificates.is_empty() {
            anyhow::bail!("The CA file contains no PEM certificate");
        }
        let builder = certificates.into_iter().fold(
            crate::net::client_builder().timeout(std::time::Duration::from_secs(30)),
            |builder, certificate| builder.add_root_certificate(certificate),
        );
        self.client = builder.build().context("Failed to build HTTP client")?;
        Ok(self)
    }

    /// Use a persisted system ID (see `runner_daemon::system_id`)
    pub fn with_system_id(mut self, system_id: String) -> Self {
        self.system_id = system_id;
        self
    }

    /// Retry helper with exponential backoff (3 attempts)
    async fn retry_with_backoff<F, Fut, T>(&self, operation: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let max_attempts = 3;
        let mut last_error = None;

        for attempt in 1..=max_attempts {
            match operation().await {
                Ok(result) => return Ok(result),
                // e.g. 413 or 403: the same request will fail the same way again
                Err(e) if e.is::<Permanent>() => return Err(e),
                Err(e) => {
                    last_error = Some(e);
                    if attempt < max_attempts {
                        let backoff = Duration::from_millis(100 * 2_u64.pow(attempt - 1));
                        warn!(
                            "Operation failed (attempt {}/{}), retrying in {:?}",
                            attempt, max_attempts, backoff
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    /// Request a new job from GitLab (GitLab 17.x compatible)
    pub async fn request_job(&self, runner_token: &str) -> Result<Option<Job>> {
        let url = format!("{}/api/v4/jobs/request", self.url);

        debug!("Requesting job from: {}", url);

        let response = self
            .client
            .post(&url)
            .json(&JobRequest {
                token: runner_token.to_string(),
                last_update: None,
                info: RunnerInfo::new(&self.executor),
                system_id: self.system_id.clone(),
                session: None,
            })
            .send()
            .await
            .context("Failed to request job from GitLab")?;

        let status = response.status();
        debug!("Job request response status: {}", status);

        match status {
            StatusCode::NO_CONTENT => {
                // No jobs available
                debug!("No jobs available");
                Ok(None)
            }
            StatusCode::OK | StatusCode::CREATED => {
                // Get response text first for debugging
                let response_text = response
                    .text()
                    .await
                    .context("Failed to read response text")?;

                // The payload carries the job token and secret variables: never log it
                let job: Job = match serde_json::from_str(&response_text) {
                    Ok(job) => job,
                    Err(e) => {
                        let reason = describe_parse_error(&e);
                        // The job is already assigned to us; fail it instead of leaving it running
                        if let Ok(identity) = serde_json::from_str::<JobIdentity>(&response_text) {
                            let trace =
                                format!("TurboCI could not parse the job payload: {}\n", reason);
                            let _ = self
                                .patch_trace(identity.id, &identity.token, trace.as_bytes(), 0)
                                .await;
                            if let Err(update_err) = self
                                .update_job(
                                    identity.id,
                                    &identity.token,
                                    JobState::Failed,
                                    Some(FailureReason::RunnerSystemFailure),
                                    None,
                                )
                                .await
                            {
                                warn!(
                                    "Failed to fail unparseable job #{}: {}",
                                    identity.id, update_err
                                );
                            }
                        }
                        return Err(anyhow::anyhow!("Failed to parse job response: {}", reason));
                    }
                };
                info!("Received job #{}", job.id);
                Ok(Some(job))
            }
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                warn!("Authentication failed - invalid runner token");
                Err(anyhow::anyhow!("Invalid runner token"))
            }
            _ => {
                let error_text = response.text().await.unwrap_or_default();
                warn!("Unexpected response: {} - {}", status, error_text);
                Err(anyhow::anyhow!("Job request failed: {}", status))
            }
        }
    }

    /// Report the job state to GitLab. Retries while GitLab answers 202 (accepted,
    /// not yet processed) or on server/network errors; gives up on 403/404, which
    /// mean the job is gone or no longer ours.
    pub async fn update_job(
        &self,
        job_id: u64,
        token: &str,
        state: JobState,
        failure_reason: Option<FailureReason>,
        exit_code: Option<i32>,
    ) -> Result<()> {
        const MAX_ATTEMPTS: u32 = 8;
        let url = format!("{}/api/v4/jobs/{}", self.url, job_id);

        let mut body = serde_json::json!({ "token": token, "state": state });
        if let Some(reason) = failure_reason.filter(|r| *r != FailureReason::JobCanceled) {
            body["failure_reason"] = serde_json::to_value(reason)?;
        }
        if let Some(code) = exit_code {
            body["exit_code"] = code.into();
        }

        let mut last_error = anyhow::anyhow!("Job update not attempted");
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                let delay = Duration::from_millis(250 * 2u64.pow((attempt - 1).min(4)));
                tokio::time::sleep(delay).await;
            }
            let response = match self.client.put(&url).json(&body).send().await {
                Ok(response) => response,
                Err(e) => {
                    last_error = anyhow::Error::new(e).context("Failed to update job status");
                    continue;
                }
            };
            match response.status() {
                StatusCode::OK => return Ok(()),
                StatusCode::ACCEPTED => {
                    last_error = anyhow::anyhow!("Job update accepted but not completed");
                }
                status @ (StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) => {
                    return Err(anyhow::anyhow!("Job update rejected: {}", status));
                }
                status if status.is_server_error() => {
                    last_error = anyhow::anyhow!("Job update failed: {}", status);
                }
                status => {
                    let error = response.text().await.unwrap_or_default();
                    return Err(anyhow::anyhow!("Job update failed: {} {}", status, error));
                }
            }
        }
        Err(last_error)
    }

    /// Append to the job trace at `offset` (Content-Range is inclusive: offset-(end-1)).
    /// Returns the new end offset, or on 416 the length GitLab already has.
    pub async fn patch_trace(
        &self,
        job_id: u64,
        token: &str,
        trace: &[u8],
        offset: usize,
    ) -> Result<TracePatch> {
        if trace.is_empty() {
            return Ok(TracePatch {
                offset,
                remote: RemoteState::Running,
                update_interval: None,
            });
        }
        let url = format!("{}/api/v4/jobs/{}/trace", self.url, job_id);
        let end_offset = offset + trace.len();
        let trace_owned = trace.to_vec();
        let token_owned = token.to_string();

        self.retry_with_backoff(|| async {
            let response = self
                .client
                .patch(&url)
                .header("JOB-TOKEN", &token_owned)
                .header("Content-Range", format!("{}-{}", offset, end_offset - 1))
                .header("Content-Type", "text/plain")
                .body(trace_owned.clone())
                .send()
                .await
                .context("Failed to stream trace")?;

            let status = response.status();
            let remote = RemoteState::from_response(&response);
            let update_interval = update_interval(&response);

            if status == StatusCode::FORBIDDEN {
                // The job is no longer running (canceled, or not ours any more)
                return Ok(TracePatch {
                    offset,
                    remote,
                    update_interval,
                });
            }

            // Handle 416 Range Not Satisfiable - parse server offset
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                if let Some(server_offset) = response
                    .headers()
                    .get("Range")
                    .and_then(|range| range.to_str().ok())
                    .and_then(|range| range.split_once('-'))
                    .and_then(|(_, end)| end.parse::<usize>().ok())
                {
                    debug!(
                        "Range mismatch: server at {}, we sent {}-{}",
                        server_offset,
                        offset,
                        end_offset - 1
                    );
                    return Ok(TracePatch {
                        offset: server_offset,
                        remote,
                        update_interval,
                    });
                }
                warn!("Range mismatch but couldn't parse server offset");
                return Err(anyhow::anyhow!("Range mismatch: {}", status));
            }

            if !status.is_success() {
                warn!(
                    "Trace streaming failed: {} - Range: {}-{}",
                    status,
                    offset,
                    end_offset - 1
                );
                return Err(anyhow::anyhow!("Trace streaming failed: {}", status));
            }

            Ok(TracePatch {
                offset: end_offset,
                remote,
                update_interval,
            })
        })
        .await
    }

    /// Tell GitLab the job is still running (a keep-alive for jobs with no new
    /// output) and learn whether it has been canceled meanwhile
    pub async fn touch_job(&self, job_id: u64, token: &str) -> Result<RemoteState> {
        let response = self
            .client
            .put(format!("{}/api/v4/jobs/{}", self.url, job_id))
            .json(&serde_json::json!({ "token": token, "state": "running" }))
            .send()
            .await
            .context("Failed to touch job")?;
        Ok(RemoteState::from_response(&response))
    }

    /// Upload an artifacts archive as gitlab-runner does: multipart `file` field,
    /// with format, type and expiry as query parameters
    pub async fn upload_artifacts(
        &self,
        job_id: u64,
        token: &str,
        upload: ArtifactUpload,
    ) -> Result<()> {
        let url = format!("{}/api/v4/jobs/{}/artifacts", self.url, job_id);
        let mut query = vec![
            ("artifact_format", upload.format.clone()),
            ("artifact_type", upload.artifact_type.clone()),
        ];
        if let Some(expire_in) = &upload.expire_in {
            query.push(("expire_in", expire_in.clone()));
        }

        self.retry_with_backoff(|| async {
            let file = tokio::fs::File::open(&upload.path)
                .await
                .with_context(|| format!("Failed to open {}", upload.path.display()))?;
            let part = reqwest::multipart::Part::stream_with_length(file, upload.len)
                .file_name(upload.file_name.clone())
                .mime_str("application/octet-stream")?;
            let response = self
                .client
                .post(&url)
                .query(&query)
                .header("JOB-TOKEN", token)
                .timeout(TRANSFER_TIMEOUT)
                .multipart(reqwest::multipart::Form::new().part("file", part))
                .send()
                .await
                .context("Failed to upload artifacts")?;

            match response.status() {
                StatusCode::CREATED | StatusCode::OK => Ok(()),
                StatusCode::PAYLOAD_TOO_LARGE => Err(Permanent(
                    "Artifact upload rejected: archive is larger than the instance limit"
                        .to_string(),
                )
                .into()),
                status => {
                    let error = response.text().await.unwrap_or_default();
                    let message = format!("Artifact upload failed: {} {}", status, error);
                    if status.is_client_error() {
                        Err(Permanent(message).into())
                    } else {
                        Err(anyhow::anyhow!(message))
                    }
                }
            }
        })
        .await?;

        info!("Artifacts uploaded for job #{}", job_id);
        Ok(())
    }

    /// Download the artifacts archive of job `job_id` (a dependency), authenticated
    /// with that dependency's token. Redirects to object storage are followed.
    pub async fn download_artifacts_to(
        &self,
        job_id: u64,
        token: &str,
        dest: &std::path::Path,
    ) -> Result<u64> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let url = format!("{}/api/v4/jobs/{}/artifacts", self.url, job_id);

        let response = self
            .client
            .get(&url)
            .header("JOB-TOKEN", token)
            .timeout(TRANSFER_TIMEOUT)
            .send()
            .await
            .context("Failed to download artifacts")?;

        match response.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => anyhow::bail!("job #{} has no artifacts", job_id),
            status => anyhow::bail!("Artifact download failed: {}", status),
        }
        // Streamed to disk: archives can be larger than the runner's memory
        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("Failed to create {}", dest.display()))?;
        let mut body = response.bytes_stream();
        let mut size = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.context("Failed to read artifact bytes")?;
            size += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(size)
    }
}

#[derive(Debug, Serialize)]
struct JobRequest {
    token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_update: Option<String>,
    info: RunnerInfo,
    system_id: String, // GitLab 17.x requirement
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionInfo>,
}

#[derive(Debug, Serialize, Clone)]
struct SessionInfo {
    url: Option<String>,
    certificate: Option<String>,
    authorization: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct RunnerInfo {
    name: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    platform: String,
    architecture: String,
    executor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    features: Option<RunnerFeatures>,
}

/// Capabilities sent to GitLab with every job request (field names follow
/// gitlab-runner's FeaturesInfo). Only what TurboCI implements is `true`, so
/// GitLab does not route jobs that rely on missing features here.
#[derive(Debug, Serialize, Clone, Default, PartialEq, Eq)]
struct RunnerFeatures {
    variables: bool,
    image: bool,
    services: bool,
    artifacts: bool,
    cache: bool,
    fallback_cache_keys: bool,
    shared: bool,
    upload_multiple_artifacts: bool,
    upload_raw_artifacts: bool,
    session: bool,
    terminal: bool,
    refspecs: bool,
    masking: bool,
    proxy: bool,
    raw_variables: bool,
    artifacts_exclude: bool,
    multi_build_steps: bool,
    trace_reset: bool,
    trace_checksum: bool,
    trace_size: bool,
    vault_secrets: bool,
    cancelable: bool,
    return_exit_code: bool,
    service_variables: bool,
    cancel_gracefully: bool,
}

impl RunnerFeatures {
    fn for_executor(executor: &str) -> Self {
        Self {
            variables: true,
            image: executor == "docker",
            services: executor == "docker",
            artifacts: true,
            cache: true,
            fallback_cache_keys: true,
            shared: executor == "shell",
            upload_multiple_artifacts: true,
            upload_raw_artifacts: true,
            refspecs: true,
            masking: true,
            raw_variables: true,
            artifacts_exclude: true,
            multi_build_steps: true,
            return_exit_code: true,
            cancelable: true,
            cancel_gracefully: true,
            ..Self::default()
        }
    }
}

impl RunnerInfo {
    fn new(executor: &str) -> Self {
        Self {
            name: "turboci-runner".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            revision: Some(env!("CARGO_PKG_VERSION").to_string()),
            platform: std::env::consts::OS.to_string(),
            architecture: std::env::consts::ARCH.to_string(),
            executor: executor.to_string(),
            features: Some(RunnerFeatures::for_executor(executor)),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Job {
    pub id: u64,
    pub token: String,
    #[serde(default)]
    pub allow_git_fetch: bool,
    #[serde(default)]
    pub job_info: Option<JobInfo>,
    #[serde(default)]
    pub git_info: Option<GitInfo>,
    #[serde(default)]
    pub runner_info: Option<RunnerVariables>,
    #[serde(default)]
    pub variables: Vec<Variable>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub image: Option<Image>,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub artifacts: Option<Vec<Artifact>>,
    #[serde(default)]
    pub cache: Vec<Cache>,
    #[serde(default)]
    pub credentials: Vec<Credential>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub timeout: u32, // Job timeout in seconds (0 = use default)
    #[serde(default)]
    pub inputs: Vec<serde_json::Value>,
    #[serde(default)]
    pub hooks: Vec<serde_json::Value>,
    #[serde(default)]
    pub features: Option<serde_json::Value>,
}

impl Job {
    /// Job timeout in seconds as configured in GitLab (sent in `runner_info.timeout`)
    pub fn timeout_secs(&self) -> Option<u64> {
        self.runner_info
            .as_ref()
            .and_then(|info| info.timeout)
            .map(u64::from)
            .or(Some(u64::from(self.timeout)))
            .filter(|&secs| secs > 0)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobInfo {
    #[serde(default)]
    pub id: u64,
    pub name: String,
    pub stage: String,
    pub project_id: u64,
    pub project_name: String,
    #[serde(default)]
    pub time_in_queue_seconds: Option<f64>,
    #[serde(default)]
    pub project_jobs_running_on_instance_runners_count: Option<String>,
    #[serde(default)]
    pub queue_size: Option<u64>,
    #[serde(default)]
    pub queue_depth: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitInfo {
    pub repo_url: String,
    #[serde(rename = "ref", alias = "ref_name")]
    pub ref_name: String,
    pub ref_type: String,
    pub sha: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub before_sha: String,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub refspecs: Vec<String>,
    #[serde(default)]
    pub repo_object_format: Option<String>,
    #[serde(default)]
    pub protected: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunnerVariables {
    #[serde(default)]
    pub ci_concurrent_id: Option<u32>,
    #[serde(default)]
    pub ci_concurrent_project_id: Option<u32>,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub runner_session_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Variable {
    pub key: String,
    #[serde(default, deserialize_with = "deserialize_null_string")]
    pub value: Option<String>,
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub masked: bool,
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub file: bool,
    #[serde(default)]
    pub internal: bool,
    #[serde(default, rename = "variable_type")]
    pub variable_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Step {
    pub name: String,
    #[serde(default)]
    pub script: Vec<String>,
    #[serde(default)]
    pub before_script: Vec<String>,
    #[serde(default)]
    pub after_script: Vec<String>,
    #[serde(default)]
    pub timeout: u32,
    #[serde(default)]
    pub when: String,
    #[serde(default)]
    pub allow_failure: bool,
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Image {
    pub name: String,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub ports: Vec<serde_json::Value>,
    #[serde(default)]
    pub executor_opts: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "string_or_list")]
    pub pull_policy: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    pub name: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default, deserialize_with = "string_or_list")]
    pub pull_policy: Vec<String>,
    /// Service-level `variables:`
    #[serde(default)]
    pub variables: Vec<Variable>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Artifact {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub untracked: bool,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub expire_in: Option<String>,
    #[serde(default)]
    pub artifact_type: Option<String>,
    #[serde(default)]
    pub artifact_format: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Cache {
    pub key: String,
    #[serde(default)]
    pub untracked: Option<bool>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "default_cache_policy")]
    pub policy: String,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub fallback_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Credential {
    #[serde(rename = "type")]
    pub cred_type: String,
    pub url: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Dependency {
    pub id: u64,
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub token: String,
    /// Absent when the dependency produced no artifacts
    #[serde(default)]
    pub artifacts_file: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetryConfig {
    #[serde(default)]
    pub max: u32,
    #[serde(default)]
    pub when: Vec<String>,
}

/// Timeout for artifact transfers (the client default of 30s is for API calls)
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(3600);

/// An error that retrying cannot fix (4xx responses)
#[derive(Debug)]
struct Permanent(String);

impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Permanent {}

/// An archive to upload as job artifacts
#[derive(Debug, Clone)]
pub struct ArtifactUpload {
    /// Archive file on disk; it is streamed, never loaded into memory
    pub path: std::path::PathBuf,
    pub len: u64,
    /// File name of the archive, e.g. `artifacts.zip`
    pub file_name: String,
    /// `zip`, `gzip` or `raw`
    pub format: String,
    /// `archive`, or a report type such as `junit`
    pub artifact_type: String,
    pub expire_in: Option<String>,
}

/// What GitLab says about a running job, from the `Job-Status` header of trace
/// and update responses (as gitlab-runner interprets it)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RemoteState {
    Running,
    /// Canceled in the UI: stop the script, still run after_script
    Canceling,
    /// Canceled, failed or no longer ours: stop everything
    Aborted,
}

impl RemoteState {
    fn from_response(response: &reqwest::Response) -> Self {
        if response.status() == StatusCode::FORBIDDEN {
            return RemoteState::Aborted;
        }
        match response
            .headers()
            .get("Job-Status")
            .and_then(|value| value.to_str().ok())
        {
            Some("canceling") => RemoteState::Canceling,
            Some("canceled") | Some("failed") => RemoteState::Aborted,
            _ => RemoteState::Running,
        }
    }
}

/// Result of appending to the trace
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TracePatch {
    /// End of what GitLab has; after a 416 this is where to resend from
    pub offset: usize,
    pub remote: RemoteState,
    /// How often GitLab wants trace updates (X-GitLab-Trace-Update-Interval):
    /// longer while nobody watches the log, shorter while someone does
    pub update_interval: Option<std::time::Duration>,
}

fn update_interval(response: &reqwest::Response) -> Option<std::time::Duration> {
    response
        .headers()
        .get("X-GitLab-Trace-Update-Interval")?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
}

/// Why a job failed, as GitLab expects it in `failure_reason`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    ScriptFailure,
    RunnerSystemFailure,
    JobExecutionTimeout,
    ImagePullFailure,
    /// Runner-internal (like gitlab-runner's job_canceled): never sent to GitLab
    JobCanceled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Pending,
    Running,
    Success,
    Failed,
    Canceled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Job payload from gitlab-runner's network tests (JobResponse fixture)
    fn upstream_job() -> serde_json::Value {
        serde_json::from_str(include_str!("testdata/job_response.json")).unwrap()
    }

    fn parse(v: serde_json::Value) -> Job {
        serde_json::from_value(v).expect("valid GitLab job payload must parse")
    }

    #[test]
    fn parses_upstream_job_payload() {
        let job = parse(upstream_job());

        assert_eq!(job.id, 10);
        assert_eq!(job.timeout_secs(), Some(3600));
        assert_eq!(job.steps.len(), 2);
        assert_eq!(job.dependencies[0].token, "other-job-token");
    }

    #[test]
    fn parses_payload_variants_gitlab_sends() {
        let mut v = upstream_job();
        v["job_info"]["time_in_queue_seconds"] = serde_json::json!(12.5);
        parse(v);

        let mut v = upstream_job();
        v["cache"][0].as_object_mut().unwrap().remove("policy");
        parse(v);

        let mut v = upstream_job();
        v["git_info"]["before_sha"] = serde_json::Value::Null;
        parse(v);

        let mut v = upstream_job();
        v["dependencies"][0]
            .as_object_mut()
            .unwrap()
            .remove("token");
        parse(v);

        let mut v = upstream_job();
        v["image"]["pull_policy"] = serde_json::json!(["always", "if-not-present"]);
        v["services"][0]["pull_policy"] = serde_json::json!("if-not-present");
        let job = parse(v);
        assert_eq!(
            job.image.unwrap().pull_policy,
            vec!["always", "if-not-present"]
        );
        assert_eq!(job.services[0].pull_policy, vec!["if-not-present"]);
        assert_eq!(
            job.services[0].command.as_deref(),
            Some(&["sleep".to_string(), "30".to_string()][..])
        );

        let mut v = upstream_job();
        v["image"] = serde_json::Value::Null;
        v["services"] = serde_json::json!([{"name": "redis:7"}]);
        parse(v);

        let mut v = upstream_job();
        v["artifacts"] = serde_json::json!([{"artifact_type": "junit", "artifact_format": "gzip",
            "paths": ["r.xml"], "when": "always", "exclude": ["a"]}]);
        parse(v);
    }

    #[test]
    fn timeout_comes_from_runner_info() {
        let mut v = upstream_job();
        v["runner_info"]["timeout"] = serde_json::json!(10800);
        assert_eq!(parse(v).timeout_secs(), Some(10800));

        let mut v = upstream_job();
        v.as_object_mut().unwrap().remove("runner_info");
        assert_eq!(parse(v).timeout_secs(), None);
    }

    fn client(server: &MockServer) -> GitLabClient {
        GitLabClient::new(server.uri(), "runner-token".to_string())
    }

    #[tokio::test]
    async fn request_job_returns_none_when_no_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .and(body_partial_json(
                serde_json::json!({"token": "runner-token"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let job = client(&server).request_job("runner-token").await.unwrap();

        assert!(job.is_none());
    }

    #[tokio::test]
    async fn request_job_parses_assigned_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 42,
                "token": "job-token",
                "steps": [{"name": "script", "script": ["echo hi"], "timeout": 3600,
                           "when": "on_success", "allow_failure": false}]
            })))
            .mount(&server)
            .await;

        let job = client(&server)
            .request_job("runner-token")
            .await
            .unwrap()
            .expect("job expected");

        assert_eq!(job.id, 42);
        assert_eq!(job.token, "job-token");
        assert_eq!(job.steps.len(), 1);
    }

    #[tokio::test]
    async fn request_job_sends_same_system_id_on_every_poll() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .and(body_partial_json(
                serde_json::json!({"system_id": "s_0123456789ab"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(2)
            .mount(&server)
            .await;
        let client = client(&server).with_system_id("s_0123456789ab".to_string());

        client.request_job("runner-token").await.unwrap();
        client.request_job("runner-token").await.unwrap();
    }

    #[tokio::test]
    async fn unparseable_job_is_reported_failed_without_echoing_values() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 77, "token": "job-token", "steps": "super-secret-value"
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/jobs/77"))
            .and(body_partial_json(
                serde_json::json!({"token": "job-token", "state": "failed"}),
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let err = client(&server)
            .request_job("runner-token")
            .await
            .unwrap_err();

        assert!(
            !format!("{:#}", err).contains("super-secret-value"),
            "{:#}",
            err
        );
        let requests = server.received_requests().await.unwrap();
        let update = requests
            .iter()
            .find(|r| r.method.as_str() == "PUT")
            .unwrap();
        assert!(!String::from_utf8_lossy(&update.body).contains("super-secret-value"));
    }

    #[tokio::test]
    async fn request_job_reports_executor_and_only_implemented_features() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .and(body_partial_json(serde_json::json!({"info": {
                "executor": "shell",
                "features": {
                    "variables": true, "refspecs": true, "masking": true,
                    "return_exit_code": true, "image": false, "services": false,
                    "trace_checksum": false, "trace_reset": false, "artifacts_exclude": true
                }
            }})))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .with_executor("shell")
            .request_job("runner-token")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn trace_and_touch_report_remote_cancellation() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(202).insert_header("Job-Status", "canceling"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).insert_header("Job-Status", "canceled"))
            .mount(&server)
            .await;
        let client = client(&server);

        let patch = client.patch_trace(7, "t", b"x", 0).await.unwrap();
        assert_eq!(patch.remote, RemoteState::Canceling);
        assert_eq!(patch.offset, 1);
        assert_eq!(
            client.touch_job(7, "t").await.unwrap(),
            RemoteState::Aborted
        );
    }

    #[tokio::test]
    async fn trace_forbidden_means_job_is_gone() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;

        let patch = client(&server).patch_trace(7, "t", b"x", 0).await.unwrap();

        assert_eq!(patch.remote, RemoteState::Aborted);
        assert_eq!(patch.offset, 0);
    }

    #[test]
    fn valid_ca_certificate_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let (key, cert) = (dir.path().join("k.pem"), dir.path().join("c.pem"));
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=gitlab.test",
            ])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .unwrap()
            .status;
        assert!(status.success());

        let pem = std::fs::read(&cert).unwrap();
        assert!(GitLabClient::new("https://x".to_string(), "t".to_string())
            .with_root_certificate(&pem)
            .is_ok());
    }

    #[test]
    fn invalid_ca_certificate_is_rejected() {
        let result = GitLabClient::new("https://x".to_string(), "t".to_string())
            .with_root_certificate(b"not a certificate");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn request_job_fails_on_forbidden() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/request"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        assert!(client(&server).request_job("bad-token").await.is_err());
    }

    #[tokio::test]
    async fn patch_trace_sends_inclusive_content_range_and_job_token() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v4/jobs/7/trace"))
            .and(header("JOB-TOKEN", "job-token"))
            .and(header("Content-Range", "10-14"))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        let next = client(&server)
            .patch_trace(7, "job-token", b"hello", 10)
            .await
            .unwrap();

        assert_eq!(next.offset, 15);
        assert_eq!(next.remote, RemoteState::Running);
    }

    #[tokio::test]
    async fn patch_trace_returns_server_offset_on_range_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v4/jobs/7/trace"))
            .respond_with(ResponseTemplate::new(416).insert_header("Range", "0-3"))
            .mount(&server)
            .await;

        let next = client(&server)
            .patch_trace(7, "job-token", b"hello", 10)
            .await
            .unwrap();

        assert_eq!(next.offset, 3);
    }

    #[tokio::test]
    async fn update_job_sends_token_and_state() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/jobs/7"))
            .and(body_partial_json(
                serde_json::json!({"token": "job-token", "state": "success"}),
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .update_job(7, "job-token", JobState::Success, None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn upload_artifacts_sends_multipart_file_with_query_parameters() {
        use wiremock::matchers::{header_exists, query_param};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/jobs/7/artifacts"))
            .and(header("JOB-TOKEN", "job-token"))
            .and(query_param("artifact_format", "zip"))
            .and(query_param("artifact_type", "archive"))
            .and(query_param("expire_in", "1 week"))
            .and(header_exists("content-type"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let staged = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(staged.path(), b"PK-zip-bytes").unwrap();
        client(&server)
            .upload_artifacts(
                7,
                "job-token",
                ArtifactUpload {
                    path: staged.path().to_path_buf(),
                    len: 12,
                    file_name: "artifacts.zip".to_string(),
                    format: "zip".to_string(),
                    artifact_type: "archive".to_string(),
                    expire_in: Some("1 week".to_string()),
                },
            )
            .await
            .unwrap();

        let request = &server.received_requests().await.unwrap()[0];
        let content_type = request.headers["content-type"].to_str().unwrap();
        assert!(
            content_type.starts_with("multipart/form-data"),
            "{}",
            content_type
        );
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            body.contains("name=\"file\"; filename=\"artifacts.zip\""),
            "{}",
            body
        );
        assert!(body.contains("PK-zip-bytes"));
    }

    #[tokio::test]
    async fn rejected_artifact_upload_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(413))
            .expect(1)
            .mount(&server)
            .await;
        let staged = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(staged.path(), b"zip").unwrap();

        let result = client(&server)
            .upload_artifacts(
                7,
                "t",
                ArtifactUpload {
                    path: staged.path().to_path_buf(),
                    len: 3,
                    file_name: "a.zip".to_string(),
                    format: "zip".to_string(),
                    artifact_type: "archive".to_string(),
                    expire_in: None,
                },
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn download_artifacts_follows_redirect_to_object_storage() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v4/jobs/9/artifacts"))
            .and(header("JOB-TOKEN", "dep-token"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/storage/archive.zip", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/storage/archive.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"zip-data".to_vec()))
            .mount(&server)
            .await;

        let dest = tempfile::NamedTempFile::new().unwrap();
        let size = client(&server)
            .download_artifacts_to(9, "dep-token", dest.path())
            .await
            .unwrap();

        assert_eq!(size, 8);
        assert_eq!(std::fs::read(dest.path()).unwrap(), b"zip-data");
    }

    #[tokio::test]
    async fn update_job_sends_failure_reason_and_exit_code() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/jobs/7"))
            .and(body_partial_json(serde_json::json!({
                "state": "failed", "failure_reason": "script_failure", "exit_code": 3
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .update_job(
                7,
                "job-token",
                JobState::Failed,
                Some(FailureReason::ScriptFailure),
                Some(3),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_job_retries_while_accepted_and_on_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(202))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .update_job(7, "job-token", JobState::Success, None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_job_gives_up_when_job_is_gone() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;

        let result = client(&server)
            .update_job(7, "job-token", JobState::Success, None, None)
            .await;

        assert!(result.is_err());
    }
}
