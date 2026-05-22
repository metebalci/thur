// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Async cloud-upload worker for VSA. Block-side counterpart to
//! `vtl/daemon/src/upload_worker.rs`.
//!
//! Architecture:
//!
//! - [`VolumeWriter::write_page_unsynced`] (in `core-block`) seals the
//!   page bytes into the local pool and sends an [`UploadTask`] over
//!   the daemon's `mpsc::Sender<UploadTask>` — returning immediately.
//!   The page-index bumps to the new hash; the upload-state sidecar
//!   marks `LocalOnly`; the page-cache sees a successful flush.
//! - This worker drains the receiver, looks up the volume's
//!   `VolumeWriter` via the registry, runs
//!   `shared_upload_worker::upload_chunk_inert` against the volume's
//!   cloud backend (which does the cloud-side HEAD probe under
//!   `Global` dedup), and on success calls
//!   `VolumeWriter::apply_page_upload_outcome` — which flips the
//!   sidecar back to `Uploaded` and notifies any
//!   `PageCache::synchronize_bytes` waiter parked on the page range.
//! - A [`Semaphore`] caps in-flight uploads at the operator's
//!   `cloud.upload.max_concurrent` (mirrors VTL's knob; sentinel `0`
//!   resolves to `min(16, num_cpus * 4)`).
//!
//! Why not [`shared_upload_worker::run_upload_pipeline`]? That
//! pipeline takes a `Vec<PendingUpload>` batch and pumps through
//! `buffer_unordered` — natural for VTL where each upload request
//! covers many chunks per cartridge. VSA sends one task per page,
//! so a `Semaphore + tokio::spawn` per-recv has the same
//! concurrency-cap semantics with less batching latency. The
//! single-PUT primitive (`upload_chunk_inert`) is shared either way.
//!
//! Crash recovery: on daemon boot
//! [`scan_and_enqueue_localonly`] walks every volume's
//! `upload.idx`, finds pages still marked `LocalOnly` (chunk in
//! pool, cloud PUT never acked), and re-enqueues them through the
//! same channel. The worker drains them indistinguishably from
//! live writes.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use core_block::{PageCache, UploadState, UploadTask};
use shared_cloud::CloudBackend;
use shared_upload_worker::upload_chunk_inert;
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};

use crate::registry::VolumeRegistry;

/// Channel depth for the daemon's upload queue. Matches VTL's
/// `100` so backpressure (`send().await`) on a saturated worker
/// arrives at the same point.
pub const UPLOAD_CHANNEL_DEPTH: usize = 100;

/// Construct the upload-task channel. Returns the cloneable sender
/// (one per `VolumeWriter` via `with_upload_sender`) and the
/// receiver (consumed by [`run_upload_worker`]).
pub fn upload_channel() -> (mpsc::Sender<UploadTask>, mpsc::Receiver<UploadTask>) {
    mpsc::channel(UPLOAD_CHANNEL_DEPTH)
}

/// Drive the daemon's async upload worker.
///
/// Drains tasks off `rx`, fans out up to `max_concurrent` PUTs in
/// flight via [`Semaphore`], and applies each outcome back through
/// the owning `VolumeWriter`. Exits cleanly once every sender has
/// been dropped (daemon shutdown).
///
/// Per-task failures (unknown volume, unknown backend, PUT error)
/// are logged and the page stays `LocalOnly`. The next host write
/// to that page resets the LocalOnly marker and re-enqueues; the
/// crash-recovery scan picks up leftover LocalOnly markers on
/// daemon restart.
pub async fn run_upload_worker(
    mut rx: mpsc::Receiver<UploadTask>,
    registry: Arc<VolumeRegistry>,
    backends: BTreeMap<String, Arc<dyn CloudBackend>>,
    max_concurrent: usize,
) -> Result<()> {
    let concurrency = max_concurrent.max(1);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let backends = Arc::new(backends);
    info!(
        "thurvsa upload worker started (max_concurrent={}, channel_depth={})",
        concurrency, UPLOAD_CHANNEL_DEPTH
    );
    while let Some(task) = rx.recv().await {
        // Permit acquisition gates concurrency. .acquire_owned()
        // hands the permit to the spawned task, which drops it on
        // completion — backpressure folds into how fast tasks
        // finish, not how fast they're spawned.
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                warn!("upload worker semaphore closed; exiting");
                break;
            }
        };
        let backends = Arc::clone(&backends);
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            let _permit = permit;
            run_one_task(task, &backends, &registry).await;
        });
    }
    info!("thurvsa upload worker shutting down (channel closed)");
    Ok(())
}

/// Single-task pipeline: resolve backend + cache, run
/// `upload_chunk_inert`, apply outcome. All errors logged and
/// swallowed — the per-page `LocalOnly` marker stays set so the
/// next write or boot-time scan re-enqueues.
async fn run_one_task(
    task: UploadTask,
    backends: &BTreeMap<String, Arc<dyn CloudBackend>>,
    registry: &VolumeRegistry,
) {
    let backend = match backends.get(&task.payload.backend_name) {
        Some(b) => Arc::clone(b),
        None => {
            warn!(
                "upload worker: backend '{}' unknown (volume='{}' page={}); leaving LocalOnly",
                task.payload.backend_name, task.volume_name, task.payload.item_id
            );
            return;
        }
    };
    let cache = match registry.get_by_name(&task.volume_name) {
        Some(c) => c,
        None => {
            warn!(
                "upload worker: volume '{}' unknown in registry (page={}); leaving LocalOnly",
                task.volume_name, task.payload.item_id
            );
            return;
        }
    };
    let item_id = task.payload.item_id;
    let outcome = match upload_chunk_inert(&*backend, &task.payload).await {
        Ok(o) => o,
        Err(e) => {
            warn!(
                "upload worker: PUT failed for volume '{}' page {} ({}): {}",
                task.volume_name, item_id, task.payload.cloud_key, e
            );
            return;
        }
    };
    if let Err(e) = cache.writer().apply_page_upload_outcome(&outcome).await {
        warn!(
            "upload worker: apply_page_upload_outcome failed for volume '{}' page {}: {}",
            task.volume_name, item_id, e
        );
        return;
    }
    // Fold the on-wire PUT size into the volume's backend-write
    // meter. `None` on a cross-namespace dedup hit (no PUT happened).
    if let Some(put_bytes) = outcome.put_bytes {
        cache.bump_backend_bytes_written(put_bytes);
    }
    debug!(
        "upload worker: volume '{}' page {} done (dedup_hit={})",
        task.volume_name, item_id, outcome.dedup_hit
    );
}

/// Crash-recovery scan. Walks every volume under `data_dir`, opens
/// each `upload.idx`, and re-enqueues a task for every page still
/// marked `LocalOnly` (pool chunk present but cloud PUT never
/// acked). Called at daemon boot before any host write path is
/// live.
///
/// Mirrors `vtl/daemon/src/upload_recovery.rs::scan_and_enqueue_orphans`.
/// The scan is best-effort — a per-volume failure is logged and
/// skipped, never propagated, so one corrupt sidecar doesn't block
/// the daemon from starting.
pub async fn scan_and_enqueue_localonly(
    data_dir: &Path,
    registry: &VolumeRegistry,
    sender: &mpsc::Sender<UploadTask>,
) {
    use core_block::VolumeManifest;

    let names = match VolumeManifest::list(data_dir) {
        Ok(n) => n,
        Err(e) => {
            warn!(
                "upload recovery: listing volumes under {} failed: {}",
                data_dir.display(),
                e
            );
            return;
        }
    };

    let mut total = 0usize;
    for name in &names {
        let cache = match registry.get_by_name(name) {
            Some(c) => c,
            None => {
                // Volume on disk but not in the registry — discovery
                // either skipped it or it's encrypted without a
                // resolved keystore. The next host write will
                // re-establish state.
                continue;
            }
        };
        match enqueue_one_volume(name, &cache, sender).await {
            Ok(n) => total += n,
            Err(e) => warn!("upload recovery: volume '{}' scan failed: {}", name, e),
        }
    }
    if total > 0 {
        info!(
            "upload recovery: re-enqueued {} LocalOnly page(s) across {} volume(s)",
            total,
            names.len()
        );
    } else {
        debug!(
            "upload recovery: no LocalOnly survivors across {} volume(s)",
            names.len()
        );
    }
}

async fn enqueue_one_volume(
    name: &str,
    cache: &PageCache,
    sender: &mpsc::Sender<UploadTask>,
) -> Result<usize> {
    let writer = cache.writer();
    let upload_idx = writer.upload_index();
    let mut enqueued = 0usize;
    for entry in upload_idx.iter()? {
        let (page_id, state) = entry?;
        if !matches!(state, UploadState::LocalOnly) {
            continue;
        }
        let Some(payload) = writer.pending_upload_payload(page_id)? else {
            // upload.idx says LocalOnly but pages.idx has no
            // entry for this page — sidecar drift. Clear the
            // marker so it doesn't stick forever.
            upload_idx.set(page_id, UploadState::Uploaded)?;
            continue;
        };
        let task = UploadTask {
            volume_name: name.to_string(),
            payload,
        };
        writer.pending_uploads().mark_pending(page_id).await;
        if sender.send(task).await.is_err() {
            // Channel closed mid-scan; roll back the pending marker
            // and stop.
            writer.pending_uploads().mark_done(page_id).await;
            return Ok(enqueued);
        }
        enqueued += 1;
    }
    Ok(enqueued)
}
