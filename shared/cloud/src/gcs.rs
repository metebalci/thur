// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Google Cloud Storage (GCS) backend for cloud storage tier
//!
//! Built on Google's official `google-cloud-storage` crate (the
//! googleapis/google-cloud-rust line). Two client handles internally:
//! `Storage` for object I/O (read/write) and `StorageControl` for the
//! metadata plane (get/list/delete/update + bucket-level ops). Both
//! are `Arc`-shaped, lazy-channel, and cheap to clone.
//!
//! Provides upload/download operations for chunks and manifests with:
//! - Automatic retry with exponential backoff
//! - GCP credential handling (service account, ADC)
//! - Optional compression support (zstd/lz4)
//! - Error handling and logging

use crate::cloud_backend::CloudBackend;
use crate::compression::{CompressionAlgo, CompressionConfig, compress_data, decompress_data};
use crate::{CloudError, Result};
use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_auth::credentials::{Builder as CredsBuilder, Credentials, service_account};
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_storage::client::{Storage, StorageControl};
use google_cloud_storage::model::Object;
use google_cloud_wkt::FieldMask;
use std::path::Path;
use std::sync::Once;
use tokio::task::JoinSet;
use tracing::{debug, warn};

/// Install rustls's aws-lc-rs default crypto provider exactly once. The
/// official GCS SDK uses reqwest + rustls 0.23 and relies on rustls's
/// auto-provider selection, which panics when multiple providers
/// (aws-lc-rs from the AWS SDK chain + ring from elsewhere) are present
/// in the dependency tree. Called from `GcsBackend::new` so the
/// non-GCS paths don't pay the install cost.
static CRYPTO_INSTALL: Once = Once::new();

fn ensure_rustls_provider() {
    CRYPTO_INSTALL.call_once(|| {
        // Ignore the "already installed" error: another GCS backend
        // (or rustls auto-detect on a different feature combo) may
        // have raced ahead.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Maximum number of retry attempts for uploads.
const MAX_UPLOAD_RETRIES: u32 = 5;
/// Maximum number of retry attempts for downloads.
const MAX_DOWNLOAD_RETRIES: u32 = 3;
// Backoff cadence is owned by `cloud_helpers::retry_async`.

/// Google Cloud Storage backend for storing chunks and manifests
#[derive(Clone)]
pub struct GcsBackend {
    /// Data plane: read_object / write_object.
    data: Storage,
    /// Metadata plane: get/list/delete/update objects + get_bucket.
    control: StorageControl,
    bucket: String,
    prefix: String,
    project_id: String,
    /// Compression configuration
    compression_config: CompressionConfig,
}

impl std::fmt::Debug for GcsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcsBackend")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("project_id", &self.project_id)
            .field("compression_config", &self.compression_config)
            .finish()
    }
}

impl GcsBackend {
    /// Create a new GCS backend
    ///
    /// Credential loading depends on `service_account_key_file`:
    /// - `None`  → Application Default Credentials chain
    ///   (`GOOGLE_APPLICATION_CREDENTIALS` env var → `gcloud auth
    ///   application-default login` user creds → GCE/GKE metadata
    ///   server).
    /// - `Some(path)` → load that JSON key file directly via
    ///   `service_account::Builder`. The chain is bypassed —
    ///   useful when running multiple GCS backends authenticated
    ///   with different service accounts.
    ///
    /// # Arguments
    /// * `bucket` - GCS bucket name
    /// * `prefix` - Object key prefix (e.g., "tapes/")
    /// * `project_id` - GCP project ID
    /// * `service_account_key_file` - Optional path to service-account JSON key file
    /// * `compression_config` - Compression configuration
    pub async fn new(
        bucket: String,
        prefix: String,
        project_id: String,
        service_account_key_file: Option<String>,
        compression_config: CompressionConfig,
    ) -> Result<Self> {
        debug!(
            "Initializing GCS backend: bucket={}, prefix={}, project={}",
            bucket, prefix, project_id
        );

        ensure_rustls_provider();

        let creds = match &service_account_key_file {
            Some(path) => {
                debug!("Using service-account key file: {} (ADC bypassed)", path);
                let json = tokio::fs::read_to_string(path).await.map_err(|e| {
                    CloudError::Other(format!(
                        "GCS service-account key file '{}' could not be loaded: {}",
                        path, e
                    ))
                })?;
                let value: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
                    CloudError::Other(format!(
                        "GCS service-account key file '{}' could not be loaded: {}",
                        path, e
                    ))
                })?;
                service_account::Builder::new(value).build().map_err(|e| {
                    CloudError::Other(format!("GCS auth from key file '{}' failed: {}", path, e))
                })?
            }
            None => {
                debug!("Using Application Default Credentials (ADC)");
                CredsBuilder::default()
                    .build()
                    .map_err(|e| CloudError::Other(format!("GCS auth failed: {}", e)))?
            }
        };

        let data = build_storage(&creds).await?;
        let control = build_storage_control(&creds).await?;

        debug!(
            "Compression algorithm: {:?}, level: {}",
            compression_config.algorithm, compression_config.level
        );

        Ok(Self {
            data,
            control,
            bucket,
            prefix,
            project_id,
            compression_config,
        })
    }

    /// Construct full GCS object name with prefix.
    fn full_key(&self, key: &str) -> String {
        crate::cloud_helpers::full_key(&self.prefix, key)
    }

    /// Canonical bucket resource name. Every metadata-plane call
    /// expects `projects/_/buckets/{bucket}`; the data plane wants
    /// the same shape for the `bucket` parameter to `read_object` /
    /// `write_object`. Centralized to avoid copy-paste drift.
    fn bucket_resource(&self) -> String {
        format!("projects/_/buckets/{}", self.bucket)
    }
}

async fn build_storage(creds: &Credentials) -> Result<Storage> {
    Storage::builder()
        .with_credentials(creds.clone())
        .build()
        .await
        .map_err(|e| CloudError::Other(format!("GCS Storage client build failed: {}", e)))
}

async fn build_storage_control(creds: &Credentials) -> Result<StorageControl> {
    StorageControl::builder()
        .with_credentials(creds.clone())
        .build()
        .await
        .map_err(|e| CloudError::Other(format!("GCS StorageControl client build failed: {}", e)))
}

/// CloudBackend trait implementation for GcsBackend
#[async_trait]
impl CloudBackend for GcsBackend {
    async fn upload_chunk(
        &self,
        key: &str,
        data: &[u8],
    ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
        let full_key = self.full_key(key);
        let uncompressed_size = data.len() as u64;

        let (data_to_upload, compressed_size, applied_algo) =
            match self.compression_config.algorithm {
                Some(algo) => {
                    let compressed = compress_data(algo, data, self.compression_config.level)?;
                    let comp_size = compressed.len() as u64;
                    debug!(
                        "Compressed chunk ({}) from {} bytes to {} bytes (ratio: {:.2}%)",
                        algo,
                        uncompressed_size,
                        comp_size,
                        (comp_size as f64 / uncompressed_size as f64) * 100.0,
                    );
                    (compressed, Some(comp_size), Some(algo))
                }
                None => {
                    debug!("Compression disabled for chunk {}", full_key);
                    (data.to_vec(), None, None)
                }
            };

        debug!(
            "Uploading chunk to GCS: {} ({} bytes)",
            full_key,
            data_to_upload.len()
        );

        // Wrap once in `bytes::Bytes` so each retry attempt clones a
        // cheap Arc handle instead of memcpy'ing the full Vec.
        let data_bytes = Bytes::from(data_to_upload);
        let bucket_resource = self.bucket_resource();

        crate::cloud_helpers::retry_async("upload_chunk", MAX_UPLOAD_RETRIES, || async {
            self.data
                .write_object(
                    bucket_resource.clone(),
                    full_key.clone(),
                    data_bytes.clone(),
                )
                .send_buffered()
                .await
                .map_err(|e| {
                    CloudError::Other(format!(
                        "GCS chunk upload failed: {} (bucket: {}, key: {})",
                        e, self.bucket, full_key
                    ))
                })?;
            Ok(())
        })
        .await?;

        Ok((uncompressed_size, compressed_size, applied_algo))
    }

    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        let full_key = self.full_key(key);

        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| CloudError::Other(format!("failed to stat file: {}", e)))?;
        let file_size = metadata.len();

        debug!(
            "Uploading chunk (zero-copy) to GCS: {} from {:?} ({} bytes)",
            full_key, file_path, file_size
        );

        if self.compression_config.enabled() {
            warn!(
                "Zero-copy upload requested but compression is enabled. Consider using upload_chunk() for compression support."
            );
        }

        let bucket_resource = self.bucket_resource();

        crate::cloud_helpers::retry_async("upload_chunk_zerocopy", MAX_UPLOAD_RETRIES, || async {
            let data = tokio::fs::read(file_path)
                .await
                .map_err(|e| CloudError::Other(format!("failed to read file: {}", e)))?;
            self.data
                .write_object(bucket_resource.clone(), full_key.clone(), Bytes::from(data))
                .send_buffered()
                .await
                .map_err(|e| {
                    CloudError::Other(format!(
                        "GCS chunk upload (zero-copy) failed: {} (bucket: {}, key: {}, file: {:?})",
                        e, self.bucket, full_key, file_path
                    ))
                })?;
            Ok(())
        })
        .await?;

        Ok(file_size)
    }

    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        let full_key = self.full_key(key);
        debug!("Downloading chunk from GCS: {}", full_key);

        let bucket_resource = self.bucket_resource();

        crate::cloud_helpers::retry_async("download_chunk", MAX_DOWNLOAD_RETRIES, || async {
            // Drain the streamed body into a single Vec inside the
            // retry closure so a mid-stream error replays the whole
            // RPC instead of returning a half-filled buffer.
            let mut resp = self
                .data
                .read_object(bucket_resource.clone(), full_key.clone())
                .send()
                .await
                .map_err(|e| {
                    CloudError::Other(format!(
                        "GCS chunk download failed: {} (bucket: {}, key: {})",
                        e, self.bucket, full_key
                    ))
                })?;
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.next().await {
                let chunk = chunk.map_err(|e| {
                    CloudError::Other(format!(
                        "GCS chunk stream read failed: {} (bucket: {}, key: {})",
                        e, self.bucket, full_key
                    ))
                })?;
                buf.extend_from_slice(&chunk);
            }
            debug!("Downloaded {} bytes from GCS: {}", buf.len(), full_key);

            // Mirror upload_chunk's compression logic: if the backend is
            // configured to compress on upload, reverse it on download.
            // TODO(beta-blocker): switch to per-object `metadata` once the
            // SDK surfaces it on `ReadObjectResponse` (today only on a
            // separate `StorageControl::get_object` RPC). See
            // `project_gcs_compression_metadata_beta_blocker.md`.
            let data = match self.compression_config.algorithm {
                Some(algo) => decompress_data(algo, &buf)?,
                None => buf,
            };
            Ok(data)
        })
        .await
    }

    async fn download_chunks_parallel(&self, keys: &[String]) -> Result<Vec<Vec<u8>>> {
        const MAX_CONCURRENT_DOWNLOADS: usize = 8;

        if keys.is_empty() {
            return Ok(Vec::new());
        }

        debug!(
            "Downloading {} chunks in parallel (max concurrency: {})",
            keys.len(),
            MAX_CONCURRENT_DOWNLOADS
        );

        let mut tasks = JoinSet::new();
        let mut results: Vec<Option<Vec<u8>>> = vec![None; keys.len()];

        for (idx, key) in keys.iter().enumerate() {
            if tasks.len() >= MAX_CONCURRENT_DOWNLOADS {
                let Some(finished) = tasks.join_next().await else {
                    return Err(CloudError::Other("Task join failed".to_string()));
                };
                let (result_idx, data) = finished
                    .map_err(|e| CloudError::Other(format!("Download task panicked: {}", e)))??;
                results[result_idx] = Some(data);
            }

            let gcs = self.clone();
            let key_clone = key.clone();

            tasks.spawn(async move {
                let data = gcs.download_chunk(&key_clone).await?;
                Ok::<(usize, Vec<u8>), CloudError>((idx, data))
            });
        }

        while let Some(finished) = tasks.join_next().await {
            let (result_idx, data) = finished
                .map_err(|e| CloudError::Other(format!("Download task panicked: {}", e)))??;
            results[result_idx] = Some(data);
        }

        results
            .into_iter()
            .enumerate()
            .map(|(idx, opt)| {
                opt.ok_or_else(|| {
                    CloudError::Other(format!(
                        "Missing download result for chunk at index {}",
                        idx
                    ))
                })
            })
            .collect()
    }

    async fn upload_manifest(&self, key: &str, json: &str) -> Result<()> {
        let full_key = self.full_key(key);
        debug!(
            "Uploading manifest to GCS: {} ({} bytes)",
            full_key,
            json.len()
        );

        let bucket_resource = self.bucket_resource();
        let body = Bytes::from(json.as_bytes().to_vec());

        crate::cloud_helpers::retry_async("upload_manifest", MAX_UPLOAD_RETRIES, || async {
            self.data
                .write_object(bucket_resource.clone(), full_key.clone(), body.clone())
                .send_buffered()
                .await
                .map_err(|e| {
                    CloudError::Other(format!(
                        "GCS manifest upload failed: {} (bucket: {}, key: {}, size: {} bytes)",
                        e,
                        self.bucket,
                        full_key,
                        json.len()
                    ))
                })?;
            Ok(())
        })
        .await
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        let full_key = self.full_key(key);
        debug!("Downloading manifest from GCS: {}", full_key);

        let bucket_resource = self.bucket_resource();

        crate::cloud_helpers::retry_async("download_manifest", MAX_DOWNLOAD_RETRIES, || async {
            let mut resp = self
                .data
                .read_object(bucket_resource.clone(), full_key.clone())
                .send()
                .await
                .map_err(|e| {
                    CloudError::Other(format!(
                        "GCS manifest download failed: {} (bucket: {}, key: {})",
                        e, self.bucket, full_key
                    ))
                })?;
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.next().await {
                let chunk = chunk.map_err(|e| {
                    CloudError::Other(format!(
                        "GCS manifest stream read failed: {} (bucket: {}, key: {})",
                        e, self.bucket, full_key
                    ))
                })?;
                buf.extend_from_slice(&chunk);
            }
            let json = String::from_utf8(buf)
                .map_err(|e| CloudError::Other(format!("manifest not valid UTF-8: {}", e)))?;
            debug!(
                "Downloaded manifest from GCS: {} ({} bytes)",
                full_key,
                json.len()
            );
            Ok(json)
        })
        .await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        let full_key = self.full_key(key);
        debug!("Checking if chunk exists in GCS: {}", full_key);

        match self
            .control
            .get_object()
            .set_bucket(self.bucket_resource())
            .set_object(full_key.clone())
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                // Object absent is a normal probe outcome — fold it
                // into Ok(false). The Google SDK can surface absence
                // either as HTTP 404 (REST) or as a gRPC `NotFound`
                // status (`Code = 5`); since the SDK's transport may
                // hand us either shape, check both. Prior to this
                // both shapes were checked only via http_status_code,
                // and gRPC-style NotFound errors would propagate as
                // hard failures — surfacing on the first
                // `chunk_exists` of any Global-scope dedup write.
                let is_absent = e.http_status_code() == Some(404)
                    || e.status()
                        .is_some_and(|s| s.code == google_cloud_gax::error::rpc::Code::NotFound);
                if is_absent {
                    Ok(false)
                } else {
                    Err(CloudError::Other(format!(
                        "GCS get_object failed: {} (bucket: {}, key: {})",
                        e, self.bucket, full_key
                    )))
                }
            }
        }
    }

    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(key_prefix);
        debug!("Listing objects in GCS with prefix: {}", full_prefix);

        // Drain every page; new SDK is paginated and silently truncates
        // at the first page if we don't.
        let mut keys: Vec<String> = Vec::new();
        let mut items = self
            .control
            .list_objects()
            .set_parent(self.bucket_resource())
            .set_prefix(&full_prefix)
            .by_item();

        while let Some(item) = items.next().await {
            let obj = item.map_err(|e| {
                CloudError::Other(format!(
                    "GCS list_objects failed: {} (bucket: {}, prefix: {})",
                    e, self.bucket, full_prefix
                ))
            })?;
            let k = obj.name;
            // Strip the bucket-side prefix so callers get relative keys.
            let rel = if !self.prefix.is_empty() && k.starts_with(&self.prefix) {
                k[self.prefix.len()..].to_string()
            } else {
                k
            };
            keys.push(rel);
        }

        debug!("Found {} objects with prefix {}", keys.len(), full_prefix);
        Ok(keys)
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        let full_key = self.full_key(key);
        debug!("Deleting object from GCS: {}", full_key);

        self.control
            .delete_object()
            .set_bucket(self.bucket_resource())
            .set_object(full_key.clone())
            .send()
            .await
            .map_err(|e| {
                CloudError::Other(format!(
                    "GCS delete_object failed: {} (bucket: {}, key: {})",
                    e, self.bucket, full_key
                ))
            })?;

        debug!("Deleted object from GCS: {}", full_key);
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "gcs"
    }

    async fn lock_state(&self) -> Result<crate::cloud_backend::LockState> {
        let bucket = self
            .control
            .get_bucket()
            .set_name(self.bucket_resource())
            .send()
            .await
            .map_err(|e| CloudError::Other(format!("GCS get_bucket on {}: {}", self.bucket, e)))?;
        debug!(
            "GCS get_bucket on {}: retention_policy={:?}",
            self.bucket, bucket.retention_policy
        );
        let policy = match bucket.retention_policy {
            Some(p) => p,
            None => {
                debug!(
                    "GCS lock_state: bucket '{}' has no retentionPolicy in API response \
                     (this is a *bucket-level* retention policy - `gcloud storage buckets update \
                     --retention-period`. Per-object retention enabled via `--enable-object-retention` \
                     is a different feature and lives elsewhere). Returning LockState::Off.",
                    self.bucket
                );
                return Ok(crate::cloud_backend::LockState::Off);
            }
        };
        // GCS retention duration is a wkt::Duration. A configured
        // policy is in effect as long as seconds > 0 — sub-day
        // periods are allowed for testing (best-effort enforcement),
        // so treat any positive period as "lock is on" and round up
        // to whole days for `default_days`.
        let secs: u64 = policy
            .retention_duration
            .map(|d| d.seconds().max(0) as u64)
            .unwrap_or(0);
        if secs == 0 {
            return Ok(crate::cloud_backend::LockState::Off);
        }
        let days: u32 = secs.div_ceil(86_400).min(u32::MAX as u64) as u32;
        debug!(
            "GCS lock_state: bucket '{}' retention_duration={}s (~{}d, rounded up), is_locked={}",
            self.bucket, secs, days, policy.is_locked
        );
        if policy.is_locked {
            Ok(crate::cloud_backend::LockState::Compliance { default_days: days })
        } else {
            Ok(crate::cloud_backend::LockState::Governance { default_days: days })
        }
    }

    async fn set_object_legal_hold(&self, key: &str, held: bool) -> Result<()> {
        let full_key = self.full_key(key);
        // Patch the object's `eventBasedHold` field. GCS legal-hold-
        // equivalent primitive; survives bucket lifecycle and prevents
        // deletion until released.
        //
        // **CRITICAL**: the field mask is required. Without it the
        // update wipes every other field on the object. Only update
        // exactly `event_based_hold`.
        let resource = Object::default()
            .set_name(full_key.clone())
            .set_event_based_hold(held);
        let mask = FieldMask::default().set_paths(["event_based_hold"]);
        self.control
            .update_object()
            .set_object(resource)
            .set_update_mask(mask)
            .send()
            .await
            .map_err(|e| {
                CloudError::Other(format!(
                    "GCS update_object (event_based_hold={}) failed: {} (bucket: {}, key: {})",
                    held, e, self.bucket, full_key
                ))
            })?;
        Ok(())
    }

    async fn get_object_legal_hold(&self, key: &str) -> Result<bool> {
        let full_key = self.full_key(key);
        let obj = self
            .control
            .get_object()
            .set_bucket(self.bucket_resource())
            .set_object(full_key.clone())
            .send()
            .await
            .map_err(|e| {
                CloudError::Other(format!(
                    "GCS get_object (legal hold) failed: {} (bucket: {}, key: {})",
                    e, self.bucket, full_key
                ))
            })?;
        Ok(obj.event_based_hold.unwrap_or(false))
    }

    fn clone_box(&self) -> Box<dyn CloudBackend> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_full_key_with_prefix() {
        // Note: We can't easily create a GcsBackend for unit tests without valid credentials
        // These tests verify key construction logic only
        let prefix = "tapes/";
        let key = "chunks/TAPE001/obj-000001.dat";
        let expected = "tapes/chunks/TAPE001/obj-000001.dat";

        let full_key = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{}", prefix, key)
        };

        assert_eq!(full_key, expected);
    }

    #[test]
    fn test_full_key_without_prefix() {
        let prefix = "";
        let key = "chunks/TAPE001/obj-000001.dat";
        let expected = "chunks/TAPE001/obj-000001.dat";

        let full_key = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{}", prefix, key)
        };

        assert_eq!(full_key, expected);
    }
}
