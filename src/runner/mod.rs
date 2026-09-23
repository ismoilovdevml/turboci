use anyhow::{Context, Result};
use rayon::prelude::*;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::config::{Config, Job, Step};
use crate::optimizer::BuildOptimizer;

pub struct ParallelRunner {
    #[allow(dead_code)]
    optimizer: BuildOptimizer,
    max_parallel_jobs: usize,
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub job_name: String,
    pub success: bool,
    pub duration: std::time::Duration,
    pub output: String,
}

#[allow(dead_code)]
impl ParallelRunner {
    pub fn new(optimizer: BuildOptimizer) -> Self {
        let max_parallel_jobs = num_cpus::cpus();
        info!(
            "🚀 Parallel runner initialized with {} workers",
            max_parallel_jobs
        );

        Self {
            optimizer,
            max_parallel_jobs,
        }
    }

    /// Execute entire CI/CD pipeline
    pub async fn execute(&self, config: Config) -> Result<()> {
        let start = Instant::now();

        info!("📦 Executing pipeline: {}", config.name);
        info!("Jobs to run: {}", config.jobs.len());

        // Optimization stats will be shown after execution

        // Create semaphore for parallel execution limit
        let semaphore = Arc::new(Semaphore::new(self.max_parallel_jobs));

        // Execute all jobs in parallel
        let results = self.execute_jobs_parallel(config.jobs, semaphore).await?;

        // Check results
        let failed: Vec<_> = results.iter().filter(|r| !r.success).collect();

        if !failed.is_empty() {
            error!("❌ {} job(s) failed:", failed.len());
            for result in failed {
                error!(
                    "  - {} (took {:.2}s)",
                    result.job_name,
                    result.duration.as_secs_f64()
                );
                if !result.output.is_empty() {
                    debug!("    Output: {}", result.output);
                }
            }
            anyhow::bail!("Pipeline failed");
        }

        let duration = start.elapsed();
        info!("✅ Pipeline completed in {:.2}s", duration.as_secs_f64());

        // Show job durations
        for result in &results {
            info!(
                "  {} - {:.2}s",
                result.job_name,
                result.duration.as_secs_f64()
            );
        }

        Ok(())
    }

    /// Execute multiple jobs in parallel
    async fn execute_jobs_parallel(
        &self,
        jobs: Vec<Job>,
        semaphore: Arc<Semaphore>,
    ) -> Result<Vec<ExecutionResult>> {
        let mut handles = Vec::new();

        for job in jobs {
            let sem = semaphore.clone();
            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                Self::execute_job(job).await
            });
            handles.push(handle);
        }

        // Wait for all jobs
        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(e)) => {
                    error!("Job execution error: {}", e);
                    return Err(e);
                }
                Err(e) => {
                    error!("Task join error: {}", e);
                    return Err(e.into());
                }
            }
        }

        Ok(results)
    }

    /// Execute a single job
    async fn execute_job(job: Job) -> Result<ExecutionResult> {
        let start = Instant::now();
        info!("▶️  Starting job: {}", job.name);

        let mut all_output = String::new();
        let mut all_success = true;

        for step in job.steps {
            match Self::execute_step(&step).await {
                Ok(output) => {
                    all_output.push_str(&output);
                    all_output.push('\n');
                }
                Err(e) => {
                    error!("Step '{}' failed: {}", step.name, e);
                    all_success = false;
                    all_output.push_str(&format!("ERROR: {}\n", e));
                    break;
                }
            }
        }

        let duration = start.elapsed();

        if all_success {
            info!(
                "✅ Job '{}' completed in {:.2}s",
                job.name,
                duration.as_secs_f64()
            );
        } else {
            error!(
                "❌ Job '{}' failed after {:.2}s",
                job.name,
                duration.as_secs_f64()
            );
        }

        Ok(ExecutionResult {
            job_name: job.name,
            success: all_success,
            duration,
            output: all_output,
        })
    }

    /// Execute a single step
    async fn execute_step(step: &Step) -> Result<String> {
        info!("  ⏩ Running step: {}", step.name);

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&step.run)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn command")?;

        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;

        // Read both pipes at the same time: a child blocked on a full stderr pipe
        // would never close stdout
        let read_stdout = async {
            let mut out = String::new();
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                println!("    {}", line);
                out.push_str(&line);
                out.push('\n');
            }
            anyhow::Ok(out)
        };
        let read_stderr = async {
            let mut out = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Some(line) = lines.next_line().await? {
                eprintln!("    {}", line);
                out.push_str(&line);
                out.push('\n');
            }
            anyhow::Ok(out)
        };
        let (out, err) = tokio::try_join!(read_stdout, read_stderr)?;
        let output = out + &err;

        let status = child.wait().await?;

        if !status.success() {
            anyhow::bail!("Command failed with exit code: {:?}", status.code());
        }

        Ok(output)
    }

    /// Execute tests in parallel
    pub async fn run_tests_parallel(
        &self,
        test_files: Vec<String>,
    ) -> Result<Vec<ExecutionResult>> {
        info!("🧪 Running {} tests in parallel...", test_files.len());

        let results: Vec<ExecutionResult> = test_files
            .par_iter()
            .map(|test_file| {
                let start = Instant::now();
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(test_file)
                    .output();

                match output {
                    Ok(output) => ExecutionResult {
                        job_name: test_file.clone(),
                        success: output.status.success(),
                        duration: start.elapsed(),
                        output: String::from_utf8_lossy(&output.stdout).to_string(),
                    },
                    Err(e) => ExecutionResult {
                        job_name: test_file.clone(),
                        success: false,
                        duration: start.elapsed(),
                        output: format!("Error: {}", e),
                    },
                }
            })
            .collect();

        let passed = results.iter().filter(|r| r.success).count();
        let failed = results.len() - passed;

        if failed > 0 {
            warn!("⚠️  Tests: {} passed, {} failed", passed, failed);
        } else {
            info!("✅ All {} tests passed", passed);
        }

        Ok(results)
    }
}

// Helper to get CPU count
mod num_cpus {
    pub fn cpus() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_step() {
        let step = Step {
            name: "Test".to_string(),
            run: "echo 'Hello World'".to_string(),
        };

        let result = ParallelRunner::execute_step(&step).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn large_stderr_before_stdout_does_not_hang() {
        let step = Step {
            name: "Noisy".to_string(),
            // ~200KB on stderr (more than a pipe buffer) before any stdout
            run: "i=0; while [ $i -lt 4000 ]; do echo 'err line padded to fifty bytes.............' >&2; i=$((i+1)); done; echo done".to_string(),
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            ParallelRunner::execute_step(&step),
        )
        .await
        .expect("step hung on a full stderr pipe")
        .unwrap();

        assert!(result.starts_with("done\n"));
    }
}
