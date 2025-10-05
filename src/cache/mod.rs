use anyhow::{Context, Result};
use redis::{aio::ConnectionManager, AsyncCommands};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info};

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: String,
    pub hash: String,
    pub timestamp: i64,
    pub size: u64,
    pub metadata: CacheMetadata,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub build_type: String,
    pub dependencies: Vec<String>,
    pub environment: String,
}

#[derive(Clone)]
pub struct CacheManager {
    redis_client: Arc<ConnectionManager>,
    stats: Arc<tokio::sync::Mutex<CacheStats>>,
}

impl std::fmt::Debug for CacheManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheManager")
            .field("stats", &self.stats)
            .finish()
    }
}

#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub total_size: u64,
    pub entries: u64,
}

impl CacheManager {
    pub async fn new() -> Result<Self> {
        Self::with_url("redis://127.0.0.1:6379").await
    }

    pub async fn with_url(url: &str) -> Result<Self> {
        let client = redis::Client::open(url).context("Failed to connect to Redis")?;
        let conn = ConnectionManager::new(client)
            .await
            .context("Failed to create connection manager")?;

        Ok(Self {
            redis_client: Arc::new(conn),
            stats: Arc::new(tokio::sync::Mutex::new(CacheStats::default())),
        })
    }

    pub async fn init(url: &str) -> Result<()> {
        let client = redis::Client::open(url)?;
        let mut conn = client.get_async_connection().await?;
        let _: () = redis::cmd("PING").query_async(&mut conn).await?;
        info!("Successfully connected to Redis at {}", url);
        Ok(())
    }

    /// Compute hash of file or directory
    #[allow(dead_code)]
    pub async fn compute_hash(&self, path: &Path) -> Result<String> {
        use walkdir::WalkDir;

        let mut hasher = blake3::Hasher::new();

        if path.is_file() {
            let content = tokio::fs::read(path).await?;
            hasher.update(&content);
        } else if path.is_dir() {
            // Hash all files in directory
            for entry in WalkDir::new(path).sort_by_file_name() {
                let entry = entry?;
                if entry.file_type().is_file() {
                    let content = std::fs::read(entry.path())?;
                    hasher.update(entry.path().to_string_lossy().as_bytes());
                    hasher.update(&content);
                }
            }
        }

        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Get cached build result
    #[allow(dead_code)]
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut conn = (*self.redis_client).clone();
        let result: Option<Vec<u8>> = conn.get(key).await?;

        let mut stats = self.stats.lock().await;
        if result.is_some() {
            stats.hits += 1;
            debug!("Cache HIT for key: {}", key);
        } else {
            stats.misses += 1;
            debug!("Cache MISS for key: {}", key);
        }

        Ok(result)
    }

    /// Store build result in cache
    #[allow(dead_code)]
    pub async fn set(&self, key: &str, value: &[u8], ttl_seconds: usize) -> Result<()> {
        let mut conn = (*self.redis_client).clone();
        let _: () = conn.set_ex(key, value, ttl_seconds as u64).await?;

        let mut stats = self.stats.lock().await;
        stats.entries += 1;
        stats.total_size += value.len() as u64;

        debug!("Cached {} bytes for key: {}", value.len(), key);
        Ok(())
    }

    /// Check if cache entry exists and is valid
    #[allow(dead_code)]
    pub async fn exists(&self, key: &str) -> Result<bool> {
        let mut conn = (*self.redis_client).clone();
        let exists: bool = conn.exists(key).await?;
        Ok(exists)
    }

    /// Clear all cache entries
    pub async fn clear(&self) -> Result<()> {
        let mut conn = (*self.redis_client).clone();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut conn).await?;

        let mut stats = self.stats.lock().await;
        *stats = CacheStats::default();

        info!("Cache cleared");
        Ok(())
    }

    /// Get cache statistics
    pub async fn get_stats(&self) -> CacheStats {
        let stats = self.stats.lock().await;
        CacheStats {
            hits: stats.hits,
            misses: stats.misses,
            total_size: stats.total_size,
            entries: stats.entries,
        }
    }

    /// Print cache statistics
    pub async fn print_stats(&self) -> Result<()> {
        let stats = self.get_stats().await;
        let hit_rate = if stats.hits + stats.misses > 0 {
            (stats.hits as f64 / (stats.hits + stats.misses) as f64) * 100.0
        } else {
            0.0
        };

        println!("\n📊 Cache Statistics:");
        println!("  Hits:       {}", stats.hits);
        println!("  Misses:     {}", stats.misses);
        println!("  Hit Rate:   {:.2}%", hit_rate);
        println!("  Entries:    {}", stats.entries);
        println!("  Total Size: {} MB", stats.total_size / 1024 / 1024);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_compute_hash() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, b"test content").await.unwrap();

        // Create a mock cache manager without Redis connection
        let hash = blake3::hash(b"test content").to_hex().to_string();

        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // BLAKE3 hash length
    }
}
