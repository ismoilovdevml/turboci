use super::{StorageBackend, StorageStats};
use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::{primitives::ByteStream, Client};
use std::sync::Arc;

#[derive(Clone)]
pub struct S3Storage {
    client: Arc<Client>,
    bucket: String,
    prefix: String,
}

impl S3Storage {
    pub async fn new(bucket: String, prefix: String) -> Result<Self> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&config);

        Ok(Self {
            client: Arc::new(client),
            bucket,
            prefix,
        })
    }

    pub async fn new_with_endpoint(
        bucket: String,
        prefix: String,
        endpoint: String,
    ) -> Result<Self> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .endpoint_url(endpoint)
            .load()
            .await;
        let client = Client::new(&config);

        Ok(Self {
            client: Arc::new(client),
            bucket,
            prefix,
        })
    }

    fn get_key(&self, key: &str) -> String {
        format!("{}/{}", self.prefix, key)
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        let s3_key = self.get_key(key);
        let body = ByteStream::from(data.to_vec());

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(body)
            .send()
            .await
            .context("Failed to upload to S3")?;

        Ok(())
    }

    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let s3_key = self.get_key(key);

        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await;

        match response {
            Ok(resp) => {
                let bytes = resp
                    .body
                    .collect()
                    .await
                    .context("Failed to read S3 object")?
                    .into_bytes();
                Ok(Some(bytes.to_vec()))
            }
            Err(_) => Ok(None),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let s3_key = self.get_key(key);

        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await;

        Ok(result.is_ok())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let s3_key = self.get_key(key);

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
            .context("Failed to delete from S3")?;

        Ok(())
    }

    async fn stats(&self) -> Result<StorageStats> {
        let prefix = format!("{}/", self.prefix);
        let objects = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&prefix)
            .send()
            .await
            .context("Failed to list S3 objects")?;

        let total_size = objects
            .contents()
            .iter()
            .map(|obj| obj.size().unwrap_or(0) as u64)
            .sum();

        let item_count = objects.key_count().unwrap_or(0) as u64;

        Ok(StorageStats {
            total_size,
            item_count,
            hit_count: 0,
            miss_count: 0,
        })
    }
}
