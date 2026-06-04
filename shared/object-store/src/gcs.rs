// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Google Cloud Storage (GCS) backend for cloud storage tier
//!
//! Built on Google's official `google-cloud-storage` crate (the
//! googleapis/google-cloud-rust line). All SDK-touching code lives in
//! [`crate::gcs_api`]; this module composes the backend on top of the
//! [`GcsApi`] seam — retry policy, compression, prefix joining,
//! key-name shaping. Tests inject a mock `GcsApi` impl to exercise
//! every `ObjectStoreBackend` method without an HTTP wire.
//!
//! Provides upload/download operations for chunks and manifests with:
//! - Automatic retry with exponential backoff
//! - GCP credential handling (service account, ADC)
//! - Optional compression support (zstd/lz4)
//! - Error handling and logging

use crate::compression::{CompressionAlgo, CompressionConfig, compress_data, decompress_data};
use crate::gcs_api::{GcsApi, RealGcsApi, build_credentials};
use crate::object_store_backend::ObjectStoreBackend;
use crate::{ObjectStoreError, Result};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::Path;
use std::sync::{Arc, Once};
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
// Backoff cadence is owned by `object_store_helpers::retry_async`.

/// Build the GCS custom-metadata pairs recording how a chunk was
/// compressed, mirroring the S3 / Azure backends' `compression`
/// object metadata. This single marker travels with the object and is
/// the decode signal the download path keys decompression off — so a
/// daemon that later switches `storage.compression` still decodes
/// pre-switch chunks correctly. `None` records `compression: none`.
///
/// Only the algorithm is recorded, never the level: the level is an
/// encoder-side effort knob, and zstd / lz4 frames are self-describing
/// on decode (`decompress_data` takes no level). The marker is a hint,
/// not the source of truth — see DEDUP.md "Backend-side compression"
/// for the content-address recovery path if it is ever lost.
fn compression_metadata(algo: Option<CompressionAlgo>) -> Vec<(String, String)> {
    let value = match algo {
        Some(a) => a.as_str(),
        None => "none",
    };
    vec![("compression".to_string(), value.to_string())]
}

/// Google Cloud Storage backend for storing chunks and manifests
#[derive(Clone)]
pub struct GcsBackend {
    api: Arc<dyn GcsApi>,
    bucket: String,
    prefix: String,
    project_id: String,
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

        let creds = build_credentials(service_account_key_file.as_deref()).await?;
        let api = RealGcsApi::from_creds(&creds).await?;

        debug!(
            "Compression algorithm: {:?}, level: {}",
            compression_config.algorithm, compression_config.level
        );

        Ok(Self {
            api: Arc::new(api),
            bucket,
            prefix,
            project_id,
            compression_config,
        })
    }

    /// Compose a `GcsBackend` from an already-built `GcsApi`. Test
    /// constructor for the mock-injected coverage; production code
    /// uses [`GcsBackend::new`].
    #[cfg(test)]
    pub(crate) fn with_api(
        api: Arc<dyn GcsApi>,
        bucket: String,
        prefix: String,
        project_id: String,
        compression_config: CompressionConfig,
    ) -> Self {
        Self {
            api,
            bucket,
            prefix,
            project_id,
            compression_config,
        }
    }

    /// Construct full GCS object name with prefix.
    fn full_key(&self, key: &str) -> String {
        crate::object_store_helpers::full_key(&self.prefix, key)
    }
}

/// ObjectStoreBackend trait implementation for GcsBackend
#[async_trait]
impl ObjectStoreBackend for GcsBackend {
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
        let bucket = self.bucket.as_str();
        let metadata = compression_metadata(applied_algo);

        crate::object_store_helpers::retry_async("upload_chunk", MAX_UPLOAD_RETRIES, || async {
            self.api
                .write_object(bucket, &full_key, data_bytes.clone(), metadata.clone())
                .await
        })
        .await?;

        Ok((uncompressed_size, compressed_size, applied_algo))
    }

    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        let full_key = self.full_key(key);

        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("failed to stat file: {}", e)))?;
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

        let bucket = self.bucket.as_str();

        // Zero-copy path never compresses, so mark the object
        // uncompressed (mirrors the S3 zero-copy upload).
        let metadata = compression_metadata(None);

        crate::object_store_helpers::retry_async(
            "upload_chunk_zerocopy",
            MAX_UPLOAD_RETRIES,
            || async {
                let data = tokio::fs::read(file_path)
                    .await
                    .map_err(|e| ObjectStoreError::Other(format!("failed to read file: {}", e)))?;
                self.api
                    .write_object(bucket, &full_key, Bytes::from(data), metadata.clone())
                    .await
            },
        )
        .await?;

        Ok(file_size)
    }

    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        let full_key = self.full_key(key);
        debug!("Downloading chunk from GCS: {}", full_key);

        let bucket = self.bucket.as_str();

        crate::object_store_helpers::retry_async("download_chunk", MAX_DOWNLOAD_RETRIES, || async {
            // Drain the streamed body into a single Vec inside the
            // retry closure so a mid-stream error replays the whole
            // RPC instead of returning a half-filled buffer.
            let (buf, metadata) = self.api.read_object(bucket, &full_key).await?;
            debug!("Downloaded {} bytes from GCS: {}", buf.len(), full_key);

            // Decompress off the per-object `compression` marker, not the
            // daemon's current `compression_config` — a config switch
            // after upload must not mis-decode pre-switch chunks. The
            // per-cartridge / per-volume manifest stays the authoritative
            // record; this metadata read mirrors the S3 backend (issue #10).
            let data = match metadata.get("compression").map(String::as_str) {
                Some("zstd") => decompress_data(CompressionAlgo::Zstd, &buf)?,
                Some("lz4") => decompress_data(CompressionAlgo::Lz4, &buf)?,
                Some("none") | None => buf,
                Some(other) => {
                    return Err(ObjectStoreError::Other(format!(
                        "unsupported compression type: {}",
                        other
                    )));
                }
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
                    return Err(ObjectStoreError::Other("Task join failed".to_string()));
                };
                let (result_idx, data) = finished.map_err(|e| {
                    ObjectStoreError::Other(format!("Download task panicked: {}", e))
                })??;
                results[result_idx] = Some(data);
            }

            let gcs = self.clone();
            let key_clone = key.clone();

            tasks.spawn(async move {
                let data = gcs.download_chunk(&key_clone).await?;
                Ok::<(usize, Vec<u8>), ObjectStoreError>((idx, data))
            });
        }

        while let Some(finished) = tasks.join_next().await {
            let (result_idx, data) = finished
                .map_err(|e| ObjectStoreError::Other(format!("Download task panicked: {}", e)))??;
            results[result_idx] = Some(data);
        }

        results
            .into_iter()
            .enumerate()
            .map(|(idx, opt)| {
                opt.ok_or_else(|| {
                    ObjectStoreError::Other(format!(
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

        let bucket = self.bucket.as_str();
        let body = Bytes::from(json.as_bytes().to_vec());

        crate::object_store_helpers::retry_async("upload_manifest", MAX_UPLOAD_RETRIES, || async {
            // Manifests carry no compression marker (mirrors S3).
            self.api
                .write_object(bucket, &full_key, body.clone(), Vec::new())
                .await
        })
        .await
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        let full_key = self.full_key(key);
        debug!("Downloading manifest from GCS: {}", full_key);

        let bucket = self.bucket.as_str();

        crate::object_store_helpers::retry_async(
            "download_manifest",
            MAX_DOWNLOAD_RETRIES,
            || async {
                let (buf, _metadata) = self.api.read_object(bucket, &full_key).await?;
                let json = String::from_utf8(buf).map_err(|e| {
                    ObjectStoreError::Other(format!("manifest not valid UTF-8: {}", e))
                })?;
                debug!(
                    "Downloaded manifest from GCS: {} ({} bytes)",
                    full_key,
                    json.len()
                );
                Ok(json)
            },
        )
        .await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        let full_key = self.full_key(key);
        debug!("Checking if chunk exists in GCS: {}", full_key);
        self.api.object_exists(&self.bucket, &full_key).await
    }

    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(key_prefix);
        debug!("Listing objects in GCS with prefix: {}", full_prefix);

        let names = self
            .api
            .list_object_names(&self.bucket, &full_prefix)
            .await?;

        // Strip the bucket-side prefix so callers get relative keys.
        let keys: Vec<String> = names
            .into_iter()
            .map(|k| {
                if !self.prefix.is_empty() && k.starts_with(&self.prefix) {
                    k[self.prefix.len()..].to_string()
                } else {
                    k
                }
            })
            .collect();

        debug!("Found {} objects with prefix {}", keys.len(), full_prefix);
        Ok(keys)
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        let full_key = self.full_key(key);
        debug!("Deleting object from GCS: {}", full_key);
        self.api.delete_object(&self.bucket, &full_key).await?;
        debug!("Deleted object from GCS: {}", full_key);
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "gcs"
    }

    async fn lock_state(&self) -> Result<crate::object_store_backend::LockState> {
        let policy = self.api.get_bucket_retention(&self.bucket).await?;
        let Some(policy) = policy else {
            debug!(
                "GCS lock_state: bucket '{}' has no retentionPolicy in API response \
                 (this is a *bucket-level* retention policy - `gcloud storage buckets update \
                 --retention-period`. Per-object retention enabled via `--enable-object-retention` \
                 is a different feature and lives elsewhere). Returning LockState::Off.",
                self.bucket
            );
            return Ok(crate::object_store_backend::LockState::Off);
        };
        // GCS retention duration is a wkt::Duration. A configured
        // policy is in effect as long as seconds > 0 — sub-day
        // periods are allowed for testing (best-effort enforcement),
        // so treat any positive period as "lock is on" and round up
        // to whole days for `default_days`.
        let days: u32 = policy.seconds.div_ceil(86_400).min(u32::MAX as u64) as u32;
        debug!(
            "GCS lock_state: bucket '{}' retention_duration={}s (~{}d, rounded up), is_locked={}",
            self.bucket, policy.seconds, days, policy.is_locked
        );
        if policy.is_locked {
            Ok(crate::object_store_backend::LockState::Compliance { default_days: days })
        } else {
            Ok(crate::object_store_backend::LockState::Governance { default_days: days })
        }
    }

    async fn set_object_legal_hold(&self, key: &str, held: bool) -> Result<()> {
        let full_key = self.full_key(key);
        self.api
            .set_event_based_hold(&self.bucket, &full_key, held)
            .await
    }

    async fn get_object_legal_hold(&self, key: &str) -> Result<bool> {
        let full_key = self.full_key(key);
        self.api.get_event_based_hold(&self.bucket, &full_key).await
    }

    fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::CompressionConfig;
    use crate::gcs_api::RetentionPolicy;
    use crate::object_store_backend::LockState;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mock `GcsApi` impl driven by per-method outcome queues.
    ///
    /// Each method pops its next `Outcome` from the matching queue and
    /// either returns the canned `Ok(...)` or the canned error. When a
    /// queue is empty, the default behaviour is `Ok(...)` of a benign
    /// value — that matches the production retry/back-off pattern where
    /// a successful next attempt ends the loop.
    /// Captured `(bucket, key, body, metadata)` from each `write_object`
    /// call. Aliased to keep clippy's `type_complexity` lint happy.
    type CapturedWrite = (String, String, Vec<u8>, Vec<(String, String)>);

    #[derive(Default, Debug)]
    struct MockGcsApi {
        write_outcomes: Mutex<Vec<Result<()>>>,
        read_outcomes: Mutex<Vec<Result<Vec<u8>>>>,
        /// Custom metadata the mock returns alongside every read (empty
        /// by default; set to exercise the metadata-driven decompress path).
        read_metadata: Mutex<HashMap<String, String>>,
        exists_outcomes: Mutex<Vec<Result<bool>>>,
        list_outcomes: Mutex<Vec<Result<Vec<String>>>>,
        delete_outcomes: Mutex<Vec<Result<()>>>,
        retention_outcomes: Mutex<Vec<Result<Option<RetentionPolicy>>>>,
        hold_get_outcomes: Mutex<Vec<Result<bool>>>,
        hold_set_outcomes: Mutex<Vec<Result<()>>>,

        write_calls: AtomicU32,
        read_calls: AtomicU32,
        exists_calls: AtomicU32,
        list_calls: AtomicU32,
        delete_calls: AtomicU32,
        retention_calls: AtomicU32,
        hold_get_calls: AtomicU32,
        hold_set_calls: AtomicU32,

        captured_write: Mutex<Vec<CapturedWrite>>,
        captured_hold_set: Mutex<Vec<(String, String, bool)>>,
    }

    impl MockGcsApi {
        fn pop_or<T>(q: &Mutex<Vec<Result<T>>>, default: impl FnOnce() -> Result<T>) -> Result<T> {
            // Poisoned mutex in test code means an earlier test panicked;
            // recover and let the current test surface the real issue.
            let mut g = match q.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.is_empty() { default() } else { g.remove(0) }
        }
    }

    #[async_trait]
    impl GcsApi for MockGcsApi {
        async fn write_object(
            &self,
            bucket: &str,
            key: &str,
            body: Bytes,
            metadata: Vec<(String, String)>,
        ) -> Result<()> {
            self.write_calls.fetch_add(1, Ordering::SeqCst);
            self.captured_write.lock().expect("write capture").push((
                bucket.to_string(),
                key.to_string(),
                body.to_vec(),
                metadata,
            ));
            Self::pop_or(&self.write_outcomes, || Ok(()))
        }
        async fn read_object(
            &self,
            _bucket: &str,
            _key: &str,
        ) -> Result<(Vec<u8>, HashMap<String, String>)> {
            self.read_calls.fetch_add(1, Ordering::SeqCst);
            let bytes = Self::pop_or(&self.read_outcomes, || Ok(Vec::new()))?;
            let meta = self.read_metadata.lock().expect("read metadata").clone();
            Ok((bytes, meta))
        }
        async fn object_exists(&self, _bucket: &str, _key: &str) -> Result<bool> {
            self.exists_calls.fetch_add(1, Ordering::SeqCst);
            Self::pop_or(&self.exists_outcomes, || Ok(false))
        }
        async fn get_event_based_hold(&self, _bucket: &str, _key: &str) -> Result<bool> {
            self.hold_get_calls.fetch_add(1, Ordering::SeqCst);
            Self::pop_or(&self.hold_get_outcomes, || Ok(false))
        }
        async fn set_event_based_hold(&self, bucket: &str, key: &str, held: bool) -> Result<()> {
            self.hold_set_calls.fetch_add(1, Ordering::SeqCst);
            self.captured_hold_set
                .lock()
                .expect("hold set capture")
                .push((bucket.to_string(), key.to_string(), held));
            Self::pop_or(&self.hold_set_outcomes, || Ok(()))
        }
        async fn list_object_names(&self, _bucket: &str, _prefix: &str) -> Result<Vec<String>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Self::pop_or(&self.list_outcomes, || Ok(Vec::new()))
        }
        async fn delete_object(&self, _bucket: &str, _key: &str) -> Result<()> {
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            Self::pop_or(&self.delete_outcomes, || Ok(()))
        }
        async fn get_bucket_retention(&self, _bucket: &str) -> Result<Option<RetentionPolicy>> {
            self.retention_calls.fetch_add(1, Ordering::SeqCst);
            Self::pop_or(&self.retention_outcomes, || Ok(None))
        }
    }

    fn backend_with(api: Arc<dyn GcsApi>) -> GcsBackend {
        GcsBackend::with_api(
            api,
            "my-bucket".to_string(),
            "tapes/".to_string(),
            "my-project".to_string(),
            CompressionConfig::disabled(),
        )
    }

    fn backend_with_compression(api: Arc<dyn GcsApi>, algo: CompressionAlgo) -> GcsBackend {
        GcsBackend::with_api(
            api,
            "my-bucket".to_string(),
            "tapes/".to_string(),
            "my-project".to_string(),
            CompressionConfig::new(Some(algo), 3),
        )
    }

    #[test]
    fn full_key_with_prefix() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api);
        assert_eq!(
            backend.full_key("chunks/TAPE001/obj-000001.dat"),
            "tapes/chunks/TAPE001/obj-000001.dat"
        );
    }

    #[test]
    fn full_key_without_prefix() {
        let api = Arc::new(MockGcsApi::default());
        let backend = GcsBackend::with_api(
            api,
            "my-bucket".to_string(),
            String::new(),
            "my-project".to_string(),
            CompressionConfig::disabled(),
        );
        assert_eq!(
            backend.full_key("chunks/TAPE001/obj-000001.dat"),
            "chunks/TAPE001/obj-000001.dat"
        );
    }

    #[test]
    fn backend_type_is_gcs_and_supports_legal_hold() {
        let backend = backend_with(Arc::new(MockGcsApi::default()));
        assert_eq!(backend.backend_type(), "gcs");
        assert!(backend.supports_legal_hold());
    }

    #[test]
    fn debug_omits_sdk_internals() {
        let backend = backend_with(Arc::new(MockGcsApi::default()));
        let s = format!("{:?}", backend);
        assert!(s.contains("bucket"));
        assert!(s.contains("my-bucket"));
        assert!(s.contains("my-project"));
    }

    #[test]
    fn clone_box_yields_independent_handle() {
        let backend = backend_with(Arc::new(MockGcsApi::default()));
        let boxed: Box<dyn ObjectStoreBackend> = Box::new(backend);
        let cloned = boxed.clone();
        assert_eq!(cloned.backend_type(), "gcs");
    }

    #[tokio::test(start_paused = true)]
    async fn upload_chunk_happy_path_no_compression() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api.clone());
        let (uncompressed, compressed, algo) = backend
            .upload_chunk("chunks/x.dat", b"hello world")
            .await
            .expect("upload_chunk");
        assert_eq!(uncompressed, 11);
        assert!(compressed.is_none());
        assert!(algo.is_none());
        assert_eq!(api.write_calls.load(Ordering::SeqCst), 1);
        let captured = api.captured_write.lock().expect("captured").clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, "my-bucket");
        assert_eq!(captured[0].1, "tapes/chunks/x.dat");
        assert_eq!(captured[0].2, b"hello world");
        // Uncompressed chunks still carry an explicit `compression: none`
        // marker, mirroring the S3 backend.
        assert_eq!(
            captured[0].3,
            vec![("compression".to_string(), "none".to_string())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn upload_chunk_compresses_when_configured() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with_compression(api.clone(), CompressionAlgo::Zstd);
        let payload = vec![0u8; 4096]; // highly compressible
        let (uncompressed, compressed, algo) = backend
            .upload_chunk("chunks/z.dat", &payload)
            .await
            .expect("upload_chunk");
        assert_eq!(uncompressed, 4096);
        let csz = compressed.expect("compressed size present");
        assert!(csz < 4096, "compressed must be smaller, got {}", csz);
        assert_eq!(algo, Some(CompressionAlgo::Zstd));
        // The compression marker (algorithm only — no level) is recorded
        // in the object's custom metadata, the decode signal on download.
        let captured = api.captured_write.lock().expect("captured").clone();
        assert_eq!(
            captured[0].3,
            vec![("compression".to_string(), "zstd".to_string())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn upload_chunk_retries_on_transient_error() {
        let api = Arc::new(MockGcsApi::default());
        // Fail twice (retryable), succeed on the 3rd attempt.
        {
            let mut g = api.write_outcomes.lock().expect("queue");
            g.push(Err(ObjectStoreError::Other(
                "503 service unavailable".into(),
            )));
            g.push(Err(ObjectStoreError::Other("502 bad gateway".into())));
            g.push(Ok(()));
        }
        let backend = backend_with(api.clone());
        backend
            .upload_chunk("chunks/x.dat", b"payload")
            .await
            .expect("eventual success");
        assert_eq!(api.write_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn upload_chunk_fails_fast_on_permanent_auth() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.write_outcomes.lock().expect("queue");
            g.push(Err(ObjectStoreError::Authz("forbidden".into())));
        }
        let backend = backend_with(api.clone());
        let err = backend
            .upload_chunk("chunks/x.dat", b"payload")
            .await
            .expect_err("must fail fast");
        match err {
            ObjectStoreError::Authz(_) => {}
            other => panic!("expected Authz, got {other:?}"),
        }
        // Permanent classification → single attempt, no retry burn.
        assert_eq!(api.write_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn upload_chunk_zerocopy_streams_file() {
        use tempfile::TempDir;
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("chunk.dat");
        tokio::fs::write(&path, b"abcdef").await.expect("seed");

        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api.clone());
        let n = backend
            .upload_chunk_zerocopy("chunks/zc.dat", &path)
            .await
            .expect("zerocopy upload");
        assert_eq!(n, 6);
        assert_eq!(api.write_calls.load(Ordering::SeqCst), 1);
        let captured = api.captured_write.lock().expect("captured").clone();
        assert_eq!(captured[0].2, b"abcdef");
        assert_eq!(
            captured[0].3,
            vec![("compression".to_string(), "none".to_string())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn upload_chunk_zerocopy_warns_when_compression_enabled() {
        // The path still uploads — the warn! is informational only.
        // (We can't easily snoop log output, just confirm behavior.)
        use tempfile::TempDir;
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("chunk.dat");
        tokio::fs::write(&path, b"xyz").await.expect("seed");

        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with_compression(api.clone(), CompressionAlgo::Zstd);
        backend
            .upload_chunk_zerocopy("chunks/zc.dat", &path)
            .await
            .expect("zerocopy with compression-enabled config still uploads raw");
        let captured = api.captured_write.lock().expect("captured").clone();
        assert_eq!(captured[0].2, b"xyz", "zerocopy uploads raw bytes");
    }

    #[tokio::test(start_paused = true)]
    async fn upload_chunk_zerocopy_missing_file_errors() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api.clone());
        let err = backend
            .upload_chunk_zerocopy("chunks/zc.dat", std::path::Path::new("/no/such/file.dat"))
            .await
            .expect_err("missing file must error");
        assert!(matches!(err, ObjectStoreError::Other(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn download_chunk_happy_path_no_compression() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.read_outcomes.lock().expect("queue");
            g.push(Ok(b"hello".to_vec()));
        }
        let backend = backend_with(api.clone());
        let got = backend
            .download_chunk("chunks/x.dat")
            .await
            .expect("download");
        assert_eq!(got, b"hello");
    }

    #[tokio::test(start_paused = true)]
    async fn download_chunk_decompresses_from_object_metadata() {
        // Pre-compress the payload so the mock returns compressed bytes;
        // the backend decompresses off the object's `compression`
        // metadata, not its own config (here: compression disabled).
        let payload = vec![0u8; 1024];
        let compressed = compress_data(CompressionAlgo::Zstd, &payload, 3).expect("compress");
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.read_outcomes.lock().expect("queue");
            g.push(Ok(compressed));
            *api.read_metadata.lock().expect("meta") =
                HashMap::from([("compression".to_string(), "zstd".to_string())]);
        }
        let backend = backend_with(api.clone());
        let got = backend
            .download_chunk("chunks/z.dat")
            .await
            .expect("download + decompress");
        assert_eq!(got, payload);
    }

    #[tokio::test(start_paused = true)]
    async fn download_chunk_uses_object_metadata_over_config() {
        // Issue #10 regression: a chunk uploaded as zstd must still
        // decode after the operator switches `storage.compression` to
        // lz4 and restarts. The per-object marker, not the live config,
        // governs decompression.
        let payload = b"the quick brown fox jumped over the lazy dog".repeat(8);
        let compressed = compress_data(CompressionAlgo::Zstd, &payload, 3).expect("compress");
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.read_outcomes.lock().expect("queue");
            g.push(Ok(compressed));
            *api.read_metadata.lock().expect("meta") =
                HashMap::from([("compression".to_string(), "zstd".to_string())]);
        }
        // Backend config says lz4 — deliberately mismatched.
        let backend = backend_with_compression(api.clone(), CompressionAlgo::Lz4);
        let got = backend
            .download_chunk("chunks/z.dat")
            .await
            .expect("decode via metadata, not config");
        assert_eq!(got, payload);
    }

    #[tokio::test(start_paused = true)]
    async fn download_chunk_rejects_unknown_compression_marker() {
        let api = Arc::new(MockGcsApi::default());
        {
            // `Other` classifies as retryable, so outlast the budget.
            let mut g = api.read_outcomes.lock().expect("queue");
            for _ in 0..8 {
                g.push(Ok(b"payload".to_vec()));
            }
            *api.read_metadata.lock().expect("meta") =
                HashMap::from([("compression".to_string(), "brotli".to_string())]);
        }
        let backend = backend_with(api);
        let err = backend
            .download_chunk("chunks/z.dat")
            .await
            .expect_err("unknown marker must error");
        match err {
            ObjectStoreError::Other(msg) => assert!(msg.contains("brotli")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn download_chunk_retries_on_transient_error() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.read_outcomes.lock().expect("queue");
            g.push(Err(ObjectStoreError::Other("504 gateway timeout".into())));
            g.push(Ok(b"ok".to_vec()));
        }
        let backend = backend_with(api.clone());
        let got = backend
            .download_chunk("chunks/x.dat")
            .await
            .expect("recovers");
        assert_eq!(got, b"ok");
        assert_eq!(api.read_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn download_chunks_parallel_preserves_order() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.read_outcomes.lock().expect("queue");
            // Three downloads — though parallel, the mock pops in order;
            // assert each result is non-empty rather than positionally
            // tied. (The per-call body isn't reachable per-key in this
            // mock; the contract under test is order preservation in
            // the returned Vec.)
            g.push(Ok(b"a".to_vec()));
            g.push(Ok(b"b".to_vec()));
            g.push(Ok(b"c".to_vec()));
        }
        let backend = backend_with(api.clone());
        let keys = vec![
            "chunks/1.dat".to_string(),
            "chunks/2.dat".to_string(),
            "chunks/3.dat".to_string(),
        ];
        let results = backend
            .download_chunks_parallel(&keys)
            .await
            .expect("parallel download");
        assert_eq!(results.len(), 3);
        // Each slot must be Some(non-empty).
        for r in &results {
            assert!(!r.is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn download_chunks_parallel_empty_input_returns_empty() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api);
        let got = backend
            .download_chunks_parallel(&[])
            .await
            .expect("empty input is Ok");
        assert!(got.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn upload_and_download_manifest_round_trip() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.read_outcomes.lock().expect("queue");
            g.push(Ok(b"{\"v\":1}".to_vec()));
        }
        let backend = backend_with(api.clone());
        backend
            .upload_manifest("manifests/m.json", "{\"v\":1}")
            .await
            .expect("upload manifest");
        let got = backend
            .download_manifest("manifests/m.json")
            .await
            .expect("download manifest");
        assert_eq!(got, "{\"v\":1}");
    }

    #[tokio::test(start_paused = true)]
    async fn download_manifest_rejects_invalid_utf8() {
        // The UTF-8 decode error classifies as `Other` → retryable, so
        // the closure retries MAX_DOWNLOAD_RETRIES times. Push enough
        // bad-byte responses to outlast the budget; the final error
        // surfacing is what we care about.
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.read_outcomes.lock().expect("queue");
            for _ in 0..8 {
                g.push(Ok(vec![0xff, 0xfe, 0xfd]));
            }
        }
        let backend = backend_with(api);
        let err = backend
            .download_manifest("manifests/bad.json")
            .await
            .expect_err("must reject");
        assert!(matches!(err, ObjectStoreError::Other(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn chunk_exists_true_and_false() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.exists_outcomes.lock().expect("queue");
            g.push(Ok(true));
            g.push(Ok(false));
        }
        let backend = backend_with(api.clone());
        assert!(backend.chunk_exists("a").await.expect("exists"));
        assert!(!backend.chunk_exists("b").await.expect("exists"));
    }

    #[tokio::test(start_paused = true)]
    async fn chunk_exists_propagates_hard_error() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.exists_outcomes.lock().expect("queue");
            g.push(Err(ObjectStoreError::Other(
                "AccessDenied: forbidden".into(),
            )));
        }
        let backend = backend_with(api);
        let err = backend.chunk_exists("a").await.expect_err("must error");
        assert!(matches!(err, ObjectStoreError::Other(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn list_objects_strips_prefix() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.list_outcomes.lock().expect("queue");
            g.push(Ok(vec![
                "tapes/chunks/a.dat".to_string(),
                "tapes/chunks/b.dat".to_string(),
            ]));
        }
        let backend = backend_with(api);
        let got = backend.list_objects("chunks/").await.expect("list");
        assert_eq!(
            got,
            vec!["chunks/a.dat".to_string(), "chunks/b.dat".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn list_objects_without_prefix_pass_through() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.list_outcomes.lock().expect("queue");
            g.push(Ok(vec!["chunks/a.dat".to_string()]));
        }
        let backend = GcsBackend::with_api(
            api,
            "my-bucket".to_string(),
            String::new(),
            "my-project".to_string(),
            CompressionConfig::disabled(),
        );
        let got = backend.list_objects("chunks/").await.expect("list");
        assert_eq!(got, vec!["chunks/a.dat".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn delete_object_invokes_api() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api.clone());
        backend.delete_object("chunks/x.dat").await.expect("delete");
        assert_eq!(api.delete_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn lock_state_off_when_policy_absent() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.retention_outcomes.lock().expect("queue");
            g.push(Ok(None));
        }
        let backend = backend_with(api);
        assert_eq!(backend.lock_state().await.expect("lock"), LockState::Off);
    }

    #[tokio::test(start_paused = true)]
    async fn lock_state_governance_when_unlocked() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.retention_outcomes.lock().expect("queue");
            g.push(Ok(Some(RetentionPolicy {
                seconds: 7 * 86_400,
                is_locked: false,
            })));
        }
        let backend = backend_with(api);
        assert_eq!(
            backend.lock_state().await.expect("lock"),
            LockState::Governance { default_days: 7 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn lock_state_compliance_when_locked() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.retention_outcomes.lock().expect("queue");
            g.push(Ok(Some(RetentionPolicy {
                seconds: 30 * 86_400,
                is_locked: true,
            })));
        }
        let backend = backend_with(api);
        assert_eq!(
            backend.lock_state().await.expect("lock"),
            LockState::Compliance { default_days: 30 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn lock_state_rounds_subday_to_one() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.retention_outcomes.lock().expect("queue");
            g.push(Ok(Some(RetentionPolicy {
                seconds: 60,
                is_locked: false,
            })));
        }
        let backend = backend_with(api);
        assert_eq!(
            backend.lock_state().await.expect("lock"),
            LockState::Governance { default_days: 1 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn lock_state_propagates_retention_error() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.retention_outcomes.lock().expect("queue");
            g.push(Err(ObjectStoreError::Other("AccessDenied".into())));
        }
        let backend = backend_with(api);
        backend.lock_state().await.expect_err("error");
    }

    #[tokio::test(start_paused = true)]
    async fn set_object_legal_hold_passes_flag_through() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api.clone());
        backend
            .set_object_legal_hold("chunks/x.dat", true)
            .await
            .expect("set hold");
        backend
            .set_object_legal_hold("chunks/x.dat", false)
            .await
            .expect("clear hold");
        let captured = api.captured_hold_set.lock().expect("captured").clone();
        assert_eq!(captured.len(), 2);
        assert!(captured[0].2);
        assert!(!captured[1].2);
        assert_eq!(captured[0].1, "tapes/chunks/x.dat");
    }

    #[tokio::test(start_paused = true)]
    async fn get_object_legal_hold_returns_api_value() {
        let api = Arc::new(MockGcsApi::default());
        {
            let mut g = api.hold_get_outcomes.lock().expect("queue");
            g.push(Ok(true));
            g.push(Ok(false));
        }
        let backend = backend_with(api);
        assert!(
            backend
                .get_object_legal_hold("chunks/x.dat")
                .await
                .expect("get hold")
        );
        assert!(
            !backend
                .get_object_legal_hold("chunks/x.dat")
                .await
                .expect("get hold")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn warmup_prefix_default_returns_zero() {
        let backend = backend_with(Arc::new(MockGcsApi::default()));
        // ObjectStoreBackend trait default — GCS doesn't override.
        let n = backend.warmup_prefix("chunks/").await.expect("warmup");
        assert_eq!(n, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn upload_versioned_delegates_to_upload_chunk() {
        let api = Arc::new(MockGcsApi::default());
        let backend = backend_with(api.clone());
        backend
            .upload_versioned("manifests/m.json", b"{}")
            .await
            .expect("upload versioned");
        assert_eq!(api.write_calls.load(Ordering::SeqCst), 1);
    }
}
