use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

#[cfg(feature = "runner")]
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct GitLabClient {
    client: Client,
    url: String,
    #[allow(dead_code)]
    token: String,
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
        }
    }

    /// Request a new job from GitLab (GitLab 17.x compatible)
    pub async fn request_job(&self, runner_token: &str) -> Result<Option<Job>> {
        let url = format!("{}/api/v4/jobs/request", self.url);

        debug!("Requesting job from: {}", url);

        #[cfg(feature = "runner")]
        let system_id = format!("s_{}", Uuid::new_v4().simple());
        #[cfg(not(feature = "runner"))]
        let system_id = format!("s_{}", "default");

        let response = self
            .client
            .post(&url)
            .json(&JobRequest {
                token: runner_token.to_string(),
                info: RunnerInfo::default(),
                system_id,
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
                let job: Job = response
                    .json()
                    .await
                    .context("Failed to parse job response")?;
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

    /// Stream job trace (real-time logs) - GitLab 17.x
    pub async fn patch_trace(
        &self,
        job_id: u64,
        token: &str,
        trace: &str,
        offset: usize,
    ) -> Result<()> {
        let url = format!("{}/api/v4/jobs/{}/trace", self.url, job_id);

        let response = self
            .client
            .patch(&url)
            .header("JOB-TOKEN", token)
            .header(
                "Content-Range",
                format!("{}-{}", offset, offset + trace.len()),
            )
            .body(trace.to_string())
            .send()
            .await
            .context("Failed to stream trace")?;

        if !response.status().is_success() {
            warn!("Trace streaming failed: {}", response.status());
        }

        Ok(())
    }

    /// Upload job artifacts (GitLab 17.x)
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

        let response = self
            .client
            .post(&url)
            .header("JOB-TOKEN", token)
            .header("Content-Type", "application/zip")
            .header("artifact-type", artifact_type)
            .body(artifact_data)
            .send()
            .await
            .context("Failed to upload artifacts")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Artifact upload failed: {}", error));
        }

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
    pub async fn upload_cache(
        &self,
        job_id: u64,
        token: &str,
        key: &str,
        cache_data: Vec<u8>,
    ) -> Result<()> {
        let url = format!("{}/api/v4/jobs/{}/cache", self.url, job_id);

        let response = self
            .client
            .post(&url)
            .header("JOB-TOKEN", token)
            .header("Cache-Key", key)
            .header("Content-Type", "application/zip")
            .body(cache_data)
            .send()
            .await
            .context("Failed to upload cache")?;

        if !response.status().is_success() {
            warn!("Cache upload failed: {}", response.status());
        } else {
            info!("Cache uploaded: {}", key);
        }

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
    info: RunnerInfo,
    system_id: String, // GitLab 17.x requirement
}

#[derive(Debug, Serialize)]
struct RunnerInfo {
    name: String,
    version: String,
    platform: String,
    architecture: String,
    executor: String,
}

impl Default for RunnerInfo {
    fn default() -> Self {
        Self {
            name: "turboci-runner".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: std::env::consts::OS.to_string(),
            architecture: std::env::consts::ARCH.to_string(),
            executor: "docker".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Job {
    pub id: u64,
    pub token: String,
    pub allow_git_fetch: bool,
    pub job_info: JobInfo,
    pub git_info: GitInfo,
    pub runner_info: RunnerVariables,
    pub variables: Vec<Variable>,
    pub steps: Vec<Step>,
    pub image: Option<Image>,
    pub services: Vec<Service>,
    pub artifacts: Vec<Artifact>,
    pub cache: Vec<Cache>,
    pub credentials: Vec<Credential>,
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub timeout: u32, // Job timeout in seconds (0 = use default)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobInfo {
    pub name: String,
    pub stage: String,
    pub project_id: u64,
    pub project_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitInfo {
    pub repo_url: String,
    pub ref_name: String,
    pub ref_type: String,
    pub sha: String,
    pub before_sha: String,
    pub depth: Option<u32>,
    pub refspecs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunnerVariables {
    pub ci_concurrent_id: u32,
    pub ci_concurrent_project_id: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Variable {
    pub key: String,
    pub value: String,
    pub public: bool,
    pub masked: bool,
    pub raw: bool,
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
    pub entrypoint: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Artifact {
    pub name: String,
    pub untracked: bool,
    pub paths: Vec<String>,
    pub when: String,
    pub expire_in: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Cache {
    pub key: String,
    pub untracked: bool,
    pub paths: Vec<String>,
    pub policy: String,
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
