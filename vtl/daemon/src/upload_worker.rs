// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Event-driven cloud upload worker (Phase 4: Event-Driven Uploads).
//!
//! Listens for upload requests from MemoryBufferManager and uploads
//! chunks to the per-cartridge sticky cloud backend. Triggered by
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
    Cartridge, CartridgeOpenMode, ChunkUploadOutcome, CloudBackend, PendingUploadPayload,
};
use shared_cloud::CloudConfig;
use shared_upload_worker::run_upload_pipeline;

use crate::{Config, memory_buffer_manager, read_cartridge_backend};

type BackendRegistry = HashMap<String, Box<dyn CloudBackend>>;

pub(crate) async fn run_event_driven_upload_worker(
    cfg: &Config,
    mut upload_rx: mpsc::Receiver<memory_buffer_manager::UploadRequest>,
    disk_cache_evict_notify: Arc<Notify>,
) -> Result<()> {
    // Per-backend cloud registry. Built lazily as we see the first
    // upload request for each backend; legacy single-backend deploys
    // populate just one entry. Initialization is a real network/auth
    // round-trip for S3/GCS/Azure, so doing it lazily keeps a quiet
    // daemon (no upload requests) cheap.
    let mut registry: BackendRegistry = HashMap::new();

    let upload_cfg = &cfg.cloud.upload;
    let (max_concurrent, max_concurrent_source) = upload_cfg.resolve_max_concurrent();

    // Note: `retry_max_attempts` from the config governs the
    // *per-backend* retry budget inside `cloud_helpers::retry_async`
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

        let Some(cloud_backend) =
            resolve_backend(cfg, &request, &cfg.cloud, &mut registry, &tapes_root).await
        else {
            continue;
        };

        let Some((mut cart, auto_hold)) =
            open_cart_and_hold_flag(&tapes_root, &request.tape_id, cloud_backend).await
        else {
            continue;
        };

        // Snapshot per-chunk upload payloads from the (mutably) owned
        // cartridge once. Skips ids that are already uploaded, unsealed,
        // or missing from the manifest — same effective filtering
        // `upload_chunk_to_cloud` would have done internally.
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
            cloud_backend,
            &request.tape_id,
            pending_payloads,
            max_concurrent,
            |outcome| {
                vtl_post_upload_hook(
                    cloud_backend,
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
            cloud_backend,
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
    cloud_config: &CloudConfig,
    registry: &'a mut BackendRegistry,
    tapes_root: &Path,
) -> Option<&'a dyn CloudBackend> {
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
        match cloud_config.create_backend_named(&backend_name).await {
            Ok(b) => {
                // Cache warmup: spawn a LIST chunks/ that seeds the
                // wrapper's cache with `Probed` entries. Clone shares
                // the same internal cache map so the warmup populates
                // the backend the registry holds. Non-blocking — a
                // LIST failure leaves the cache cold; next write does
                // a real HEAD/PUT.
                let warmup: Box<dyn CloudBackend> = b.clone();
                let warmup_name = backend_name.clone();
                tokio::spawn(async move {
                    match warmup.warmup_prefix("chunks/").await {
                        Ok(n) => tracing::info!(
                            "cloud cache warmup: seeded {} chunks/ keys for backend '{}'",
                            n,
                            warmup_name
                        ),
                        Err(e) => warn!(
                            "cloud cache warmup failed for backend '{}': {} (continuing with cold cache)",
                            warmup_name, e
                        ),
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

/// Open the cartridge against `cloud_backend` and read its legal-hold
/// sentinel. Returns `None` on cart-open failure (logged warn); a hold-read
/// failure is logged at debug and treated as "not held".
///
/// Single open per request: parallel chunk uploads run via stateless
/// `upload_chunk_inert` against the shared handle, then outcomes are
/// applied serially. Reopening per task tripped on `create_new(staging)`
/// for the trailing chunk that `resume_or_create_active` allocates
/// whenever all chunks are sealed (issue surfaced 2026-05-03 by
/// `test-backup-cloud.sh`).
async fn open_cart_and_hold_flag(
    tapes_root: &Path,
    tape_id: &str,
    cloud_backend: &dyn CloudBackend,
) -> Option<(Cartridge, bool)> {
    let cart = match Cartridge::open_with_cloud(
        tapes_root,
        tape_id,
        CartridgeOpenMode::Open,
        Some(cloud_backend.clone_box()),
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
    let backend_arc: Arc<dyn CloudBackend> = Arc::from(cloud_backend.clone_box());
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
    cloud_backend: &dyn CloudBackend,
    tape_id: &str,
    auto_hold: bool,
    disk_cache_evict_notify: &Arc<Notify>,
    outcome: ChunkUploadOutcome,
) {
    if auto_hold
        && let Err(e) = cloud_backend
            .set_object_legal_hold(&outcome.cloud_key, true)
            .await
    {
        warn!(
            "Auto-hold: failed to apply legal hold to chunk {} ({}) for {}: {}",
            outcome.item_id, outcome.cloud_key, tape_id, e
        );
    }
    disk_cache_evict_notify.notify_one();
}

/// Apply successful chunk-upload outcomes to the cartridge's chunk index
/// (O(1) pwrite per outcome via `apply_chunk_upload_outcome`) and back up
/// the manifest to cloud. If the cartridge is held, re-apply per-object
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
    cloud_backend: &dyn CloudBackend,
    tape_id: &str,
    auto_hold: bool,
) {
    if !outcomes.is_empty() {
        for outcome in outcomes {
            cart.apply_chunk_upload_outcome(outcome);
        }
    }

    match cart.backup_manifest_to_cloud().await {
        Ok(outcome) => {
            if auto_hold {
                // Body first (index pages + versioned backup), sentinel
                // last. Best-effort per key: a failure logs but does not
                // tear down the upload (data is already durable).
                for page_key in &outcome.index_page_keys {
                    if let Err(e) = cloud_backend.set_object_legal_hold(page_key, true).await {
                        warn!(
                            "Auto-hold: failed to apply legal hold to index page {} for {}: {}",
                            page_key, tape_id, e
                        );
                    }
                }
                if let Err(e) = cloud_backend
                    .set_object_legal_hold(&outcome.versioned_key, true)
                    .await
                {
                    warn!(
                        "Auto-hold: failed to apply legal hold to manifest backup {} for {}: {}",
                        outcome.versioned_key, tape_id, e
                    );
                }
                if let Err(e) = cloud_backend
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
// `vtl/scripts/test-backup-cloud.sh` end-to-end run.
