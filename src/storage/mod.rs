use anyhow::Result;
use async_trait::async_trait;

pub mod redis_storage;
pub mod s3_storage;

/// Storage backend trait for cache artifacts
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Store data with a key
    async fn store(&self, key: &str, data: &[u8]) -> Result<()>;

    /// Retrieve data by key
    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Check if key exists
    async fn exists(&self, key: &str) -> Result<bool>;

    /// Delete data by key
    async fn delete(&self, key: &str) -> Result<()>;

    /// Get storage statistics
    async fn stats(&self) -> Result<StorageStats>;
}

#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_size: u64,
    pub item_count: u64,
    pub hit_count: u64,
    pub miss_count: u64,
}

/// Multi-tier storage: Redis (hot) + S3 (cold)
pub struct HybridStorage {
    redis: redis_storage::RedisStorage,
    s3: s3_storage::S3Storage,
    size_threshold: usize, // > threshold → S3
}

impl HybridStorage {
    pub fn new(
        redis: redis_storage::RedisStorage,
        s3: s3_storage::S3Storage,
        size_threshold: usize,
    ) -> Self {
        Self {
            redis,
            s3,
            size_threshold,
        }
    }
}

#[async_trait]
impl StorageBackend for HybridStorage {
    async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        if data.len() < self.size_threshold {
            // Small files → Redis (fast)
            self.redis.store(key, data).await
        } else {
            // Large files → S3 (cheap)
            // Store metadata in Redis, data in S3
            self.s3.store(key, data).await?;
            self.redis.store(&format!("meta:{}", key), b"s3").await
        }
    }

    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        // Try Redis first
        if let Some(data) = self.redis.retrieve(key).await? {
            return Ok(Some(data));
        }

        // Check if it's in S3
        if let Some(meta) = self.redis.retrieve(&format!("meta:{}", key)).await? {
            if meta == b"s3" {
                return self.s3.retrieve(key).await;
            }
        }

        Ok(None)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        if self.redis.exists(key).await? {
            return Ok(true);
        }

        if self.redis.exists(&format!("meta:{}", key)).await? {
            return self.s3.exists(key).await;
        }

        Ok(false)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.redis.delete(key).await?;

        if self.redis.exists(&format!("meta:{}", key)).await? {
            self.redis.delete(&format!("meta:{}", key)).await?;
            self.s3.delete(key).await?;
        }

        Ok(())
    }

    async fn stats(&self) -> Result<StorageStats> {
        let redis_stats = self.redis.stats().await?;
        let s3_stats = self.s3.stats().await?;

        Ok(StorageStats {
            total_size: redis_stats.total_size + s3_stats.total_size,
            item_count: redis_stats.item_count + s3_stats.item_count,
            hit_count: redis_stats.hit_count + s3_stats.hit_count,
            miss_count: redis_stats.miss_count + s3_stats.miss_count,
        })
    }
}
