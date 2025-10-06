use super::compression::{compress_if_beneficial, decompress};
use super::{StorageBackend, StorageStats};
use anyhow::Result;
use async_trait::async_trait;
use redis::{aio::ConnectionManager, AsyncCommands};
use std::sync::Arc;
use tracing::debug;

#[derive(Clone)]
pub struct RedisStorage {
    client: Arc<ConnectionManager>,
    ttl_seconds: usize,
    enable_compression: bool,
}

impl RedisStorage {
    pub async fn new(redis_url: &str, ttl_seconds: usize) -> Result<Self> {
        Self::new_with_compression(redis_url, ttl_seconds, true).await
    }

    pub async fn new_with_compression(
        redis_url: &str,
        ttl_seconds: usize,
        enable_compression: bool,
    ) -> Result<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;

        Ok(Self {
            client: Arc::new(conn),
            ttl_seconds,
            enable_compression,
        })
    }
}

#[async_trait]
impl StorageBackend for RedisStorage {
    async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        let mut conn = (*self.client).clone();

        let (data_to_store, is_compressed) = if self.enable_compression {
            compress_if_beneficial(data)?
        } else {
            (data.to_vec(), false)
        };

        if is_compressed {
            debug!(
                "Compressed cache key {} from {} to {} bytes ({:.1}% reduction)",
                key,
                data.len(),
                data_to_store.len(),
                100.0 * (1.0 - (data_to_store.len() as f64 / data.len() as f64))
            );
            // Store with compression marker
            let _: () = conn
                .set_ex(
                    format!("{}:compressed", key),
                    &data_to_store,
                    self.ttl_seconds as u64,
                )
                .await?;
        } else {
            let _: () = conn
                .set_ex(key, &data_to_store, self.ttl_seconds as u64)
                .await?;
        }

        Ok(())
    }

    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut conn = (*self.client).clone();

        // Try compressed version first
        let compressed_key = format!("{}:compressed", key);
        let compressed: Option<Vec<u8>> = conn.get(&compressed_key).await?;

        if let Some(compressed_data) = compressed {
            debug!("Retrieved compressed cache key {}", key);
            let decompressed = decompress(&compressed_data)?;
            return Ok(Some(decompressed));
        }

        // Fall back to uncompressed
        let result: Option<Vec<u8>> = conn.get(key).await?;
        Ok(result)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let mut conn = (*self.client).clone();
        let exists: bool = conn.exists(key).await?;
        Ok(exists)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut conn = (*self.client).clone();
        let _: () = conn.del(key).await?;
        Ok(())
    }

    async fn stats(&self) -> Result<StorageStats> {
        let mut conn = (*self.client).clone();
        let db_size: u64 = redis::cmd("DBSIZE").query_async(&mut conn).await?;

        Ok(StorageStats {
            total_size: 0, // Redis doesn't provide easy total size
            item_count: db_size,
            hit_count: 0,
            miss_count: 0,
        })
    }
}
