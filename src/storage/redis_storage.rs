use super::{StorageBackend, StorageStats};
use anyhow::Result;
use async_trait::async_trait;
use redis::{aio::ConnectionManager, AsyncCommands};
use std::sync::Arc;

#[derive(Clone)]
pub struct RedisStorage {
    client: Arc<ConnectionManager>,
    ttl_seconds: usize,
}

impl RedisStorage {
    pub async fn new(redis_url: &str, ttl_seconds: usize) -> Result<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;

        Ok(Self {
            client: Arc::new(conn),
            ttl_seconds,
        })
    }
}

#[async_trait]
impl StorageBackend for RedisStorage {
    async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        let mut conn = (*self.client).clone();
        let _: () = conn.set_ex(key, data, self.ttl_seconds as u64).await?;
        Ok(())
    }

    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut conn = (*self.client).clone();
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
