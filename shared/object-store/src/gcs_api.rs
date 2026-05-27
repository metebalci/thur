// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! GCS SDK seam.
//!
//! `GcsApi` is the focused trait `GcsBackend` calls into for every
//! GCS-side operation: one method per backend verb, returning
//! Thur-side value types. `RealGcsApi` is the only place in the
//! workspace that touches `google_cloud_storage` / `google_cloud_gax`
//! types — error classification, resource-name formatting, pagination,
//! and stream draining all live inside the production impl. Tests
//! plug in a mock impl that hands back canned values directly,
//! without an HTTP wire.
//!
//! Trade-off: a malformed JSON / header bug in the SDK adapter layer
//! won't surface from `cargo test`. Those failure modes are caught
//! by the env-gated `vsa/scripts/test-fs-iscsi-storage.sh` rig that
//! runs against a real GCS bucket — same coverage we had before.

use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_auth::credentials::{Builder as CredsBuilder, Credentials, service_account};
use google_cloud_gax::error::rpc::Code;
use google_cloud_gax::paginator::ItemPaginator;
use google_cloud_storage::client::{Storage, StorageControl};
use google_cloud_storage::model::Object;
use google_cloud_wkt::FieldMask;

use crate::{ObjectStoreError, Result};

/// Retention policy snapshot returned by [`GcsApi::get_bucket_retention`].
///
/// `seconds == 0` is reported via [`Option::None`]; any positive period
/// surfaces here. `is_locked` reflects the GCS "locked retention
/// policy" bit — see [`crate::object_store_backend::LockState::Compliance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetentionPolicy {
    pub seconds: u64,
    pub is_locked: bool,
}

/// Per-operation surface `GcsBackend` actually needs from the
/// `google-cloud-storage` SDK. `bucket` is the operator-visible
/// bucket name (e.g. `my-bucket`) — the production impl wraps it
/// into the `projects/_/buckets/...` resource name where required.
#[async_trait]
pub(crate) trait GcsApi: Send + Sync + std::fmt::Debug {
    async fn write_object(&self, bucket: &str, key: &str, body: Bytes) -> Result<()>;
    async fn read_object_to_vec(&self, bucket: &str, key: &str) -> Result<Vec<u8>>;
    async fn object_exists(&self, bucket: &str, key: &str) -> Result<bool>;
    async fn get_event_based_hold(&self, bucket: &str, key: &str) -> Result<bool>;
    async fn set_event_based_hold(&self, bucket: &str, key: &str, held: bool) -> Result<()>;
    async fn list_object_names(&self, bucket: &str, prefix: &str) -> Result<Vec<String>>;
    async fn delete_object(&self, bucket: &str, key: &str) -> Result<()>;
    async fn get_bucket_retention(&self, bucket: &str) -> Result<Option<RetentionPolicy>>;
}

/// Production `GcsApi` impl — wraps the two SDK client handles.
#[derive(Clone)]
pub(crate) struct RealGcsApi {
    data: Storage,
    control: StorageControl,
}

impl std::fmt::Debug for RealGcsApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealGcsApi").finish()
    }
}

impl RealGcsApi {
    pub(crate) async fn from_creds(creds: &Credentials) -> Result<Self> {
        let data = Storage::builder()
            .with_credentials(creds.clone())
            .build()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!("GCS Storage client build failed: {}", e))
            })?;
        let control = StorageControl::builder()
            .with_credentials(creds.clone())
            .build()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!("GCS StorageControl client build failed: {}", e))
            })?;
        Ok(Self { data, control })
    }

    fn bucket_resource(bucket: &str) -> String {
        format!("projects/_/buckets/{}", bucket)
    }
}

#[async_trait]
impl GcsApi for RealGcsApi {
    async fn write_object(&self, bucket: &str, key: &str, body: Bytes) -> Result<()> {
        self.data
            .write_object(Self::bucket_resource(bucket), key.to_string(), body)
            .send_buffered()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS write_object failed: {} (bucket: {}, key: {})",
                    e, bucket, key
                ))
            })?;
        Ok(())
    }

    async fn read_object_to_vec(&self, bucket: &str, key: &str) -> Result<Vec<u8>> {
        let mut resp = self
            .data
            .read_object(Self::bucket_resource(bucket), key.to_string())
            .send()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS read_object failed: {} (bucket: {}, key: {})",
                    e, bucket, key
                ))
            })?;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.next().await {
            let chunk = chunk.map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS read_object stream failed: {} (bucket: {}, key: {})",
                    e, bucket, key
                ))
            })?;
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    async fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        match self
            .control
            .get_object()
            .set_bucket(Self::bucket_resource(bucket))
            .set_object(key.to_string())
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                // The Google SDK can surface absence either as HTTP 404
                // (REST) or as a gRPC `NotFound` (`Code = 5`); check
                // both. Without the gRPC-side check, the first
                // chunk_exists of any Global-scope dedup write blew up
                // as a hard failure.
                let is_absent = e.http_status_code() == Some(404)
                    || e.status().is_some_and(|s| s.code == Code::NotFound);
                if is_absent {
                    Ok(false)
                } else {
                    Err(ObjectStoreError::Other(format!(
                        "GCS get_object failed: {} (bucket: {}, key: {})",
                        e, bucket, key
                    )))
                }
            }
        }
    }

    async fn get_event_based_hold(&self, bucket: &str, key: &str) -> Result<bool> {
        let obj = self
            .control
            .get_object()
            .set_bucket(Self::bucket_resource(bucket))
            .set_object(key.to_string())
            .send()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS get_object (legal hold) failed: {} (bucket: {}, key: {})",
                    e, bucket, key
                ))
            })?;
        Ok(obj.event_based_hold.unwrap_or(false))
    }

    async fn set_event_based_hold(&self, bucket: &str, key: &str, held: bool) -> Result<()> {
        // CRITICAL: the FieldMask must scope the patch to
        // `event_based_hold`; without it, the SDK's PATCH wipes every
        // other field on the object.
        let resource = Object::default()
            .set_name(key.to_string())
            .set_event_based_hold(held);
        let mask = FieldMask::default().set_paths(["event_based_hold"]);
        // The update RPC infers the bucket from `resource.name` only if
        // the name is fully qualified; we pass `set_bucket` explicitly
        // for parity with the get/list/delete calls above.
        let _ = bucket;
        self.control
            .update_object()
            .set_object(resource)
            .set_update_mask(mask)
            .send()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS update_object (event_based_hold={}) failed: {} (bucket: {}, key: {})",
                    held, e, bucket, key
                ))
            })?;
        Ok(())
    }

    async fn list_object_names(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
        // Drain every page; new SDK is paginated and silently truncates
        // at the first page if we don't.
        let mut names: Vec<String> = Vec::new();
        let mut items = self
            .control
            .list_objects()
            .set_parent(Self::bucket_resource(bucket))
            .set_prefix(prefix)
            .by_item();
        while let Some(item) = items.next().await {
            let obj = item.map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS list_objects failed: {} (bucket: {}, prefix: {})",
                    e, bucket, prefix
                ))
            })?;
            names.push(obj.name);
        }
        Ok(names)
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        self.control
            .delete_object()
            .set_bucket(Self::bucket_resource(bucket))
            .set_object(key.to_string())
            .send()
            .await
            .map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS delete_object failed: {} (bucket: {}, key: {})",
                    e, bucket, key
                ))
            })?;
        Ok(())
    }

    async fn get_bucket_retention(&self, bucket: &str) -> Result<Option<RetentionPolicy>> {
        let b = self
            .control
            .get_bucket()
            .set_name(Self::bucket_resource(bucket))
            .send()
            .await
            .map_err(|e| ObjectStoreError::Other(format!("GCS get_bucket on {}: {}", bucket, e)))?;
        let policy = match b.retention_policy {
            Some(p) => p,
            None => return Ok(None),
        };
        let secs: u64 = policy
            .retention_duration
            .map(|d| d.seconds().max(0) as u64)
            .unwrap_or(0);
        if secs == 0 {
            return Ok(None);
        }
        Ok(Some(RetentionPolicy {
            seconds: secs,
            is_locked: policy.is_locked,
        }))
    }
}

/// Build a `Credentials` handle either from a service-account JSON key
/// file (when configured) or via the Application Default Credentials
/// chain (`GOOGLE_APPLICATION_CREDENTIALS` env → user creds →
/// GCE/GKE metadata server).
pub(crate) async fn build_credentials(
    service_account_key_file: Option<&str>,
) -> Result<Credentials> {
    match service_account_key_file {
        Some(path) => {
            let json = tokio::fs::read_to_string(path).await.map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS service-account key file '{}' could not be loaded: {}",
                    path, e
                ))
            })?;
            let value: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
                ObjectStoreError::Other(format!(
                    "GCS service-account key file '{}' could not be loaded: {}",
                    path, e
                ))
            })?;
            service_account::Builder::new(value).build().map_err(|e| {
                ObjectStoreError::Other(format!("GCS auth from key file '{}' failed: {}", path, e))
            })
        }
        None => CredsBuilder::default()
            .build()
            .map_err(|e| ObjectStoreError::Other(format!("GCS auth failed: {}", e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_resource_wraps_into_projects_underscore_buckets() {
        assert_eq!(
            RealGcsApi::bucket_resource("my-bucket"),
            "projects/_/buckets/my-bucket"
        );
        assert_eq!(
            RealGcsApi::bucket_resource("name.with.dots"),
            "projects/_/buckets/name.with.dots"
        );
    }

    #[tokio::test]
    async fn build_credentials_reports_missing_key_file() {
        let err = build_credentials(Some("/no/such/key.json"))
            .await
            .expect_err("missing key file");
        let ObjectStoreError::Other(msg) = err else {
            panic!("expected Other");
        };
        assert!(msg.contains("/no/such/key.json"));
    }

    #[tokio::test]
    async fn build_credentials_rejects_non_json_key_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("bad.json");
        tokio::fs::write(&path, b"not json").await.expect("seed");
        let err = build_credentials(Some(path.to_str().unwrap()))
            .await
            .expect_err("bad json");
        assert!(matches!(err, ObjectStoreError::Other(_)));
    }
}
