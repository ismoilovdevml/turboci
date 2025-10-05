use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber;

mod cache;
mod config;
mod optimizer;
mod runner;

use cache::CacheManager;
use config::Config;
use optimizer::BuildOptimizer;
use runner::ParallelRunner;

#[derive(Parser)]
#[command(name = "turboci")]
#[command(about = "⚡ Super fast CI/CD runner with distributed caching", long_about = None)]
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
            cache.print_stats().await?;
        }
    }

    Ok(())
}
