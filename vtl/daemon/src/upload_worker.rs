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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{Notify, Semaphore, mpsc};
use tracing::{debug, info, warn};

use core_mediachanger::{
    Cartridge, CartridgeOpenMode, CartridgeOpenOptions, ChunkUploadOutcome, ObjectStoreBackend,
    PendingUploadPayload,
};
use shared_object_store::ObjectStoreConfig;
use shared_upload_worker::run_upload_pipeline;

use crate::{Config, memory_buffer_manager, read_cartridge_backend};

type BackendRegistry = HashMap<String, Box<dyn ObjectStoreBackend>>;

/// Per-tape inbound queue depth. A request carries only a tape id + up
/// to `UPLOAD_MAX_BATCH_SIZE` chunk ids (tiny), so a deep queue is cheap.
/// When it fills the tape is genuinely backpressured (a slow/down
/// backend) — the dispatcher then drops the request rather than block,
/// and the periodic orphan sweep (#107) re-drives those chunks.
const TAPE_QUEUE_DEPTH: usize = 256;

/// A per-tape upload window holds one cartridge handle + one legal-hold
/// read open and coalesces every batch that arrives during it into a
/// single end-of-window manifest backup (issue #216). `WINDOW_LINGER`
/// keeps the window open briefly after the queue drains so a stuttery
/// stream still backs up once per burst; `WINDOW_MAX` caps the window so
/// a sustained stream's durable-backup lag (and the staleness of the
/// frozen chunk-index view) stays bounded — the cap also re-opens the
/// cartridge, picking up chunks the drive sealed mid-window.
const WINDOW_LINGER: Duration = Duration::from_millis(250);
const WINDOW_MAX: Duration = Duration::from_secs(5);

/// Dispatcher: fans upload requests out to one worker task per tape so
/// distinct tapes — even on the same backend — upload concurrently.
/// Before this, a single consumer awaited each request to completion, so
/// one slow/backpressured backend stalled upload progress (and converted
/// into front-end write backpressure) for *every* other drive (#216).
/// Per-backend PUT concurrency is still capped at `max_concurrent` by a
/// shared semaphore, so concurrency across tapes doesn't multiply the
/// in-flight (RAM-pinning) PUT count on any one backend.
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
    // Per-backend in-flight PUT ceiling, shared across every tape bound
    // to that backend so concurrent tapes honor the documented
    // `upload.max_concurrent` bound instead of multiplying it.
    let mut backend_sems: HashMap<String, Arc<Semaphore>> = HashMap::new();
    // Live per-tape task inbound channels + their join handles.
    let mut tape_tx: HashMap<String, mpsc::Sender<memory_buffer_manager::UploadRequest>> =
        HashMap::new();
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

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
    // any chunk that still fails surfaces immediately. A failed
    // chunk keeps `uploaded=false` in `chunks.idx`; nothing in the
    // live event flow re-drives it (the manager forgets chunks at
    // dispatch), so the periodic orphan sweep in `upload_recovery`
    // is the retry boundary (issue #107) — it re-queues
    // sealed-but-not-uploaded chunks every few minutes, and the
    // boot-time pass of the same sweep covers daemon restarts.
    info!(
        "Event-driven upload worker initialized (per-tape concurrent, max_concurrent={}/backend ({}), per-backend retry budget={})",
        max_concurrent, max_concurrent_source, upload_cfg.retry_max_attempts
    );

    let tapes_root = Path::new(&cfg.data_dir).join("tapes");

    while let Some(request) = upload_rx.recv().await {
        debug!(
            "Dispatch upload request for {}: {} chunks",
            request.tape_id,
            request.chunk_ids.len()
        );

        // Spawn a worker for this tape on first sight (or if a prior one
        // died). Resolving + initializing (+ warming) the sticky backend
        // happens once here, at spawn, not per request.
        let needs_spawn = tape_tx.get(&request.tape_id).is_none_or(|tx| tx.is_closed());
        if needs_spawn {
            let Some(backend_name) =
                ensure_backend(&request.tape_id, &cfg.storage, &mut registry, &tapes_root).await
            else {
                // Backend unreadable/uninitializable — this request's
                // chunks fall to the orphan sweep.
                continue;
            };
            let sem = backend_sems
                .entry(backend_name.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(max_concurrent.max(1))))
                .clone();
            let backend = registry
                .get(&backend_name)
                .expect("backend ensured above")
                .clone_box();
            let (tx, rx) = mpsc::channel(TAPE_QUEUE_DEPTH);
            handles.push(tokio::spawn(run_tape_upload_task(
                request.tape_id.clone(),
                rx,
                backend,
                sem,
                max_concurrent,
                disk_cache_evict_notify.clone(),
                tapes_root.clone(),
            )));
            tape_tx.insert(request.tape_id.clone(), tx);
        }

        // Route without blocking — a full per-tape queue means that tape
        // is backpressured; defer its chunks to the orphan sweep rather
        // than stall the dispatcher (and thus every other tape).
        let tx = tape_tx
            .get(&request.tape_id)
            .expect("present after spawn")
            .clone();
        if let Err(e) = tx.try_send(request) {
            match e {
                mpsc::error::TrySendError::Full(req) => warn!(
                    "Upload worker: tape {} queue full — deferring {} chunk(s) to the orphan sweep",
                    req.tape_id,
                    req.chunk_ids.len()
                ),
                mpsc::error::TrySendError::Closed(req) => {
                    // Worker died between the spawn check and the send;
                    // drop the stale sender so the next request respawns.
                    tape_tx.remove(&req.tape_id);
                    warn!(
                        "Upload worker: tape {} task closed — deferring {} chunk(s) to the orphan sweep",
                        req.tape_id,
                        req.chunk_ids.len()
                    );
                }
            }
        }
    }

    // Channel closed (daemon shutting down): drop every per-tape sender
    // so each task drains its queue, runs a final manifest backup, and
    // exits, then join them so shutdown waits for that last backup.
    drop(tape_tx);
    info!(
        "Upload worker dispatcher shutting down; draining {} per-tape task(s)",
        handles.len()
    );
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

/// One worker per tape. Processes requests in debounced *windows*: each
/// window opens the cartridge once (its chunk-index view frozen at open),
/// uploads every batch that arrives during the window, applies the
/// outcomes, and backs up the manifest **once** — instead of re-opening
/// the cartridge + reading the legal-hold sentinel + running a full
/// manifest backup (>=3 backend round trips, plus one retained manifest
/// object) per 8-chunk batch (issue #216). Chunks the drive seals
/// mid-window are simply picked up by the next window (fresh open) or the
/// orphan sweep — bounded lag, never lost.
async fn run_tape_upload_task(
    tape_id: String,
    mut rx: mpsc::Receiver<memory_buffer_manager::UploadRequest>,
    backend: Box<dyn ObjectStoreBackend>,
    backend_sem: Arc<Semaphore>,
    max_concurrent: usize,
    disk_cache_evict_notify: Arc<Notify>,
    tapes_root: PathBuf,
) {
    loop {
        // Block until there is work (or the dispatcher dropped our sender).
        let Some(first) = rx.recv().await else { break };

        let Some((mut cart, auto_hold)) =
            open_cart_and_hold_flag(&tapes_root, &tape_id, backend.as_ref()).await
        else {
            // Cart open failed: defer to the sweep, keep draining.
            continue;
        };

        let mut applied_any = process_request(
            &mut cart,
            &tape_id,
            auto_hold,
            backend.as_ref(),
            &backend_sem,
            max_concurrent,
            &disk_cache_evict_notify,
            first,
        )
        .await;

        // Coalesce the burst into this window until idle (past a short
        // linger) or WINDOW_MAX.
        let window_start = Instant::now();
        let mut closed = false;
        loop {
            if window_start.elapsed() >= WINDOW_MAX {
                break;
            }
            match tokio::time::timeout(WINDOW_LINGER, rx.recv()).await {
                Ok(Some(req)) => {
                    applied_any |= process_request(
                        &mut cart,
                        &tape_id,
                        auto_hold,
                        backend.as_ref(),
                        &backend_sem,
                        max_concurrent,
                        &disk_cache_evict_notify,
                        req,
                    )
                    .await;
                }
                Ok(None) => {
                    closed = true;
                    break;
                }
                Err(_) => break, // idle past the linger
            }
        }

        // One manifest backup for everything applied this window. Skip it
        // entirely when nothing was uploaded (a no-op batch — e.g. a sweep
        // re-drive of already-uploaded chunks) so an idle window doesn't
        // PUT a fresh manifest object for no reason.
        if applied_any {
            backup_manifest_with_holds(&mut cart, &tape_id, auto_hold, backend.as_ref()).await;
        }
        drop(cart);

        if closed {
            break;
        }
    }
    debug!("Upload worker: per-tape task for {} exiting", tape_id);
}

/// Snapshot one request's pending chunk payloads from the (window-owned)
/// cartridge, pipeline the PUTs (bounded per-backend by `backend_sem`),
/// and apply each successful outcome to the chunk index. Returns whether
/// any outcome was applied, so the caller can skip a no-op manifest
/// backup. The manifest backup itself is debounced to window end.
#[allow(clippy::too_many_arguments)]
async fn process_request(
    cart: &mut Cartridge,
    tape_id: &str,
    auto_hold: bool,
    backend: &dyn ObjectStoreBackend,
    backend_sem: &Arc<Semaphore>,
    max_concurrent: usize,
    disk_cache_evict_notify: &Arc<Notify>,
    request: memory_buffer_manager::UploadRequest,
) -> bool {
    // Skip ids that are already uploaded, unsealed, or missing from the
    // manifest — same effective filtering `upload_chunk_to_storage` did.
    let pending_payloads: Vec<PendingUploadPayload> = request
        .chunk_ids
        .iter()
        .filter_map(|&id| cart.pending_upload_payload(id as u64))
        .collect();

    if pending_payloads.is_empty() {
        debug!(
            "Upload worker: no pending uploads for {} (already uploaded or unsealed)",
            tape_id
        );
        return false;
    }

    let outcomes = run_upload_pipeline(
        backend,
        tape_id,
        pending_payloads,
        max_concurrent,
        Some(backend_sem.clone()),
        |outcome| vtl_post_upload_hook(backend, tape_id, auto_hold, disk_cache_evict_notify, outcome),
    )
    .await;

    // Cartridge is the sole writer to its own chunk index; keeping the
    // mutation here (not in the spawned PUT tasks) is what makes the
    // parallel-upload architecture safe.
    for outcome in &outcomes {
        cart.apply_chunk_upload_outcome(outcome);
    }
    !outcomes.is_empty()
}

/// Read the cartridge's sticky backend from its manifest and lazy-init
/// the named backend in the registry (a real network/auth round-trip for
/// S3/GCS/Azure, plus a one-shot cache warmup) if not already present.
/// Returns the backend name so the dispatcher can hand the per-tape
/// worker a `clone_box` of the shared handle and its per-backend
/// semaphore.
///
/// `None` on either path (missing manifest backend / failed init) — the
/// dispatcher skips this request and continues.
async fn ensure_backend(
    tape_id: &str,
    storage_config: &ObjectStoreConfig,
    registry: &mut BackendRegistry,
    tapes_root: &Path,
) -> Option<String> {
    let backend_name = match read_cartridge_backend(tapes_root, tape_id) {
        Some(name) => name,
        None => {
            warn!(
                "Upload worker: cannot read backend for cartridge {} - skipping upload",
                tape_id
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
                    backend_name, tape_id
                );
                return None;
            }
        }
    }

    Some(backend_name)
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
    // upload window (was: per batch — issue #216). If set, the window
    // re-applies the per-object hold to every freshly-PUT chunk and to
    // the new manifest backup objects so chunks written *after*
    // `legal-hold set` are not silently un-held; a hold set mid-window
    // is caught on the next window's open (and the explicit `legal-hold
    // set` path reapplies over existing objects regardless). A read
    // failure here (typically: sentinel does not exist
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

/// Back up the manifest to storage at the end of an upload window — once
/// for every batch coalesced into the window, not once per 8-chunk batch
/// (issue #216). The chunk-index outcomes were already applied (and are
/// durable on local disk) per batch in `process_request`; this ships the
/// dirty index pages + the versioned manifest + the `manifest-latest`
/// sentinel to storage for DR. If the cartridge is held, re-apply
/// per-object holds to the new index pages, the versioned backup, and the
/// `manifest-latest` sentinel — body→sentinel ordering matches the
/// explicit `legal-hold set` path.
async fn backup_manifest_with_holds(
    cart: &mut Cartridge,
    tape_id: &str,
    auto_hold: bool,
    storage_backend: &dyn ObjectStoreBackend,
) {
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
// guards. Tape-side glue (per-tape dispatch, window debounce, cartridge
// open, hook wiring, backup_manifest_with_holds) is covered by the
// `vtl/scripts/test-backup-storage.sh` end-to-end run.
