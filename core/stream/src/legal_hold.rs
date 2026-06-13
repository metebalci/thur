// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cartridge-level legal hold orchestration.
//!
//! Source of truth is the storage provider's per-object hold primitive
//! (S3 `PutObjectLegalHold`, GCS `eventBasedHold`, Azure
//! `Set Blob Legal Hold`). Thur VTL keeps **no** local "is held" flag
//! — set/clear walks every chunk key the cartridge's manifest
//! references plus its manifest backups, and status reads provider
//! state back. The trade-off is that a held cartridge's iSCSI surface
//! is unchanged: the host won't see a write-protect signal; it will
//! see opaque upload/delete failures only when the daemon tries to
//! mutate a held storage object. The promise is data preservation, not
//! host-visible write rejection.
//!
//! ## The sentinel: `manifests/<barcode>/manifest-latest.json`
//!
//! The "is this cartridge held?" question is answered by reading the
//! hold flag on a single sentinel key — `manifests/<barcode>/manifest-latest.json`.
//! That key always exists once anything has been uploaded for the
//! cartridge, gets refreshed after every batch of chunks, and is
//! stable across the cartridge's lifetime. Apply order is structured
//! so the sentinel lags reality:
//!
//! - **Set:** apply hold to every chunk + versioned manifest backup
//!   first, then the sentinel last. The sentinel is only "held" once
//!   every other key is held.
//! - **Clear:** release the sentinel last. Until every other key has
//!   been released, the sentinel still reads "held."
//!
//! This makes the sentinel a definitive runtime answer for both the
//! status command and (future) the upload worker that needs to know
//! whether to apply the hold to freshly-PUT chunks.

use crate::errors::{Result, SmcError};
use serde::Deserialize;
use shared_object_store::ObjectStoreBackend;
use std::path::Path;
use std::sync::Arc;

// `find_drive_for_loaded_cartridge` previously lived here but reaches
// into `LibraryInventory` (smc-side state). It moved to core-mediachanger so
// core-stream can compile without core-mediachanger as a dependency. Callers
// import it via `core_mediachanger::find_drive_for_loaded_cartridge`.

/// Subset of `manifest.json` we need to enumerate storage keys without
/// pulling in the whole `Cartridge` machinery (which would drag in a
/// `ObjectStoreBackend` and a runtime). Backend-flat: pool keys are
/// `chunks/<aa>/<bb>/<hash>.dat`; the manifest backups live under
/// `manifests/<label>/manifest-{latest,TIMESTAMP}.json`.
#[derive(Debug, Deserialize)]
struct ManifestSlice {
    label: String,
    /// Sticky dedup scope (see `Cartridge`/`Manifest`). Under `Local`
    /// the chunk + manifest storage keys are namespaced under the
    /// cartridge barcode — so legal-hold key enumeration must match.
    /// Sealed chunk hashes come from `chunks.idx`, not the manifest.
    #[serde(default)]
    dedup: crate::cartridge::DedupScope,
}

/// Result of one provider call (set or get) for a single key.
#[derive(Debug)]
pub struct PerKeyOutcome {
    pub key: String,
    pub result: Result<()>,
}

/// Aggregated outcome of a full `set` / `clear` over every key the
/// cartridge references. The CLI summarizes successes vs. failures and
/// returns a non-zero exit if `failures` is non-empty.
#[derive(Debug, Default)]
pub struct HoldRunReport {
    pub total: usize,
    pub successes: usize,
    pub failures: Vec<PerKeyOutcome>,
}

/// The sentinel key for "is this cartridge held?" — `manifest-latest.json`
/// under the cartridge's manifest prefix. Stable, always present once
/// the cartridge has uploaded anything, and re-PUT after every chunk
/// batch (which is why the sentinel must be applied/released LAST so
/// it lags the rest of the cartridge's keys).
pub fn manifest_latest_sentinel_key(barcode: &str) -> String {
    format!("manifests/{}/manifest-latest.json", barcode)
}

/// Split a cartridge's storage keys into `(others, sentinel)`. Used by
/// set/clear so callers can apply the body before/after the sentinel.
pub struct CartridgeKeys {
    /// Every chunk + every versioned manifest backup the cartridge
    /// references on its bound backend. Order is not significant.
    pub others: Vec<String>,
    /// The `manifest-latest.json` sentinel — always present in the
    /// list returned by `list_objects` for any cartridge that has
    /// uploaded anything.
    pub sentinel: Option<String>,
}

/// Read a cartridge's `manifest.json` from disk and enumerate the
/// storage keys that should be put under (or released from) legal hold.
/// The sentinel (`manifest-latest.json`) is split out from `others`
/// so callers can sequence apply/clear in the right order.
///
/// Takes `Arc<dyn ObjectStoreBackend>` by value so callers don't have to
/// hold a borrow across the `list_objects` await — borrows of
/// `&dyn ObjectStoreBackend` across awaits trip the rustc HRTB-Send check
/// when the future flows through axum's `Handler` trait machinery.
pub async fn collect_cartridge_keys(
    tapes_dir: &Path,
    barcode: String,
    backend: Arc<dyn ObjectStoreBackend>,
) -> Result<CartridgeKeys> {
    use crate::chunk_store::ChunkStore;

    let cart_root = tapes_dir.join(&barcode);
    let manifest_path = cart_root.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(SmcError::Io)?;
    let m: ManifestSlice = serde_json::from_str(&raw).map_err(SmcError::SerdeJson)?;
    if m.label != barcode {
        return Err(SmcError::InvalidOp("manifest label does not match barcode"));
    }

    let cartridge_namespace: Option<&str> = m.dedup.namespace(&m.label);
    // Enumerate sealed chunk hashes from `chunks.idx`, NOT the manifest:
    // per-chunk records moved out of manifest.json into the chunk index,
    // so the manifest carries no chunk hashes. Reading them from the
    // (absent) manifest field left every tape DATA object unprotected by
    // legal hold (issue #119). Mirrors `cartridge_migrate`'s walk.
    let chunk_idx = crate::chunk_index::ChunkIndexFile::open_or_create(&cart_root)?;
    let mut others: Vec<String> = Vec::new();
    for entry in chunk_idx.iter() {
        let (_id, rec) = entry?;
        if let Some(hash) = rec.hash {
            others.push(ChunkStore::object_key_for(cartridge_namespace, &hash));
        }
    }
    drop(chunk_idx);

    // Manifest backups: list the cartridge's manifests/<label>/ prefix
    // on the bound backend. This covers `manifest-latest.json` plus
    // the versioned backups; we split the sentinel out so callers can
    // sequence it last in apply ordering.
    let manifest_prefix = format!("manifests/{}/", barcode);
    let sentinel_key = manifest_latest_sentinel_key(&barcode);
    let listed = backend.list_objects(&manifest_prefix).await?;
    let mut sentinel: Option<String> = None;
    for k in listed {
        if k == sentinel_key {
            sentinel = Some(k);
        } else {
            others.push(k);
        }
    }

    Ok(CartridgeKeys { others, sentinel })
}

/// Apply `held` to every `keys` entry on `backend`, with bounded
/// concurrency. Returns the per-key tally.
pub async fn apply_legal_hold_to_keys(
    backend: Arc<dyn ObjectStoreBackend>,
    keys: Vec<String>,
    held: bool,
    concurrency: usize,
) -> HoldRunReport {
    use futures::stream::{self, StreamExt};

    let total = keys.len();
    let outcomes: Vec<PerKeyOutcome> = stream::iter(keys)
        .map(|key| {
            let backend = Arc::clone(&backend);
            async move {
                let result = backend
                    .set_object_legal_hold(&key, held)
                    .await
                    .map_err(SmcError::from);
                PerKeyOutcome { key, result }
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    let mut successes = 0usize;
    let mut failures = Vec::new();
    for o in outcomes {
        if o.result.is_ok() {
            successes += 1;
        } else {
            failures.push(o);
        }
    }
    HoldRunReport {
        total,
        successes,
        failures,
    }
}

/// High-level: apply legal hold to the cartridge's full set of storage
/// keys with the sentinel-last (set) / sentinel-first (clear)
/// ordering described in the module docs. The sentinel is only set
/// after every other key succeeds; it's only cleared first when
/// `held == false`.
pub async fn apply_cartridge_legal_hold(
    backend: Arc<dyn ObjectStoreBackend>,
    keys: &CartridgeKeys,
    held: bool,
    concurrency: usize,
) -> HoldRunReport {
    if held {
        // Set: body first, then sentinel.
        let mut report =
            apply_legal_hold_to_keys(Arc::clone(&backend), keys.others.clone(), true, concurrency)
                .await;
        if let Some(s) = keys.sentinel.as_ref() {
            report.total += 1;
            // Sentinel is only meaningful if every other key succeeded.
            if report.failures.is_empty() {
                match backend.set_object_legal_hold(s, true).await {
                    Ok(()) => report.successes += 1,
                    Err(e) => report.failures.push(PerKeyOutcome {
                        key: s.clone(),
                        result: Err(SmcError::from(e)),
                    }),
                }
            } else {
                report.failures.push(PerKeyOutcome {
                    key: s.clone(),
                    result: Err(SmcError::InvalidOp(
                        "skipped sentinel: prior keys failed to apply hold",
                    )),
                });
            }
        }
        report
    } else {
        // Clear: sentinel first, then body.
        let mut report = HoldRunReport::default();
        if let Some(s) = keys.sentinel.as_ref() {
            report.total += 1;
            match backend.set_object_legal_hold(s, false).await {
                Ok(()) => report.successes += 1,
                Err(e) => {
                    report.failures.push(PerKeyOutcome {
                        key: s.clone(),
                        result: Err(SmcError::from(e)),
                    });
                    return report; // body still held — surface the error
                }
            }
        }
        let body = apply_legal_hold_to_keys(backend, keys.others.clone(), false, concurrency).await;
        report.total += body.total;
        report.successes += body.successes;
        report.failures.extend(body.failures);
        report
    }
}

/// Single-key status read for the sentinel. Returns `Ok(true)` if the
/// cartridge is held, `Ok(false)` if not, `Err(...)` on IO/permission
/// problems. This is the canonical "is X held?" question — one round
/// trip, no sampling.
pub async fn read_cartridge_held(
    backend: Arc<dyn ObjectStoreBackend>,
    barcode: String,
) -> Result<bool> {
    let key = manifest_latest_sentinel_key(&barcode);
    Ok(backend.get_object_legal_hold(&key).await?)
}

/// Status sweep: read provider state for every supplied key. Used by
/// `legal-hold status --full` to verify each chunk + backup matches
/// what the sentinel says.
pub async fn read_legal_hold_for_keys(
    backend: Arc<dyn ObjectStoreBackend>,
    keys: Vec<String>,
    concurrency: usize,
) -> Vec<(String, Result<bool>)> {
    use futures::stream::{self, StreamExt};

    stream::iter(keys.into_iter().enumerate())
        .map(|(idx, key)| {
            let backend = Arc::clone(&backend);
            async move {
                let r = backend
                    .get_object_legal_hold(&key)
                    .await
                    .map_err(SmcError::from);
                (idx, key, r)
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|(_, k, r)| (k, r))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_object_store::{LockState, ObjectStoreBackend, ObjectStoreError};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// In-memory `ObjectStoreBackend` mock. `LocalBackend` returns
    /// `NotSupported` for the legal-hold ops, so the legal-hold module
    /// can only be exercised against a backend that implements them —
    /// this mock keeps the hold flags in a `HashMap` and replays a
    /// configured key list from `list_objects`. Every other trait
    /// method is an inert stub.
    #[derive(Debug, Clone)]
    struct HoldMock {
        holds: Arc<Mutex<HashMap<String, bool>>>,
        listing: Arc<Vec<String>>,
        fail: bool,
    }

    impl HoldMock {
        fn new(listing: Vec<String>) -> Self {
            Self {
                holds: Arc::new(Mutex::new(HashMap::new())),
                listing: Arc::new(listing),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                holds: Arc::new(Mutex::new(HashMap::new())),
                listing: Arc::new(Vec::new()),
                fail: true,
            }
        }
    }

    #[async_trait::async_trait]
    impl ObjectStoreBackend for HoldMock {
        async fn upload_chunk(
            &self,
            _key: &str,
            data: &[u8],
        ) -> shared_object_store::Result<(
            u64,
            Option<u64>,
            Option<shared_object_store::CompressionAlgo>,
        )> {
            Ok((data.len() as u64, None, None))
        }
        async fn upload_chunk_zerocopy(
            &self,
            _key: &str,
            _path: &Path,
        ) -> shared_object_store::Result<u64> {
            Ok(0)
        }
        async fn download_chunk(&self, _key: &str) -> shared_object_store::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn download_chunks_parallel(
            &self,
            _keys: &[String],
        ) -> shared_object_store::Result<Vec<Vec<u8>>> {
            Ok(Vec::new())
        }
        async fn upload_manifest(
            &self,
            _key: &str,
            _json: &str,
        ) -> shared_object_store::Result<()> {
            Ok(())
        }
        async fn download_manifest(&self, _key: &str) -> shared_object_store::Result<String> {
            Ok(String::new())
        }
        async fn chunk_exists(&self, _key: &str) -> shared_object_store::Result<bool> {
            Ok(false)
        }
        async fn list_objects(&self, prefix: &str) -> shared_object_store::Result<Vec<String>> {
            Ok(self
                .listing
                .iter()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
        async fn delete_object(&self, _key: &str) -> shared_object_store::Result<()> {
            Ok(())
        }
        fn backend_type(&self) -> &'static str {
            "mock"
        }
        async fn lock_state(&self) -> shared_object_store::Result<LockState> {
            Ok(LockState::Off)
        }
        async fn set_object_legal_hold(
            &self,
            key: &str,
            held: bool,
        ) -> shared_object_store::Result<()> {
            if self.fail {
                return Err(ObjectStoreError::NotSupported("mock failure".to_string()));
            }
            self.holds
                .lock()
                .expect("mock lock")
                .insert(key.to_string(), held);
            Ok(())
        }
        async fn get_object_legal_hold(&self, key: &str) -> shared_object_store::Result<bool> {
            if self.fail {
                return Err(ObjectStoreError::NotSupported("mock failure".to_string()));
            }
            Ok(self
                .holds
                .lock()
                .expect("mock lock")
                .get(key)
                .copied()
                .unwrap_or(false))
        }
        fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
            Box::new(self.clone())
        }
    }

    fn backend(mock: HoldMock) -> Arc<dyn ObjectStoreBackend> {
        Arc::new(mock)
    }

    #[test]
    fn sentinel_key_is_the_manifest_latest_object() {
        assert_eq!(
            manifest_latest_sentinel_key("TAPE01"),
            "manifests/TAPE01/manifest-latest.json",
        );
    }

    #[tokio::test]
    async fn collect_cartridge_keys_splits_sentinel_from_the_body() {
        let tmp = TempDir::new().expect("temp dir");
        let cart_dir = tmp.path().join("TAPE01");
        std::fs::create_dir_all(&cart_dir).expect("cart dir");
        let hash = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        std::fs::write(
            cart_dir.join("manifest.json"),
            r#"{"label":"TAPE01"}"#,
        )
        .expect("write manifest");
        // Sealed chunk lives in chunks.idx, not the manifest (issue #119).
        let idx = crate::chunk_index::ChunkIndexFile::open_or_create(&cart_dir).expect("chunk idx");
        idx.append(&crate::chunk_index::ChunkRec {
            size: 4096,
            hash: Some(hash.to_string()),
            location: crate::chunk_index::LocationTag::LocalOnly,
            uploaded: true,
            compression: None,
        })
        .expect("append sealed chunk");
        drop(idx);

        let sentinel = manifest_latest_sentinel_key("TAPE01");
        let versioned = "manifests/TAPE01/manifest-20260101T000000Z.json".to_string();
        let mock = HoldMock::new(vec![sentinel.clone(), versioned.clone()]);

        let keys = collect_cartridge_keys(tmp.path(), "TAPE01".to_string(), backend(mock))
            .await
            .expect("collect");
        assert_eq!(keys.sentinel.as_deref(), Some(sentinel.as_str()));
        // The chunk key + the versioned manifest backup, sentinel excluded.
        assert!(keys.others.iter().any(|k| k.contains(hash)));
        assert!(keys.others.contains(&versioned));
        assert!(!keys.others.contains(&sentinel));
    }

    #[tokio::test]
    async fn collect_cartridge_keys_rejects_a_label_mismatch() {
        let tmp = TempDir::new().expect("temp dir");
        let cart_dir = tmp.path().join("TAPE01");
        std::fs::create_dir_all(&cart_dir).expect("cart dir");
        std::fs::write(
            cart_dir.join("manifest.json"),
            r#"{"label":"WRONG","chunks":[]}"#,
        )
        .expect("write manifest");
        let mock = HoldMock::new(Vec::new());
        let err = collect_cartridge_keys(tmp.path(), "TAPE01".to_string(), backend(mock)).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn apply_legal_hold_to_keys_tallies_successes_and_failures() {
        let keys = vec!["chunks/a.dat".to_string(), "chunks/b.dat".to_string()];

        let ok =
            apply_legal_hold_to_keys(backend(HoldMock::new(Vec::new())), keys.clone(), true, 4)
                .await;
        assert_eq!(ok.total, 2);
        assert_eq!(ok.successes, 2);
        assert!(ok.failures.is_empty());

        let bad = apply_legal_hold_to_keys(backend(HoldMock::failing()), keys, true, 4).await;
        assert_eq!(bad.total, 2);
        assert_eq!(bad.successes, 0);
        assert_eq!(bad.failures.len(), 2);
    }

    #[tokio::test]
    async fn apply_cartridge_legal_hold_set_then_clear_round_trips() {
        let mock = HoldMock::new(Vec::new());
        let b = backend(mock.clone());
        let keys = CartridgeKeys {
            others: vec!["chunks/a.dat".to_string(), "chunks/b.dat".to_string()],
            sentinel: Some(manifest_latest_sentinel_key("TAPE01")),
        };

        // Set: body first, sentinel last — every key ends up held.
        let set = apply_cartridge_legal_hold(Arc::clone(&b), &keys, true, 4).await;
        assert_eq!(set.total, 3);
        assert_eq!(set.successes, 3);
        assert!(set.failures.is_empty());
        assert!(
            read_cartridge_held(Arc::clone(&b), "TAPE01".to_string())
                .await
                .expect("status")
        );

        // Clear: sentinel first, then body.
        let clear = apply_cartridge_legal_hold(Arc::clone(&b), &keys, false, 4).await;
        assert_eq!(clear.total, 3);
        assert_eq!(clear.successes, 3);
        assert!(
            !read_cartridge_held(b, "TAPE01".to_string())
                .await
                .expect("status")
        );
    }

    #[tokio::test]
    async fn apply_cartridge_legal_hold_set_skips_sentinel_when_body_fails() {
        let keys = CartridgeKeys {
            others: vec!["chunks/a.dat".to_string()],
            sentinel: Some(manifest_latest_sentinel_key("TAPE01")),
        };
        let report = apply_cartridge_legal_hold(backend(HoldMock::failing()), &keys, true, 4).await;
        // Body key fails, and the sentinel is reported as skipped.
        assert_eq!(report.total, 2);
        assert_eq!(report.successes, 0);
        assert_eq!(report.failures.len(), 2);
    }

    #[tokio::test]
    async fn read_legal_hold_for_keys_reports_per_key_state() {
        let b = backend(HoldMock::new(Vec::new()));
        let keys = vec!["chunks/a.dat".to_string(), "chunks/b.dat".to_string()];
        apply_legal_hold_to_keys(Arc::clone(&b), keys.clone(), true, 4).await;
        let states = read_legal_hold_for_keys(b, keys, 4).await;
        assert_eq!(states.len(), 2);
        for (_, r) in states {
            assert!(r.expect("state read"));
        }
    }
}
