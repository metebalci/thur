// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Volume page-write pipeline: seal → enqueue → page-index.
//!
//! [`VolumeWriter`] is the per-page primitive the SBC-3 WRITE
//! handler calls once it has gathered a full `page_size_bytes` slab
//! (after any RMW carryover for partial-page writes through
//! [`crate::cache::PageCache`]).
//!
//! Two dispatch modes, picked by whether the daemon wired a
//! [`with_upload_sender`](VolumeWriter::with_upload_sender):
//!
//! - **Async (production)** — `write_page_unsynced` seals the bytes
//!   into the pool, marks the [`UploadIndexFile`] sidecar
//!   `LocalOnly`, sends an [`UploadTask`] over the mpsc channel,
//!   then bumps the page-index hash and returns. The daemon's
//!   upload worker drains the channel, runs
//!   `shared_upload_worker::upload_chunk_inert`, and calls
//!   [`VolumeWriter::apply_page_upload_outcome`] on completion —
//!   which flips the sidecar back to `Uploaded` and wakes any
//!   [`PageCache::synchronize_bytes`] waiter parked on that page
//!   range. The SBC-3 SYNCHRONIZE CACHE handler awaits the
//!   pending tracker so the host's `fsync(2)` still means
//!   "bytes are in cloud."
//! - **Inline (tests, CLI)** — no sender wired; `write_page_unsynced`
//!   runs `upload_chunk_inert` itself before returning. Same
//!   correctness contract as the pre-async era — every successful
//!   call returns with the cloud copy already durable.
//!
//! Crash semantics — async path: a crash with surviving
//! `LocalOnly` markers leaves chunks present in the pool but not
//! yet in cloud. The daemon's boot-time `scan_and_enqueue_localonly`
//! walks every volume's `upload.idx`, finds the survivors, and
//! re-enqueues them; the worker drains them indistinguishably from
//! live writes. PUTs are idempotent on every supported backend, so
//! the re-upload is safe even if the original PUT did land partial.
//! Inline path: same as before — pool insert is atomic, upload
//! either succeeds or returns an error, page-index pwrite is one
//! 64-byte record.

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use shared_object_store::{ObjectStoreBackend, ObjectStoreError};
use shared_pool::{BackpressureError, PoolBudget};
use shared_upload_worker::{PendingUpload, UploadOutcome, upload_chunk_inert};
use thiserror::Error;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::chunk_pool::{ChunkPool, ChunkPoolError};
use crate::lru_index::{LruIndexError, LruIndexFile};
use crate::page_index::{ChunkHash, PageId, PageIndex, PageIndexError};
use crate::runtime_state::VolumeRuntime;
use crate::upload_index::{UploadIndexError, UploadIndexFile, UploadState};
use crate::volume::{SyncAfter, VolumeError, VolumeManifest};
use shared_crypto::{CryptoError, KEY_LEN};

/// Default backpressure wait deadline used when a `VolumeWriter` is
/// built without a daemon-supplied budget. Matches `core-stream`'s
/// per-cartridge default — long enough that a brief eviction stall
/// completes inside one host-write retry, short enough that a wedged
/// uploader surfaces NOT READY rather than hanging the SCSI session.
pub const DEFAULT_BACKPRESSURE_DEADLINE: Duration = Duration::from_secs(30);

/// One unit of work for the daemon's async upload worker. Carries
/// the per-volume identity so the worker can route the outcome back
/// to the right `VolumeWriter` (via its volume registry) for
/// `apply_page_upload_outcome`.
///
/// Constructed inside [`VolumeWriter::write_page_unsynced`] when an
/// async upload sender is wired; the daemon's upload worker
/// (`vsa/daemon/src/upload_worker.rs`) consumes them off the mpsc
/// channel and drives them through
/// `shared_upload_worker::run_upload_pipeline`.
#[derive(Debug, Clone)]
pub struct UploadTask {
    /// Volume the task belongs to. Used by the worker to look up
    /// the right `VolumeWriter` in the daemon's volume registry so
    /// it can call `apply_page_upload_outcome` after the PUT.
    pub volume_name: String,
    /// Page-shaped upload payload (`PendingUpload::item_id` is the
    /// `page_id`).
    pub payload: PendingUpload,
}

/// In-flight upload tracker, owned by `VolumeWriter` in async mode.
/// Counts which `page_id`s have a pending PUT and lets SYNCHRONIZE
/// CACHE drain on the relevant range.
///
/// Single per-volume `tokio::sync::Notify` wakes every waiter on
/// any completion; waiters re-check the set under the lock and
/// keep waiting if their target range still has pending entries.
/// The pre-`enable()` sequencing in
/// [`PendingUploads::wait_for_range`] closes the
/// snapshot-vs-completion race window (Tokio Notify semantics
/// require subscribing before the relevant state check).
///
/// Cheap to clone — internally an `Arc`.
#[derive(Clone, Debug, Default)]
pub struct PendingUploads {
    inner: Arc<PendingUploadsInner>,
}

#[derive(Debug, Default)]
struct PendingUploadsInner {
    pending: Mutex<HashSet<PageId>>,
    notify: Notify,
}

impl PendingUploads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `page_id`'s upload as pending. Called by
    /// [`VolumeWriter::write_page_unsynced`] right before sending
    /// the task into the worker's channel, while the page-index
    /// hasn't yet been bumped.
    pub async fn mark_pending(&self, page_id: PageId) {
        self.inner.pending.lock().await.insert(page_id);
    }

    /// Mark `page_id`'s upload as done and wake every current
    /// waiter. Idempotent: removing a never-pending or
    /// already-cleared id is a no-op. The wake fires whether or not
    /// the entry was present — overlapping waiters covering a
    /// neighbouring range still benefit.
    pub async fn mark_done(&self, page_id: PageId) {
        self.inner.pending.lock().await.remove(&page_id);
        self.inner.notify.notify_waiters();
    }

    /// Block until no pending uploads remain for any `page_id` in
    /// `range`. Returns immediately if the range is already clean.
    /// Safe under racing completions: the `Notify` is enabled
    /// before the pending-set check so a completion in the window
    /// between the check and the await still wakes us.
    pub async fn wait_for_range(&self, range: RangeInclusive<PageId>) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            // Subscribe BEFORE the pending-set check — closes the
            // race where a completion between check and await
            // would otherwise be missed.
            notified.as_mut().enable();
            let any_pending = {
                let p = self.inner.pending.lock().await;
                p.iter().any(|pid| range.contains(pid))
            };
            if !any_pending {
                return;
            }
            notified.await;
        }
    }

    /// Snapshot of currently-pending page ids (for diagnostics /
    /// tests). Not used on the hot path.
    pub async fn snapshot(&self) -> HashSet<PageId> {
        self.inner.pending.lock().await.clone()
    }
}

/// Error type for the volume-write pipeline. Aggregates the
/// per-layer errors so callers can `?`-propagate without dragging
/// each underlying type into their signatures.
#[derive(Error, Debug)]
pub enum UploaderError {
    #[error("chunk pool: {0}")]
    ChunkPool(#[from] ChunkPoolError),

    #[error("page index: {0}")]
    PageIndex(#[from] PageIndexError),

    #[error("lru index: {0}")]
    LruIndex(#[from] LruIndexError),

    #[error("upload index: {0}")]
    UploadIndex(#[from] UploadIndexError),

    #[error("volume: {0}")]
    Volume(#[from] VolumeError),

    #[error("cloud: {0}")]
    Cloud(#[from] ObjectStoreError),

    #[error("upload-worker: {0}")]
    UploadWorker(#[from] shared_upload_worker::UploadInertError),

    #[error("upload-worker channel closed (daemon shutdown or worker crashed)")]
    UploadChannelClosed,

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("encrypt: {0}")]
    Encrypt(String),

    #[error("decrypt: {0}")]
    Decrypt(&'static str),

    #[error(
        "volume manifest says encryption: {algorithm} but the daemon \
         supplied no key (keystore lookup must succeed before \
         VolumeWriter::open)"
    )]
    MissingKey { algorithm: &'static str },

    #[error(
        "operator-supplied encryption key has wrong length: got {got} B, \
         expected {KEY_LEN} B (AES-256)"
    )]
    KeyWrongLength { got: usize },

    #[error(
        "page bytes ({len} B) do not match volume page size ({page_size} B); \
         partial-page writes are the SBC-3 layer's job"
    )]
    PageSizeMismatch { len: usize, page_size: u32 },

    #[error(
        "page id {page_id} is past the end of the volume \
         (size {size_bytes} B, page size {page_size} B)"
    )]
    PageOutOfRange {
        page_id: u64,
        page_size: u32,
        size_bytes: u64,
    },

    #[error(
        "cannot clone between volumes with mismatched page sizes \
         (src page_size = {src} B, dst page_size = {dst} B)"
    )]
    IncompatiblePageSize { src: u32, dst: u32 },

    #[error(
        "resize target {size_bytes} B is not a whole multiple of the \
         {sector_bytes} B sector size"
    )]
    ResizeNotSectorAligned { size_bytes: u64, sector_bytes: u32 },

    #[error(
        "resize target {requested} B does not grow the volume \
         (current size {current} B); shrink is not supported"
    )]
    ResizeNotGrow { current: u64, requested: u64 },

    #[error("invalid hash from chunk pool: {0}")]
    BadHash(String),

    /// Upload backpressure timed out: a page-seal would have pushed
    /// the local pool past its hard cap (or under
    /// `disk_cache.disk_free_min_gb`), and waiting on
    /// `upload.backpressure_max_wait_seconds` did not free enough
    /// headroom. Mapped at the SBC-3 layer to NOT READY +
    /// ASC/ASCQ 0x04/0x07 ("LOGICAL UNIT NOT READY, OPERATION IN
    /// PROGRESS"); backup software (Veeam, Bareos, restic, fs
    /// drivers) treats that as transient and retries.
    #[error("{0}")]
    Backpressured(#[from] BackpressureError),
}

/// Outcome of one [`VolumeWriter::write_page`] call.
#[derive(Debug, Clone)]
pub struct WritePageOutcome {
    pub page_id: PageId,
    pub hash_hex: String,
    /// `true` if the page bytes were already in the local pool
    /// (within-volume dedup hit). Informational; the cloud upload
    /// path makes its own decision via [`Self::cloud_dedup_hit`].
    pub local_dedup_hit: bool,
    /// `true` if the cloud upload was skipped because a HEAD probe
    /// found the object already present. Only ever `true` under
    /// [`DedupScope::Global`] — `Local`-scope cloud keys are
    /// volume-namespaced, so the HEAD is guaranteed to miss except
    /// under retry-after-crash, where an idempotent re-PUT is
    /// preferable to a race-prone HEAD-skip.
    pub cloud_dedup_hit: bool,
    /// On-wire bytes the upload actually transferred, when known
    /// at return time. `Some(n)` on the inline upload path (no
    /// async sender — `n` is `upload_chunk_inert`'s `put_bytes`,
    /// `None` when that inner call short-circuited on a cloud HEAD
    /// hit). `None` on the async path, where the worker bumps the
    /// per-volume `backend_bytes_written` meter itself after the
    /// PUT completes — the caller has no PUT size to report at
    /// this point in the flow.
    pub put_bytes: Option<u64>,
}

/// Bundle of (manifest + page index + pool + backend) needed to
/// service writes against a single volume. The backend is held by
/// `Arc` so the daemon can share one client across volumes that
/// happen to point at the same cloud entry. Single-writer per
/// volume is the contract — concurrent `write_page` calls against
/// the same `VolumeWriter` are not synchronized internally; the
/// SBC-3 session layer fences them upstream.
pub struct VolumeWriter {
    data_dir: PathBuf,
    manifest: VolumeManifest,
    runtime: VolumeRuntime,
    pool: ChunkPool,
    page_index: PageIndex,
    /// Per-volume LRU sidecar. Touched on every `write_page` /
    /// `read_page`; the daemon's eviction worker reads back the
    /// timestamps to sort eviction candidates oldest-first. Losing
    /// the file is acceptable (it gets rebuilt on next open as
    /// zeros — first eviction cycle then picks uniformly).
    lru_index: LruIndexFile,
    /// Per-volume upload-state sidecar. Set to `LocalOnly` at
    /// `write_page_unsynced` enqueue / inline-start; flipped to
    /// `Uploaded` once the worker (or the inline path) acks the
    /// PUT via [`Self::apply_page_upload_outcome`]. The
    /// `DiskCacheManager` eviction filter (commit 3 of the
    /// async-upload landing) consults this sidecar before evicting
    /// a pool chunk; until then every chunk stays evictable
    /// (steady-state LocalOnly window is short).
    upload_index: UploadIndexFile,
    backend: Arc<dyn ObjectStoreBackend>,
    /// AES-256 data key when this volume is encrypted at rest.
    /// Loaded from the daemon's keystore at
    /// `<data_dir>/keys/<uuid_hex>.key` and passed in via
    /// [`Self::open_with_key`]. Zeroized in `Drop`. `None` for
    /// unencrypted volumes — the encrypt/decrypt paths in
    /// `write_page` / `read_page` short-circuit on `None` so an
    /// existing v1 / unencrypted-v2 volume keeps its current shape.
    encryption_key: Option<[u8; KEY_LEN]>,
    /// Per-backend chunk-pool budget gate. The page-seal path
    /// reserves `payload.len()` against this budget before
    /// inserting into the pool; on dedup hit or upload failure the
    /// reservation is released. Defaulted to an unbounded budget so
    /// non-daemon callers (tests, future CLI tooling) can construct
    /// a writer without wiring real backpressure bookkeeping. The
    /// daemon supplies the real per-backend `Arc<PoolBudget>` via
    /// [`Self::with_pool_budget`] before sharing the writer through
    /// the `PageCache`.
    pool_budget: Arc<PoolBudget>,
    /// Max time `try_reserve` will park on backpressure before
    /// surfacing [`UploaderError::Backpressured`]. Mirrors
    /// `Cartridge::backpressure_deadline`. Defaulted to
    /// [`DEFAULT_BACKPRESSURE_DEADLINE`].
    backpressure_deadline: Duration,
    /// Async upload hand-off channel. `Some` when the daemon has
    /// wired in its upload worker (the common production path);
    /// `None` for tests / CLI tools that want the legacy
    /// "synchronous seal" behaviour where `write_page_unsynced`
    /// awaits the cloud PUT before returning. The daemon supplies
    /// this via [`Self::with_upload_sender`].
    upload_sender: Option<mpsc::Sender<UploadTask>>,
    /// In-flight upload tracker, populated in async mode only.
    /// `PageCache::synchronize_bytes` consults it to drain the
    /// upload queue inside SCSI SYNCHRONIZE CACHE so the host's
    /// fsync still means "bytes are in cloud."
    pending_uploads: PendingUploads,
    /// Hot-path lock-free cache of the volume's current
    /// [`SyncAfter`] tier. Initialised from `runtime.json` at
    /// `open_inner`; mutated by [`Self::set_sync_after`] (which
    /// also rewrites `runtime.json`). `PageCache::synchronize_bytes`
    /// loads it on every SYNC; the atomic costs <1 ns and avoids
    /// taking any lock on the hot path. Sharable via the field's
    /// `Arc<AtomicU8>` so a future API surface (admin handler
    /// dispatch, telemetry) can read the live value without
    /// holding the `VolumeWriter`.
    sync_after: Arc<AtomicU8>,
    /// Hot-path lock-free shadow of the volume's logical size. Seeded
    /// from `manifest.size_bytes` at `open_inner`; mutated by
    /// [`Self::set_size`] (which also rewrites `manifest.json`). This —
    /// not `self.manifest.size_bytes`, which stays the boot snapshot —
    /// is the live source of truth read by [`Self::size_bytes`] /
    /// [`Self::last_page_id`] and, through
    /// [`crate::cache::PageCache::size_bytes`], by READ CAPACITY,
    /// Identify Namespace, and every data-path range check. An online
    /// `volume resize` flips it so both transports see the new capacity
    /// without a daemon restart (issue #76).
    live_size_bytes: Arc<AtomicU64>,
    /// Per-backend ghost list of recently-evicted chunk hashes. When
    /// set, the cache-miss path in `read_page` consults it before
    /// each backend GET and records the eviction age into the
    /// `cache_miss_after_eviction` histogram. None for CLI / test
    /// paths; the daemon calls `set_ghost_list` after open.
    ghost_list: Option<Arc<shared_pool::GhostList>>,
    /// Latch: set the first time an `lru.idx` touch fails so the
    /// warning + disk-cache alert fire exactly once per volume rather
    /// than once per page write. The sidecar is a local cache hint
    /// (never uploaded); a persistent touch failure degrades eviction
    /// to first-seen ordering but is otherwise non-fatal — see
    /// [`Self::note_lru_touch_failed`].
    lru_touch_failed: AtomicBool,
}

impl Drop for VolumeWriter {
    fn drop(&mut self) {
        // Zeroize the in-memory key on close. Mirrors
        // `core-stream::encryption::DriveEncryptionState`'s zeroize on
        // cartridge unload — keeping the key out of memory after the
        // volume is no longer in use is cheap and useful.
        if let Some(key) = self.encryption_key.as_mut() {
            for b in key.iter_mut() {
                *b = 0;
            }
        }
    }
}

impl VolumeWriter {
    /// Open an existing volume against the given cloud backend.
    /// `backend` must already be authenticated and ready to upload
    /// — we do no additional validation here.
    ///
    /// Refuses to open an encrypted volume — use [`Self::open_with_key`]
    /// for those. The daemon side knows whether a key is needed by
    /// inspecting the manifest's `encryption` field before deciding
    /// which constructor to call.
    pub fn open(
        data_dir: &Path,
        name: &str,
        backend: Arc<dyn ObjectStoreBackend>,
    ) -> Result<Self, UploaderError> {
        Self::open_inner(data_dir, name, backend, None)
    }

    /// Open an encrypted volume with the operator-supplied
    /// 32-byte AES-256 key. The caller (daemon) is responsible for
    /// sourcing the key from the keystore and validating that the
    /// volume's manifest actually carries an `encryption` entry —
    /// `open_with_key` accepts the key on faith and short-circuits to
    /// the plaintext path if the manifest disagrees, surfacing as a
    /// `MissingKey` would be more confusing than ignoring the
    /// extraneous key.
    pub fn open_with_key(
        data_dir: &Path,
        name: &str,
        backend: Arc<dyn ObjectStoreBackend>,
        key: [u8; KEY_LEN],
    ) -> Result<Self, UploaderError> {
        Self::open_inner(data_dir, name, backend, Some(key))
    }

    fn open_inner(
        data_dir: &Path,
        name: &str,
        backend: Arc<dyn ObjectStoreBackend>,
        key: Option<[u8; KEY_LEN]>,
    ) -> Result<Self, UploaderError> {
        let manifest = VolumeManifest::load(data_dir, name)?;
        if manifest.encryption.is_some() && key.is_none() {
            return Err(UploaderError::MissingKey {
                algorithm: "aes_256_gcm",
            });
        }
        let vol_dir = VolumeManifest::dir_for(data_dir, name);
        let runtime = VolumeRuntime::load(&vol_dir)?;
        let pool = match manifest.pool_namespace() {
            Some(ns) => ChunkPool::new_namespaced(data_dir, &manifest.backend, &ns)?,
            None => ChunkPool::new(data_dir, &manifest.backend)?,
        };
        let idx_path = PageIndex::path_for(&vol_dir);
        let page_index = PageIndex::open(
            &idx_path,
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )?;
        let lru_index = LruIndexFile::open_or_create(&vol_dir)?;
        let upload_index = UploadIndexFile::open_or_create(&vol_dir)?;
        // Only retain the key if the manifest expects encryption.
        // An operator-supplied key against an unencrypted manifest
        // is a no-op, not an error — covers the "I passed a key but
        // the volume is plaintext" case without an explicit refusal.
        let encryption_key = if manifest.encryption.is_some() {
            key
        } else {
            None
        };
        let sync_after = Arc::new(AtomicU8::new(runtime.sync_after.as_u8()));
        let live_size_bytes = Arc::new(AtomicU64::new(manifest.size_bytes));
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            manifest,
            runtime,
            pool,
            page_index,
            lru_index,
            upload_index,
            backend,
            encryption_key,
            pool_budget: Arc::new(PoolBudget::unbounded(data_dir.to_path_buf())),
            backpressure_deadline: DEFAULT_BACKPRESSURE_DEADLINE,
            upload_sender: None,
            pending_uploads: PendingUploads::new(),
            sync_after,
            live_size_bytes,
            ghost_list: None,
            lru_touch_failed: AtomicBool::new(false),
        })
    }

    /// Borrow the LRU sidecar — the daemon's eviction worker walks
    /// it alongside `page_index` to sort eviction candidates
    /// oldest-first.
    pub fn lru_index(&self) -> &LruIndexFile {
        &self.lru_index
    }

    /// Record an `lru.idx` touch failure. The sidecar is a local cache
    /// hint that is never uploaded, so a touch failure is non-fatal:
    /// the eviction worker reads a missing/stale entry as 0 (= oldest).
    /// But a *persistent* failure (permissions / disk full / corruption)
    /// silently degrades eviction to first-seen ordering, and — left
    /// unrated — would log a warning on every page read and write. Latch
    /// on the first failure so the warning and the disk-cache alert each
    /// fire exactly once per volume.
    fn note_lru_touch_failed(&self, page_id: PageId, error: &LruIndexError) {
        if !latch_first_failure(&self.lru_touch_failed) {
            return; // Already warned + alerted for this volume.
        }
        tracing::warn!(
            page_id,
            volume = %self.manifest.name,
            "lru.idx touch failed (ignored; eviction degraded to first-seen): {}",
            error
        );
        shared_alerting::record::lru_index_degraded(&self.manifest.name, &error.to_string());
    }

    /// Wire in the daemon-managed per-backend pool budget. Bytes
    /// reserved by every `write_page` seal block on this gate when
    /// the backend's slice of the chunk pool is at its hard cap.
    /// Builder-style (consumes + returns `Self`) so the daemon can
    /// chain straight into `Arc::new(VolumeWriter::open(...)?.
    /// with_pool_budget(budget, deadline))` before sharing the
    /// writer through the `PageCache`.
    pub fn with_pool_budget(mut self, budget: Arc<PoolBudget>, deadline: Duration) -> Self {
        self.pool_budget = budget;
        self.backpressure_deadline = deadline;
        self
    }

    /// Wire the per-backend ghost list. The `read_page` miss site
    /// consults it on every backend GET to bucket eviction-to-refetch
    /// ages into the `cache_miss_after_eviction` histogram. Mirrors
    /// the `set_ghost_list` on `DiskCacheManager` — the same `Arc`
    /// flows through both so the read side reads what the eviction
    /// side wrote.
    pub fn with_ghost_list(mut self, gl: Arc<shared_pool::GhostList>) -> Self {
        self.ghost_list = Some(gl);
        self
    }

    /// Wire in the daemon's async upload-worker sender. Once set,
    /// every [`Self::write_page_unsynced`] call enqueues an
    /// [`UploadTask`] and returns without awaiting cloud — the
    /// worker drives the PUT in the background and calls
    /// [`Self::apply_page_upload_outcome`] when it's done. Without
    /// this builder (tests, CLI), the writer falls back to the
    /// inline path: `upload_chunk_inert` runs synchronously inside
    /// `write_page_unsynced`, matching the pre-async semantic.
    pub fn with_upload_sender(mut self, sender: mpsc::Sender<UploadTask>) -> Self {
        self.upload_sender = Some(sender);
        self
    }

    /// Borrow the per-volume in-flight upload tracker. The daemon's
    /// `PageCache::synchronize_bytes` calls
    /// `pending_uploads().wait_for_range(...)` to drain pending
    /// PUTs inside SCSI SYNCHRONIZE CACHE.
    pub fn pending_uploads(&self) -> &PendingUploads {
        &self.pending_uploads
    }

    /// Borrow the upload-state sidecar — exposed for the daemon's
    /// crash-recovery scan (walks every volume's `upload.idx` on
    /// boot, re-enqueues surviving `LocalOnly` pages) and the
    /// eviction worker (commit 3).
    pub fn upload_index(&self) -> &UploadIndexFile {
        &self.upload_index
    }

    /// Construct the [`PendingUpload`] payload for `page_id` — the
    /// shared upload pipeline's per-task input. Returns `None` if
    /// the page isn't currently allocated in `pages.idx` (nothing
    /// to upload). Called by the daemon's crash-recovery scan when
    /// re-enqueuing `LocalOnly` survivors found on boot.
    pub fn pending_upload_payload(
        &self,
        page_id: PageId,
    ) -> Result<Option<PendingUpload>, UploaderError> {
        let Some(hash) = self.page_index.get(page_id)? else {
            return Ok(None);
        };
        let hash_hex = hex::encode(hash);
        Ok(Some(PendingUpload {
            item_id: u64::from(page_id),
            object_key: self.pool.object_key(&hash_hex),
            local_path: self.pool.store_path(&hash_hex),
            hash: hash_hex,
            dedup: self.manifest.dedup_scope,
            backend_name: self.manifest.backend.clone(),
        }))
    }

    /// Apply a successful upload outcome to the per-volume upload
    /// sidecar: flip `LocalOnly → Uploaded` and clear the pending
    /// tracker entry so any `synchronize_bytes` waiter unblocks.
    /// Called by the daemon's upload worker after each successful
    /// PUT. The inline write path (no upload sender) calls this
    /// from `write_page_unsynced` itself before returning.
    pub async fn apply_page_upload_outcome(
        &self,
        outcome: &UploadOutcome,
    ) -> Result<(), UploaderError> {
        let page_id = match PageId::try_from(outcome.item_id) {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(
                    "apply_page_upload_outcome: outcome.item_id {} doesn't fit PageId - \
                     dropping (no sidecar mutation, no waiter wake)",
                    outcome.item_id
                );
                return Ok(());
            }
        };
        self.upload_index.set(page_id, UploadState::Uploaded)?;
        self.pending_uploads.mark_done(page_id).await;
        Ok(())
    }

    pub fn manifest(&self) -> &VolumeManifest {
        &self.manifest
    }

    pub fn runtime(&self) -> &VolumeRuntime {
        &self.runtime
    }

    /// Current SCSI SYNCHRONIZE CACHE durability tier. Lock-free
    /// read off the live atomic; updated by
    /// [`Self::set_sync_after`] when the operator runs
    /// `thurvsa volume modify --sync-after <MODE>`.
    pub fn sync_after(&self) -> SyncAfter {
        SyncAfter::from_u8(self.sync_after.load(Ordering::Relaxed))
    }

    /// Update the per-volume SYNC durability tier and rewrite
    /// `runtime.json` so the new value survives a daemon restart.
    /// New SYNC calls use the new mode; in-flight SYNCs finish
    /// under the mode that was active when they started (no
    /// preemption — the flush worker doesn't read this atomic).
    ///
    /// The contract change is **not signalled to the SCSI
    /// initiator** — a host fsync-heavy workload silently gains or
    /// loses durability on a flip; operators should pair flips with
    /// workload-level awareness.
    pub fn set_sync_after(&self, mode: SyncAfter) -> Result<(), UploaderError> {
        // Persist first so a crash between atomic-store and disk
        // write doesn't leave the live mode disagreeing with what
        // the recovery scan will see on the next boot.
        //
        // Load-merge-persist rather than rebuilding from `self.runtime`:
        // that field is the at-open snapshot, so rebuilding from it
        // would clobber every byte counter the live `PageCache` has
        // advanced since boot. Re-reading `runtime.json` keeps the
        // counters (as of the last flush / persist tick) and flips
        // only the durability tier.
        let vol_dir = VolumeManifest::dir_for(&self.data_dir, &self.manifest.name);
        let mut runtime = VolumeRuntime::load(&vol_dir)?;
        runtime.sync_after = mode;
        runtime.modified_at = chrono::Utc::now();
        runtime.persist(&vol_dir)?;
        self.sync_after.store(mode.as_u8(), Ordering::Relaxed);
        Ok(())
    }

    /// Live logical size of the volume in bytes. Lock-free read off the
    /// shadow atomic, which an online [`Self::set_size`] keeps current —
    /// so this, not `self.manifest().size_bytes` (the boot snapshot),
    /// is what READ CAPACITY / Identify Namespace / the data-path range
    /// checks must consult after a `volume resize` (issue #76).
    pub fn size_bytes(&self) -> u64 {
        self.live_size_bytes.load(Ordering::Relaxed)
    }

    /// Grow the volume's logical size and rewrite `manifest.json` so the
    /// new capacity survives a daemon restart. Grow-only: rejects a
    /// shrink or a no-op. The new size must be a whole multiple of the
    /// volume's sector size.
    ///
    /// Grow is metadata-only — the page table is sparse, so pages past
    /// the old end already read as zero and the data path admits I/O
    /// into the grown region the moment the shadow flips. Persist the
    /// manifest first, then flip the atomic: a crash in between leaves
    /// disk = new, live = old, and the next boot reseeds the shadow from
    /// the persisted manifest, converging on the new size. The reverse
    /// ordering could advertise capacity that isn't durable.
    pub fn set_size(&self, new_size: u64) -> Result<(), UploaderError> {
        let sector = u64::from(self.manifest.sector_bytes);
        if sector == 0 || !new_size.is_multiple_of(sector) {
            return Err(UploaderError::ResizeNotSectorAligned {
                size_bytes: new_size,
                sector_bytes: self.manifest.sector_bytes,
            });
        }
        let current = self.size_bytes();
        if new_size <= current {
            return Err(UploaderError::ResizeNotGrow {
                current,
                requested: new_size,
            });
        }
        // Clone the boot snapshot and flip just the size — unlike
        // `runtime.json`, `manifest.json` has no hot-path writer to
        // merge against (it's frozen post-create), so there is nothing
        // to reload.
        let vol_dir = VolumeManifest::dir_for(&self.data_dir, &self.manifest.name);
        let mut m = self.manifest.clone();
        m.size_bytes = new_size;
        m.persist(&vol_dir)?;
        self.live_size_bytes.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    pub fn pool(&self) -> &ChunkPool {
        &self.pool
    }

    pub fn page_index(&self) -> &PageIndex {
        &self.page_index
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Largest legal `page_id` for this volume, inclusive. A
    /// volume of `size_bytes` with `page_size_bytes` pages has
    /// `size_bytes / page_size_bytes` pages addressed `0 ..
    /// page_count`. `page_count - 1` is the last legal id.
    pub fn last_page_id(&self) -> u64 {
        // Live size, not the boot snapshot: a grown volume must admit
        // writes into the new page range (issue #76).
        let count = self.size_bytes() / u64::from(self.manifest.page_size_bytes);
        count.saturating_sub(1)
    }

    /// Seal one page's bytes through to durable storage. Strict
    /// "returns means durable" — the page-index record is fsync'd
    /// before the call returns. The cache's parallel-flush drain
    /// uses [`Self::write_page_unsynced`] + a single trailing
    /// [`Self::page_index_sync`] instead.
    ///
    /// Steps:
    /// 1. BLAKE3-hash the page bytes (delegated to [`ChunkPool::insert_bytes`]).
    /// 2. Insert into the local chunk pool — atomic; no-op if the
    ///    hash was already there.
    /// 3. Upload to the cloud backend. Under `Global` scope a HEAD
    ///    probe gates the upload so cross-volume dedup hits skip
    ///    the PUT.
    /// 4. Record `(page_id, hash)` in the page index (pwrite + fsync).
    ///
    /// `bytes.len()` must equal the volume's `page_size_bytes`; the
    /// SBC-3 write handler is responsible for assembling full
    /// pages from sub-page host writes (RMW).
    pub async fn write_page(
        &self,
        page_id: PageId,
        bytes: &[u8],
    ) -> Result<WritePageOutcome, UploaderError> {
        let outcome = self.write_page_unsynced(page_id, bytes).await?;
        self.page_index.sync()?;
        Ok(outcome)
    }

    /// Same as [`Self::write_page`] but leaves the page-index record
    /// unsynced — the pwrite goes through to the OS page cache;
    /// durability requires a later [`Self::page_index_sync`]. Used
    /// by the cache's parallel-flush drain so an N-page cohort pays
    /// one `fdatasync` instead of N redundant ones. Crash semantics
    /// match the SCSI write-back contract: bytes that the host has
    /// not yet SYNCHRONIZE-CACHE'd can be lost on power loss.
    pub async fn write_page_unsynced(
        &self,
        page_id: PageId,
        bytes: &[u8],
    ) -> Result<WritePageOutcome, UploaderError> {
        if bytes.len() != self.manifest.page_size_bytes as usize {
            return Err(UploaderError::PageSizeMismatch {
                len: bytes.len(),
                page_size: self.manifest.page_size_bytes,
            });
        }
        if u64::from(page_id) > self.last_page_id() {
            return Err(UploaderError::PageOutOfRange {
                page_id: u64::from(page_id),
                page_size: self.manifest.page_size_bytes,
                size_bytes: self.size_bytes(),
            });
        }

        // Encrypt-on-write: AES-256-GCM with per-page IV derived from
        // (volume_uuid, page_id, 0). For encrypted volumes the BLAKE3
        // hash that follows runs over ciphertext, which means two
        // encrypted volumes with the same plaintext never collide in
        // the chunk pool — a feature, not a bug (cross-volume sharing
        // of encrypted data would defeat the encryption boundary).
        let ciphertext;
        let payload: &[u8] = if let Some(key) = self.encryption_key.as_ref() {
            let iv = shared_crypto::derive_iv(&self.manifest.uuid, u64::from(page_id), 0);
            ciphertext = shared_crypto::encrypt_block(key, &iv, bytes)
                .map_err(|e: CryptoError| UploaderError::Encrypt(e.to_string()))?;
            &ciphertext
        } else {
            bytes
        };

        // Backpressure gate: reserve `payload.len()` against the
        // backend's pool budget *before* inserting into the pool.
        // If the backend's slice is at its hard cap and the eviction
        // worker can't free space within `backpressure_deadline`,
        // surface `Backpressured` (mapped to SBC-3 NOT READY 0x04/0x07
        // at the data-path layer; host backup software retries).
        // Mirrors `core_stream::cartridge::chunking::seal_current_chunk`.
        let reserved_bytes = payload.len() as u64;
        let namespace = self.manifest.pool_namespace();
        self.pool_budget.try_reserve(
            reserved_bytes,
            namespace.as_deref(),
            self.backpressure_deadline,
        )?;

        let insert_result = self.pool.insert_bytes(payload);
        let (hash_hex, was_new) = match insert_result {
            Ok(v) => v,
            Err(e) => {
                // Pool insert failed before any bytes hit disk —
                // release the quota we reserved.
                self.pool_budget
                    .release(reserved_bytes, namespace.as_deref());
                return Err(e.into());
            }
        };
        if !was_new {
            // Local dedup hit: the chunk was already in the pool, so
            // our reservation never consumed new disk — release it
            // before continuing so the budget reflects reality.
            //
            // This is safe even though eviction / a CloudOnly state
            // can leave a chunk's bytes absent while the page index
            // still references the hash: `insert_bytes` guarantees on
            // `Ok` that `store_path(hash)` is present on disk
            // regardless of `was_new` (a `false` means it found the
            // file already there). The `was_new` flag reflects
            // present-or-not *at insert time*, not a stale view — so
            // the dedup-hit chunk we're releasing the reservation for
            // really is on disk right now, and the payload built below
            // points the worker at a live file.
            self.pool_budget
                .release(reserved_bytes, namespace.as_deref());
        }
        let hash_bytes = decode_hash(&hash_hex)?;

        // Record the page->chunk hash in `pages.idx` *before* any
        // state a SYNCHRONIZE CACHE waiter can observe — the
        // `LocalOnly` flag, the pending-upload marker, and the
        // worker's eventual `Uploaded` flip. The worker runs on a
        // separate task and can drain the PUT, flip the page to
        // `Uploaded`, and wake a `synchronize_bytes` waiter the
        // instant we hand off below; if the hash write trailed the
        // hand-off, the host's `fsync` could settle to "durable"
        // while `pages.idx` still had no hash for the page, and a
        // crash in that window would lose the page->chunk mapping
        // even though the chunk is already in the pool. The write
        // only buffers (pwrite to the OS page cache); the trailing
        // `page_index_sync` in the flush drain — or a concurrent
        // SYNC's own `page_index_sync` — fsyncs it.
        self.page_index.set_unsynced(page_id, &hash_bytes)?;

        // Build the shared upload payload once — both the async and
        // inline dispatch paths use it. `object_key` derivation is
        // namespace-aware (per `DedupScope`).
        let payload_obj = PendingUpload {
            item_id: u64::from(page_id),
            hash: hash_hex.clone(),
            local_path: self.pool.store_path(&hash_hex),
            object_key: self.pool.object_key(&hash_hex),
            dedup: self.manifest.dedup_scope,
            backend_name: self.manifest.backend.clone(),
        };

        // Flag the sidecar as LocalOnly *before* the hand-off so a
        // crash between here and the worker's completion leaves a
        // recoverable record (the boot-time recovery scan finds it
        // and re-enqueues).
        self.upload_index.set(page_id, UploadState::LocalOnly)?;

        // The async-vs-inline branch: with a sender, hand off and
        // return — the worker drives `upload_chunk_inert` and calls
        // `apply_page_upload_outcome` on completion. Without one
        // (tests, CLI), run the upload inline so the legacy
        // "returns means cloud-durable" semantic stands.
        //
        // `inline_put_bytes` is the on-wire PUT size for the inline
        // path; the async path leaves it `None` because the worker
        // handles its own `backend_bytes_written` bump after the PUT.
        let cloud_dedup_hit;
        let inline_put_bytes: Option<u64>;
        if let Some(sender) = &self.upload_sender {
            self.pending_uploads.mark_pending(page_id).await;
            let task = UploadTask {
                volume_name: self.manifest.name.clone(),
                payload: payload_obj,
            };
            if sender.send(task).await.is_err() {
                // Worker exited; roll back the pending marker and
                // upload-state so the caller sees a clean failure
                // and the eviction filter doesn't think the chunk
                // is still LocalOnly. Bytes are still in the pool;
                // a daemon restart will re-discover and re-enqueue.
                self.pending_uploads.mark_done(page_id).await;
                self.upload_index.set(page_id, UploadState::Uploaded)?;
                return Err(UploaderError::UploadChannelClosed);
            }
            // We don't know yet whether the worker's HEAD probe
            // will hit; report it as miss for now. Outcome wired
            // through telemetry on the worker side.
            cloud_dedup_hit = false;
            inline_put_bytes = None;
        } else {
            // Inline path — used by tests and any future CLI tool
            // that wants strict synchronous semantics. Same
            // dispatch logic as the worker: `upload_chunk_inert`
            // does the HEAD probe (Global only) + PUT, then we
            // flip the sidecar back to Uploaded.
            let outcome = upload_chunk_inert(&*self.backend, &payload_obj).await?;
            cloud_dedup_hit = outcome.dedup_hit;
            // Forward `put_bytes` up to the caller so it can bump
            // `PageCache::backend_bytes_written` — the async worker
            // does this itself, but the inline path has no `cache`
            // handle and must surface it via the return value.
            inline_put_bytes = outcome.put_bytes;
            self.apply_page_upload_outcome(&outcome).await?;
        }

        // LRU sidecar touch — local cache hint, never uploaded.
        // Failure is non-fatal: the eviction worker tolerates a
        // missing entry (reads as 0 = oldest).
        if let Err(e) = self.lru_index.touch(page_id, now_unix_secs()) {
            self.note_lru_touch_failed(page_id, &e);
        }

        tracing::debug!(
            page_id = page_id,
            hash = &hash_hex[..16.min(hash_hex.len())],
            local_dedup_hit = !was_new,
            cloud_dedup_hit,
            async_dispatch = self.upload_sender.is_some(),
            "thurvsa page write sealed (unsynced)"
        );

        Ok(WritePageOutcome {
            page_id,
            hash_hex,
            local_dedup_hit: !was_new,
            cloud_dedup_hit,
            put_bytes: inline_put_bytes,
        })
    }

    /// Flush every prior [`Self::write_page_unsynced`] to disk.
    /// Returns once `fdatasync(2)` on the page index has completed.
    pub fn page_index_sync(&self) -> Result<(), UploaderError> {
        self.page_index.sync()?;
        Ok(())
    }

    /// Read a page's bytes back. Returns `Ok(None)` if `page_id`
    /// is unallocated (sparse hole or never written). Tries the
    /// local pool first; on miss, downloads from cloud, verifies the
    /// bytes hash to the expected page hash, and warms the local
    /// pool before returning.
    ///
    /// BLAKE3 verify via `ChunkPool::insert_verified_bytes` catches
    /// cloud bit-rot / wrong-bytes-for-hash; mismatch surfaces as
    /// `UploaderError::ChunkPool(ChunkPoolError::HashMismatch)` and
    /// the SBC-3 layer maps that to MEDIUM ERROR + UNRECOVERED READ
    /// ERROR (0x03 / 0x11 / 0x00).
    /// Atomically rewrite this volume's `runtime.json` from the
    /// supplied [`VolumeRuntime`] snapshot — the byte counters plus a
    /// refreshed `modified_at`. Identity (`manifest.json`) is
    /// creation-frozen and never touched here. Called by `PageCache`
    /// at flush boundaries and by the daemon's periodic persist timer,
    /// so a restart picks up roughly the counters the in-memory
    /// atomics were tracking.
    pub fn persist_runtime(&self, runtime: &VolumeRuntime) -> Result<(), UploaderError> {
        let vol_dir = VolumeManifest::dir_for(&self.data_dir, &self.manifest.name);
        runtime.persist(&vol_dir)?;
        Ok(())
    }

    /// Read one page back. The `u64` in the success tuple is the
    /// cloud-fetched byte count: `0` on a local-pool hit, the
    /// downloaded length on a cache miss — the caller folds it into
    /// `PageCache`'s `backend_bytes_read` meter.
    pub async fn read_page(
        &self,
        page_id: PageId,
    ) -> Result<Option<(Vec<u8>, u64)>, UploaderError> {
        let Some(hash) = self.page_index.get(page_id)? else {
            return Ok(None);
        };
        // LRU sidecar touch — fires on every read of an allocated
        // page so the eviction worker can sort by genuine recency,
        // not by write time alone. Non-fatal if it fails.
        if let Err(e) = self.lru_index.touch(page_id, now_unix_secs()) {
            self.note_lru_touch_failed(page_id, &e);
        }
        let hash_hex = hex::encode(hash);
        // `cloud_bytes` is the cache-miss download size — 0 on a
        // local-pool hit, the fetched length on a cloud miss.
        let (payload, cloud_bytes) = if self.pool.exists(&hash_hex) {
            (self.pool.read_bytes(&hash_hex)?, 0u64)
        } else {
            if let Some(gl) = self.ghost_list.as_ref() {
                if let Some(age) = gl.lookup(&hash, now_unix_secs()) {
                    shared_telemetry::record::cache_miss_after_eviction(gl.backend(), age as f64);
                }
            }
            let object_key = self.pool.object_key(&hash_hex);
            let bytes = self.backend.download_chunk(&object_key).await?;
            let cloud_bytes = bytes.len() as u64;
            // Cache-miss refetch grows the local pool — account it
            // against the budget so `current_bytes()` stays equal to
            // on-disk pool bytes (the eviction worker reads the budget
            // instead of rescanning). Insert FIRST, then reserve only
            // when the insert actually wrote the file (`was_new`): a
            // chunk already resident — e.g. warmed by a concurrent
            // refetch of the same page while this read's `exists()` probe
            // and download were in flight — reports `was_new == false`,
            // so it is never double-counted. A failed insert (hash
            // mismatch / IO) returns early via `?` with no reservation
            // made. `force_reserve`, not `try_reserve`: a host READ must
            // never block on backpressure — the bytes already left the
            // backend and the page must be served.
            let was_new = self.pool.insert_verified_bytes(&hash_hex, &bytes)?;
            if was_new {
                self.pool_budget
                    .force_reserve(cloud_bytes, self.manifest.pool_namespace().as_deref());
            }
            (bytes, cloud_bytes)
        };
        // Decrypt-on-read: for encrypted volumes the bytes we just
        // pulled from pool / cloud are ciphertext-plus-tag and must
        // be peeled back to plaintext before returning to the SCSI
        // READ handler. IV is re-derived from the same identity
        // tuple used at write time, no per-chunk metadata required.
        if let Some(key) = self.encryption_key.as_ref() {
            let iv = shared_crypto::derive_iv(&self.manifest.uuid, u64::from(page_id), 0);
            let plaintext =
                shared_crypto::decrypt_block(key, &iv, &payload).map_err(|e| match e {
                    CryptoError::Decrypt(msg) => UploaderError::Decrypt(msg),
                    CryptoError::Input(_) => UploaderError::Decrypt("invalid decrypt input"),
                    CryptoError::Encrypt => UploaderError::Decrypt("encrypt error during decrypt"),
                })?;
            Ok(Some((plaintext, cloud_bytes)))
        } else {
            Ok(Some((payload, cloud_bytes)))
        }
    }
}

fn now_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Latch helper for once-per-volume failure reporting: returns `true`
/// on the first call (the caller should warn + alert), `false`
/// thereafter, leaving `flag` set. Pulled out of
/// [`VolumeWriter::note_lru_touch_failed`] so the gating is unit-
/// testable without injecting a live filesystem failure into the touch
/// path.
fn latch_first_failure(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

fn decode_hash(hash_hex: &str) -> Result<ChunkHash, UploaderError> {
    let raw = hex::decode(hash_hex)
        .map_err(|_| UploaderError::BadHash(format!("not hex: {hash_hex}")))?;
    if raw.len() != 32 {
        return Err(UploaderError::BadHash(format!(
            "want 32 bytes, got {} bytes ({hash_hex})",
            raw.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&raw);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES, DedupScope};
    use shared_object_store::LocalBackend;
    use tempfile::TempDir;

    #[test]
    fn lru_touch_latch_fires_once_then_suppresses() {
        // Models the once-per-volume gating in `note_lru_touch_failed`:
        // the first failure reports (warn + disk-cache alert), every
        // subsequent failure is suppressed even though the flag stays
        // latched.
        let flag = AtomicBool::new(false);
        assert!(latch_first_failure(&flag), "first failure must report");
        assert!(
            !latch_first_failure(&flag),
            "second failure must be suppressed"
        );
        assert!(
            !latch_first_failure(&flag),
            "later failures stay suppressed"
        );
    }

    /// Stand up a 4 MiB volume with the given dedup scope and a
    /// LocalBackend rooted at `<tmp>/cloud`. Returns
    /// (data_dir, volume_name, backend).
    async fn fixture(scope: DedupScope) -> (TempDir, String, Arc<dyn ObjectStoreBackend>) {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let cloud_root = data_dir.join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

        let name = "vol1".to_string();
        VolumeManifest::new(
            name.clone(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            scope,
            false,
            0,
        )
        .unwrap()
        .create(&data_dir)
        .unwrap();

        (tmp, name, backend)
    }

    fn page_bytes(seed: u8) -> Vec<u8> {
        let mut v = vec![0u8; DEFAULT_PAGE_SIZE_BYTES as usize];
        for (i, b) in v.iter_mut().enumerate() {
            *b = seed.wrapping_add((i & 0xFF) as u8);
        }
        v
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend.clone()).unwrap();
        let bytes = page_bytes(0xAB);

        let outcome = writer.write_page(7, &bytes).await.unwrap();
        assert!(!outcome.local_dedup_hit);
        assert!(!outcome.cloud_dedup_hit);
        assert_eq!(outcome.hash_hex.len(), 64);

        // Read straight after write: the chunk is in the local pool,
        // so no cloud fetch — the byte count is 0.
        let read_back = writer.read_page(7).await.unwrap();
        assert_eq!(read_back, Some((bytes.clone(), 0)));

        // Index records it, pool has it, cloud has it.
        assert!(writer.page_index().get(7).unwrap().is_some());
        assert!(writer.pool().exists(&outcome.hash_hex));
        let object_key = writer.pool().object_key(&outcome.hash_hex);
        assert!(backend.chunk_exists(&object_key).await.unwrap());
    }

    /// Read-miss budget accounting (#49): a cache-miss refetch must
    /// `force_reserve` the warmed bytes so the per-backend `PoolBudget`
    /// returns to its pre-eviction value, and a second read of the
    /// now-resident chunk must NOT double-count. Global scope keeps the
    /// budget namespace `None` so the assertions are exact.
    #[tokio::test]
    async fn read_miss_refetch_reserves_budget_exactly_once() {
        let (tmp, name, backend) = fixture(DedupScope::Global).await;
        let budget = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 0, 0, 80));
        let writer = VolumeWriter::open(tmp.path(), &name, backend.clone())
            .unwrap()
            .with_pool_budget(budget.clone(), Duration::from_secs(5));
        let bytes = page_bytes(0x5A);

        let outcome = writer.write_page(7, &bytes).await.unwrap();
        let seal_bytes = budget.current_bytes();
        assert!(seal_bytes > 0, "seal must reserve the chunk bytes");
        assert!(writer.pool().exists(&outcome.hash_hex));

        // Simulate the eviction worker dropping the local pool file:
        // remove + release, exactly the (size, namespace=None) pairing
        // `evict_lru_chunks` uses for a Global-scope chunk. Cloud copy
        // stays, so the next read is a genuine local-pool miss.
        writer.pool().remove(&outcome.hash_hex).unwrap();
        budget.release(seal_bytes, None);
        assert_eq!(budget.current_bytes(), 0);
        assert!(!writer.pool().exists(&outcome.hash_hex));

        // Read → cache miss → download from cloud → re-warm the pool.
        // The refetch must re-reserve exactly the chunk bytes.
        let (got, cloud_bytes) = writer.read_page(7).await.unwrap().unwrap();
        assert_eq!(got, bytes);
        assert!(cloud_bytes > 0, "a miss must report a cloud fetch");
        assert_eq!(
            budget.current_bytes(),
            seal_bytes,
            "refetch must re-reserve exactly the chunk bytes"
        );

        // Second read: the chunk is resident again → local hit → no
        // reserve. The budget must not double-count.
        let (got2, cloud_bytes2) = writer.read_page(7).await.unwrap().unwrap();
        assert_eq!(got2, bytes);
        assert_eq!(cloud_bytes2, 0, "second read is a local-pool hit");
        assert_eq!(
            budget.current_bytes(),
            seal_bytes,
            "a resident-chunk read must not double-count the budget"
        );
    }

    /// Durability-ordering regression: on the async-dispatch path the
    /// page->chunk hash must land in `pages.idx` *before* the upload is
    /// handed to the worker, so a worker that flips the page to
    /// `Uploaded` and wakes a `synchronize_bytes` waiter can never let
    /// the host see "durable" while the index still has no hash. Attach
    /// a sender and never drain it — the worker never runs, yet the
    /// hash is already recorded when `write_page_unsynced` returns.
    #[tokio::test]
    async fn async_dispatch_records_hash_before_handoff() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        // Keep `_rx` bound so the channel stays open (a dropped
        // receiver would send-fail into the rollback path).
        let (tx, _rx) = mpsc::channel::<UploadTask>(8);
        let writer = VolumeWriter::open(tmp.path(), &name, backend)
            .unwrap()
            .with_upload_sender(tx);
        let bytes = page_bytes(0x5A);

        let outcome = writer.write_page_unsynced(3, &bytes).await.unwrap();

        let recorded = writer.page_index().get(3).unwrap();
        assert!(
            recorded.is_some(),
            "page->chunk hash must be in pages.idx before the upload hand-off, \
             even though the worker has not run"
        );
        assert_eq!(hex::encode(recorded.unwrap()), outcome.hash_hex);
    }

    #[tokio::test]
    async fn read_unallocated_page_returns_none() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend).unwrap();
        let read_back = writer.read_page(0).await.unwrap();
        assert_eq!(read_back, None);
    }

    #[tokio::test]
    async fn rewriting_same_bytes_dedups_locally() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend).unwrap();
        let bytes = page_bytes(0x11);

        let first = writer.write_page(0, &bytes).await.unwrap();
        assert!(!first.local_dedup_hit);
        let second = writer.write_page(1, &bytes).await.unwrap();
        assert!(second.local_dedup_hit);
        assert_eq!(first.hash_hex, second.hash_hex);

        // Pool holds exactly one chunk despite two pages pointing
        // at the same hash.
        assert_eq!(writer.pool().iter_chunks().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn overwriting_a_page_replaces_the_index_entry() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend).unwrap();

        let a = page_bytes(0x01);
        let b = page_bytes(0x02);
        let r1 = writer.write_page(3, &a).await.unwrap();
        let r2 = writer.write_page(3, &b).await.unwrap();
        assert_ne!(r1.hash_hex, r2.hash_hex);

        // Page id 3 now resolves to the second hash.
        let read_back = writer.read_page(3).await.unwrap();
        assert_eq!(read_back, Some((b, 0)));

        // Both chunks linger in the pool — GC reclaims orphans.
        let chunks: Vec<_> = writer
            .pool()
            .iter_chunks()
            .unwrap()
            .into_iter()
            .map(|(h, _)| h)
            .collect();
        assert!(chunks.contains(&r1.hash_hex));
        assert!(chunks.contains(&r2.hash_hex));
    }

    #[tokio::test]
    async fn global_scope_skips_upload_on_cloud_hit() {
        let (tmp, name, backend) = fixture(DedupScope::Global).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend.clone()).unwrap();
        let bytes = page_bytes(0x77);

        let first = writer.write_page(0, &bytes).await.unwrap();
        assert!(!first.cloud_dedup_hit, "first write must upload");

        // Second write of the same bytes to a different page id —
        // the cloud HEAD probe should hit and skip the PUT.
        let second = writer.write_page(1, &bytes).await.unwrap();
        assert!(second.cloud_dedup_hit, "second write should HEAD-hit");
    }

    #[tokio::test]
    async fn local_scope_does_not_head_probe_cloud() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend).unwrap();
        let bytes = page_bytes(0x33);

        let first = writer.write_page(0, &bytes).await.unwrap();
        let second = writer.write_page(1, &bytes).await.unwrap();
        // Local scope never reports a cloud HEAD hit even though the
        // bytes are identical — re-PUTs are cheap and idempotent.
        assert!(!first.cloud_dedup_hit);
        assert!(!second.cloud_dedup_hit);
    }

    #[tokio::test]
    async fn rejects_wrong_size_buffer() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend).unwrap();
        let err = writer.write_page(0, b"too short").await.unwrap_err();
        assert!(matches!(err, UploaderError::PageSizeMismatch { .. }));
    }

    #[tokio::test]
    async fn rejects_page_past_end_of_volume() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend).unwrap();
        // 4 MiB volume / 64 KiB page = 64 pages addressed 0..63.
        assert_eq!(writer.last_page_id(), 63);
        let bytes = page_bytes(0);
        let err = writer.write_page(64, &bytes).await.unwrap_err();
        assert!(matches!(err, UploaderError::PageOutOfRange { .. }));
    }

    #[tokio::test]
    async fn set_size_grows_admits_new_range_and_persists() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend.clone()).unwrap();
        // 4 MiB / 64 KiB = 64 pages (0..63). Page 64 is past the end.
        assert_eq!(writer.size_bytes(), 4 * (1u64 << 20));
        assert_eq!(writer.last_page_id(), 63);
        let bytes = page_bytes(0x33);
        assert!(matches!(
            writer.write_page(64, &bytes).await.unwrap_err(),
            UploaderError::PageOutOfRange { .. }
        ));

        // Grow to 8 MiB: live size + last_page_id move immediately, the
        // previously-rejected page is now admitted, and a write past the
        // *new* end is still rejected.
        writer.set_size(8 * (1u64 << 20)).unwrap();
        assert_eq!(writer.size_bytes(), 8 * (1u64 << 20));
        assert_eq!(writer.last_page_id(), 127);
        writer.write_page(64, &bytes).await.unwrap();
        assert_eq!(writer.read_page(64).await.unwrap(), Some((bytes, 0)));
        assert!(matches!(
            writer.write_page(128, &page_bytes(0)).await.unwrap_err(),
            UploaderError::PageOutOfRange { .. }
        ));

        // Persisted to manifest.json: a fresh open boots at the new size.
        assert_eq!(
            VolumeManifest::load(tmp.path(), &name).unwrap().size_bytes,
            8 * (1u64 << 20)
        );
        let reopened = VolumeWriter::open(tmp.path(), &name, backend).unwrap();
        assert_eq!(reopened.size_bytes(), 8 * (1u64 << 20));
        assert_eq!(reopened.last_page_id(), 127);
    }

    #[tokio::test]
    async fn set_size_rejects_shrink_noop_and_unaligned() {
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open(tmp.path(), &name, backend).unwrap();
        let orig = 4 * (1u64 << 20);

        assert!(matches!(
            writer.set_size(2 * (1u64 << 20)).unwrap_err(),
            UploaderError::ResizeNotGrow { .. }
        ));
        assert!(matches!(
            writer.set_size(orig).unwrap_err(),
            UploaderError::ResizeNotGrow { .. }
        ));
        assert!(matches!(
            writer.set_size(orig + 1).unwrap_err(),
            UploaderError::ResizeNotSectorAligned { .. }
        ));

        // No rejected call touched the live shadow or the on-disk manifest.
        assert_eq!(writer.size_bytes(), orig);
        assert_eq!(
            VolumeManifest::load(tmp.path(), &name).unwrap().size_bytes,
            orig
        );
    }

    // -- At-rest encryption -----------------------------------------------

    use crate::volume::VolumeEncryptionAlgorithm;

    /// Stand up an *encrypted* volume in the same shape as `fixture`,
    /// returning the AES-256 key alongside so the test can pass it
    /// into `open_with_key`.
    async fn encrypted_fixture(
        scope: DedupScope,
    ) -> (TempDir, String, Arc<dyn ObjectStoreBackend>, [u8; KEY_LEN]) {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let cloud_root = data_dir.join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

        let name = "vol-enc".to_string();
        VolumeManifest::new(
            name.clone(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            scope,
            false,
            0,
        )
        .unwrap()
        .with_encryption(VolumeEncryptionAlgorithm::Aes256Gcm)
        .create(&data_dir)
        .unwrap();

        let mut key = [0u8; KEY_LEN];
        for (i, b) in key.iter_mut().enumerate() {
            *b = 0xA0 ^ i as u8;
        }

        (tmp, name, backend, key)
    }

    #[tokio::test]
    async fn encrypted_volume_write_then_read_round_trips() {
        let (tmp, name, backend, key) = encrypted_fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open_with_key(tmp.path(), &name, backend, key).unwrap();
        let plaintext = page_bytes(0xAB);

        let outcome = writer.write_page(5, &plaintext).await.unwrap();
        assert!(!outcome.local_dedup_hit);

        // Reading back through the same VolumeWriter decrypts
        // transparently — host sees plaintext.
        let read_back = writer.read_page(5).await.unwrap();
        assert_eq!(read_back, Some((plaintext.clone(), 0)));
    }

    #[tokio::test]
    async fn encrypted_volume_stores_ciphertext_in_pool() {
        let (tmp, name, backend, key) = encrypted_fixture(DedupScope::Local).await;
        let writer = VolumeWriter::open_with_key(tmp.path(), &name, backend, key).unwrap();
        let plaintext = page_bytes(0x55);

        let outcome = writer.write_page(2, &plaintext).await.unwrap();

        // The on-disk chunk under `outcome.hash_hex` is the ciphertext
        // + 16-byte AES-GCM tag, not the plaintext. Length differs by
        // TAG_LEN and the bytes themselves don't equal the plaintext.
        let on_disk = writer.pool().read_bytes(&outcome.hash_hex).unwrap();
        assert_eq!(on_disk.len(), plaintext.len() + shared_crypto::TAG_LEN);
        assert_ne!(&on_disk[..plaintext.len()], plaintext.as_slice());
    }

    #[tokio::test]
    async fn open_without_key_refuses_encrypted_volume() {
        let (tmp, name, backend, _key) = encrypted_fixture(DedupScope::Local).await;
        match VolumeWriter::open(tmp.path(), &name, backend) {
            Err(UploaderError::MissingKey { .. }) => {}
            Err(other) => panic!("expected MissingKey, got {other:?}"),
            Ok(_) => panic!("encrypted volume opened without a key"),
        }
    }

    #[tokio::test]
    async fn wrong_key_fails_decrypt_after_write() {
        let (tmp, name, backend, key) = encrypted_fixture(DedupScope::Local).await;
        // Write with the real key, then re-open with a different one.
        let writer =
            VolumeWriter::open_with_key(tmp.path(), &name, Arc::clone(&backend), key).unwrap();
        let plaintext = page_bytes(0xF0);
        writer.write_page(1, &plaintext).await.unwrap();
        drop(writer);

        let bad_key = [0u8; KEY_LEN];
        let writer = VolumeWriter::open_with_key(tmp.path(), &name, backend, bad_key).unwrap();
        let err = writer.read_page(1).await.unwrap_err();
        assert!(matches!(err, UploaderError::Decrypt(_)));
    }

    #[tokio::test]
    async fn unencrypted_volume_ignores_supplied_key() {
        // Passing a key against a plaintext volume is a no-op, not
        // an error — handlers can pass the key without first inspecting
        // the manifest, simplifying the daemon's open path.
        let (tmp, name, backend) = fixture(DedupScope::Local).await;
        let throwaway_key = [0xCC; KEY_LEN];
        let writer =
            VolumeWriter::open_with_key(tmp.path(), &name, backend, throwaway_key).unwrap();
        let bytes = page_bytes(0x42);
        writer.write_page(0, &bytes).await.unwrap();
        let read_back = writer.read_page(0).await.unwrap();
        // No encrypt-on-write happened, so the read returns the
        // plaintext we put in.
        assert_eq!(read_back, Some((bytes, 0)));
    }
}
