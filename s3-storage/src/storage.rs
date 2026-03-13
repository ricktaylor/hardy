#[cfg(feature = "tracing")]
use tracing::instrument;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use hardy_bpa::{Bytes, async_trait, storage};
use std::sync::Arc;

#[derive(Debug)]
pub(super) struct Storage {
    client: aws_sdk_s3::Client,
    bucket: String,
    /// Key prefix with trailing slash, or empty string if no prefix is configured.
    /// Pre-computed at construction to avoid re-allocating on every key operation.
    key_prefix: String,
    /// Bundle size threshold above which multipart upload is used.
    multipart_threshold: usize,
    /// Size of each part (except the last) in a multipart upload.
    part_size: usize,
}

impl Storage {
    pub(super) fn new(
        client: aws_sdk_s3::Client,
        bucket: String,
        prefix: &str,
        multipart_threshold: usize,
        part_size: usize,
    ) -> Self {
        Self {
            client,
            bucket,
            key_prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
            multipart_threshold,
            part_size,
        }
    }

    fn full_key(&self, storage_name: &str) -> String {
        format!("{}{}", self.key_prefix, storage_name)
    }

    fn strip_prefix<'a>(&self, key: &'a str) -> Option<&'a str> {
        if self.key_prefix.is_empty() {
            Some(key)
        } else {
            key.strip_prefix(&self.key_prefix)
        }
    }

    /// Uploads `data` to `key` using the S3 multipart upload API.
    ///
    /// Parts are uploaded sequentially. On any failure after `CreateMultipartUpload`
    /// succeeds, `AbortMultipartUpload` is called to release the incomplete parts and
    /// avoid accruing storage charges for abandoned uploads.
    async fn save_multipart(&self, key: &str, data: Bytes) -> storage::Result<()> {
        let upload_id = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/octet-stream")
            .send()
            .await?
            .upload_id()
            .ok_or("S3 CreateMultipartUpload returned no upload ID")?
            .to_owned();

        match self.upload_parts(key, &upload_id, data).await {
            Ok(parts) => {
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(parts))
                            .build(),
                    )
                    .send()
                    .await?;
                Ok(())
            }
            Err(e) => {
                // Best-effort abort. Ignore the abort error to surface the original failure.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        data: Bytes,
    ) -> storage::Result<Vec<CompletedPart>> {
        let mut parts = Vec::new();
        let mut offset = 0usize;
        let mut part_number = 1i32;

        while offset < data.len() {
            let end = (offset + self.part_size).min(data.len());
            let chunk = data.slice(offset..end);

            let resp = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(chunk))
                .send()
                .await?;

            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(resp.e_tag().unwrap_or_default())
                    .build(),
            );

            offset = end;
            part_number += 1;
        }

        Ok(parts)
    }
}

#[async_trait]
impl storage::BundleStorage for Storage {
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn recover(&self, tx: storage::Sender<storage::RecoveryResponse>) -> storage::Result<()> {
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self.client.list_objects_v2().bucket(&self.bucket);
            if !self.key_prefix.is_empty() {
                req = req.prefix(&self.key_prefix);
            }
            if let Some(token) = continuation_token.as_deref() {
                req = req.continuation_token(token);
            }

            let resp = req.send().await?;

            for obj in resp.contents() {
                let Some(key) = obj.key() else {
                    continue;
                };

                // Skip objects that do not carry the configured prefix.
                // Guards against foreign or manually-uploaded objects in the bucket
                // emitting garbage storage names to the recovery consumer.
                let Some(storage_name) = self.strip_prefix(key) else {
                    continue;
                };

                let storage_name: Arc<str> = storage_name.into();
                let received_at = obj
                    .last_modified()
                    .and_then(|t| time::OffsetDateTime::from_unix_timestamp(t.secs()).ok())
                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);

                if tx.send_async((storage_name, received_at)).await.is_err() {
                    // Consumer closed early; exit cleanly.
                    return Ok(());
                }
            }

            match resp.is_truncated() {
                Some(true) => match resp.next_continuation_token() {
                    Some(token) => continuation_token = Some(String::from(token)),
                    None => break,
                },
                Some(false) | None => break,
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn load(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.full_key(storage_name))
            .send()
            .await
        {
            Ok(resp) => Ok(Some(resp.body.collect().await?.into_bytes())),
            Err(e) => {
                if let aws_sdk_s3::error::SdkError::ServiceError(ref se) = e {
                    if se.err().is_no_such_key() {
                        return Ok(None);
                    }
                }
                Err(Box::new(e))
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn save(&self, data: Bytes) -> storage::Result<Arc<str>> {
        let storage_name = uuid::Uuid::new_v4().to_string();
        let key = self.full_key(&storage_name);

        if data.len() >= self.multipart_threshold {
            self.save_multipart(&key, data).await?;
        } else {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .content_type("application/octet-stream")
                .body(ByteStream::from(data))
                .send()
                .await?;
        }

        Ok(storage_name.into())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn delete(&self, storage_name: &str) -> storage::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.full_key(storage_name))
            .send()
            .await?;
        Ok(())
    }
}
