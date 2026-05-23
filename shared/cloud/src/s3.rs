// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! S3 backend for cloud storage tier
//!
//! Provides upload/download operations for chunks and manifests with:
//! - Automatic retry with exponential backoff
//! - AWS credential handling (env vars, IAM, shared credentials)
//! - Error handling and logging

use crate::cloud_backend::CloudBackend;
use crate::cloud_config::ResolvedS3Auth;
use crate::compression::{CompressionAlgo, CompressionConfig, compress_data, decompress_data};
use crate::{CloudError, Result};
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;

/// Format an `SdkError` into a one-line description that distinguishes
/// service errors, dispatch failures (network vs. credential signing),
/// timeouts, and construction failures.
///
/// Returned strings carry a short tag that the cloud-config classifier
/// matches against (e.g. `dispatch failure (io:`, `dispatch failure (user:`).
fn describe_sdk_error<E, R>(op: &str, err: &SdkError<E, R>) -> String
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    match err {
        SdkError::ServiceError(svc) => {
            let inner = svc.err();
            format!(
                "{op} failed: service error (code: {:?}, message: {})",
                inner.code(),
                inner.message().unwrap_or("no message"),
            )
        }
        SdkError::DispatchFailure(d) => {
            // Drill into the connector error so we can tell network from
            // credentials/configuration failures.
            let kind = if d.is_io() {
                "io"
            } else if d.is_timeout() {
                "timeout"
            } else if d.is_user() {
                "user" // typically: missing/invalid credentials, signing failure
            } else {
                "other"
            };
            // Also surface the underlying source(s) for the raw line.
            let mut detail = String::new();
            if let Some(conn) = d.as_connector_error() {
                detail.push_str(&format!("{}", conn));
                let mut src: Option<&dyn std::error::Error> = std::error::Error::source(conn);
                while let Some(s) = src {
                    detail.push_str(&format!(" -> {}", s));
                    src = s.source();
                }
            } else {
                detail.push_str("no connector error attached");
            }
            format!("{op} failed: dispatch failure ({kind}: {detail})")
        }
        SdkError::TimeoutError(_) => format!("{op} failed: timeout"),
        SdkError::ConstructionFailure(c) => {
            format!(
                "{op} failed: construction failure ({c:?}) — typically missing/invalid credentials or bad configuration"
            )
        }
        SdkError::ResponseError(r) => format!("{op} failed: response error ({r:?})"),
        _ => format!("{op} failed: {err}"),
    }
}
use std::path::Path;
use tokio::task::JoinSet;
use tracing::{debug, warn};

/// Maximum number of retry attempts for uploads.
const MAX_UPLOAD_RETRIES: u32 = 5;
/// Maximum number of retry attempts for downloads.
const MAX_DOWNLOAD_RETRIES: u32 = 3;
// Backoff cadence is owned by `cloud_helpers::retry_async`.

/// S3 backend for storing chunks and manifests
#[derive(Clone, Debug)]
pub struct S3Backend {
    client: Client,
    bucket: String,
    prefix: String,
    /// Compression configuration (Milestone 5 Phase 4)
    compression_config: CompressionConfig,
}

impl S3Backend {
    /// Create a new S3 backend
    ///
    /// Credential loading depends on the `auth` parameter:
    /// - `None`  → standard AWS credential chain (env vars → IRSA →
    ///   SSO → ECS task role → EC2 IMDS → `~/.aws/credentials`).
    /// - `Some(ResolvedS3Auth::Static)` → static access-key / secret
    ///   (and optional session token) injected directly into the SDK.
    ///   The chain is bypassed entirely.
    /// - `Some(ResolvedS3Auth::Profile)` → named profile from
    ///   `~/.aws/credentials` / `~/.aws/config`. Other chain rungs
    ///   are bypassed.
    ///
    /// Strict-override is a feature, not a bug: the AWS chain is
    /// process-global and can carry only one set of credentials, so
    /// running multiple S3-flavored backends (AWS S3 + MinIO + Wasabi)
    /// in one daemon requires explicit per-backend auth.
    ///
    /// # Arguments
    /// * `bucket` - S3 bucket name
    /// * `prefix` - Key prefix (e.g., "tapes/")
    /// * `region` - AWS region (e.g., "us-east-1")
    /// * `endpoint_url` - Optional custom S3 endpoint (e.g., "http://localhost:9000" for MinIO)
    /// * `path_style` - Optional path-style override. `None` infers
    ///   from `endpoint_url` (path-style on, virtual-host off).
    ///   `Some(true)`/`Some(false)` overrides the inference.
    /// * `auth` - Optional per-backend credentials; see above
    /// * `compression_config` - Compression configuration (preset, adaptive, etc.)
    pub async fn new(
        bucket: String,
        prefix: String,
        region: String,
        endpoint_url: Option<String>,
        path_style: Option<bool>,
        auth: Option<ResolvedS3Auth>,
        compression_config: CompressionConfig,
    ) -> Result<Self> {
        debug!(
            "Initializing S3 backend: bucket={}, prefix={}, region={}",
            bucket, prefix, region
        );
        if let Some(ref endpoint) = endpoint_url {
            debug!("Using custom S3 endpoint: {}", endpoint);
        }

        // Load AWS configuration. With `auth = None` we fall through
        // to the SDK's default credential chain. With `auth = Some(...)`
        // we explicitly inject a credentials provider, which the SDK
        // treats as a strict override — no env vars / IMDS / shared
        // credentials file lookup happens for this backend.
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region));
        match &auth {
            None => {
                debug!("Using AWS credential chain (env vars, IAM, or shared credentials file)");
            }
            Some(ResolvedS3Auth::Static {
                access_key_id,
                secret_access_key,
                session_token,
            }) => {
                debug!(
                    "Using explicit static credentials (chain bypassed). access_key_id starts with '{}...'",
                    access_key_id.chars().take(4).collect::<String>(),
                );
                let creds = Credentials::new(
                    access_key_id.clone(),
                    secret_access_key.clone(),
                    session_token.clone(),
                    None, // expiration — None = "never expires" from the SDK's POV
                    "thurvtl-config",
                );
                loader = loader.credentials_provider(creds);
            }
            Some(ResolvedS3Auth::Profile { name }) => {
                debug!("Using named profile '{}' (chain bypassed)", name);
                let provider = aws_config::profile::ProfileFileCredentialsProvider::builder()
                    .profile_name(name)
                    .build();
                loader = loader.credentials_provider(provider);
            }
        }
        let config = loader.load().await;

        // Build S3 client with optional custom endpoint
        let mut s3_config_builder = aws_sdk_s3::config::Builder::from(&config);

        let endpoint_was_set = endpoint_url.is_some();
        if let Some(endpoint) = endpoint_url {
            s3_config_builder = s3_config_builder.endpoint_url(endpoint);
        } else {
            // Native AWS: opt into the dualstack endpoint variant
            // (`s3.dualstack.<region>.amazonaws.com`) so IPv6-capable
            // hosts skip the IPv4 NAT path. Dualstack endpoints publish
            // both A and AAAA records, so IPv4-only hosts transparently
            // fall back. Skipped when a custom endpoint_url is set —
            // MinIO / AIStor / Wasabi / etc. don't have a dualstack
            // variant, and the operator's URL is already authoritative.
            s3_config_builder = s3_config_builder.use_dual_stack(true);
        }

        // Path-style resolution: explicit override wins; otherwise
        // default to path-style for custom endpoints (MinIO / Ceph RGW
        // / AIStor only support that shape) and leave AWS at its
        // virtual-host default.
        let use_path_style = path_style.unwrap_or(endpoint_was_set);
        if use_path_style {
            s3_config_builder = s3_config_builder.force_path_style(true);
        }

        let s3_config = s3_config_builder.build();
        let client = Client::from_conf(s3_config);

        debug!(
            "Compression algorithm: {:?}, level: {}",
            compression_config.algorithm, compression_config.level
        );

        Ok(Self {
            client,
            bucket,
            prefix,
            compression_config,
        })
    }

    /// Upload a chunk to S3 with retry logic and optional compression
    ///
    /// # Arguments
    /// * `key` - S3 object key (without prefix, e.g., "chunks/TAPE001/obj-000001.dat")
    /// * `data` - Chunk data bytes
    ///
    /// # Returns
    /// Ok((uncompressed_size, compressed_size_opt, compression_algo)) — original
    /// size, compressed size if a compressor was applied, and which algorithm
    /// was used (None when uploaded uncompressed). The algorithm is recorded
    /// per-chunk in the manifest so reads can decompress without consulting
    /// the daemon's current config.
    pub async fn upload_chunk(
        &self,
        key: &str,
        data: &[u8],
    ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
        let full_key = self.full_key(key);
        let uncompressed_size = data.len() as u64;

        // Compress if a cloud-side algorithm is configured.
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
                        (comp_size as f64 / uncompressed_size as f64) * 100.0
                    );
                    (compressed, Some(comp_size), Some(algo))
                }
                None => {
                    debug!("Compression disabled for chunk {}", full_key);
                    (data.to_vec(), None, None)
                }
            };

        debug!(
            "Uploading chunk to S3: {} ({} bytes)",
            full_key,
            data_to_upload.len()
        );

        // Wrap once in `bytes::Bytes` so each retry attempt clones a
        // cheap Arc handle instead of memcpy'ing the full Vec. For a
        // 32 MiB FastCDC chunk × 6 attempts × concurrent upload of 8
        // that's ~1.5 GiB of avoided churn on a stalled batch.
        let data_bytes = bytes::Bytes::from(data_to_upload);

        crate::cloud_helpers::retry_async("upload_chunk", MAX_UPLOAD_RETRIES, || async {
            let body = ByteStream::from(data_bytes.clone());
            let mut put_request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(&full_key)
                .body(body);

            // Record the algorithm in object metadata so an out-of-band
            // tool can tell what's on disk without the manifest.
            match applied_algo {
                Some(algo) => {
                    put_request = put_request.metadata("compression", algo.as_str());
                    if matches!(algo, CompressionAlgo::Zstd) {
                        put_request = put_request.metadata(
                            "compression_level",
                            self.compression_config.level.to_string(),
                        );
                    }
                }
                None => {
                    put_request = put_request.metadata("compression", "none");
                }
            }

            put_request.send().await.map_err(|e| {
                // Extract detailed error information from the AWS SDK error
                let error_msg = if let Some(service_err) = e.as_service_error() {
                    format!(
                        "chunk upload failed: {} (code: {:?}, message: {}, bucket: {}, key: {})",
                        e,
                        service_err.code(),
                        service_err.message().unwrap_or("no message"),
                        self.bucket,
                        full_key
                    )
                } else {
                    format!(
                        "chunk upload failed: {} (bucket: {}, key: {}, error type: {:?})",
                        e,
                        self.bucket,
                        full_key,
                        std::any::type_name_of_val(&e)
                    )
                };
                CloudError::Other(error_msg)
            })?;
            Ok(())
        })
        .await?;

        Ok((uncompressed_size, compressed_size, applied_algo))
    }

    /// Upload a chunk from a file path using zero-copy (sendfile-like) streaming
    ///
    /// This method streams the file directly to S3 without loading it into memory,
    /// providing better performance for large chunks.
    ///
    /// # Arguments
    /// * `key` - S3 object key (without prefix, e.g., "chunks/TAPE001/obj-000001.dat")
    /// * `file_path` - Path to the chunk file on disk
    ///
    /// # Returns
    /// Ok(file_size) - Returns the size of the uploaded file
    ///
    /// # Note
    /// This method does NOT support compression. Use upload_chunk() if compression is needed.
    pub async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        let full_key = self.full_key(key);

        // Get file metadata for content-length
        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| CloudError::Other(format!("failed to stat file: {}", e)))?;
        let file_size = metadata.len();

        debug!(
            "Uploading chunk (zero-copy) to S3: {} from {:?} ({} bytes)",
            full_key, file_path, file_size
        );

        if self.compression_config.enabled() {
            warn!(
                "Zero-copy upload requested but compression is enabled. Consider using upload_chunk() for compression support."
            );
        }

        crate::cloud_helpers::retry_async(
            "upload_chunk_zerocopy",
            MAX_UPLOAD_RETRIES,
            || async {
                // Use ByteStream::from_path for zero-copy streaming
                let body = ByteStream::from_path(file_path)
                    .await
                    .map_err(|e| CloudError::Other(format!("failed to create stream from path: {}", e)))?;

                let mut put_request = self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(&full_key)
                    .body(body)
                    .content_length(file_size as i64);

                // Mark as uncompressed
                put_request = put_request.metadata("compression", "none");

                put_request
                    .send()
                    .await
                    .map_err(|e| {
                        // Extract detailed error information from the AWS SDK error
                        let error_msg = if let Some(service_err) = e.as_service_error() {
                            format!("chunk upload (zero-copy) failed: {} (code: {:?}, message: {}, bucket: {}, key: {})",
                                e,
                                service_err.code(),
                                service_err.message().unwrap_or("no message"),
                                self.bucket,
                                full_key)
                        } else {
                            format!("chunk upload (zero-copy) failed: {} (bucket: {}, key: {}, error type: {:?})",
                                e, self.bucket, full_key, std::any::type_name_of_val(&e))
                        };
                        CloudError::Other(error_msg)
                    })?;
                Ok(())
            },
        )
        .await?;

        Ok(file_size)
    }

    /// Download a chunk from S3 with retry logic
    ///
    /// # Arguments
    /// * `key` - S3 object key (without prefix)
    ///
    /// # Returns
    /// Chunk data bytes on success (decompressed if needed), error after MAX_DOWNLOAD_RETRIES failures
    pub async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        let full_key = self.full_key(key);
        debug!("Downloading chunk from S3: {}", full_key);

        crate::cloud_helpers::retry_async(
            "download_chunk",
            MAX_DOWNLOAD_RETRIES,
            || async {
                let resp = self.client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(&full_key)
                    .send()
                    .await
                    .map_err(|e| {
                        // Extract detailed error information from the AWS SDK error
                        let error_msg = if let Some(service_err) = e.as_service_error() {
                            format!("chunk download failed: {} (code: {:?}, message: {}, bucket: {}, key: {})",
                                e,
                                service_err.code(),
                                service_err.message().unwrap_or("no message"),
                                self.bucket,
                                full_key)
                        } else {
                            format!("chunk download failed: {} (bucket: {}, key: {}, error type: {:?})",
                                e, self.bucket, full_key, std::any::type_name_of_val(&e))
                        };
                        CloudError::Other(error_msg)
                    })?;

                // Check compression metadata (clone the string to avoid borrow issues)
                let compression_type = resp
                    .metadata()
                    .and_then(|m| m.get("compression"))
                    .map(|s| s.to_string());

                let compressed_data = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| CloudError::Other(format!("failed to read body: {}", e)))?
                    .into_bytes()
                    .to_vec();

                debug!("Downloaded {} bytes from S3: {}", compressed_data.len(), full_key);

                // Decompress based on metadata. Manifest is the
                // source of truth for the per-chunk algorithm; this
                // metadata read is a belt-and-suspenders fallback for
                // out-of-band tools / direct downloads.
                let data = match compression_type.as_deref() {
                    Some("zstd") => {
                        debug!("Decompressing chunk (zstd)");
                        decompress_data(CompressionAlgo::Zstd, &compressed_data)?
                    }
                    Some("lz4") => {
                        debug!("Decompressing chunk (lz4)");
                        decompress_data(CompressionAlgo::Lz4, &compressed_data)?
                    }
                    Some("none") | None => {
                        debug!("No decompression needed");
                        compressed_data
                    }
                    Some(other) => {
                        return Err(CloudError::Other(format!(
                            "unsupported compression type: {}",
                            other
                        )));
                    }
                };

                Ok(data)
            },
        )
        .await
    }

    /// Download multiple chunks from S3 in parallel with retry logic
    ///
    /// This method downloads up to MAX_CONCURRENT_DOWNLOADS chunks concurrently,
    /// providing better performance for restore operations.
    ///
    /// # Arguments
    /// * `keys` - S3 object keys (without prefix) to download
    ///
    /// # Returns
    /// Vec of chunk data bytes on success (decompressed if needed), preserving order of input keys
    pub async fn download_chunks_parallel(&self, keys: &[String]) -> Result<Vec<Vec<u8>>> {
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
            // Wait if we've hit the concurrency limit
            if tasks.len() >= MAX_CONCURRENT_DOWNLOADS {
                let Some(finished) = tasks.join_next().await else {
                    return Err(CloudError::Other("Task join failed".to_string()));
                };
                let (result_idx, data) = finished
                    .map_err(|e| CloudError::Other(format!("Download task panicked: {}", e)))??;
                results[result_idx] = Some(data);
            }

            let s3 = self.clone();
            let key_clone = key.clone();

            tasks.spawn(async move {
                let data = s3.download_chunk(&key_clone).await?;
                Ok::<(usize, Vec<u8>), CloudError>((idx, data))
            });
        }

        // Collect remaining results
        while let Some(finished) = tasks.join_next().await {
            let (result_idx, data) = finished
                .map_err(|e| CloudError::Other(format!("Download task panicked: {}", e)))??;
            results[result_idx] = Some(data);
        }

        // Convert Option<Vec<u8>> to Vec<u8>, checking for missing results
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

    /// Upload a manifest JSON to S3
    ///
    /// # Arguments
    /// * `key` - S3 object key (without prefix, e.g., "manifests/TAPE001/manifest-latest.json")
    /// * `json` - Manifest JSON string
    pub async fn upload_manifest(&self, key: &str, json: &str) -> Result<()> {
        let full_key = self.full_key(key);
        debug!(
            "Uploading manifest to S3: {} ({} bytes)",
            full_key,
            json.len()
        );

        crate::cloud_helpers::retry_async("upload_manifest", MAX_UPLOAD_RETRIES, || async {
            let body = ByteStream::from(json.as_bytes().to_vec());
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&full_key)
                .content_type("application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    let detail = describe_sdk_error("manifest upload", &e);
                    CloudError::Other(format!(
                        "{detail} (bucket: {}, key: {})",
                        self.bucket, full_key
                    ))
                })?;
            Ok(())
        })
        .await
    }

    /// Download a manifest JSON from S3
    ///
    /// # Arguments
    /// * `key` - S3 object key (without prefix)
    ///
    /// # Returns
    /// Manifest JSON string on success
    pub async fn download_manifest(&self, key: &str) -> Result<String> {
        let full_key = self.full_key(key);
        debug!("Downloading manifest from S3: {}", full_key);

        crate::cloud_helpers::retry_async(
            "download_manifest",
            MAX_DOWNLOAD_RETRIES,
            || async {
                let resp = self.client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(&full_key)
                    .send()
                    .await
                    .map_err(|e| {
                        // Extract detailed error information from the AWS SDK error
                        let error_msg = if let Some(service_err) = e.as_service_error() {
                            format!("manifest download failed: {} (code: {:?}, message: {}, bucket: {}, key: {})",
                                e,
                                service_err.code(),
                                service_err.message().unwrap_or("no message"),
                                self.bucket,
                                full_key)
                        } else {
                            format!("manifest download failed: {} (bucket: {}, key: {}, error type: {:?})",
                                e, self.bucket, full_key, std::any::type_name_of_val(&e))
                        };
                        CloudError::Other(error_msg)
                    })?;

                let data = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| CloudError::Other(format!("failed to read manifest body: {}", e)))?
                    .into_bytes();

                let json = String::from_utf8(data.to_vec())
                    .map_err(|e| CloudError::Other(format!("manifest not valid UTF-8: {}", e)))?;

                debug!("Downloaded manifest from S3: {} ({} bytes)", full_key, json.len());
                Ok(json)
            },
        )
        .await
    }

    /// Check if a chunk exists in S3
    ///
    /// # Arguments
    /// * `key` - S3 object key (without prefix)
    ///
    /// # Returns
    /// true if object exists, false if not found
    pub async fn chunk_exists(&self, key: &str) -> Result<bool> {
        let full_key = self.full_key(key);
        debug!("Checking if chunk exists in S3: {}", full_key);

        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&full_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                // The AWS SDK returns a typed `NotFound` variant when
                // the object doesn't exist. Check via the service-error
                // metadata: `code()` returns Some("NotFound") for HEAD
                // 404s. Don't rely on `e.to_string()` containing the
                // word — under newer SDK versions the Display impl
                // emits "service error" without the upstream code.
                if let Some(service_err) = e.as_service_error()
                    && service_err.code() == Some("NotFound")
                {
                    return Ok(false);
                }
                let err_str = e.to_string();
                if err_str.contains("404") || err_str.contains("NotFound") {
                    return Ok(false);
                }
                let error_msg = if let Some(service_err) = e.as_service_error() {
                    format!(
                        "head_object failed: {} (code: {:?}, message: {}, bucket: {}, key: {})",
                        e,
                        service_err.code(),
                        service_err.message().unwrap_or("no message"),
                        self.bucket,
                        full_key
                    )
                } else {
                    format!(
                        "head_object failed: {} (bucket: {}, key: {}, error type: {:?})",
                        e,
                        self.bucket,
                        full_key,
                        std::any::type_name_of_val(&e)
                    )
                };
                Err(CloudError::Other(error_msg))
            }
        }
    }

    /// List objects with a given prefix
    ///
    /// # Arguments
    /// * `key_prefix` - Key prefix to list (without bucket prefix, e.g., "manifests/TAPE001/")
    ///
    /// # Returns
    /// Vector of object keys (without bucket prefix)
    pub async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(key_prefix);
        debug!("Listing objects in S3 with prefix: {}", full_prefix);

        let resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&full_prefix)
            .send()
            .await
            .map_err(|e| {
                let detail = describe_sdk_error("list_objects", &e);
                CloudError::Other(format!(
                    "{detail} (bucket: {}, prefix: {})",
                    self.bucket, full_prefix
                ))
            })?;

        let keys: Vec<String> = resp
            .contents()
            .iter()
            .filter_map(|obj| {
                obj.key().map(|k| {
                    // Strip the bucket prefix to return relative keys
                    if !self.prefix.is_empty() && k.starts_with(&self.prefix) {
                        k[self.prefix.len()..].to_string()
                    } else {
                        k.to_string()
                    }
                })
            })
            .collect();

        debug!("Found {} objects with prefix {}", keys.len(), full_prefix);
        Ok(keys)
    }

    /// Delete an object from S3
    ///
    /// # Arguments
    /// * `key` - S3 object key (without prefix)
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        let full_key = self.full_key(key);
        debug!("Deleting object from S3: {}", full_key);

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&full_key)
            .send()
            .await
            .map_err(|e| {
                let detail = describe_sdk_error("delete_object", &e);
                CloudError::Other(format!(
                    "{detail} (bucket: {}, key: {})",
                    self.bucket, full_key
                ))
            })?;

        debug!("Deleted object from S3: {}", full_key);
        Ok(())
    }

    /// Construct full S3 key with prefix.
    fn full_key(&self, key: &str) -> String {
        crate::cloud_helpers::full_key(&self.prefix, key)
    }
}

/// CloudBackend trait implementation for S3Backend
#[async_trait]
impl CloudBackend for S3Backend {
    async fn upload_chunk(
        &self,
        key: &str,
        data: &[u8],
    ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
        S3Backend::upload_chunk(self, key, data).await
    }

    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        S3Backend::upload_chunk_zerocopy(self, key, file_path).await
    }

    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        S3Backend::download_chunk(self, key).await
    }

    async fn download_chunks_parallel(&self, keys: &[String]) -> Result<Vec<Vec<u8>>> {
        S3Backend::download_chunks_parallel(self, keys).await
    }

    async fn upload_manifest(&self, key: &str, json: &str) -> Result<()> {
        S3Backend::upload_manifest(self, key, json).await
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        S3Backend::download_manifest(self, key).await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        S3Backend::chunk_exists(self, key).await
    }

    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        S3Backend::list_objects(self, key_prefix).await
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        S3Backend::delete_object(self, key).await
    }

    fn backend_type(&self) -> &'static str {
        "s3"
    }

    async fn lock_state(&self) -> Result<crate::cloud_backend::LockState> {
        use aws_sdk_s3::types::ObjectLockRetentionMode;
        // GetObjectLockConfiguration on a bucket without Object Lock
        // enabled returns InvalidRequest / ObjectLockConfigurationNotFoundError —
        // which we map to Off rather than a hard error so that
        // retention_mode: none on a regular bucket is the happy path.
        let resp = match self
            .client
            .get_object_lock_configuration()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("{:?}", e);
                if msg.contains("ObjectLockConfigurationNotFoundError")
                    || msg.contains("InvalidRequest")
                {
                    return Ok(crate::cloud_backend::LockState::Off);
                }
                return Err(crate::CloudError::Other(format!(
                    "GetObjectLockConfiguration on {}: {}",
                    self.bucket, msg
                )));
            }
        };
        let lock_cfg = match resp.object_lock_configuration() {
            Some(c) => c,
            None => return Ok(crate::cloud_backend::LockState::Off),
        };
        // ObjectLockEnabled "Enabled" is required for retention to apply;
        // anything else means Off.
        if lock_cfg.object_lock_enabled().map(|e| e.as_str()) != Some("Enabled") {
            return Ok(crate::cloud_backend::LockState::Off);
        }
        let rule = match lock_cfg.rule() {
            Some(r) => r,
            None => return Ok(crate::cloud_backend::LockState::Off),
        };
        let default = match rule.default_retention() {
            Some(d) => d,
            None => return Ok(crate::cloud_backend::LockState::Off),
        };
        // Default retention can be expressed in days OR years; normalize
        // to days for our LockState shape (years -> days * 365 is close
        // enough for validation, the bucket is the contract).
        let days: u32 = if let Some(d) = default.days() {
            d.max(0) as u32
        } else if let Some(y) = default.years() {
            (y.max(0) as u32).saturating_mul(365)
        } else {
            return Ok(crate::cloud_backend::LockState::Off);
        };
        match default.mode() {
            Some(ObjectLockRetentionMode::Governance) => {
                Ok(crate::cloud_backend::LockState::Governance { default_days: days })
            }
            Some(ObjectLockRetentionMode::Compliance) => {
                Ok(crate::cloud_backend::LockState::Compliance { default_days: days })
            }
            _ => Ok(crate::cloud_backend::LockState::Off),
        }
    }

    async fn set_object_legal_hold(&self, key: &str, held: bool) -> Result<()> {
        use aws_sdk_s3::types::{ObjectLockLegalHold, ObjectLockLegalHoldStatus};
        let full_key = self.full_key(key);
        let status = if held {
            ObjectLockLegalHoldStatus::On
        } else {
            ObjectLockLegalHoldStatus::Off
        };
        let lh = ObjectLockLegalHold::builder().status(status).build();
        self.client
            .put_object_legal_hold()
            .bucket(&self.bucket)
            .key(&full_key)
            .legal_hold(lh)
            .send()
            .await
            .map_err(|e| {
                let detail = describe_sdk_error("put_object_legal_hold", &e);
                CloudError::Other(format!(
                    "{detail} (bucket: {}, key: {})",
                    self.bucket, full_key
                ))
            })?;
        Ok(())
    }

    async fn get_object_legal_hold(&self, key: &str) -> Result<bool> {
        use aws_sdk_s3::types::ObjectLockLegalHoldStatus;
        let full_key = self.full_key(key);
        let resp = self
            .client
            .get_object_legal_hold()
            .bucket(&self.bucket)
            .key(&full_key)
            .send()
            .await
            .map_err(|e| {
                let detail = describe_sdk_error("get_object_legal_hold", &e);
                CloudError::Other(format!(
                    "{detail} (bucket: {}, key: {})",
                    self.bucket, full_key
                ))
            })?;
        Ok(matches!(
            resp.legal_hold().and_then(|lh| lh.status()),
            Some(ObjectLockLegalHoldStatus::On)
        ))
    }

    fn clone_box(&self) -> Box<dyn CloudBackend> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_key_with_prefix() {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let backend = S3Backend {
            client: Client::from_conf(config),
            bucket: "test-bucket".to_string(),
            prefix: "tapes/".to_string(),
            compression_config: crate::compression::CompressionConfig::disabled(),
        };

        assert_eq!(
            backend.full_key("chunks/TAPE001/obj-000001.dat"),
            "tapes/chunks/TAPE001/obj-000001.dat"
        );
    }

    #[test]
    fn test_full_key_without_prefix() {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let backend = S3Backend {
            client: Client::from_conf(config),
            bucket: "test-bucket".to_string(),
            prefix: "".to_string(),
            compression_config: crate::compression::CompressionConfig::disabled(),
        };

        assert_eq!(
            backend.full_key("chunks/TAPE001/obj-000001.dat"),
            "chunks/TAPE001/obj-000001.dat"
        );
    }

    #[test]
    fn test_compression_round_trip() {
        // Test that compression and decompression work correctly
        let test_data = vec![0u8; 1024]; // 1 KB of zeros (highly compressible)

        // Compress
        let compressed = zstd::encode_all(&test_data[..], 3).unwrap();
        assert!(
            compressed.len() < test_data.len(),
            "Compressed data should be smaller"
        );

        // Decompress
        let decompressed = zstd::decode_all(&compressed[..]).unwrap();
        assert_eq!(
            decompressed, test_data,
            "Decompressed data should match original"
        );
    }

    // -- SDK-shape error classification tests --------------------------------
    //
    // Stand up an in-process wiremock server, point the AWS S3 SDK at it
    // via endpoint_url, and verify that canned XML error responses with
    // realistic AWS error codes flow through the SDK -> describe_sdk_error
    // -> CloudError::Other -> classify() pipeline to the correct
    // FailureKind. These are the contracts that decide whether a failure
    // burns through the retry budget or fails fast.
    //
    // The existing tests in cloud_helpers.rs cover the retry policy
    // against hand-built CloudError values; these cover the SDK adapter
    // layer with real SDK errors.

    use crate::cloud_config::{FailureKind, classify, is_retryable};
    use crate::compression::CompressionConfig;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mock_s3_backend(server: &MockServer) -> S3Backend {
        S3Backend::new(
            "test-bucket".into(),
            "tapes/".into(),
            "us-east-1".into(),
            Some(server.uri()),
            None,
            Some(ResolvedS3Auth::Static {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "SECRETEXAMPLE".into(),
                session_token: None,
            }),
            CompressionConfig::disabled(),
        )
        .await
        .expect("S3Backend::new must succeed against mock endpoint")
    }

    fn xml_error(code: &str, message: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>{code}</Code><Message>{message}</Message>\
             <RequestId>R</RequestId><HostId>H</HostId></Error>"
        )
    }

    async fn capture_list_error(server: MockServer) -> crate::CloudError {
        let backend = mock_s3_backend(&server).await;
        backend
            .list_objects("")
            .await
            .expect_err("mock returns a 4xx/5xx; list_objects must error")
    }

    #[tokio::test]
    async fn s3_401_invalid_access_key_classifies_as_auth() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(xml_error(
                        "InvalidAccessKeyId",
                        "The AWS Access Key Id you provided does not exist in our records.",
                    )),
            )
            .mount(&server)
            .await;
        let err = capture_list_error(server).await;
        assert_eq!(classify(&err), FailureKind::Auth);
        assert!(!is_retryable(FailureKind::Auth));
    }

    #[tokio::test]
    async fn s3_403_access_denied_classifies_as_authz() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(xml_error("AccessDenied", "Access Denied")),
            )
            .mount(&server)
            .await;
        let err = capture_list_error(server).await;
        assert_eq!(classify(&err), FailureKind::Authz);
        assert!(!is_retryable(FailureKind::Authz));
    }

    #[tokio::test]
    async fn s3_404_no_such_bucket_classifies_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(404)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(xml_error(
                        "NoSuchBucket",
                        "The specified bucket does not exist",
                    )),
            )
            .mount(&server)
            .await;
        let err = capture_list_error(server).await;
        assert_eq!(classify(&err), FailureKind::NotFound);
        assert!(!is_retryable(FailureKind::NotFound));
    }

    #[tokio::test]
    async fn s3_500_internal_error_classifies_as_retryable_other() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(500)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(xml_error(
                        "InternalError",
                        "We encountered an internal error. Please try again.",
                    )),
            )
            .mount(&server)
            .await;
        let err = capture_list_error(server).await;
        // 500 with no auth/authz/not-found/region marker falls through
        // to the catch-all retryable Other class.
        let kind = classify(&err);
        assert!(is_retryable(kind), "500 must be retryable, got {:?}", kind);
    }

    #[tokio::test]
    async fn s3_503_slow_down_classifies_as_retryable() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(xml_error("SlowDown", "Reduce your request rate.")),
            )
            .mount(&server)
            .await;
        let err = capture_list_error(server).await;
        let kind = classify(&err);
        assert!(is_retryable(kind), "503 must be retryable, got {:?}", kind);
    }

    #[tokio::test]
    async fn s3_permanent_redirect_classifies_as_region_mismatch() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(xml_error(
                        "PermanentRedirect",
                        "The bucket you are attempting to access must be addressed using the specified endpoint.",
                    )),
            )
            .mount(&server)
            .await;
        let err = capture_list_error(server).await;
        assert_eq!(classify(&err), FailureKind::RegionMismatch);
        assert!(!is_retryable(FailureKind::RegionMismatch));
    }

    // -- Success-path tests --------------------------------------------------
    //
    // Stand up a wiremock that returns realistic 2xx responses for each
    // S3 verb and verify the happy-path data flow: PUT round-trips,
    // GET decodes the body (with and without compression metadata),
    // HEAD maps to exists, list_objects_v2 parses the XML, and the
    // retention / legal-hold reads parse their respective XML shapes.

    use wiremock::matchers::method;

    #[tokio::test]
    async fn s3_upload_chunk_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let (uncompressed, compressed, algo) = backend
            .upload_chunk("chunks/T1/obj-1.dat", b"payload-bytes")
            .await
            .expect("upload must succeed against 200 mock");
        assert_eq!(uncompressed, 13);
        // Compression is disabled in mock_s3_backend.
        assert_eq!(compressed, None);
        assert_eq!(algo, None);
    }

    #[tokio::test]
    async fn s3_upload_chunk_compresses_when_configured() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let backend = S3Backend::new(
            "test-bucket".into(),
            "tapes/".into(),
            "us-east-1".into(),
            Some(server.uri()),
            None,
            Some(ResolvedS3Auth::Static {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "SECRETEXAMPLE".into(),
                session_token: None,
            }),
            CompressionConfig::new(Some(crate::compression::CompressionAlgo::Zstd), 3),
        )
        .await
        .expect("backend");
        // Highly compressible payload so the compressed size is smaller.
        let data = vec![0u8; 4096];
        let (uncompressed, compressed, algo) = backend
            .upload_chunk("chunks/T1/zeros.dat", &data)
            .await
            .expect("compressed upload");
        assert_eq!(uncompressed, 4096);
        assert!(compressed.expect("compressed size") < 4096);
        assert_eq!(algo, Some(crate::compression::CompressionAlgo::Zstd));
    }

    #[tokio::test]
    async fn s3_upload_manifest_and_versioned_succeed() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        backend
            .upload_manifest("manifests/T1/m.json", "{\"v\":1}")
            .await
            .expect("manifest upload");
        backend
            .upload_versioned("indexes/T1/blocks.idx", b"index-bytes")
            .await
            .expect("versioned upload");
    }

    #[tokio::test]
    async fn s3_zerocopy_upload_streams_file() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("chunk.bin");
        tokio::fs::write(&path, b"zero-copy-bytes").await.unwrap();
        let size = backend
            .upload_chunk_zerocopy("chunks/T1/zc.dat", &path)
            .await
            .expect("zerocopy upload");
        assert_eq!(size, 15);
    }

    #[tokio::test]
    async fn s3_download_chunk_uncompressed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-amz-meta-compression", "none")
                    .set_body_bytes(b"chunk-content".to_vec()),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let got = backend
            .download_chunk("chunks/T1/obj-1.dat")
            .await
            .expect("download");
        assert_eq!(got, b"chunk-content");
    }

    #[tokio::test]
    async fn s3_download_chunk_decompresses_zstd_metadata() {
        let server = MockServer::start().await;
        let original = vec![7u8; 2048];
        let compressed = crate::compression::compress_data(
            crate::compression::CompressionAlgo::Zstd,
            &original,
            3,
        )
        .expect("compress");
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-amz-meta-compression", "zstd")
                    .set_body_bytes(compressed),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let got = backend
            .download_chunk("chunks/T1/z.dat")
            .await
            .expect("download+decompress");
        assert_eq!(got, original);
    }

    #[tokio::test]
    async fn s3_download_chunk_rejects_unknown_compression() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-amz-meta-compression", "brotli")
                    .set_body_bytes(b"x".to_vec()),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let err = backend
            .download_chunk("chunks/T1/bad.dat")
            .await
            .expect_err("unknown compression must error");
        assert!(format!("{err}").contains("unsupported compression"));
    }

    #[tokio::test]
    async fn s3_download_manifest_returns_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"version\":2}"))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let json = backend
            .download_manifest("manifests/T1/m.json")
            .await
            .expect("manifest download");
        assert_eq!(json, "{\"version\":2}");
    }

    #[tokio::test]
    async fn s3_download_chunks_parallel_preserves_order() {
        let server = MockServer::start().await;
        // Every GET returns the same body; the order assertion is on
        // the result vector lining up with the input key vector.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-amz-meta-compression", "none")
                    .set_body_bytes(b"data".to_vec()),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let keys: Vec<String> = (0..3).map(|i| format!("chunks/T1/obj-{i}.dat")).collect();
        let got = backend
            .download_chunks_parallel(&keys)
            .await
            .expect("parallel download");
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|c| c == b"data"));

        // Empty input is a fast path returning an empty vec.
        let empty = backend
            .download_chunks_parallel(&[])
            .await
            .expect("empty parallel download");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn s3_chunk_exists_true_on_head_200() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        assert!(
            backend
                .chunk_exists("chunks/T1/obj-1.dat")
                .await
                .expect("head")
        );
    }

    #[tokio::test]
    async fn s3_chunk_exists_false_on_head_404() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        assert!(
            !backend
                .chunk_exists("chunks/T1/missing.dat")
                .await
                .expect("head 404 maps to false")
        );
    }

    #[tokio::test]
    async fn s3_list_objects_parses_xml_and_strips_prefix() {
        let server = MockServer::start().await;
        // ListBucketResult with two keys under the "tapes/" prefix that
        // mock_s3_backend configures; list_objects must strip it.
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
            <Name>test-bucket</Name><Prefix>tapes/</Prefix><KeyCount>2</KeyCount>\
            <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>\
            <Contents><Key>tapes/chunks/a.dat</Key><Size>1</Size>\
            <LastModified>2026-01-01T00:00:00.000Z</LastModified>\
            <ETag>\"e\"</ETag><StorageClass>STANDARD</StorageClass></Contents>\
            <Contents><Key>tapes/chunks/b.dat</Key><Size>2</Size>\
            <LastModified>2026-01-01T00:00:00.000Z</LastModified>\
            <ETag>\"e\"</ETag><StorageClass>STANDARD</StorageClass></Contents>\
            </ListBucketResult>";
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        let mut keys = backend.list_objects("chunks/").await.expect("list");
        keys.sort();
        assert_eq!(
            keys,
            vec!["chunks/a.dat".to_string(), "chunks/b.dat".to_string()]
        );
    }

    #[tokio::test]
    async fn s3_delete_object_succeeds_on_204() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        backend
            .delete_object("chunks/T1/obj-1.dat")
            .await
            .expect("delete");
    }

    #[tokio::test]
    async fn s3_lock_state_off_when_object_lock_not_configured() {
        let server = MockServer::start().await;
        // S3 returns a 404 ObjectLockConfigurationNotFoundError on a
        // bucket without Object Lock; the backend maps that to Off.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(404)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(xml_error(
                        "ObjectLockConfigurationNotFoundError",
                        "Object Lock configuration does not exist for this bucket",
                    )),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        assert_eq!(
            backend.lock_state().await.expect("lock state"),
            crate::cloud_backend::LockState::Off
        );
    }

    #[tokio::test]
    async fn s3_lock_state_parses_compliance_rule() {
        let server = MockServer::start().await;
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <ObjectLockConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
            <ObjectLockEnabled>Enabled</ObjectLockEnabled>\
            <Rule><DefaultRetention><Mode>COMPLIANCE</Mode><Days>30</Days>\
            </DefaultRetention></Rule></ObjectLockConfiguration>";
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        assert_eq!(
            backend.lock_state().await.expect("lock state"),
            crate::cloud_backend::LockState::Compliance { default_days: 30 }
        );
    }

    #[tokio::test]
    async fn s3_lock_state_parses_governance_rule() {
        let server = MockServer::start().await;
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <ObjectLockConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
            <ObjectLockEnabled>Enabled</ObjectLockEnabled>\
            <Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Years>1</Years>\
            </DefaultRetention></Rule></ObjectLockConfiguration>";
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        assert_eq!(
            backend.lock_state().await.expect("lock state"),
            crate::cloud_backend::LockState::Governance { default_days: 365 }
        );
    }

    #[tokio::test]
    async fn s3_set_object_legal_hold_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        backend
            .set_object_legal_hold("chunks/T1/obj-1.dat", true)
            .await
            .expect("set legal hold on");
        backend
            .set_object_legal_hold("chunks/T1/obj-1.dat", false)
            .await
            .expect("set legal hold off");
        assert!(backend.supports_legal_hold());
    }

    #[tokio::test]
    async fn s3_get_object_legal_hold_reads_status() {
        let server = MockServer::start().await;
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <LegalHold xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
            <Status>ON</Status></LegalHold>";
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let backend = mock_s3_backend(&server).await;
        assert!(
            backend
                .get_object_legal_hold("chunks/T1/obj-1.dat")
                .await
                .expect("get legal hold")
        );
    }

    #[test]
    fn s3_backend_type_and_clone_box() {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let backend = S3Backend {
            client: Client::from_conf(config),
            bucket: "b".to_string(),
            prefix: String::new(),
            compression_config: CompressionConfig::disabled(),
        };
        assert_eq!(backend.backend_type(), "s3");
        let boxed = backend.clone_box();
        assert_eq!(boxed.backend_type(), "s3");
    }
}
