//! Shell executor: runs job scripts directly on the host, as the runner's user

use super::*;

/// Shell Executor - Runs jobs directly on host
#[derive(Clone, Debug)]
pub struct ShellExecutor {
    work_dir: String,
    /// Custom CA of the GitLab server, given to jobs
    pub(super) ca_pem: Option<String>,
}

impl ShellExecutor {
    pub fn new(work_dir: Option<String>) -> Self {
        Self {
            work_dir: work_dir.unwrap_or_else(|| "/tmp/turboci".to_string()),
            ca_pem: None,
        }
    }

    /// `git ls-files --others -z` of the project at `project`
    pub(super) async fn untracked_files(&self, project: &Path) -> Result<String> {
        let out = Command::new("git")
            .arg("-c")
            .arg(format!("safe.directory={}", project.display()))
            .args(NO_REPO_COMMANDS)
            .arg("-C")
            .arg(project)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["ls-files", "--others", "-z"])
            .output()
            .await
            .context("Failed to run git")?;
        if !out.status.success() {
            anyhow::bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub(super) fn job_dir(&self, job_id: u64) -> PathBuf {
        Path::new(&self.work_dir).join(format!("job-{}", job_id))
    }

    pub(super) async fn execute(
        &self,
        job: &Job,
        trace: &mut TraceWriter<'_>,
        restore: &dyn Restore,
    ) -> JobOutcome {
        trace.write("Using Shell executor...\n").await;

        let job_dir = self.job_dir(job.id);
        let dirs = JobDirs {
            builds: job_dir.to_string_lossy().into_owned(),
            project: job_dir
                .join(script::project_subdir(job).map_err(JobFailure::system)?)
                .to_string_lossy()
                .into_owned(),
        };
        tokio::fs::create_dir_all(&dirs.project)
            .await
            .map_err(JobFailure::system)?;

        let env = script::job_env(
            job,
            &dirs.builds,
            &job_dir,
            &script::RunnerVars {
                disposable: false,
                ca_pem: self.ca_pem.clone(),
            },
        );
        write_variable_files(&env)
            .await
            .map_err(JobFailure::system)?;

        let runner = ShellRunner { env: env.vars };
        run_job_steps(job, &runner, &runner, trace, &dirs, restore).await
    }
}

struct ShellRunner {
    env: Vec<(String, String)>,
}

#[async_trait]
impl ScriptRunner for ShellRunner {
    async fn run(
        &self,
        script: &str,
        workdir: &str,
        trace: &mut TraceWriter<'_>,
        limits: &Limits<'_>,
    ) -> Result<RunStatus> {
        let mut command = Command::new("sh");
        command
            .args(["-c", SHELL_DETECT, "sh", script])
            .current_dir(workdir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group, so a timeout kills everything the script started
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().context("Failed to start sh")?;
        let pid = child.id();
        let mut out = child.stdout.take().context("stdout not captured")?;
        let mut err = child.stderr.take().context("stderr not captured")?;
        let (mut out_buf, mut err_buf) = ([0u8; 8192], [0u8; 8192]);
        let (mut out_open, mut err_open) = (true, true);
        let (mut stdout, mut stderr) = (Utf8Decoder::default(), Utf8Decoder::default());

        let mut flush = tokio::time::interval(TRACE_FLUSH_INTERVAL);
        while out_open || err_open {
            tokio::select! {
                _ = flush.tick() => {
                    trace.flush_if_due().await;
                }
                n = out.read(&mut out_buf), if out_open => match n? {
                    0 => out_open = false,
                    n => trace.write(&stdout.decode(&out_buf[..n])).await,
                },
                n = err.read(&mut err_buf), if err_open => match n? {
                    0 => err_open = false,
                    n => trace.write(&stderr.decode(&err_buf[..n])).await,
                },
                _ = tokio::time::sleep_until(limits.deadline) => {
                    kill_process_group(pid);
                    let _ = child.wait().await;
                    return Ok(RunStatus::TimedOut);
                }
                _ = limits.cancel.reached(limits.stop_at) => {
                    kill_process_group(pid);
                    let _ = child.wait().await;
                    return Ok(RunStatus::Canceled);
                }
            }
        }
        trace.write(&stdout.finish()).await;
        trace.write(&stderr.finish()).await;

        let status = tokio::select! {
            status = child.wait() => status?,
            _ = tokio::time::sleep_until(limits.deadline) => {
                kill_process_group(pid);
                let _ = child.wait().await;
                return Ok(RunStatus::TimedOut);
            }
            _ = limits.cancel.reached(limits.stop_at) => {
                kill_process_group(pid);
                let _ = child.wait().await;
                return Ok(RunStatus::Canceled);
            }
        };
        Ok(RunStatus::Exited(status.code().unwrap_or(-1)))
    }
}

fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", pid)])
            .status();
    }
}
