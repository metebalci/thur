// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cloud backend trait for modular storage support
//!
//! This module defines a common interface for cloud storage backends,
//! allowing Thur VTL to support multiple cloud providers (S3, GCS, etc.)

use crate::Result;
use async_trait::async_trait;
use std::fmt::Debug;
use std::path::Path;

/// State of a backend bucket's immutability / retention configuration,
/// observed by querying the cloud provider directly. Used by
/// startup validation to confirm the bucket matches the operator's
/// declared `retention_mode` in `cloud.backends`. Mismatches in either
/// direction are fatal — the bucket is the contract, and a mismatch
/// means every WORM tape written from that point would lie about its
/// retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// No bucket-level immutability/retention is configured.
    Off,
    /// Object Lock enabled with a default retention rule that can be
    /// shortened by privileged users (S3 GOVERNANCE, GCS unlocked
    /// retention policy, Azure unlocked time-based policy).
    Governance { default_days: u32 },
    /// Same as governance but irrevocable — even root cannot shorten
    /// retention before the retain-until date (S3 COMPLIANCE, GCS
    /// locked retention policy, Azure locked time-based policy).
    Compliance { default_days: u32 },
}

impl LockState {
    /// True if the bucket has any kind of immutability turned on.
    pub fn is_locked(self) -> bool {
        !matches!(self, LockState::Off)
    }

    /// Short label for log/error messages.
    pub fn label(self) -> &'static str {
        match self {
            LockState::Off => "off",
            LockState::Governance { .. } => "governance",
            LockState::Compliance { .. } => "compliance",
        }
    }
}

/// Cloud backend trait for chunk and manifest operations
///
/// This trait provides a common interface for all cloud storage backends,
/// enabling support for AWS S3, Google Cloud Storage, Azure Blob Storage, etc.
///
/// All implementations must be thread-safe (Clone + Send + Sync).
#[async_trait]
pub trait CloudBackend: Debug + Send + Sync {
    /// Upload a chunk to cloud storage with retry logic and optional compression
    ///
    /// # Arguments
    /// * `key` - Object key (without prefix, e.g., "chunks/TAPE001/obj-000001.dat")
    /// * `data` - Chunk data bytes
    ///
    /// # Returns
    /// Ok((uncompressed_size, compressed_size_opt, applied_algo)) — original
    /// size, compressed size if a compressor was applied, and which
    /// algorithm was used (None when uploaded uncompressed). The algorithm
    /// is recorded per-chunk in the manifest so reads can decompress
    /// without consulting the daemon's current config.
    async fn upload_chunk(
        &self,
        key: &str,
        data: &[u8],
    ) -> Result<(
        u64,
        Option<u64>,
        Option<crate::compression::CompressionAlgo>,
    )>;

    /// Upload an object whose key may be re-used with new content
    /// (versioned overwrite, not content-addressed). Bypasses any
    /// upload-side caching the wrapper may apply — the meta-cache
    /// assumes once-and-done content addressing, so a cache hit on a
    /// versioned key would silently skip the PUT and the new content
    /// would never reach cloud.
    ///
    /// Use this for index-page snapshots, backup index files, and
    /// any other "same key, new content" write path. Content-
    /// addressed chunk PUTs (`chunks/<hash>.dat`) keep using
    /// `upload_chunk` — same hash = same content = cache is correct.
    ///
    /// Default impl delegates to `upload_chunk` and discards the
    /// size tuple, so concrete backends inherit it for free. Only
    /// the meta-cache wrapper overrides to skip-and-invalidate.
    async fn upload_versioned(&self, key: &str, data: &[u8]) -> Result<()> {
        self.upload_chunk(key, data).await.map(|_| ())
    }

    /// Upload a chunk from a file path using zero-copy streaming
    ///
    /// This method streams the file directly to cloud storage without loading it into memory,
    /// providing better performance for large chunks.
    ///
    /// # Arguments
    /// * `key` - Object key (without prefix, e.g., "chunks/TAPE001/obj-000001.dat")
    /// * `file_path` - Path to the chunk file on disk
    ///
    /// # Returns
    /// Ok(file_size) - Returns the size of the uploaded file
    ///
    /// # Note
    /// This method does NOT support compression. Use upload_chunk() if compression is needed.
    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64>;

    /// Download a chunk from cloud storage with retry logic
    ///
    /// # Arguments
    /// * `key` - Object key (without prefix)
    ///
    /// # Returns
    /// Chunk data bytes on success (decompressed if needed)
    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>>;

    /// Download multiple chunks in parallel with retry logic
    ///
    /// # Arguments
    /// * `keys` - Object keys (without prefix) to download
    ///
    /// # Returns
    /// Vec of chunk data bytes on success (decompressed if needed), preserving order of input keys
    async fn download_chunks_parallel(&self, keys: &[String]) -> Result<Vec<Vec<u8>>>;

    /// Upload a manifest JSON to cloud storage
    ///
    /// # Arguments
    /// * `key` - Object key (without prefix, e.g., "manifests/TAPE001/manifest-latest.json")
    /// * `json` - Manifest JSON string
    async fn upload_manifest(&self, key: &str, json: &str) -> Result<()>;

    /// Download a manifest JSON from cloud storage
    ///
    /// # Arguments
    /// * `key` - Object key (without prefix)
    ///
    /// # Returns
    /// Manifest JSON string on success
    async fn download_manifest(&self, key: &str) -> Result<String>;

    /// Check if a chunk exists in cloud storage
    ///
    /// # Arguments
    /// * `key` - Object key (without prefix)
    ///
    /// # Returns
    /// true if object exists, false if not found
    async fn chunk_exists(&self, key: &str) -> Result<bool>;

    /// List objects with a given prefix
    ///
    /// # Arguments
    /// * `key_prefix` - Key prefix to list (without bucket prefix, e.g., "manifests/TAPE001/")
    ///
    /// # Returns
    /// Vector of object keys (without bucket prefix)
    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>>;

    /// Delete an object from cloud storage
    ///
    /// # Arguments
    /// * `key` - Object key (without prefix)
    async fn delete_object(&self, key: &str) -> Result<()>;

    /// Get the backend type name (e.g., "s3", "gcs", "azure")
    fn backend_type(&self) -> &'static str;

    /// Query the bucket / container for its current immutability
    /// configuration. Each provider exposes a different API:
    ///   - S3:    `GetObjectLockConfiguration`
    ///   - GCS:   bucket `retentionPolicy` + `isLocked` flag
    ///   - Azure: container immutability policy `policyMode`
    ///   - Local: always `LockState::Off`
    ///
    /// Used by startup validation to confirm the bucket matches the
    /// operator's declared `retention_mode`. Mismatches are fatal.
    async fn lock_state(&self) -> Result<LockState>;

    /// True if this backend can apply per-object legal hold via a
    /// provider-native primitive. False for `local` (no enforcement
    /// primitive exists). Cartridge legal-hold operations consult this
    /// up front so an operator gets a clear refusal rather than a
    /// per-key cascade of errors.
    fn supports_legal_hold(&self) -> bool {
        true
    }

    /// Set the per-object legal hold flag.
    /// - S3: `PutObjectLegalHold` (Status = ON / OFF) — bucket must be
    ///   Object-Lock-enabled (legal hold does not require a retention
    ///   period).
    /// - GCS: PATCH object with `eventBasedHold = true / false` — works
    ///   on any bucket.
    /// - Azure: `Set Blob Legal Hold` data-plane REST call (`?comp=legalhold`,
    ///   `x-ms-legal-hold: true|false`) — container must be
    ///   immutable-storage capable; AAD auth required (Data Owner role).
    /// - Local: returns `NotSupported`.
    ///
    /// `held = true` engages the hold; `held = false` releases it.
    /// Idempotent — calling set twice with the same value is a no-op
    /// at the provider.
    async fn set_object_legal_hold(&self, key: &str, held: bool) -> Result<()>;

    /// Read the per-object legal hold flag. Returns `Ok(true)` if the
    /// hold is engaged, `Ok(false)` if not. Errors propagate IO /
    /// permission problems rather than masquerading as "false".
    async fn get_object_legal_hold(&self, key: &str) -> Result<bool>;

    /// LIST `prefix` and prime any internal cache the wrapper may
    /// hold. Returns the number of entries newly populated.
    ///
    /// Default impl is a no-op (returns 0): concrete S3/GCS/Azure/
    /// Local backends have no cache to warm. The meta-cache wrapper
    /// overrides this to seed `Probed` entries for every key it
    /// finds, so that subsequent `chunk_exists` / `upload_chunk`
    /// calls hit the cache instead of the network.
    ///
    /// Non-blocking from the daemon's perspective — call from a
    /// `tokio::spawn` at startup or registry-insert time. LIST
    /// failures should not be fatal: the cache simply stays cold and
    /// the next write does a real HEAD / PUT (same as pre-cache
    /// behaviour).
    async fn warmup_prefix(&self, _prefix: &str) -> Result<usize> {
        Ok(0)
    }

    /// Clone the backend (required for Arc<dyn CloudBackend>)
    fn clone_box(&self) -> Box<dyn CloudBackend>;
}

/// Enable cloning for boxed CloudBackend trait objects
impl Clone for Box<dyn CloudBackend> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalBackend;
    use tempfile::TempDir;

    #[test]
    fn lock_state_is_locked_per_variant() {
        assert!(!LockState::Off.is_locked());
        assert!(LockState::Governance { default_days: 7 }.is_locked());
        assert!(LockState::Compliance { default_days: 30 }.is_locked());
    }

    #[test]
    fn lock_state_label_per_variant() {
        assert_eq!(LockState::Off.label(), "off");
        assert_eq!(
            LockState::Governance { default_days: 7 }.label(),
            "governance"
        );
        assert_eq!(
            LockState::Compliance { default_days: 30 }.label(),
            "compliance"
        );
    }

    #[test]
    fn lock_state_is_copy_and_eq() {
        let a = LockState::Governance { default_days: 5 };
        let b = a;
        assert_eq!(a, b);
        assert_ne!(LockState::Off, LockState::Compliance { default_days: 5 });
    }

    #[tokio::test]
    async fn default_upload_versioned_delegates_to_upload_chunk() {
        let dir = TempDir::new().expect("tempdir");
        let backend = LocalBackend::new(dir.path()).await.expect("backend");
        // The default trait impl of upload_versioned forwards to
        // upload_chunk and discards the size tuple.
        backend
            .upload_versioned("versioned/key", b"payload")
            .await
            .expect("versioned upload");
        let got = backend
            .download_chunk("versioned/key")
            .await
            .expect("download");
        assert_eq!(got, b"payload");
    }

    #[tokio::test]
    async fn default_warmup_prefix_is_a_noop() {
        let dir = TempDir::new().expect("tempdir");
        let backend = LocalBackend::new(dir.path()).await.expect("backend");
        // Concrete backends have no cache; the default warmup returns 0.
        let primed = backend.warmup_prefix("chunks/").await.expect("warmup");
        assert_eq!(primed, 0);
    }

    #[tokio::test]
    async fn boxed_backend_clone_yields_independent_handle() {
        let dir = TempDir::new().expect("tempdir");
        let backend = LocalBackend::new(dir.path()).await.expect("backend");
        let boxed: Box<dyn CloudBackend> = Box::new(backend);
        let cloned = boxed.clone();
        assert_eq!(cloned.backend_type(), "local");
        assert_eq!(boxed.backend_type(), "local");
    }
}
