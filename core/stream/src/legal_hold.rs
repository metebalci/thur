// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cartridge-level legal hold orchestration.
//!
//! Source of truth is the cloud provider's per-object hold primitive
//! (S3 `PutObjectLegalHold`, GCS `eventBasedHold`, Azure
//! `Set Blob Legal Hold`). Thur VTL keeps **no** local "is held" flag
//! — set/clear walks every chunk key the cartridge's manifest
//! references plus its manifest backups, and status reads provider
//! state back. The trade-off is that a held cartridge's iSCSI surface
//! is unchanged: the host won't see a write-protect signal; it will
//! see opaque upload/delete failures only when the daemon tries to
//! mutate a held cloud object. The promise is data preservation, not
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
use shared_cloud::CloudBackend;
use std::path::Path;
use std::sync::Arc;

// `find_drive_for_loaded_cartridge` previously lived here but reaches
// into `LibraryInventory` (smc-side state). It moved to core-mediachanger so
// core-stream can compile without core-mediachanger as a dependency. Callers
// import it via `core_mediachanger::find_drive_for_loaded_cartridge`.

/// Subset of `manifest.json` we need to enumerate cloud keys without
/// pulling in the whole `Cartridge` machinery (which would drag in a
/// `CloudBackend` and a runtime). Backend-flat: pool keys are
/// `chunks/<aa>/<bb>/<hash>.dat`; the manifest backups live under
/// `manifests/<label>/manifest-{latest,TIMESTAMP}.json`.
#[derive(Debug, Deserialize)]
struct ManifestSlice {
    label: String,
    #[serde(default)]
    chunks: Vec<ManifestChunkSlice>,
    /// Sticky dedup scope (see `Cartridge`/`Manifest`). Under `Local`
    /// the chunk + manifest cloud keys are namespaced under the
    /// cartridge barcode — so legal-hold key enumeration must match.
    #[serde(default)]
    dedup: crate::cartridge::DedupScope,
}

#[derive(Debug, Deserialize)]
struct ManifestChunkSlice {
    /// `Some(hex)` once the chunk has been sealed into the pool. `None`
    /// means the chunk is still in `.staging/` and was never uploaded —
    /// nothing to hold cloud-side.
    #[serde(default)]
    hash: Option<String>,
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

/// Split a cartridge's cloud keys into `(others, sentinel)`. Used by
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
/// cloud keys that should be put under (or released from) legal hold.
/// The sentinel (`manifest-latest.json`) is split out from `others`
/// so callers can sequence apply/clear in the right order.
///
/// Takes `Arc<dyn CloudBackend>` by value so callers don't have to
/// hold a borrow across the `list_objects` await — borrows of
/// `&dyn CloudBackend` across awaits trip the rustc HRTB-Send check
/// when the future flows through axum's `Handler` trait machinery.
pub async fn collect_cartridge_keys(
    tapes_dir: &Path,
    barcode: String,
    backend: Arc<dyn CloudBackend>,
) -> Result<CartridgeKeys> {
    use crate::chunk_store::ChunkStore;

    let manifest_path = tapes_dir.join(&barcode).join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(SmcError::Io)?;
    let m: ManifestSlice = serde_json::from_str(&raw).map_err(SmcError::SerdeJson)?;
    if m.label != barcode {
        return Err(SmcError::InvalidOp("manifest label does not match barcode"));
    }

    let cartridge_namespace: Option<&str> = m.dedup.namespace(&m.label);
    let mut others: Vec<String> = m
        .chunks
        .iter()
        .filter_map(|c| {
            c.hash
                .as_ref()
                .map(|h| ChunkStore::cloud_key_for(cartridge_namespace, h))
        })
        .collect();

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
    backend: Arc<dyn CloudBackend>,
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

/// High-level: apply legal hold to the cartridge's full set of cloud
/// keys with the sentinel-last (set) / sentinel-first (clear)
/// ordering described in the module docs. The sentinel is only set
/// after every other key succeeds; it's only cleared first when
/// `held == false`.
pub async fn apply_cartridge_legal_hold(
    backend: Arc<dyn CloudBackend>,
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
pub async fn read_cartridge_held(backend: Arc<dyn CloudBackend>, barcode: String) -> Result<bool> {
    let key = manifest_latest_sentinel_key(&barcode);
    Ok(backend.get_object_legal_hold(&key).await?)
}

/// Status sweep: read provider state for every supplied key. Used by
/// `legal-hold status --full` to verify each chunk + backup matches
/// what the sentinel says.
pub async fn read_legal_hold_for_keys(
    backend: Arc<dyn CloudBackend>,
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
