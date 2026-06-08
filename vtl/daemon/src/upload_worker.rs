// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Event-driven storage upload worker (Phase 4: Event-Driven Uploads).
//!
//! Listens for upload requests from MemoryBufferManager and uploads
//! chunks to the per-cartridge sticky storage backend. Triggered by
//! buffer fullness or cartridge unload events.
//!
//! Bounded-concurrency PUT pipeline + per-completion retry-classified
//! single-attempt semantics now live in
//! `shared_upload_worker::run_upload_pipeline`; this module supplies
//! the tape-side bits (per-cartridge open, manifest-side outcome
//! application, manifest backup, legal-hold reapply, eviction-Notify
//! coalescing) around it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use core_mediachanger::{
    Cartridge, CartridgeOpenMode, CartridgeOpenOptions, ChunkUploadOutcome, ObjectStoreBackend,
    PendingUploadPayload,
};
use shared_object_store::ObjectStoreConfig;
use shared_upload_worker::run_upload_pipeline;

use crate::{Config, memory_buffer_manager, read_cartridge_backend};

type BackendRegistry = HashMap<String, Box<dyn ObjectStoreBackend>>;

pub(crate) async fn run_event_driven_upload_worker(
    cfg: &Config,
    mut upload_rx: mpsc::Receiver<memory_buffer_manager::UploadRequest>,
    disk_cache_evict_notify: Arc<Notify>,
) -> Result<()> {
    // Per-backend storage registry. Built lazily as we see the first
    // upload request for each backend; legacy single-backend deploys
    // populate just one entry. Initialization is a real network/auth
    // round-trip for S3/GCS/Azure, so doing it lazily keeps a quiet
    // daemon (no upload requests) cheap.
    let mut registry: BackendRegistry = HashMap::new();

    let upload_cfg = &cfg.storage.upload;
    let (max_concurrent, max_concurrent_source) = upload_cfg.resolve_max_concurrent();

    // Note: `retry_max_attempts` from the config governs the
    // *per-backend* retry budget inside `object_store_helpers::retry_async`
    // (already classify-and-fail-fast on permanent errors, jittered
    // backoff on transient ones). Pre-Batch-F this worker layered a
    // second retry loop on top of every `upload_chunk_inert` call, so
    // a transient outage burned `retry_max_attempts × retry_max_attempts`
    // composed attempts (≈30 minutes worst-case at the default of 10)
    // before the failure surfaced. The outer layer is gone now —
    // `upload_chunk_inert` is invoked exactly once per dispatched
    // chunk, the inner backend retry handles short-lived errors, and
    // any chunk that still fails surfaces immediately. The
    // unload-flush loop in `MemoryBufferManager::on_cartridge_unloaded`
    // is the next-level retry boundary; persistent failure also leaves
    // the chunk's `uploaded=false` flag in `chunks.idx` for a future
    // load to reclaim.
    info!(
        "Event-driven upload worker initialized (max_concurrent={} ({}), per-backend retry budget={})",
        max_concurrent, max_concurrent_source, upload_cfg.retry_max_attempts
    );

    let tapes_root = Path::new(&cfg.data_dir).join("tapes");

    while let Some(request) = upload_rx.recv().await {
        info!(
            "Received upload request for {}: {} chunks",
            request.tape_id,
            request.chunk_ids.len()
        );

        let Some(storage_backend) =
            resolve_backend(cfg, &request, &cfg.storage, &mut registry, &tapes_root).await
        else {
            continue;
        };

        let Some((mut cart, auto_hold)) =
            open_cart_and_hold_flag(&tapes_root, &request.tape_id, storage_backend).await
        else {
            continue;
        };

        // Snapshot per-chunk upload payloads from the (mutably) owned
        // cartridge once. Skips ids that are already uploaded, unsealed,
        // or missing from the manifest — same effective filtering
        // `upload_chunk_to_storage` would have done internally.
        let pending_payloads: Vec<PendingUploadPayload> = request
            .chunk_ids
            .iter()
            .filter_map(|&id| cart.pending_upload_payload(id as u64))
            .collect();

        if pending_payloads.is_empty() {
            debug!(
                "Upload worker: no pending uploads for {} (already uploaded or unsealed)",
                request.tape_id
            );
        }

        let outcomes = run_upload_pipeline(
            storage_backend,
            &request.tape_id,
            pending_payloads,
            max_concurrent,
            |outcome| {
                vtl_post_upload_hook(
                    storage_backend,
                    &request.tape_id,
                    auto_hold,
                    &disk_cache_evict_notify,
                    outcome,
                )
            },
        )
        .await;

        apply_outcomes_and_backup_manifest(
            &mut cart,
            &outcomes,
            storage_backend,
            &request.tape_id,
            auto_hold,
        )
        .await;
    }

    info!("Upload worker shutting down (channel closed)");
    Ok(())
}

/// Read the cartridge's sticky backend from its manifest, lazy-initialize
/// the named backend in the registry if needed, and return a reference to
/// the resolved handle.
///
/// `None` on either path (missing manifest backend / failed init) — the
/// worker skips this request and continues.
async fn resolve_backend<'a>(
    _cfg: &Config,
    request: &memory_buffer_manager::UploadRequest,
    storage_config: &ObjectStoreConfig,
    registry: &'a mut BackendRegistry,
    tapes_root: &Path,
) -> Option<&'a dyn ObjectStoreBackend> {
    let backend_name = match read_cartridge_backend(tapes_root, &request.tape_id) {
        Some(name) => name,
        None => {
            warn!(
                "Upload worker: cannot read backend for cartridge {} - skipping upload",
                request.tape_id
            );
            return None;
        }
    };

    if !registry.contains_key(&backend_name) {
        match storage_config.create_backend_named(&backend_name).await {
            Ok(b) => {
                // Cache warmup: spawn a LIST chunks/ that seeds the
                // wrapper's cache with `Probed` entries. Clone shares
                // the same internal cache map so the warmup populates
                // the backend the registry holds. Non-blocking — a
                // LIST failure leaves the cache cold; next write does
                // a real HEAD/PUT.
                //
                // Best-effort by design: the result handle is observed by
                // a tiny supervisor (below) only so an unexpected *panic*
                // in `warmup_prefix` is logged rather than silently lost.
                // A cold cache is always safe to fall back to.
                let warmup: Box<dyn ObjectStoreBackend> = b.clone();
                let warmup_name = backend_name.clone();
                let handle = tokio::spawn(async move {
                    match warmup.warmup_prefix("chunks/").await {
                        Ok(n) => tracing::info!(
                            "storage cache warmup: seeded {} chunks/ keys for backend '{}'",
                            n,
                            warmup_name
                        ),
                        Err(e) => warn!(
                            "storage cache warmup failed for backend '{}': {} (continuing with cold cache)",
                            warmup_name, e
                        ),
                    }
                });
                let observe_name = backend_name.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle.await
                        && e.is_panic()
                    {
                        warn!(
                            "storage cache warmup task for backend '{}' panicked: {} (continuing with cold cache)",
                            observe_name, e
                        );
                    }
                });
                registry.insert(backend_name.clone(), b);
            }
            Err(e) => {
                warn!(
                    "Upload worker: failed to init backend '{}' for cartridge {}: {e}",
                    backend_name, request.tape_id
                );
                return None;
            }
        }
    }

    Some(
        registry
            .get(&backend_name)
            .expect("backend just inserted into registry above")
            .as_ref(),
    )
}

/// Open the cartridge against `storage_backend` and read its legal-hold
/// sentinel. Returns `None` on cart-open failure (logged warn); a hold-read
/// failure is logged at debug and treated as "not held".
///
/// Single open per request: parallel chunk uploads run via stateless
/// `upload_chunk_inert` against the shared handle, then outcomes are
/// applied serially. Reopening per task tripped on `create_new(staging)`
/// for the trailing chunk that `resume_or_create_active` allocates
/// whenever all chunks are sealed (issue surfaced 2026-05-03 by
/// `test-backup-storage.sh`).
///
/// View-only handle: the drive-side primary owns the trailing staging
/// chunk. Without `with_view_only`, this handle's `Drop` would call
/// `flush_and_seal` → unlink the staging file the drive is still
/// writing to and truncate the chunk_index slot the block-index already
/// references. Subsequent reads then ENOENT (issue #28). The flag tells
/// `Cartridge::drop` to skip `flush_and_seal` and `runtime.persist`.
async fn open_cart_and_hold_flag(
    tapes_root: &Path,
    tape_id: &str,
    storage_backend: &dyn ObjectStoreBackend,
) -> Option<(Cartridge, bool)> {
    let cart = match Cartridge::open_with(
        tapes_root,
        tape_id,
        CartridgeOpenMode::Open,
        CartridgeOpenOptions::new()
            .with_storage(Some(storage_backend.clone_box()))
            .with_view_only(),
    ) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to open cartridge {}: {e:?}", tape_id);
            return None;
        }
    };

    // Auto-hold: read the cartridge's legal-hold sentinel once per
    // request. If set, the caller re-applies the per-object hold to
    // every freshly-PUT chunk and to the new manifest backup objects
    // so chunks written *after* `legal-hold set` are not silently
    // un-held. A read failure here (typically: sentinel does not exist
    // yet because nothing has ever been uploaded for this cartridge,
    // or the backend does not support hold — local) is treated as
    // "not held" and logged at debug.
    let backend_arc: Arc<dyn ObjectStoreBackend> = Arc::from(storage_backend.clone_box());
    let auto_hold = match core_mediachanger::read_cartridge_held(backend_arc, tape_id.to_string())
        .await
    {
        Ok(h) => h,
        Err(e) => {
            debug!(
                "Upload worker: legal-hold sentinel read for {} returned {} - treating as not-held",
                tape_id, e
            );
            false
        }
    };
    if auto_hold {
        info!(
            "Upload worker: cartridge {} is under legal hold - auto-applying hold to fresh objects",
            tape_id
        );
    }

    Some((cart, auto_hold))
}

/// Tape-side per-completion hook for the shared upload pipeline.
/// Re-applies the per-object legal hold (under `auto_hold`) and
/// nudges the disk-cache eviction worker (Notify coalesces, so a
/// sustained upload stream still produces at most one eviction pass
/// per debounce window). The slow-chunk case doesn't delay either
/// signal behind its batchmates — the hook fires per-task before
/// the outcome is yielded into the result vector.
async fn vtl_post_upload_hook(
    storage_backend: &dyn ObjectStoreBackend,
    tape_id: &str,
    auto_hold: bool,
    disk_cache_evict_notify: &Arc<Notify>,
    outcome: ChunkUploadOutcome,
) {
    if auto_hold
        && let Err(e) = storage_backend
            .set_object_legal_hold(&outcome.object_key, true)
            .await
    {
        warn!(
            "Auto-hold: failed to apply legal hold to chunk {} ({}) for {}: {}",
            outcome.item_id, outcome.object_key, tape_id, e
        );
    }
    disk_cache_evict_notify.notify_one();
}

/// Apply successful chunk-upload outcomes to the cartridge's chunk index
/// (O(1) pwrite per outcome via `apply_chunk_upload_outcome`) and back up
/// the manifest to storage. If the cartridge is held, re-apply per-object
/// holds to the new index pages, the versioned backup, and the
/// `manifest-latest` sentinel — body→sentinel ordering matches the
/// explicit `legal-hold set` path.
///
/// Cartridge is the sole writer to its own chunk index; keeping the
/// mutation here (not in spawned tasks) is what makes the
/// parallel-upload architecture safe.
async fn apply_outcomes_and_backup_manifest(
    cart: &mut Cartridge,
    outcomes: &[ChunkUploadOutcome],
    storage_backend: &dyn ObjectStoreBackend,
    tape_id: &str,
    auto_hold: bool,
) {
    if !outcomes.is_empty() {
        for outcome in outcomes {
            cart.apply_chunk_upload_outcome(outcome);
        }
    }

    match cart.backup_manifest_to_storage().await {
        Ok(outcome) => {
            if auto_hold {
                // Body first (index pages + versioned backup), sentinel
                // last. Best-effort per key: a failure logs but does not
                // tear down the upload (data is already durable).
                for page_key in &outcome.index_page_keys {
                    if let Err(e) = storage_backend.set_object_legal_hold(page_key, true).await {
                        warn!(
                            "Auto-hold: failed to apply legal hold to index page {} for {}: {}",
                            page_key, tape_id, e
                        );
                    }
                }
                if let Err(e) = storage_backend
                    .set_object_legal_hold(&outcome.versioned_key, true)
                    .await
                {
                    warn!(
                        "Auto-hold: failed to apply legal hold to manifest backup {} for {}: {}",
                        outcome.versioned_key, tape_id, e
                    );
                }
                if let Err(e) = storage_backend
                    .set_object_legal_hold(&outcome.latest_key, true)
                    .await
                {
                    warn!(
                        "Auto-hold: failed to apply legal hold to manifest sentinel {} for {}: {}",
                        outcome.latest_key, tape_id, e
                    );
                }
            }
        }
        Err(e) => {
            warn!("Failed to backup manifest for {}: {e:?}", tape_id);
        }
    }
}

// Pipeline-no-gate test moved to
// `shared/upload-worker/src/pipeline.rs` alongside the function it
// guards. Tape-side glue (cartridge open, hook wiring,
// apply_outcomes_and_backup_manifest) is covered by the
// `vtl/scripts/test-backup-storage.sh` end-to-end run.
