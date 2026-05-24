// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Stateless per-chunk uploader. No cartridge / volume borrow held
//! during the await; safe to run in parallel worker tasks.

use shared_cloud::{CloudBackend, CloudError, DedupScope};
use thiserror::Error;

use crate::payload::{PendingUpload, UploadOutcome};

/// Error type for [`upload_chunk_inert`]. Aggregates the cloud-trait
/// error and the local-IO error so callers can `?`-propagate without
/// dragging both upstream types into their signatures.
#[derive(Error, Debug)]
pub enum UploadInertError {
    #[error("cloud: {0}")]
    Cloud(#[from] CloudError),

    #[error("io reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Stateless companion to per-product `pending_upload_payload`
/// helpers: performs the cloud-side dedup probe (under
/// [`DedupScope::Global`]) and PUT against an externally-supplied
/// backend, without holding any per-product borrow. Returns the
/// outcome so the owning cartridge / volume can apply manifest /
/// index mutations serially after a parallel upload batch completes
/// — keeping the per-product state machine the sole writer to its
/// own index.
///
/// Lifted from `core_stream::cartridge::mod::upload_chunk_inert`; the
/// `chunk_id` field is now [`PendingUpload::item_id`].
pub async fn upload_chunk_inert(
    backend: &dyn CloudBackend,
    payload: &PendingUpload,
) -> Result<UploadOutcome, UploadInertError> {
    let cloud_key = payload.cloud_key.clone();

    // Cloud-side dedup HEAD probe is only useful under `Global`
    // scope, where a sibling cartridge / volume may have already
    // uploaded the same hash. Under `Local` the cloud key is
    // namespaced by construction, so the HEAD is guaranteed to miss
    // — wasted round-trip per chunk.
    if matches!(payload.dedup, DedupScope::Global) {
        shared_telemetry::record::chunk_cloud_head_probe(&payload.backend_name);
        if backend.chunk_exists(&cloud_key).await? {
            shared_telemetry::record::chunk_cloud_head_hit(&payload.backend_name);
            tracing::debug!(
                "Cloud-side dedup hit for item {} (hash {}..); skipping upload",
                payload.item_id,
                &payload.hash[..8.min(payload.hash.len())]
            );
            return Ok(UploadOutcome {
                item_id: payload.item_id,
                cloud_key,
                dedup_hit: true,
                put_compression: None,
                put_bytes: None,
            });
        }
    }

    // Up to 128 MiB per chunk on the LocalBackend path; sync
    // `std::fs::read` would park a tokio worker for the whole file
    // load. `tokio::fs::read` runs the syscall on the blocking
    // pool.
    let data = tokio::fs::read(&payload.local_path)
        .await
        .map_err(|source| UploadInertError::Io {
            path: payload.local_path.display().to_string(),
            source,
        })?;
    let logical_bytes = data.len() as u64;
    tracing::debug!(
        "Uploading item {} (hash {}..) to cloud: {} ({} bytes)",
        payload.item_id,
        &payload.hash[..8.min(payload.hash.len())],
        cloud_key,
        logical_bytes
    );
    let (_uncompressed_size, compressed_size, applied_algo) =
        backend.upload_chunk(&cloud_key, &data).await?;
    shared_telemetry::record::chunk_uploaded_bytes(&payload.backend_name, logical_bytes);

    Ok(UploadOutcome {
        item_id: payload.item_id,
        cloud_key,
        dedup_hit: false,
        put_compression: applied_algo,
        // On-wire size: the compressed length when a compressor ran,
        // else the uncompressed payload (`logical_bytes` is the bytes
        // handed to `upload_chunk`).
        put_bytes: Some(compressed_size.unwrap_or(logical_bytes)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_cloud::LocalBackend;
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn local_backend() -> (TempDir, Arc<dyn CloudBackend>) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let b = LocalBackend::new(&root).await.unwrap();
        (tmp, Arc::new(b))
    }

    #[tokio::test]
    async fn put_then_head_hit_short_circuits_under_global() {
        let (tmp, backend) = local_backend().await;
        let local = tmp.path().join("payload.dat");
        std::fs::write(&local, b"hello cloud").unwrap();

        let p = PendingUpload {
            item_id: 7,
            hash: "deadbeef".repeat(8),
            local_path: local,
            cloud_key: "chunks/de/ad/deadbeef.dat".into(),
            dedup: DedupScope::Global,
            backend_name: "primary".into(),
        };

        // First PUT — `put_bytes` is the on-wire size (LocalBackend
        // does not compress, so it equals the 11-byte payload).
        let o1 = upload_chunk_inert(backend.as_ref(), &p).await.unwrap();
        assert!(!o1.dedup_hit);
        assert_eq!(o1.put_bytes, Some(11));
        // Second call sees cloud HEAD hit; no PUT, dedup_hit=true,
        // and `put_bytes` is None so the caller doesn't double-count.
        let o2 = upload_chunk_inert(backend.as_ref(), &p).await.unwrap();
        assert!(o2.dedup_hit);
        assert!(o2.put_compression.is_none());
        assert_eq!(o2.put_bytes, None);
    }

    #[tokio::test]
    async fn local_scope_skips_head_probe_and_always_puts() {
        let (tmp, backend) = local_backend().await;
        let local = tmp.path().join("payload.dat");
        std::fs::write(&local, b"hello local").unwrap();

        let p = PendingUpload {
            item_id: 1,
            hash: "cafebabe".repeat(8),
            local_path: local,
            cloud_key: "chunks/vol1/ca/fe/cafebabe.dat".into(),
            dedup: DedupScope::Local,
            backend_name: "primary".into(),
        };

        let o1 = upload_chunk_inert(backend.as_ref(), &p).await.unwrap();
        assert!(!o1.dedup_hit);
        assert_eq!(o1.put_bytes, Some(11));
        // Re-PUT under Local: no HEAD probe, always reports a fresh
        // PUT (the backend's PUT is idempotent so the bytes match).
        let o2 = upload_chunk_inert(backend.as_ref(), &p).await.unwrap();
        assert!(!o2.dedup_hit);
        assert_eq!(o2.put_bytes, Some(11));
    }

    #[tokio::test]
    async fn missing_local_file_surfaces_io_error() {
        let (tmp, backend) = local_backend().await;
        let p = PendingUpload {
            item_id: 1,
            hash: "00".repeat(32),
            local_path: tmp.path().join("does-not-exist.dat"),
            cloud_key: "chunks/00/00/zeros.dat".into(),
            dedup: DedupScope::Local,
            backend_name: "primary".into(),
        };
        let err = upload_chunk_inert(backend.as_ref(), &p).await.unwrap_err();
        assert!(matches!(err, UploadInertError::Io { .. }));
    }

    #[tokio::test]
    async fn head_probe_error_propagates_as_cloud_error_and_no_put_attempted() {
        use crate::test_support::MockBackend;
        use shared_cloud::CloudError;

        let backend = MockBackend::default();
        *backend.head_err.lock().unwrap() = Some(CloudError::Other("HEAD blew up".into()));

        let tmp = TempDir::new().unwrap();
        let local = tmp.path().join("payload.dat");
        std::fs::write(&local, b"x").unwrap();
        let p = PendingUpload {
            item_id: 1,
            hash: "ab".repeat(32),
            local_path: local,
            cloud_key: "chunks/ab/ab.dat".into(),
            dedup: DedupScope::Global,
            backend_name: "primary".into(),
        };
        let err = upload_chunk_inert(&backend, &p).await.unwrap_err();
        assert!(matches!(err, UploadInertError::Cloud(_)));
        assert_eq!(backend.heads(), 1);
        assert_eq!(backend.puts(), 0);
    }

    #[tokio::test]
    async fn put_error_after_head_miss_propagates_as_cloud_error() {
        use crate::test_support::MockBackend;
        use shared_cloud::CloudError;

        let backend = MockBackend::default();
        *backend.put_err.lock().unwrap() = Some(CloudError::Other("PUT 5xx after retries".into()));

        let tmp = TempDir::new().unwrap();
        let local = tmp.path().join("payload.dat");
        std::fs::write(&local, b"hello").unwrap();
        let p = PendingUpload {
            item_id: 2,
            hash: "cd".repeat(32),
            local_path: local,
            cloud_key: "chunks/cd/cd.dat".into(),
            dedup: DedupScope::Global,
            backend_name: "primary".into(),
        };
        let err = upload_chunk_inert(&backend, &p).await.unwrap_err();
        assert!(matches!(err, UploadInertError::Cloud(_)));
        assert_eq!(backend.heads(), 1);
        assert_eq!(backend.puts(), 1);
    }

    #[tokio::test]
    async fn compressed_put_records_compressed_bytes_and_algo() {
        use crate::test_support::MockBackend;
        use shared_cloud::CompressionAlgo;

        let backend = MockBackend::default();
        *backend.put_compressed_as.lock().unwrap() = Some((60, CompressionAlgo::Zstd));

        let tmp = TempDir::new().unwrap();
        let local = tmp.path().join("payload.dat");
        std::fs::write(&local, vec![0u8; 100]).unwrap();
        let p = PendingUpload {
            item_id: 3,
            hash: "ef".repeat(32),
            local_path: local,
            cloud_key: "chunks/ef/ef.dat".into(),
            dedup: DedupScope::Local,
            backend_name: "primary".into(),
        };
        let outcome = upload_chunk_inert(&backend, &p).await.unwrap();
        assert!(!outcome.dedup_hit);
        assert_eq!(outcome.put_bytes, Some(60));
        assert_eq!(outcome.put_compression, Some(CompressionAlgo::Zstd));
        // Local scope skips the HEAD probe.
        assert_eq!(backend.heads(), 0);
        assert_eq!(backend.puts(), 1);
    }
}
