use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

mod gitlab;
mod net;
mod runner_daemon;
mod security;

#[derive(Parser)]
#[command(name = "turboci")]
#[command(about = "⚡ GitLab CI/CD runner: runs GitLab jobs in Docker containers or on the host", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the runner: take jobs from GitLab and run them
    RunnerStart {
        #[arg(short, long, default_value = "runner-config.toml")]
        config: String,
    },
    /// Create an example runner configuration file
    InitRunner {
        #[arg(short, long, default_value = "runner-config.toml")]
        output: String,
    },
    /// Check a runner configuration and show local cache usage
    RunnerStats {
        #[arg(short, long, default_value = "runner-config.toml")]
        config: String,
    },
    /// Upgrade TurboCI to the latest version
    Upgrade,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::RunnerStart { config } => {
            use gitlab::GitLabClient;
            use runner_daemon::executor::{DockerExecutor, ExecutorType, ShellExecutor};
            use runner_daemon::RunnerDaemon;

            info!("🚀 Starting TurboCI Runner Daemon...");
            let runner_config = runner_daemon::config::RunnerConfig::load(&config)?;

            // Initialize GitLab client with a system ID that survives restarts
            // Kept in the state directory (the service can not write next to
            // its config in /etc). Runners installed side by side get their
            // own ID, so one never removes the other's containers.
            let instance = if config == "/etc/turboci-runner.toml" {
                ""
            } else {
                config.as_str()
            };
            let system_id = runner_daemon::system_id::load_or_create(
                &std::path::Path::new(&runner_config.state_dir).join(".runner_system_id"),
                instance,
            );
            info!("   System ID: {}", system_id);
            let gitlab = GitLabClient::new(
                runner_config.gitlab_url.clone(),
                runner_config.runner_token.clone(),
            )
            .with_system_id(system_id.clone())
            .with_executor(&runner_config.executor.executor_type);
            let network = runner_config.network()?;
            let gitlab = gitlab.with_network(&network)?;
            if let Some(proxy) = &network.proxy {
                // Only the host: the URL may hold a password
                let host = reqwest::Url::parse(proxy)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_string))
                    .unwrap_or_default();
                info!("   Proxy: {}", host);
            }

            // Initialize executor based on config
            let executor = match runner_config.executor.executor_type.as_str() {
                "shell" => {
                    info!("⚡ Using Shell executor (direct execution, super fast!)");
                    tracing::warn!(
                        "Shell executor runs job scripts directly on this host with the \
                         runner's privileges and no isolation. Use executor_type = \"docker\" \
                         unless every project using this runner is trusted."
                    );
                    ExecutorType::Shell(ShellExecutor::new(Some(
                        runner_config.executor.shell.work_dir.clone(),
                    )))
                }
                "docker" => {
                    info!("🐳 Using Docker executor (isolated containers)");
                    let docker = DockerExecutor::new(runner_config.executor.docker.clone())?
                        .with_owner(&system_id);
                    // In the background: resolving registry names with a slow
                    // DNS must not delay taking jobs
                    let (check, proxy) = (docker.clone(), runner_config.proxy.is_some());
                    tokio::spawn(async move {
                        for warning in check.daemon_warnings(proxy).await {
                            tracing::warn!("{}", warning);
                        }
                    });
                    ExecutorType::Docker(docker)
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unknown executor type: {}. Use 'shell' or 'docker'",
                        runner_config.executor.executor_type
                    ));
                }
            };

            let executor = executor.with_ca_pem(network.ca_pem.clone());
            let cache_s3 = runner_config.s3_cache().cloned();
            let mut daemon = RunnerDaemon::new(runner_config, gitlab, executor);
            if let Some(s3) = &cache_s3 {
                let bucket = runner_daemon::s3::Bucket::new(s3, network.client_builder()?)?;
                // In the background: a slow S3 must not delay taking jobs
                let (probe, url) = (bucket.clone(), s3.url.clone());
                tokio::spawn(async move {
                    match probe.probe().await {
                        Ok(()) => info!("   S3 cache: {}", url),
                        Err(message) => tracing::warn!(
                            "S3 cache {}: {} (jobs keep using the local cache)",
                            url,
                            message
                        ),
                    }
                });
                daemon = daemon.with_s3_cache(bucket);
            }
            spawn_signal_handler(daemon.shutdown_handle())?;
            daemon.start().await?;
        }
        Commands::InitRunner { output } => {
            use runner_daemon::config::RunnerConfig;

            info!("📝 Creating example runner configuration...");
            RunnerConfig::create_example(&output)?;
            info!("✅ Configuration created at: {}", output);
            println!("\n💡 Next steps:");
            println!("  1. Edit {} and configure your GitLab token", output);
            println!("  2. Make sure Docker is installed (default executor)");
            println!("  3. Run: turboci runner-start -c {}", output);
        }
        Commands::RunnerStats { config } => {
            let runner_config = runner_daemon::config::RunnerConfig::load(&config)?;
            let cache = runner_daemon::job_cache::LocalCache::new(&runner_config.cache_dir);
            let (archives, bytes) = cache.stats();

            println!("\n📊 TurboCI Runner Statistics:");
            println!("  Cache directory: {}", runner_config.cache_dir);
            println!("  Cache archives:  {}", archives);
            println!("  Cache size:      {:.1} MB", bytes as f64 / 1_048_576.0);
        }
        Commands::Upgrade => upgrade().await?,
    }

    Ok(())
}

/// SIGQUIT: finish running jobs, then exit. SIGTERM/SIGINT: stop running jobs,
/// report them failed and exit (the same signals gitlab-runner uses)
fn spawn_signal_handler(shutdown: runner_daemon::ShutdownHandle) -> Result<()> {
    use runner_daemon::Shutdown;
    use tokio::signal::unix::{signal, SignalKind};

    let mut quit = signal(SignalKind::quit())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::spawn(async move {
        loop {
            let mode = tokio::select! {
                _ = quit.recv() => {
                    info!("⚠️  SIGQUIT: taking no new jobs, waiting for running jobs to finish");
                    Shutdown::Graceful
                }
                _ = terminate.recv() => {
                    info!("⚠️  SIGTERM: stopping running jobs");
                    Shutdown::Abort
                }
                _ = interrupt.recv() => {
                    info!("⚠️  SIGINT: stopping running jobs");
                    Shutdown::Abort
                }
            };
            shutdown.request(mode);
        }
    });
    Ok(())
}

const RELEASES_API: &str = "https://api.github.com/repos/ismoilovdevml/turboci/releases/latest";

/// The release binary built for this platform
const RELEASE_ASSET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("turboci-x86_64-unknown-linux-musl")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("turboci-aarch64-unknown-linux-musl")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("turboci-aarch64-apple-darwin")
} else {
    None
};

/// Replace this binary with the latest release, after checking it against the
/// release's SHA256SUMS and making sure it runs
async fn upgrade() -> Result<()> {
    info!("🔄 Checking for updates...");
    let current_version = env!("CARGO_PKG_VERSION");
    println!("Current version: {}", current_version);

    let client = net::client_builder()
        .user_agent("TurboCI")
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let release: serde_json::Value = client
        .get(RELEASES_API)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Latest release has no tag"))?;
    let latest_version = tag.trim_start_matches('v');
    println!("Latest version: {}", latest_version);
    if current_version == latest_version {
        println!("✅ You are already running the latest version!");
        return Ok(());
    }

    let asset =
        RELEASE_ASSET.ok_or_else(|| anyhow::anyhow!("Unsupported platform for auto-upgrade"))?;
    let base = format!(
        "https://github.com/ismoilovdevml/turboci/releases/download/{}",
        tag
    );

    println!("📥 Downloading TurboCI {}...", latest_version);
    let download = |name: String| {
        let client = client.clone();
        let url = format!("{}/{}", base, name);
        async move {
            let bytes = client
                .get(&url)
                .send()
                .await?
                .error_for_status()
                .map_err(|e| anyhow::anyhow!("Failed to download {}: {}", url, e))?
                .bytes()
                .await?;
            anyhow::Ok(bytes)
        }
    };
    let binary = download(asset.to_string()).await?;
    let sums = download("SHA256SUMS".to_string())
        .await
        .map_err(|e| anyhow::anyhow!("{} (refusing to upgrade without checksums)", e))?;
    verify_checksum(&binary, &String::from_utf8_lossy(&sums), asset)?;
    println!("✅ Checksum verified");

    let current_exe = std::env::current_exe()?;
    let temp_path = current_exe.with_extension("new");
    std::fs::write(&temp_path, &binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o755))?;
    }
    let runs = std::process::Command::new(&temp_path)
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !runs {
        let _ = std::fs::remove_file(&temp_path);
        anyhow::bail!("The downloaded binary does not run; keeping the current version");
    }
    std::fs::rename(&temp_path, &current_exe)?;

    println!("✅ Successfully upgraded to version {}", latest_version);
    println!("🔄 Please restart TurboCI to use the new version");
    Ok(())
}

/// Check `data` against the entry for `asset` in a `sha256sum` style SHA256SUMS file
fn verify_checksum(data: &[u8], sums: &str, asset: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    let expected = sums
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim().trim_start_matches('*') == asset)
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("SHA256SUMS has no entry for {}", asset))?;
    let actual: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    if actual != expected {
        anyhow::bail!(
            "Checksum mismatch for {}: expected {}, got {}",
            asset,
            expected,
            actual
        );
    }
    Ok(())
}

#[cfg(test)]
mod upgrade_tests {
    use super::verify_checksum;

    // sha256("hello")
    const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn accepts_matching_checksum() {
        let sums = format!(
            "{}  turboci\n{}  turboci-x86_64-unknown-linux-musl\n",
            "0".repeat(64),
            HELLO
        );
        verify_checksum(b"hello", &sums, "turboci-x86_64-unknown-linux-musl").unwrap();
    }

    #[test]
    fn rejects_mismatch_and_missing_entry() {
        let sums = format!("{}  turboci-x86_64-unknown-linux-musl\n", HELLO);
        assert!(verify_checksum(b"tampered", &sums, "turboci-x86_64-unknown-linux-musl").is_err());
        assert!(verify_checksum(b"hello", &sums, "turboci-other").is_err());
        assert!(
            verify_checksum(b"<html>404</html>", "", "turboci-x86_64-unknown-linux-musl").is_err()
        );
    }

    // CI tests on every platform a release is built for
    #[test]
    fn this_platform_has_a_release_asset() {
        let asset = super::RELEASE_ASSET.unwrap();
        assert!(asset.contains(std::env::consts::ARCH), "{}", asset);
    }
}
