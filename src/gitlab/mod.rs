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
}

impl GitLabClient {
    pub fn new(url: String, token: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            url,
            token,
            system_id: format!("r_{}", &Uuid::new_v4().simple().to_string()[..12]),
        }
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
                info: RunnerInfo::default(),
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
                            if let Err(update_err) = self
                                .update_job(
                                    identity.id,
                                    &identity.token,
                                    JobState::Failed,
                                    Some(&trace),
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

    /// Update job status with trace streaming
    pub async fn update_job(
        &self,
        job_id: u64,
        token: &str,
        state: JobState,
        trace: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/api/v4/jobs/{}", self.url, job_id);

        let mut body = serde_json::json!({
            "token": token,
            "state": state,
        });

        if let Some(trace_data) = trace {
            body["trace"] = serde_json::Value::String(trace_data.to_string());
        }

        let response = self
            .client
            .put(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to update job status")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Job update failed: {}", error));
        }

        Ok(())
    }

    /// Stream job trace (real-time logs) - GitLab 17.x with retry
    /// GitLab expects Content-Range format: "0-{size}" for total trace size
    pub async fn patch_trace(
        &self,
        job_id: u64,
        token: &str,
        trace: &str,
        offset: usize,
    ) -> Result<usize> {
        let url = format!("{}/api/v4/jobs/{}/trace", self.url, job_id);
        let trace_bytes = trace.as_bytes();
        let end_offset = offset + trace_bytes.len();
        let trace_owned = trace.to_string();
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

            // Handle 416 Range Not Satisfiable - parse server offset
            if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                if let Some(range_header) = response.headers().get("Range") {
                    if let Ok(range_str) = range_header.to_str() {
                        // Parse "0-123" format - take end offset
                        if let Some((_start, end)) = range_str.split_once('-') {
                            if let Ok(server_offset) = end.parse::<usize>() {
                                debug!(
                                    "Range mismatch: server at {}, we sent {}-{}",
                                    server_offset,
                                    offset,
                                    end_offset - 1
                                );
                                return Ok(server_offset);
                            }
                        }
                    }
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

            Ok(end_offset)
        })
        .await
    }

    /// Upload job artifacts (GitLab 17.x) with retry
    pub async fn upload_artifacts(
        &self,
        job_id: u64,
        token: &str,
        artifact_data: Vec<u8>,
        artifact_type: &str,
        expire_in: Option<&str>,
    ) -> Result<()> {
        let mut url = format!("{}/api/v4/jobs/{}/artifacts", self.url, job_id);

        if let Some(expiration) = expire_in {
            url = format!("{}?expire_in={}", url, expiration);
        }

        let token_owned = token.to_string();
        let artifact_type_owned = artifact_type.to_string();

        self.retry_with_backoff(|| async {
            let response = self
                .client
                .post(&url)
                .header("JOB-TOKEN", &token_owned)
                .header("Content-Type", "application/zip")
                .header("artifact-type", &artifact_type_owned)
                .body(artifact_data.clone())
                .send()
                .await
                .context("Failed to upload artifacts")?;

            if !response.status().is_success() {
                let error = response.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("Artifact upload failed: {}", error));
            }

            Ok(())
        })
        .await?;

        info!("Artifacts uploaded for job #{}", job_id);
        Ok(())
    }

    /// Download job artifacts (GitLab 17.x)
    #[allow(dead_code)]
    pub async fn download_artifacts(&self, job_id: u64, token: &str) -> Result<Vec<u8>> {
        let url = format!("{}/api/v4/jobs/{}/artifacts", self.url, job_id);

        let response = self
            .client
            .get(&url)
            .header("JOB-TOKEN", token)
            .send()
            .await
            .context("Failed to download artifacts")?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!("Artifact download failed"));
        }

        let bytes = response
            .bytes()
            .await
            .context("Failed to read artifact bytes")?;

        Ok(bytes.to_vec())
    }

    /// Upload cache archive (GitLab 17.x)
    /// Upload cache with retry
    pub async fn upload_cache(
        &self,
        job_id: u64,
        token: &str,
        key: &str,
        cache_data: Vec<u8>,
    ) -> Result<()> {
        let url = format!("{}/api/v4/jobs/{}/cache", self.url, job_id);
        let token_owned = token.to_string();
        let key_owned = key.to_string();

        self.retry_with_backoff(|| async {
            let response = self
                .client
                .post(&url)
                .header("JOB-TOKEN", &token_owned)
                .header("Cache-Key", &key_owned)
                .header("Content-Type", "application/zip")
                .body(cache_data.clone())
                .send()
                .await
                .context("Failed to upload cache")?;

            if !response.status().is_success() {
                return Err(anyhow::anyhow!(
                    "Cache upload failed: {}",
                    response.status()
                ));
            }

            Ok(())
        })
        .await?;

        info!("Cache uploaded: {}", key);
        Ok(())
    }

    /// Download cache archive (GitLab 17.x)
    pub async fn download_cache(
        &self,
        job_id: u64,
        token: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/api/v4/jobs/{}/cache?key={}", self.url, job_id, key);

        let response = self
            .client
            .get(&url)
            .header("JOB-TOKEN", token)
            .send()
            .await
            .context("Failed to download cache")?;

        match response.status() {
            StatusCode::OK => {
                let bytes = response.bytes().await?.to_vec();
                info!("Cache downloaded: {}", key);
                Ok(Some(bytes))
            }
            StatusCode::NOT_FOUND => {
                debug!("Cache not found: {}", key);
                Ok(None)
            }
            _ => {
                warn!("Cache download failed: {}", response.status());
                Ok(None)
            }
        }
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

#[derive(Debug, Serialize, Clone)]
struct RunnerFeatures {
    trace_checksum: bool,
    trace_size: bool,
    trace_reset: bool,
    trace_update_interval: bool,
    session: bool,
    terminal: bool,
    refspecs: bool,
    multi_build_steps: bool,
    vault_secrets: bool,
    return_exit_code: bool,
    raw_variables: bool,
    artifacts_exclude: bool,
    cancelable_stages: bool,
}

impl Default for RunnerFeatures {
    fn default() -> Self {
        Self {
            trace_checksum: true,
            trace_size: true,
            trace_reset: true,
            trace_update_interval: true,
            session: false,
            terminal: false,
            refspecs: true,
            multi_build_steps: true,
            vault_secrets: false,
            return_exit_code: true,
            raw_variables: true,
            artifacts_exclude: true,
            cancelable_stages: false,
        }
    }
}

impl Default for RunnerInfo {
    fn default() -> Self {
        Self {
            name: "turboci-runner".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            revision: Some(env!("CARGO_PKG_VERSION").to_string()),
            platform: std::env::consts::OS.to_string(),
            architecture: std::env::consts::ARCH.to_string(),
            executor: "docker".to_string(),
            features: Some(RunnerFeatures::default()),
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
    #[serde(default)]
    pub pull_policy: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    pub name: String,
    pub alias: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetryConfig {
    #[serde(default)]
    pub max: u32,
    #[serde(default)]
    pub when: Vec<String>,
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
            .patch_trace(7, "job-token", "hello", 10)
            .await
            .unwrap();

        assert_eq!(next, 15);
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
            .patch_trace(7, "job-token", "hello", 10)
            .await
            .unwrap();

        assert_eq!(next, 3);
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
            .update_job(7, "job-token", JobState::Success, None)
            .await
            .unwrap();
    }
}
