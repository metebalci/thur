// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Async storage-upload worker for VSA. Block-side counterpart to
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
//!   storage backend (which does the storage-side HEAD probe under
//!   `Global` dedup), and on success calls
//!   `VolumeWriter::apply_page_upload_outcome` — which flips the
//!   sidecar back to `Uploaded` and notifies any
//!   `PageCache::synchronize_bytes` waiter parked on the page range.
//! - A [`Semaphore`] caps in-flight uploads at the operator's
//!   `storage.upload.max_concurrent` (mirrors VTL's knob; sentinel `0`
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
//! pool, storage PUT never acked), and re-enqueues them through the
//! same channel. The worker drains them indistinguishably from
//! live writes.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use core_block::{PageCache, UploadState, UploadTask};
use shared_object_store::ObjectStoreBackend;
use shared_upload_worker::upload_chunk_inert;
use tokio::sync::{Mutex, Semaphore, mpsc};
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
    backends: Arc<Mutex<BTreeMap<String, Arc<dyn ObjectStoreBackend>>>>,
    max_concurrent: usize,
    evict_notify: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let concurrency = max_concurrent.max(1);
    let semaphore = Arc::new(Semaphore::new(concurrency));
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
        let evict_notify = Arc::clone(&evict_notify);
        tokio::spawn(async move {
            let _permit = permit;
            // A successful upload flips a page LocalOnly -> Both, making
            // its pool chunk evictable — wake the eviction worker so a
            // cache-full write burst recovers within the backpressure
            // deadline rather than the 5 min interval (issue #225).
            if run_one_task(task, &backends, &registry).await {
                evict_notify.notify_one();
            }
        });
    }
    info!("thurvsa upload worker shutting down (channel closed)");
    Ok(())
}

/// Single-task pipeline: resolve backend + cache, run
/// `upload_chunk_inert`, apply outcome. All errors logged and
/// swallowed — the per-page `LocalOnly` marker stays set so the
/// next write, fence-driven retry, or boot-time scan re-enqueues.
/// Every failure path settles the in-flight marker via
/// `task.pending.mark_failed` (issue #106): a waiter parked on the
/// page (SYNCHRONIZE CACHE, flush_all) wakes with a failure verdict
/// instead of hanging forever on an upload that already gave up.
/// Returns `true` iff the page was uploaded and its outcome applied
/// (LocalOnly -> Both) — the caller uses that to wake the eviction
/// worker (issue #225). Every failure path returns `false`.
async fn run_one_task(
    task: UploadTask,
    backends: &Arc<Mutex<BTreeMap<String, Arc<dyn ObjectStoreBackend>>>>,
    registry: &VolumeRegistry,
) -> bool {
    let page_id = u32::try_from(task.payload.item_id).ok();
    // Share the same map AdminState holds so backends instantiated by
    // a runtime `volume create` (via admin/handlers.rs's
    // `get_or_init_backend`) are visible here. Pre-fix this was a
    // snapshot taken at boot, which meant any post-boot create's
    // pages dispatched into the worker hit "backend unknown" and the
    // upload silently no-op'd into LocalOnly forever — invisible to
    // operators because daemon restart re-populates discovery's map.
    let backend = {
        let guard = backends.lock().await;
        match guard.get(&task.payload.backend_name) {
            Some(b) => Arc::clone(b),
            None => {
                warn!(
                    "upload worker: backend '{}' unknown (volume='{}' page={}); leaving LocalOnly",
                    task.payload.backend_name, task.volume_name, task.payload.item_id
                );
                shared_telemetry::record::chunk_upload_stranded(
                    &task.payload.backend_name,
                    "backend_unknown",
                );
                if let Some(p) = page_id {
                    task.pending.mark_failed(p).await;
                }
                return false;
            }
        }
    };
    let cache = match registry.get_by_name(&task.volume_name) {
        Some(c) => c,
        None => {
            warn!(
                "upload worker: volume '{}' unknown in registry (page={}); leaving LocalOnly",
                task.volume_name, task.payload.item_id
            );
            shared_telemetry::record::chunk_upload_stranded(
                &task.payload.backend_name,
                "entity_unknown",
            );
            if let Some(p) = page_id {
                task.pending.mark_failed(p).await;
            }
            return false;
        }
    };
    let item_id = task.payload.item_id;
    let outcome = match upload_chunk_inert(&*backend, &task.payload).await {
        Ok(o) => o,
        Err(e) => {
            warn!(
                "upload worker: PUT failed for volume '{}' page {} ({}): {}",
                task.volume_name, item_id, task.payload.object_key, e
            );
            if let Some(p) = page_id {
                task.pending.mark_failed(p).await;
            }
            return false;
        }
    };
    if let Err(e) = cache.writer().apply_page_upload_outcome(&outcome).await {
        warn!(
            "upload worker: apply_page_upload_outcome failed for volume '{}' page {}: {}",
            task.volume_name, item_id, e
        );
        if let Some(p) = page_id {
            task.pending.mark_failed(p).await;
        }
        return false;
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
    true
}

/// Crash-recovery scan. Walks every volume under `data_dir`, opens
/// each `upload.idx`, and re-enqueues a task for every page still
/// marked `LocalOnly` (pool chunk present but storage PUT never
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
            pending: writer.pending_uploads().clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::{DedupScope, DrainOutcome, PendingUploads};
    use shared_object_store::LocalBackend;
    use shared_upload_worker::PendingUpload;
    use tempfile::TempDir;

    fn task_for(volume: &str, backend: &str, pending: &PendingUploads) -> UploadTask {
        UploadTask {
            volume_name: volume.to_string(),
            pending: pending.clone(),
            payload: PendingUpload {
                item_id: 0,
                hash: "00".repeat(32),
                local_path: std::path::PathBuf::from("/nonexistent"),
                object_key: "test/key".to_string(),
                dedup: DedupScope::Local,
                backend_name: backend.to_string(),
            },
        }
    }

    /// Issue #106: every per-task failure path must settle the
    /// in-flight marker as failed so a waiter parked on the page
    /// wakes with a verdict instead of hanging forever. Covers the
    /// two resolution failures (unknown backend, unknown volume);
    /// the PUT-failure path runs the same mark on the same tracker.
    #[tokio::test]
    async fn failed_resolution_settles_the_pending_marker() {
        let registry = VolumeRegistry::new();
        let backends: Arc<Mutex<BTreeMap<String, Arc<dyn ObjectStoreBackend>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        // Backend unknown.
        let pending = PendingUploads::new();
        pending.mark_pending(0).await;
        run_one_task(
            task_for("ghost", "no-such-backend", &pending),
            &backends,
            &registry,
        )
        .await;
        assert_eq!(pending.wait_for_range(0..=0).await, DrainOutcome::Failed);

        // Backend resolves, volume unknown in the registry (the
        // mid-destroy race shape from #103).
        let tmp = TempDir::new().unwrap();
        let local: Arc<dyn ObjectStoreBackend> =
            Arc::new(LocalBackend::new(tmp.path()).await.unwrap());
        backends.lock().await.insert("local".to_string(), local);
        let pending = PendingUploads::new();
        pending.mark_pending(0).await;
        run_one_task(task_for("ghost", "local", &pending), &backends, &registry).await;
        assert_eq!(pending.wait_for_range(0..=0).await, DrainOutcome::Failed);
    }
}
