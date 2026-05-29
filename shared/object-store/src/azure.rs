// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Azure Blob Storage backend for cloud storage tier.
//!
//! Mirrors `s3.rs` / `gcs.rs`. Provides upload/download operations for chunks
//! and manifests with:
//! - Automatic retry with exponential backoff
//! - Credential discovery: SAS URL (`AZURE_STORAGE_SAS_URL`), AAD service
//!   principal (`AZURE_TENANT_ID` + `_CLIENT_ID` + `_CLIENT_SECRET`), or the
//!   AAD fallback chain (managed identity → `az` CLI)
//! - Optional zstd / lz4 compression (per-chunk) round-trip via blob metadata
//!
//! Migrated 2026-05-10 from the legacy `azure_storage` /
//! `azure_storage_blobs` 0.21 line to Microsoft's official
//! `azure_storage_blob` 0.12 + `azure_core` / `azure_identity` 0.35.
//! Storage-account shared-key auth is no longer supported by the new
//! SDK; SAS or AAD remain.

use crate::compression::{CompressionAlgo, CompressionConfig, compress_data, decompress_data};
use crate::object_store_backend::ObjectStoreBackend;
use crate::object_store_config::{FailureKind, ResolvedAzureAuth, http_status_to_failure_kind};
use crate::{ObjectStoreError, Result};
use async_trait::async_trait;
use azure_core::Bytes;
use azure_core::credentials::{AccessToken, Secret, TokenCredential, TokenRequestOptions};
use azure_core::http::Url;
use azure_identity::{ClientSecretCredential, DeveloperToolsCredential, ManagedIdentityCredential};
use azure_storage_blob::models::{
    BlobClientGetPropertiesResultHeaders, BlobClientUploadOptions,
    BlobContainerClientListBlobsOptions,
};
use azure_storage_blob::{BlobClient, BlobContainerClient};
use futures::TryStreamExt;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

/// Maximum number of retry attempts for uploads.
const MAX_UPLOAD_RETRIES: u32 = 5;
/// Maximum number of retry attempts for downloads.
const MAX_DOWNLOAD_RETRIES: u32 = 3;
// Backoff cadence is owned by `object_store_helpers::retry_async`.

/// Classify an Azure error into a retry class off its structured shape —
/// the [`azure_core::error::ErrorKind`] discriminant and the HTTP status —
/// never the rendered message string.
///
/// `Credential` (couldn't get/refresh a token) is auth; `Connection` /
/// `Io` (request never reached the service, or local IO) are network;
/// otherwise the HTTP status (when the error is an `HttpResponse`) decides.
/// This is what fixes the old 401 gap: Azure's body for an authentication
/// failure is `AuthenticationFailed`, which matched none of the legacy
/// substring needles, so a revoked-credential 401 used to misclassify as
/// retryable `Other` and burn the whole backoff budget. A structured 401
/// is now `Auth` → fail fast.
fn classify_azure_signal(http: Option<u16>, is_credential: bool, is_network: bool) -> FailureKind {
    if is_credential {
        return FailureKind::Auth;
    }
    if is_network {
        return FailureKind::Network;
    }
    match http {
        Some(status) => http_status_to_failure_kind(status),
        None => FailureKind::Other,
    }
}

/// Classify an [`azure_core::Error`] via [`classify_azure_signal`].
fn classify_azure_error(e: &azure_core::Error) -> FailureKind {
    use azure_core::error::ErrorKind;
    let (is_credential, is_network) = match e.kind() {
        ErrorKind::Credential => (true, false),
        ErrorKind::Connection | ErrorKind::Io => (false, true),
        _ => (false, false),
    };
    classify_azure_signal(e.http_status().map(u16::from), is_credential, is_network)
}

/// Azure Blob Storage backend for storing chunks and manifests.
///
/// `BlobContainerClient` is not `Clone`, but it's cheap to share via
/// `Arc` (its internals are an `Url` + a refcounted `Pipeline`), so
/// we hold one and `clone()` is a single Arc bump.
#[derive(Clone)]
pub struct AzureBackend {
    container: Arc<BlobContainerClient>,
    account: String,
    container_name: String,
    prefix: String,
    /// Optional custom data-plane endpoint (Azurite, sovereign cloud).
    /// Stored alongside the SDK client because the management-plane
    /// REST call (`lock_state`) has to honor the same endpoint
    /// override when targeting non-public clouds. (The SDK already
    /// honors it for data-plane ops.)
    endpoint_url: Option<String>,
    compression_config: CompressionConfig,
    /// Azure subscription ID hosting the storage account. Required to
    /// query the container's immutability policy on the ARM
    /// management plane (see `lock_state()`). `None` is acceptable
    /// for non-WORM Azure backends.
    subscription_id: Option<String>,
    /// Resource group hosting the storage account. Same as
    /// `subscription_id` — required for management-plane queries,
    /// optional otherwise.
    resource_group: Option<String>,
    /// AAD credential used for bearer-token operations:
    ///   - management plane (`https://management.azure.com/.default`),
    ///     e.g. immutability-policy lookup in `lock_state()`
    ///   - any per-blob op the SDK pipeline mints a bearer token for
    ///     when this credential is also the data-plane credential
    ///
    /// `None` when the data-plane auth was via SAS — that flow can't
    /// mint AAD tokens, so management-plane introspection fails with
    /// a clear error pointing the operator at AAD.
    aad_credential: Option<Arc<dyn TokenCredential>>,
}

impl std::fmt::Debug for AzureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureBackend")
            .field("account", &self.account)
            .field("container", &self.container_name)
            .field("prefix", &self.prefix)
            .field("endpoint_url", &self.endpoint_url)
            .field("subscription_id", &self.subscription_id)
            .field("resource_group", &self.resource_group)
            .field("compression_config", &self.compression_config)
            .field("aad_credential_present", &self.aad_credential.is_some())
            .finish()
    }
}

impl AzureBackend {
    /// Build an Azure backend.
    ///
    /// When `auth = Some(...)`, the daemon uses **only** what's
    /// specified — the env-var auto-detect chain below is bypassed.
    /// When `auth = None`, credentials are discovered via env vars:
    ///
    /// 1. SAS URL — `AZURE_STORAGE_SAS_URL`. Takes precedence.
    /// 2. AAD service principal — `AZURE_TENANT_ID` +
    ///    `AZURE_CLIENT_ID` + `AZURE_CLIENT_SECRET`.
    /// 3. AAD fallback chain — `ManagedIdentityCredential` (Azure
    ///    VM / AKS via IMDS) → `DeveloperToolsCredential` (`az
    ///    login`).
    ///
    /// `endpoint_url` is optional and used for Azurite or sovereign
    /// clouds. If omitted, `https://<account>.blob.core.windows.net/`
    /// is used.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        account: String,
        container_name: String,
        prefix: String,
        endpoint_url: Option<String>,
        subscription_id: Option<String>,
        resource_group: Option<String>,
        auth: Option<ResolvedAzureAuth>,
        compression_config: CompressionConfig,
    ) -> Result<Self> {
        debug!(
            "Initializing Azure backend: account={}, container={}, prefix={}",
            account, container_name, prefix
        );

        // Resolve auth. SAS path returns (endpoint_for_sdk, None);
        // AAD paths return (None for endpoint override, Some(creds)).
        let (sas_url_override, aad_credential) = match &auth {
            Some(ResolvedAzureAuth::SasUrl(sas)) => {
                info!("Azure backend '{}': auth = SAS URL (per-backend)", account);
                (Some(sas.clone()), None)
            }
            Some(ResolvedAzureAuth::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            }) => {
                info!(
                    "Azure backend '{}': auth = AAD service principal (per-backend)",
                    account
                );
                let cred = ClientSecretCredential::new(
                    tenant_id,
                    client_id.clone(),
                    Secret::from(client_secret.clone()),
                    None,
                )
                .map_err(|e| {
                    ObjectStoreError::Other(format!(
                        "ClientSecretCredential build failed for backend '{account}': {e}"
                    ))
                })?;
                (None, Some(cred as Arc<dyn TokenCredential>))
            }
            None => discover_credentials_from_env(&account)?,
        };

        // For SAS auth the SAS URL itself is a fully-qualified
        // container URL with the auth token in the query string —
        // pass it to `from_url` directly so we don't double-append
        // the container name. For AAD auth we use
        // `new(account_endpoint, container, Some(cred), None)`,
        // which appends the container path itself.
        let container = if let Some(sas) = &sas_url_override {
            let url = Url::parse(sas)
                .map_err(|e| ObjectStoreError::Other(format!("Azure SAS URL parse failed: {e}")))?;
            BlobContainerClient::from_url(url, None, None).map_err(|e| {
                ObjectStoreError::Other(format!(
                    "BlobContainerClient::from_url (SAS) failed (account: {account}, \
                     container: {container_name}): {e}"
                ))
            })?
        } else {
            let endpoint_for_sdk = endpoint_url
                .as_deref()
                .map(|s| {
                    debug!("Using custom Azure endpoint: {}", s);
                    s.to_string()
                })
                .unwrap_or_else(|| format!("https://{account}.blob.core.windows.net/"));
            BlobContainerClient::new(
                &endpoint_for_sdk,
                &container_name,
                aad_credential.clone(),
                None,
            )
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "BlobContainerClient::new failed (account: {account}, container: \
                     {container_name}): {e}"
                ))
            })?
        };
        let container = Arc::new(container);

        debug!(
            "Compression algorithm: {:?}, level: {}",
            compression_config.algorithm, compression_config.level
        );

        Ok(Self {
            container,
            account,
            container_name,
            prefix,
            endpoint_url,
            compression_config,
            subscription_id,
            resource_group,
            aad_credential,
        })
    }

    /// Construct full blob name with prefix.
    fn full_key(&self, key: &str) -> String {
        crate::object_store_helpers::full_key(&self.prefix, key)
    }

    /// Get a blob client for `full_key`.
    fn blob(&self, full_key: &str) -> BlobClient {
        self.container.blob_client(full_key)
    }
}

/// Resolved credential pair: an optional SAS URL override (used when
/// auth is via SAS, in which case the SAS URL itself is the data-plane
/// endpoint and the bearer credential is `None`) and an optional AAD
/// bearer credential (used when auth is via AAD).
type ResolvedCredential = (Option<String>, Option<Arc<dyn TokenCredential>>);

/// Walk the env-var auto-detect chain when no per-backend `auth` is
/// supplied: SAS URL → AAD service principal → AAD fallback chain
/// (managed identity → `az` CLI). Operators have repeatedly hit "I
/// added my SP but it's still failing" because a stale env var from
/// earlier testing was still set and quietly outranked the intended
/// auth — we surface that explicitly at startup.
fn discover_credentials_from_env(account: &str) -> Result<ResolvedCredential> {
    let has_sas = std::env::var("AZURE_STORAGE_SAS_URL").is_ok();
    let has_aad_env = std::env::var("AZURE_TENANT_ID").is_ok()
        && std::env::var("AZURE_CLIENT_ID").is_ok()
        && std::env::var("AZURE_CLIENT_SECRET").is_ok();

    if has_sas {
        info!(
            "Azure backend '{}': auth = SAS URL (AZURE_STORAGE_SAS_URL)",
            account
        );
        if has_aad_env {
            warn!(
                "Azure backend '{}': AZURE_STORAGE_SAS_URL is set, so SAS auth wins - \
                 AAD service-principal env vars (AZURE_TENANT_ID/CLIENT_ID/CLIENT_SECRET) \
                 are being ignored. SAS auth cannot query the ARM management plane; \
                 lock_state() needs AAD. Unset AZURE_STORAGE_SAS_URL to fall through to \
                 AAD.",
                account
            );
        }
        let sas = std::env::var("AZURE_STORAGE_SAS_URL").unwrap_or_default();
        Ok((Some(sas), None))
    } else if has_aad_env {
        info!(
            "Azure backend '{}': auth = AAD service principal (AZURE_TENANT_ID + \
             AZURE_CLIENT_ID + AZURE_CLIENT_SECRET)",
            account
        );
        let tenant_id = std::env::var("AZURE_TENANT_ID").unwrap_or_default();
        let client_id = std::env::var("AZURE_CLIENT_ID").unwrap_or_default();
        let client_secret = std::env::var("AZURE_CLIENT_SECRET").unwrap_or_default();
        let cred =
            ClientSecretCredential::new(&tenant_id, client_id, Secret::from(client_secret), None)
                .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "ClientSecretCredential build failed from env: {e}"
                ))
            })?;
        Ok((None, Some(cred as Arc<dyn TokenCredential>)))
    } else {
        info!(
            "Azure backend '{}': auth = AAD fallback chain (managed identity -> az CLI)",
            account
        );
        let cred = build_default_aad_chain().map_err(|e| {
            ObjectStoreError::Other(format!(
                "Azure AAD fallback chain build failed: {e} — set \
                 AZURE_STORAGE_SAS_URL, AZURE_TENANT_ID/CLIENT_ID/CLIENT_SECRET, or \
                 ensure the host is on Azure with a managed identity / has run \
                 `az login`."
            ))
        })?;
        Ok((None, Some(cred)))
    }
}

/// Build a chained credential analogous to the old
/// `azure_identity::create_default_credential()` (which the new SDK
/// no longer ships): try `ManagedIdentityCredential` first
/// (production path on Azure VMs / AKS via IMDS), then
/// `DeveloperToolsCredential` (dev path: `az login` →
/// `AzureCliCredential`).
fn build_default_aad_chain() -> azure_core::Result<Arc<dyn TokenCredential>> {
    let managed = ManagedIdentityCredential::new(None)?;
    let developer = DeveloperToolsCredential::new(None)?;
    Ok(Arc::new(ChainedCredential {
        sources: vec![managed, developer],
    }))
}

/// Tries each inner credential in order on every `get_token` call,
/// returning the first success. Errors from non-final sources are
/// logged at debug; only the final source's error propagates.
#[derive(Debug)]
struct ChainedCredential {
    sources: Vec<Arc<dyn TokenCredential>>,
}

#[async_trait]
impl TokenCredential for ChainedCredential {
    async fn get_token(
        &self,
        scopes: &[&str],
        options: Option<TokenRequestOptions<'_>>,
    ) -> azure_core::Result<AccessToken> {
        let last = self.sources.len().saturating_sub(1);
        for (i, src) in self.sources.iter().enumerate() {
            match src.get_token(scopes, options.clone()).await {
                Ok(t) => return Ok(t),
                Err(e) if i < last => {
                    debug!("Azure AAD chain source #{i} failed: {e}; trying next");
                }
                Err(e) => return Err(e),
            }
        }
        // Empty source list — should never happen in practice.
        Err(azure_core::Error::with_message(
            azure_core::error::ErrorKind::Credential,
            "ChainedCredential has no sources",
        ))
    }
}

/// Build the upload-options struct for a chunk upload. Carries the
/// compression-mode metadata so a downloader can route the bytes
/// through the right decompressor (or skip it).
fn upload_options_with_metadata(
    applied_algo: Option<CompressionAlgo>,
    level: i32,
) -> BlobClientUploadOptions<'static> {
    let mut metadata: HashMap<String, String> = HashMap::new();
    match applied_algo {
        Some(algo) => {
            metadata.insert("compression".to_string(), algo.as_str().to_string());
            if matches!(algo, CompressionAlgo::Zstd) {
                metadata.insert("compression_level".to_string(), level.to_string());
            }
        }
        None => {
            metadata.insert("compression".to_string(), "none".to_string());
        }
    }
    BlobClientUploadOptions {
        metadata: Some(metadata),
        ..Default::default()
    }
}

#[async_trait]
impl ObjectStoreBackend for AzureBackend {
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
            "Uploading chunk to Azure: {} ({} bytes)",
            full_key,
            data_to_upload.len()
        );

        let level = self.compression_config.level;
        // Wrap once: per-retry `Bytes::clone` is just an Arc bump.
        let data_bytes = Bytes::from(data_to_upload);
        crate::object_store_helpers::retry_async("upload_chunk", MAX_UPLOAD_RETRIES, || {
            let full_key = full_key.clone();
            let body = data_bytes.clone();
            let blob = self.blob(&full_key);
            let account = self.account.clone();
            let container_name = self.container_name.clone();
            async move {
                let opts = upload_options_with_metadata(applied_algo, level);
                blob.upload(body.into(), Some(opts)).await.map_err(|e| {
                    ObjectStoreError::classified(
                        classify_azure_error(&e),
                        format!(
                            "Azure chunk upload failed: {e} (account: {account}, \
                             container: {container_name}, key: {full_key})"
                        ),
                    )
                })?;
                Ok(())
            }
        })
        .await?;

        Ok((uncompressed_size, compressed_size, applied_algo))
    }

    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        let full_key = self.full_key(key);
        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| ObjectStoreError::Other(format!("failed to stat file: {e}")))?;
        let file_size = metadata.len();

        debug!(
            "Uploading chunk (zero-copy) to Azure: {} from {:?} ({} bytes)",
            full_key, file_path, file_size
        );

        if self.compression_config.enabled() {
            warn!(
                "Zero-copy upload requested but compression is enabled. Consider using upload_chunk() for compression support."
            );
        }

        crate::object_store_helpers::retry_async(
            "upload_chunk_zerocopy",
            MAX_UPLOAD_RETRIES,
            || {
                let full_key = full_key.clone();
                let blob = self.blob(&full_key);
                let path = file_path.to_path_buf();
                let account = self.account.clone();
                let container_name = self.container_name.clone();
                async move {
                    let data = tokio::fs::read(&path).await.map_err(|e| {
                        ObjectStoreError::Other(format!("failed to read file: {e}"))
                    })?;
                    let body = Bytes::from(data);
                    let opts = upload_options_with_metadata(None, 0);
                    blob.upload(body.into(), Some(opts)).await.map_err(|e| {
                        ObjectStoreError::classified(
                            classify_azure_error(&e),
                            format!(
                                "Azure chunk upload (zero-copy) failed: {e} (account: \
                                 {account}, container: {container_name}, key: {full_key}, \
                                 file: {path:?})"
                            ),
                        )
                    })?;
                    Ok(())
                }
            },
        )
        .await?;

        Ok(file_size)
    }

    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        let full_key = self.full_key(key);
        debug!("Downloading chunk from Azure: {}", full_key);

        crate::object_store_helpers::retry_async("download_chunk", MAX_DOWNLOAD_RETRIES, || {
            let full_key = full_key.clone();
            let blob = self.blob(&full_key);
            let account = self.account.clone();
            let container_name = self.container_name.clone();
            async move {
                // get_properties first so we know the on-blob
                // compression mode without parsing it out of the
                // download stream's headers (the new SDK's
                // BlobClientDownloadResult collects bytes directly,
                // and exposing per-response metadata alongside the
                // body would be a duplicated-trait dance). Two
                // round-trips, both small — the bulk of the time is
                // the body transfer.
                let props = blob.get_properties(None).await.map_err(|e| {
                    ObjectStoreError::classified(
                        classify_azure_error(&e),
                        format!(
                            "Azure chunk get_properties failed: {e} (account: {account}, \
                             container: {container_name}, key: {full_key})"
                        ),
                    )
                })?;
                let metadata = props.metadata().map_err(|e| {
                    ObjectStoreError::Other(format!(
                        "Azure chunk metadata header parse: {e} (account: {account}, \
                         container: {container_name}, key: {full_key})"
                    ))
                })?;
                let compression_type = metadata.get("compression").cloned();

                let response = blob.download(None).await.map_err(|e| {
                    ObjectStoreError::classified(
                        classify_azure_error(&e),
                        format!(
                            "Azure chunk download failed: {e} (account: {account}, \
                             container: {container_name}, key: {full_key})"
                        ),
                    )
                })?;
                let buffer: Vec<u8> = response
                    .body
                    .collect()
                    .await
                    .map_err(|e| ObjectStoreError::Other(format!("failed to read body: {e}")))?
                    .to_vec();

                debug!("Downloaded {} bytes from Azure: {}", buffer.len(), full_key);

                let data = match compression_type.as_deref() {
                    Some("zstd") => {
                        debug!("Decompressing chunk (zstd)");
                        decompress_data(CompressionAlgo::Zstd, &buffer)?
                    }
                    Some("lz4") => {
                        debug!("Decompressing chunk (lz4)");
                        decompress_data(CompressionAlgo::Lz4, &buffer)?
                    }
                    Some("none") | None => buffer,
                    Some(other) => {
                        return Err(ObjectStoreError::Other(format!(
                            "unsupported compression type: {other}"
                        )));
                    }
                };
                Ok(data)
            }
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
                    ObjectStoreError::Other(format!("Download task panicked: {e}"))
                })??;
                results[result_idx] = Some(data);
            }

            let azure = self.clone();
            let key_clone = key.clone();

            tasks.spawn(async move {
                let data = azure.download_chunk(&key_clone).await?;
                Ok::<(usize, Vec<u8>), ObjectStoreError>((idx, data))
            });
        }

        while let Some(finished) = tasks.join_next().await {
            let (result_idx, data) = finished
                .map_err(|e| ObjectStoreError::Other(format!("Download task panicked: {e}")))??;
            results[result_idx] = Some(data);
        }

        results
            .into_iter()
            .enumerate()
            .map(|(idx, opt)| {
                opt.ok_or_else(|| {
                    ObjectStoreError::Other(format!(
                        "Missing download result for chunk at index {idx}"
                    ))
                })
            })
            .collect()
    }

    async fn upload_manifest(&self, key: &str, json: &str) -> Result<()> {
        let full_key = self.full_key(key);
        debug!(
            "Uploading manifest to Azure: {} ({} bytes)",
            full_key,
            json.len()
        );

        let body_len = json.len();
        crate::object_store_helpers::retry_async("upload_manifest", MAX_UPLOAD_RETRIES, || {
            let full_key = full_key.clone();
            let body = Bytes::copy_from_slice(json.as_bytes());
            let blob = self.blob(&full_key);
            let account = self.account.clone();
            let container_name = self.container_name.clone();
            async move {
                let opts = BlobClientUploadOptions {
                    blob_content_type: Some("application/json".to_string()),
                    ..Default::default()
                };
                blob.upload(body.into(), Some(opts)).await.map_err(|e| {
                    ObjectStoreError::classified(
                        classify_azure_error(&e),
                        format!(
                            "Azure manifest upload failed: {e} (account: {account}, \
                             container: {container_name}, key: {full_key}, size: \
                             {body_len} bytes)"
                        ),
                    )
                })?;
                Ok(())
            }
        })
        .await
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        let full_key = self.full_key(key);
        debug!("Downloading manifest from Azure: {}", full_key);

        crate::object_store_helpers::retry_async("download_manifest", MAX_DOWNLOAD_RETRIES, || {
            let full_key = full_key.clone();
            let blob = self.blob(&full_key);
            let account = self.account.clone();
            let container_name = self.container_name.clone();
            async move {
                let response = blob.download(None).await.map_err(|e| {
                    ObjectStoreError::classified(
                        classify_azure_error(&e),
                        format!(
                            "Azure manifest download failed: {e} (account: {account}, \
                             container: {container_name}, key: {full_key})"
                        ),
                    )
                })?;
                let bytes: Vec<u8> = response
                    .body
                    .collect()
                    .await
                    .map_err(|e| ObjectStoreError::Other(format!("failed to read body: {e}")))?
                    .to_vec();
                let json = String::from_utf8(bytes).map_err(|e| {
                    ObjectStoreError::Other(format!("manifest not valid UTF-8: {e}"))
                })?;
                debug!(
                    "Downloaded manifest from Azure: {} ({} bytes)",
                    full_key,
                    json.len()
                );
                Ok(json)
            }
        })
        .await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        let full_key = self.full_key(key);
        debug!("Checking if chunk exists in Azure: {}", full_key);

        self.blob(&full_key).exists().await.map_err(|e| {
            ObjectStoreError::classified(
                classify_azure_error(&e),
                format!(
                    "Azure exists check failed: {e} (account: {}, container: {}, key: \
                     {full_key})",
                    self.account, self.container_name
                ),
            )
        })
    }

    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(key_prefix);
        debug!("Listing blobs in Azure with prefix: {}", full_prefix);

        let opts = BlobContainerClientListBlobsOptions {
            prefix: Some(full_prefix.clone()),
            ..Default::default()
        };
        let mut pager = self.container.list_blobs(Some(opts)).map_err(|e| {
            ObjectStoreError::classified(
                classify_azure_error(&e),
                format!(
                    "Azure list_blobs build failed: {e} (account: {}, container: {}, \
                     prefix: {full_prefix})",
                    self.account, self.container_name
                ),
            )
        })?;

        // The Pager auto-flattens pages into `BlobItem`s — its
        // `Stream::Item` is `Result<BlobItem>`, not the page wrapper.
        // (`Pager::into_pages()` would give us per-page access if we
        // ever need next-marker / continuation handling.)
        let mut keys = Vec::new();
        while let Some(item) = pager.try_next().await.map_err(|e| {
            ObjectStoreError::classified(
                classify_azure_error(&e),
                format!(
                    "Azure list_blobs failed: {e} (account: {}, container: {}, prefix: \
                     {full_prefix})",
                    self.account, self.container_name
                ),
            )
        })? {
            let Some(name) = item.name else {
                continue;
            };
            let stripped = if !self.prefix.is_empty() && name.starts_with(&self.prefix) {
                name[self.prefix.len()..].to_string()
            } else {
                name
            };
            keys.push(stripped);
        }

        debug!("Found {} blobs with prefix {}", keys.len(), full_prefix);
        Ok(keys)
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        let full_key = self.full_key(key);
        debug!("Deleting blob from Azure: {}", full_key);

        self.blob(&full_key).delete(None).await.map_err(|e| {
            ObjectStoreError::classified(
                classify_azure_error(&e),
                format!(
                    "Azure delete_blob failed: {e} (account: {}, container: {}, key: \
                     {full_key})",
                    self.account, self.container_name
                ),
            )
        })?;

        debug!("Deleted blob from Azure: {}", full_key);
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "azure"
    }

    async fn lock_state(&self) -> Result<crate::object_store_backend::LockState> {
        // Container immutability policies live on the ARM management
        // plane (https://management.azure.com/.../immutabilityPolicies/default),
        // not on the data-plane blob endpoint. We hit the REST API
        // directly with a bearer token because the data-plane crate
        // (`azure_storage_blob`) doesn't expose this surface.
        //
        // Without subscription_id + resource_group we have no way to
        // address the policy resource, so report Off — the startup
        // retention validation already enforces "if retention_mode
        // != none then both fields must be set" via
        // ObjectStoreConfig::validate, so this branch only triggers for
        // non-WORM Azure backends where the answer is correctly Off.
        let (subscription, resource_group) = match (&self.subscription_id, &self.resource_group) {
            (Some(s), Some(r)) => (s, r),
            _ => return Ok(crate::object_store_backend::LockState::Off),
        };
        let credential = match &self.aad_credential {
            Some(c) => c,
            None => {
                // SAS auth: management plane requires AAD, so we
                // can't introspect. Report a clear error so the
                // operator knows to switch auth.
                return Err(ObjectStoreError::Other(
                    "Azure lock_state requires AAD (managed identity / az login / \
                     service principal); SAS auth cannot query the ARM management \
                     plane"
                        .to_string(),
                ));
            }
        };

        let token = credential
            .get_token(&["https://management.azure.com/.default"], None)
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!("Azure ARM token acquisition failed: {e}"))
            })?;

        let url = format!(
            "https://management.azure.com/subscriptions/{}/resourceGroups/{}/\
             providers/Microsoft.Storage/storageAccounts/{}/blobServices/default/\
             containers/{}/immutabilityPolicies/default?api-version=2023-01-01",
            subscription, resource_group, self.account, self.container_name
        );

        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(token.token.secret())
            .send()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!("Azure ARM immutabilityPolicies GET: {e}"))
            })?;

        // 404 from this endpoint means "no immutability policy exists"
        // — bucket is mutable, lock state is Off.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(crate::object_store_backend::LockState::Off);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ObjectStoreError::Other(format!(
                "Azure ARM immutabilityPolicies GET returned {status}: {body}"
            )));
        }

        // Response shape (excerpt):
        //   { "properties": {
        //       "immutabilityPeriodSinceCreationInDays": 2555,
        //       "state": "Locked" | "Unlocked",
        //       ...
        //   }}
        #[derive(serde::Deserialize)]
        struct PolicyResponse {
            properties: Option<PolicyProperties>,
        }
        #[derive(serde::Deserialize)]
        struct PolicyProperties {
            #[serde(rename = "immutabilityPeriodSinceCreationInDays")]
            immutability_period_since_creation_in_days: Option<u32>,
            state: Option<String>,
        }
        let body: PolicyResponse = resp
            .json()
            .await
            .map_err(|e| ObjectStoreError::Other(format!("Azure ARM JSON parse: {e}")))?;
        let props = match body.properties {
            Some(p) => p,
            None => return Ok(crate::object_store_backend::LockState::Off),
        };
        let days = props
            .immutability_period_since_creation_in_days
            .unwrap_or(0);
        if days == 0 {
            return Ok(crate::object_store_backend::LockState::Off);
        }
        match props.state.as_deref() {
            Some("Locked") => {
                Ok(crate::object_store_backend::LockState::Compliance { default_days: days })
            }
            Some("Unlocked") => {
                Ok(crate::object_store_backend::LockState::Governance { default_days: days })
            }
            _ => Ok(crate::object_store_backend::LockState::Off),
        }
    }

    async fn set_object_legal_hold(&self, key: &str, held: bool) -> Result<()> {
        let full_key = self.full_key(key);
        // 0.12 ships native `set_legal_hold(bool, opts)` so we no
        // longer need to mint a bearer token and PUT
        // `?comp=legalhold` ourselves. SAS-auth backends still can't
        // satisfy this — Azure requires AAD with the right RBAC role
        // to set/clear legal hold — and the SDK surfaces that as a
        // 403 from the server.
        match self.blob(&full_key).set_legal_hold(held, None).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let detail = format!(
                    "Azure Set Blob Legal Hold for {}/{}: {e}",
                    self.container_name, full_key
                );
                // 412 ≠ 409 ≠ generic. Operators have to
                // differentiate these to know whether to ask
                // AAD/policy ("immutability disallows") or just retry
                // ("concurrent writer raced"):
                //   412 Precondition Failed → CloudPreconditionFailed
                //   409 Conflict            → CloudConflict
                //   anything else           → ObjectStoreError (existing default)
                use azure_core::http::StatusCode;
                Err(match e.http_status() {
                    Some(StatusCode::PreconditionFailed) => {
                        ObjectStoreError::PreconditionFailed(detail)
                    }
                    Some(StatusCode::Conflict) => ObjectStoreError::Conflict(detail),
                    Some(other) => ObjectStoreError::Other(format!("status {other}: {detail}")),
                    None => ObjectStoreError::Other(detail),
                })
            }
        }
    }

    async fn get_object_legal_hold(&self, key: &str) -> Result<bool> {
        let full_key = self.full_key(key);
        let props = self
            .blob(&full_key)
            .get_properties(None)
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "Azure Get Blob Properties (legal hold) failed: {e} (account: {}, \
                 container: {}, key: {full_key})",
                    self.account, self.container_name
                ))
            })?;
        let held = props
            .legal_hold()
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "Azure Get Blob Properties legal_hold header parse: {e}"
                ))
            })?
            .unwrap_or(false);
        Ok(held)
    }

    fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_full_key_with_prefix() {
        let prefix = "tapes/";
        let key = "chunks/aa/abc123.dat";
        let expected = "tapes/chunks/aa/abc123.dat";
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
        let key = "chunks/aa/abc123.dat";
        let full_key = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{}", prefix, key)
        };
        assert_eq!(full_key, key);
    }

    // -- SDK-shape error classification tests --------------------------------
    //
    // Stand up an in-process wiremock server, point the Azure Blob SDK
    // at it via a synthetic SAS URL, and verify canned status responses
    // flow through the SDK -> classify_azure_error (off the structured
    // ErrorKind / HTTP status, not a rendered string) -> the typed
    // ObjectStoreError carrier -> classify() pipeline to the correct
    // FailureKind.
    //
    // Coverage scope:
    //   401 → Unauthorized → Auth   (the gap the structured rewrite closes:
    //         Azure's 401 body is "AuthenticationFailed", which matched none
    //         of the old substring needles, so a revoked credential used to
    //         misclassify as retryable Other and burn the whole budget)
    //   403 → Forbidden    → Authz
    //   404 → NotFound     → NotFound
    //
    // Not covered:
    //   503 — the SDK's internal retry policy adds ~60 s to each test
    //     against a wiremock that returns 5xx; covered cheaply on the
    //     S3 side instead.

    use super::{AzureBackend, classify_azure_signal};
    use crate::ObjectStoreError;
    use crate::compression::CompressionConfig;
    use crate::object_store_backend::ObjectStoreBackend;
    use crate::object_store_config::{FailureKind, ResolvedAzureAuth, classify, is_retryable};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn classify_azure_signal_table() {
        // Credential errors win regardless of (absent) HTTP status.
        assert_eq!(classify_azure_signal(None, true, false), FailureKind::Auth);
        // Connection / IO -> network.
        assert_eq!(
            classify_azure_signal(None, false, true),
            FailureKind::Network
        );
        // HTTP status mapping.
        assert_eq!(
            classify_azure_signal(Some(401), false, false),
            FailureKind::Auth
        );
        assert_eq!(
            classify_azure_signal(Some(403), false, false),
            FailureKind::Authz
        );
        assert_eq!(
            classify_azure_signal(Some(404), false, false),
            FailureKind::NotFound
        );
        assert_eq!(
            classify_azure_signal(Some(412), false, false),
            FailureKind::Authz
        );
        // Conflict / throttle / 5xx / unknown / no-signal: retryable Other.
        for http in [None, Some(409), Some(429), Some(500), Some(503)] {
            assert_eq!(
                classify_azure_signal(http, false, false),
                FailureKind::Other,
                "http={http:?}"
            );
            assert!(is_retryable(classify_azure_signal(http, false, false)));
        }
    }

    async fn mock_azure_backend(server: &MockServer) -> AzureBackend {
        // Synthetic SAS URL that points the SDK at the wiremock server.
        // The query string is fake — the SDK doesn't validate it.
        let sas_url = format!("{}/testcontainer?sv=fake&sig=fake", server.uri());
        AzureBackend::new(
            "testaccount".into(),
            "testcontainer".into(),
            "tapes/".into(),
            None,
            None,
            None,
            Some(ResolvedAzureAuth::SasUrl(sas_url)),
            CompressionConfig::disabled(),
        )
        .await
        .expect("AzureBackend::new must succeed against mock SAS URL")
    }

    fn azure_xml(code: &str, message: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <Error><Code>{code}</Code><Message>{message}</Message></Error>"
        )
    }

    async fn capture_azure_list_error(server: MockServer) -> ObjectStoreError {
        let backend = mock_azure_backend(&server).await;
        backend
            .list_objects("")
            .await
            .expect_err("mock returns 4xx/5xx; list_objects must error")
    }

    #[tokio::test]
    async fn azure_401_classifies_as_auth() {
        // The gap the structured rewrite closes: a 401 with body
        // "AuthenticationFailed" used to fall through to retryable Other
        // (no substring matched). Off the HTTP status it is now Auth ->
        // fail fast, so a revoked credential surfaces in seconds.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(azure_xml(
                        "AuthenticationFailed",
                        "Server failed to authenticate the request.",
                    )),
            )
            .mount(&server)
            .await;
        let err = capture_azure_list_error(server).await;
        assert_eq!(classify(&err), FailureKind::Auth);
        assert!(!is_retryable(classify(&err)));
    }

    #[tokio::test]
    async fn azure_403_classifies_as_authz() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(azure_xml(
                        "AuthorizationPermissionMismatch",
                        "This request is not authorized to perform this operation using this permission.",
                    )),
            )
            .mount(&server)
            .await;
        let err = capture_azure_list_error(server).await;
        assert_eq!(classify(&err), FailureKind::Authz);
    }

    #[tokio::test]
    async fn azure_404_classifies_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(404)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(azure_xml(
                        "ContainerNotFound",
                        "The specified container does not exist.",
                    )),
            )
            .mount(&server)
            .await;
        let err = capture_azure_list_error(server).await;
        assert_eq!(classify(&err), FailureKind::NotFound);
    }

    // -- Success-path tests --------------------------------------------------
    //
    // Drive the happy path: a wiremock that answers each Azure Blob
    // verb with a realistic 2xx response. Upload is a PUT block blob,
    // download is HEAD (get_properties) then GET, exists is HEAD,
    // list is GET with an EnumerationResults XML body, delete is a
    // 202. Routed by HTTP method since the data-plane verbs map
    // 1:1 onto methods here.

    use wiremock::matchers::method;

    #[tokio::test]
    async fn azure_upload_chunk_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        let (uncompressed, compressed, algo) = backend
            .upload_chunk("chunks/T1/obj-1.dat", b"azure-payload")
            .await
            .expect("upload must succeed against 201 mock");
        assert_eq!(uncompressed, 13);
        assert_eq!(compressed, None);
        assert_eq!(algo, None);
    }

    #[tokio::test]
    async fn azure_upload_manifest_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        backend
            .upload_manifest("manifests/T1/m.json", "{\"v\":1}")
            .await
            .expect("manifest upload");
    }

    #[tokio::test]
    async fn azure_zerocopy_upload_streams_file() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("chunk.bin");
        tokio::fs::write(&path, b"zero-copy-azure").await.unwrap();
        let size = backend
            .upload_chunk_zerocopy("chunks/T1/zc.dat", &path)
            .await
            .expect("zerocopy upload");
        assert_eq!(size, 15);
    }

    #[tokio::test]
    async fn azure_delete_object_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        backend
            .delete_object("chunks/T1/obj-1.dat")
            .await
            .expect("delete");
    }

    #[tokio::test]
    async fn azure_chunk_exists_true_on_head_200() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "10")
                    .insert_header("x-ms-blob-type", "BlockBlob")
                    .insert_header("ETag", "\"0x1\"")
                    .insert_header("Last-Modified", "Mon, 01 Jan 2026 00:00:00 GMT"),
            )
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        assert!(
            backend
                .chunk_exists("chunks/T1/obj-1.dat")
                .await
                .expect("exists head")
        );
    }

    #[tokio::test]
    async fn azure_chunk_exists_false_on_head_404() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(404)
                    .insert_header("Content-Type", "application/xml")
                    .insert_header("x-ms-error-code", "BlobNotFound")
                    .set_body_string(azure_xml(
                        "BlobNotFound",
                        "The specified blob does not exist.",
                    )),
            )
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        assert!(
            !backend
                .chunk_exists("chunks/T1/missing.dat")
                .await
                .expect("head 404 maps to false")
        );
    }

    #[tokio::test]
    async fn azure_download_chunk_uncompressed() {
        let server = MockServer::start().await;
        // HEAD (get_properties) returns the compression metadata.
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "13")
                    .insert_header("x-ms-blob-type", "BlockBlob")
                    .insert_header("x-ms-meta-compression", "none")
                    .insert_header("ETag", "\"0x1\"")
                    .insert_header("Last-Modified", "Mon, 01 Jan 2026 00:00:00 GMT"),
            )
            .mount(&server)
            .await;
        // GET returns the body bytes.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "13")
                    .insert_header("x-ms-blob-type", "BlockBlob")
                    .set_body_bytes(b"azure-content".to_vec()),
            )
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        let got = backend
            .download_chunk("chunks/T1/obj-1.dat")
            .await
            .expect("download");
        assert_eq!(got, b"azure-content");
    }

    #[tokio::test]
    async fn azure_download_chunk_decompresses_zstd() {
        let server = MockServer::start().await;
        let original = vec![5u8; 2048];
        let compressed = crate::compression::compress_data(
            crate::compression::CompressionAlgo::Zstd,
            &original,
            3,
        )
        .expect("compress");
        let clen = compressed.len().to_string();
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", clen.as_str())
                    .insert_header("x-ms-blob-type", "BlockBlob")
                    .insert_header("x-ms-meta-compression", "zstd")
                    .insert_header("ETag", "\"0x1\"")
                    .insert_header("Last-Modified", "Mon, 01 Jan 2026 00:00:00 GMT"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", clen.as_str())
                    .insert_header("x-ms-blob-type", "BlockBlob")
                    .set_body_bytes(compressed),
            )
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        let got = backend
            .download_chunk("chunks/T1/z.dat")
            .await
            .expect("download+decompress");
        assert_eq!(got, original);
    }

    #[tokio::test]
    async fn azure_download_manifest_returns_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "13")
                    .insert_header("x-ms-blob-type", "BlockBlob")
                    .set_body_string("{\"version\":3}"),
            )
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        let json = backend
            .download_manifest("manifests/T1/m.json")
            .await
            .expect("manifest download");
        assert_eq!(json, "{\"version\":3}");
    }

    #[tokio::test]
    async fn azure_list_objects_parses_enumeration_xml() {
        let server = MockServer::start().await;
        // EnumerationResults with two blobs under the "tapes/" prefix
        // configured by mock_azure_backend; list_objects strips it.
        let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
            <EnumerationResults ServiceEndpoint=\"http://x\" ContainerName=\"testcontainer\">\
            <Prefix>tapes/</Prefix><MaxResults>5000</MaxResults>\
            <Blobs>\
            <Blob><Name>tapes/chunks/a.dat</Name><Properties>\
            <Content-Length>1</Content-Length><BlobType>BlockBlob</BlobType>\
            </Properties></Blob>\
            <Blob><Name>tapes/chunks/b.dat</Name><Properties>\
            <Content-Length>2</Content-Length><BlobType>BlockBlob</BlobType>\
            </Properties></Blob>\
            </Blobs><NextMarker/></EnumerationResults>";
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/xml")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        let mut keys = backend.list_objects("chunks/").await.expect("list");
        keys.sort();
        assert_eq!(
            keys,
            vec!["chunks/a.dat".to_string(), "chunks/b.dat".to_string()]
        );
    }

    #[tokio::test]
    async fn azure_lock_state_off_without_sub_and_rg() {
        // mock_azure_backend supplies no subscription_id / resource_group,
        // so lock_state short-circuits to Off without any ARM call.
        let server = MockServer::start().await;
        let backend = mock_azure_backend(&server).await;
        assert_eq!(
            backend.lock_state().await.expect("lock state"),
            crate::object_store_backend::LockState::Off
        );
        assert_eq!(backend.backend_type(), "azure");
        assert!(backend.supports_legal_hold());
    }

    #[tokio::test]
    async fn azure_get_object_legal_hold_reads_header() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "10")
                    .insert_header("x-ms-blob-type", "BlockBlob")
                    .insert_header("x-ms-legal-hold", "true")
                    .insert_header("ETag", "\"0x1\"")
                    .insert_header("Last-Modified", "Mon, 01 Jan 2026 00:00:00 GMT"),
            )
            .mount(&server)
            .await;
        let backend = mock_azure_backend(&server).await;
        assert!(
            backend
                .get_object_legal_hold("chunks/T1/obj-1.dat")
                .await
                .expect("get legal hold")
        );
    }

    #[tokio::test]
    async fn azure_clone_box_yields_azure_backend() {
        let server = MockServer::start().await;
        let backend = mock_azure_backend(&server).await;
        let boxed = backend.clone_box();
        assert_eq!(boxed.backend_type(), "azure");
    }
}
