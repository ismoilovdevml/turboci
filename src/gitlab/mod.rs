use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

pub mod api;
pub mod job;

#[derive(Debug, Clone)]
pub struct GitLabClient {
    client: Client,
    url: String,
    token: String,
}

impl GitLabClient {
    pub fn new(url: String, token: String) -> Self {
        Self {
            client: Client::new(),
            url,
            token,
        }
    }

    /// Request a new job from GitLab
    pub async fn request_job(&self, runner_token: &str) -> Result<Option<Job>> {
        let url = format!("{}/api/v4/jobs/request", self.url);

        let response = self
            .client
            .post(&url)
            .json(&JobRequest {
                token: runner_token.to_string(),
                info: RunnerInfo::default(),
            })
            .send()
            .await
            .context("Failed to request job from GitLab")?;

        if response.status() == 204 {
            // No jobs available
            return Ok(None);
        }

        let job: Job = response.json().await.context("Failed to parse job response")?;
        Ok(Some(job))
    }

    /// Update job status
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

        self.client
            .put(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to update job status")?;

        Ok(())
    }

    /// Upload job artifacts
    pub async fn upload_artifacts(
        &self,
        job_id: u64,
        token: &str,
        artifact_data: Vec<u8>,
    ) -> Result<()> {
        let url = format!("{}/api/v4/jobs/{}/artifacts", self.url, job_id);

        self.client
            .post(&url)
            .header("JOB-TOKEN", token)
            .header("Content-Type", "application/zip")
            .body(artifact_data)
            .send()
            .await
            .context("Failed to upload artifacts")?;

        Ok(())
    }

    /// Download job artifacts
    pub async fn download_artifacts(&self, job_id: u64, token: &str) -> Result<Vec<u8>> {
        let url = format!("{}/api/v4/jobs/{}/artifacts", self.url, job_id);

        let response = self
            .client
            .get(&url)
            .header("JOB-TOKEN", token)
            .send()
            .await
            .context("Failed to download artifacts")?;

        let bytes = response
            .bytes()
            .await
            .context("Failed to read artifact bytes")?;

        Ok(bytes.to_vec())
    }
}

#[derive(Debug, Serialize)]
struct JobRequest {
    token: String,
    info: RunnerInfo,
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
    pub script: Vec<String>,
    pub timeout: u32,
    pub when: String,
    pub allow_failure: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Pending,
    Running,
    Success,
    Failed,
    Canceled,
}
