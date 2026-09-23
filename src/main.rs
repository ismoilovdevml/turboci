use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

mod cache;
mod config;
mod optimizer;
mod runner;

// API module (optional)
#[cfg(feature = "api")]
mod api;

// Runner modules (optional, for future)
#[cfg(feature = "runner")]
mod gitlab;
#[cfg(feature = "runner")]
mod runner_daemon;
#[cfg(feature = "runner")]
mod security;
#[cfg(feature = "runner")]
// Only runner statistics use it until the S3 cache backend lands (#14)
#[allow(dead_code)]
mod storage;

use cache::CacheManager;
use config::Config;
use optimizer::BuildOptimizer;
use runner::ParallelRunner;

#[derive(Parser)]
#[command(name = "turboci")]
#[command(about = "⚡ Super fast CI/CD runner with distributed caching", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run CI/CD pipeline
    Run {
        #[arg(short, long, default_value = "turboci.yml")]
        config: String,
    },
    /// Initialize cache system
    InitCache {
        #[arg(short, long)]
        redis_url: Option<String>,
    },
    /// Clear build cache
    ClearCache,
    /// Show cache statistics
    CacheStats,
    /// Start GitLab runner daemon (requires --features runner)
    #[cfg(feature = "runner")]
    RunnerStart {
        #[arg(short, long, default_value = "runner-config.toml")]
        config: String,
    },
    /// Show content hash for directory
    Hash {
        #[arg(short, long, default_value = ".")]
        path: String,
    },
    /// Create example runner configuration file
    #[cfg(feature = "runner")]
    InitRunner {
        #[arg(short, long, default_value = "runner-config.toml")]
        output: String,
    },
    /// Show runner statistics (requires runner daemon)
    #[cfg(feature = "runner")]
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
        Commands::Run { config } => {
            info!("🚀 Starting TurboCI pipeline...");
            let cfg = Config::load(&config)?;
            let cache = CacheManager::new().await?;
            let optimizer = BuildOptimizer::new(cache.clone());
            let runner = ParallelRunner::new(optimizer);

            runner.execute(cfg).await?;
            info!("✅ Pipeline completed successfully!");
        }
        Commands::InitCache { redis_url } => {
            info!("Initializing cache system...");
            let url = redis_url.unwrap_or_else(|| "redis://127.0.0.1:6379".to_string());
            CacheManager::init(&url).await?;
            info!("✅ Cache initialized");
        }
        Commands::ClearCache => {
            info!("Clearing build cache...");
            let cache = CacheManager::new().await?;
            cache.clear().await?;
            info!("✅ Cache cleared");
        }
        Commands::CacheStats => {
            let cache = CacheManager::new().await?;

            // Test cache functionality
            if cache.exists("test_key").await? {
                info!("Found test cache entry");
                if let Some(data) = cache.get("test_key").await? {
                    info!("Retrieved {} bytes from cache", data.len());
                }
            }

            cache.print_stats().await?;
        }
        #[cfg(feature = "runner")]
        Commands::RunnerStart { config } => {
            use gitlab::GitLabClient;
            use runner_daemon::executor::{DockerExecutor, ExecutorType, ShellExecutor};
            use runner_daemon::RunnerDaemon;
            use storage::{redis_storage::RedisStorage, s3_storage::S3Storage, HybridStorage};

            info!("🚀 Starting TurboCI Runner Daemon...");
            let runner_config = runner_daemon::config::RunnerConfig::load(&config)?;

            // Initialize GitLab client with a system ID that survives restarts
            let system_id = runner_daemon::system_id::load_or_create(
                &std::path::Path::new(&config).with_file_name(".runner_system_id"),
            );
            info!("   System ID: {}", system_id);
            let gitlab = GitLabClient::new(
                runner_config.gitlab_url.clone(),
                runner_config.runner_token.clone(),
            )
            .with_system_id(system_id);

            // Initialize storage
            let redis = RedisStorage::new(&runner_config.redis_url, 3600).await?;

            let storage = if let Some(bucket) = &runner_config.s3_bucket {
                // S3 is configured - use hybrid storage
                info!("💾 Using Hybrid storage (Redis + S3)");
                let s3 = if let Some(endpoint) = &runner_config.s3_endpoint {
                    S3Storage::new_with_endpoint(
                        bucket.clone(),
                        runner_config.s3_prefix.clone(),
                        endpoint.clone(),
                    )
                    .await?
                } else {
                    S3Storage::new(bucket.clone(), runner_config.s3_prefix.clone()).await?
                };
                HybridStorage::new(redis, s3, runner_config.storage_threshold)
            } else {
                // S3 not configured - use Redis-only storage (faster!)
                info!("⚡ Using Redis-only storage for maximum speed!");
                HybridStorage::redis_only(redis)
            };

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
                    ExecutorType::Docker(DockerExecutor::new(
                        runner_config.executor.docker.default_image.clone(),
                    )?)
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unknown executor type: {}. Use 'shell' or 'docker'",
                        runner_config.executor.executor_type
                    ));
                }
            };

            // Start daemon with graceful shutdown
            let daemon = RunnerDaemon::new(runner_config, gitlab, storage, executor);

            // Handle shutdown signals (SIGTERM, SIGINT)
            tokio::select! {
                result = daemon.start() => {
                    result?;
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("⚠️  Received SIGINT (Ctrl+C), shutting down gracefully...");
                    info!("✅ Runner stopped");
                }
            }
        }
        Commands::Hash { path } => {
            use cache::content_hash::ContentHasher;
            use std::path::Path;

            info!("🔍 Computing content hash for: {}", path);
            let hasher = ContentHasher::new();
            let hash = hasher.hash_directory(Path::new(&path))?;

            // Also compute dependency hash
            if let Ok(dep_hash) = hasher.hash_dependencies(Path::new(&path)) {
                println!("\n🔗 Dependency Hash: {}", dep_hash);
            }

            println!("\n📊 Content Hash Results:");
            println!("  Overall Hash: {}", hash.hash);
            println!("  Total Files: {}", hash.file_hashes.len());
            println!("\n📁 File Hashes:");
            for (file, file_hash) in hash.file_hashes.iter().take(10) {
                println!("  {} -> {}", file, &file_hash[..16]);
            }
            if hash.file_hashes.len() > 10 {
                println!("  ... and {} more files", hash.file_hashes.len() - 10);
            }

            // Show filtered results
            let rs_files = hash.filter_files("*.rs");
            if !rs_files.is_empty() {
                println!("\n🦀 Rust Files ({}):", rs_files.len());
                for (file, _) in rs_files.iter().take(5) {
                    println!("  {}", file);
                }
            }
        }
        #[cfg(feature = "runner")]
        Commands::InitRunner { output } => {
            use runner_daemon::config::RunnerConfig;

            info!("📝 Creating example runner configuration...");
            RunnerConfig::create_example(&output)?;
            info!("✅ Configuration created at: {}", output);
            println!("\n💡 Next steps:");
            println!("  1. Edit {} and configure your GitLab token", output);
            println!("  2. Set up Redis and S3/MinIO storage");
            println!("  3. Run: turboci runner-start -c {}", output);
        }
        #[cfg(feature = "runner")]
        Commands::RunnerStats { config } => {
            use gitlab::GitLabClient;
            use runner_daemon::executor::{DockerExecutor, ExecutorType, ShellExecutor};
            use runner_daemon::RunnerDaemon;
            use storage::{redis_storage::RedisStorage, s3_storage::S3Storage, HybridStorage};

            let runner_config = runner_daemon::config::RunnerConfig::load(&config)?;

            let gitlab = GitLabClient::new(
                runner_config.gitlab_url.clone(),
                runner_config.runner_token.clone(),
            );

            let redis = RedisStorage::new(&runner_config.redis_url, 3600).await?;

            let storage = if let Some(bucket) = &runner_config.s3_bucket {
                let s3 = if let Some(endpoint) = &runner_config.s3_endpoint {
                    S3Storage::new_with_endpoint(
                        bucket.clone(),
                        runner_config.s3_prefix.clone(),
                        endpoint.clone(),
                    )
                    .await?
                } else {
                    S3Storage::new(bucket.clone(), runner_config.s3_prefix.clone()).await?
                };
                HybridStorage::new(redis, s3, runner_config.storage_threshold)
            } else {
                HybridStorage::redis_only(redis)
            };

            let executor = match runner_config.executor.executor_type.as_str() {
                "shell" => ExecutorType::Shell(ShellExecutor::new(Some(
                    runner_config.executor.shell.work_dir.clone(),
                ))),
                "docker" => ExecutorType::Docker(DockerExecutor::new(
                    runner_config.executor.docker.default_image.clone(),
                )?),
                _ => ExecutorType::Shell(ShellExecutor::new(None)),
            };

            let daemon = RunnerDaemon::new(runner_config, gitlab, storage, executor);
            let stats = daemon.stats().await?;

            println!("\n📊 TurboCI Runner Statistics:");
            println!("  Cache Hit Rate: {:.2}%", stats.cache_hit_rate);
            println!(
                "  Total Cached Size: {} MB",
                stats.total_cached_size / 1024 / 1024
            );
            println!("  Cached Items: {}", stats.cached_items);
        }
        Commands::Upgrade => {
            info!("🔄 Checking for updates...");

            // Get current version
            let current_version = env!("CARGO_PKG_VERSION");
            println!("Current version: {}", current_version);

            // Fetch latest release from GitHub
            let client = reqwest::Client::new();
            let response = client
                .get("https://api.github.com/repos/ismoilovdevml/turboci/releases/latest")
                .header("User-Agent", "TurboCI")
                .send()
                .await?;

            let release: serde_json::Value = response.json().await?;
            let latest_version = release["tag_name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Failed to get latest version"))?
                .trim_start_matches('v');

            println!("Latest version: {}", latest_version);

            if current_version == latest_version {
                println!("✅ You are already running the latest version!");
                return Ok(());
            }

            println!("📥 Downloading TurboCI {}...", latest_version);

            // Detect architecture
            let target = if cfg!(target_arch = "x86_64") && cfg!(target_os = "linux") {
                "x86_64-unknown-linux-musl"
            } else {
                return Err(anyhow::anyhow!("Unsupported platform for auto-upgrade"));
            };

            let download_url = format!(
                "https://github.com/ismoilovdevml/turboci/releases/download/v{}/turboci-{}",
                latest_version, target
            );

            // Download new binary
            let binary_data = client.get(&download_url).send().await?.bytes().await?;

            // Get current executable path
            let current_exe = std::env::current_exe()?;
            let temp_path = current_exe.with_extension("new");

            // Write new binary
            std::fs::write(&temp_path, binary_data)?;

            // Make executable
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&temp_path)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&temp_path, perms)?;
            }

            // Replace current binary
            std::fs::rename(&temp_path, &current_exe)?;

            println!("✅ Successfully upgraded to version {}", latest_version);
            println!("🔄 Please restart TurboCI to use the new version");
        }
    }

    Ok(())
}
