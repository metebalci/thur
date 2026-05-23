// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-volume in-memory write-back page cache fronting
//! [`VolumeWriter`].
//!
//! Lifts the page-aligned constraint in the SBC-3 dispatcher: sub-
//! page WRITE / READ / CAW / UNMAP turn into read-modify-write
//! through a per-page in-memory buffer. The buffer is the single
//! source of truth for "what is the host-visible content of this
//! page right now"; the underlying `VolumeWriter` handles cloud
//! durability when a dirty page eventually flushes.
//!
//! ## Crash semantics
//!
//! Without a host SYNCHRONIZE CACHE, recently-written bytes live
//! only in this cache and are lost on a daemon crash — that's the
//! SCSI write-back contract (no FUA, no SYNC = no durability
//! guarantee). After SYNCHRONIZE CACHE returns, every page in the
//! requested LBA range has been flushed through `VolumeWriter::
//! write_page` to the cloud.
//!
//! ## Concurrency
//!
//! One [`tokio::sync::Mutex`] guards the cache state. Loads and
//! flushes drop the lock during the cloud IO and re-acquire to
//! commit, so a slow cloud read doesn't block unrelated cache
//! operations on different pages. The dispatcher already serializes
//! conflicting CAW operations via a per-LUN async mutex; concurrent
//! plain WRITE/READ on the same LBA range is host-side undefined
//! behavior, matching real SAN targets.
//!
//! ## Eviction
//!
//! LRU under a byte budget (default 64 MiB / 1024 pages at the
//! 64 KiB default page size). Eviction picks the LRU clean page if
//! one exists; otherwise it flushes the LRU dirty page through
//! `VolumeWriter::write_page_unsynced` first. Eviction runs inline
//! on the load/insert path that pushed the cache over budget; the
//! inner cache lock is dropped during the cloud upload so other
//! cache operations (notably parallel `flush_all` cohort members)
//! don't block on the eviction's PUT. Each eviction bumps
//! `thurvsa_cache_evictions_total{outcome=clean|dirty}` so an
//! operator can see whether the cache budget is undersized.
//!
//! ## Background flush worker
//!
//! [`PageCache::run_flush_worker`] is a future the daemon spawns
//! after construction. It awaits a [`tokio::sync::Notify`] (signaled
//! when dirty page count crosses a soft watermark) and a periodic
//! tick, drains dirty pages through `VolumeWriter::write_page`, and
//! exits when [`PageCache::shutdown`] sets the shutdown flag. The
//! cache stays correct without it (sync flushes inline on demand);
//! the worker just smooths the dirty-page queue under sustained
//! write workloads.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::stream::{self, StreamExt};
use tokio::sync::{Mutex, Notify};

use crate::page_index::PageId;
use crate::runtime_state::VolumeRuntime;
use crate::uploader::{UploaderError, VolumeWriter};
use crate::volume::{SyncAfter, VolumeManifest};

/// Default cache budget — 64 MiB. At the default 64 KiB page size
/// this is 1024 cached pages. Sized to fit a few concurrent host
/// streams without burning RAM; tuneable via
/// [`PageCache::with_budget`] for tests / future config.
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Soft watermark — when dirty pages cross this fraction of the
/// budget, the cache signals the flush worker. 50% by default; below
/// the watermark the worker idles.
const DIRTY_WATERMARK_NUMERATOR: usize = 1;
const DIRTY_WATERMARK_DENOMINATOR: usize = 2;

/// Background flush tick. The worker also wakes on the dirty
/// notification, so this is just a backstop for quiet writers — a
/// host that writes once and stops still gets its bytes committed
/// to cloud within roughly this window.
const FLUSH_TICK: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageState {
    Clean,
    Dirty,
}

struct CacheEntry {
    bytes: Vec<u8>,
    state: PageState,
    /// Monotonic write counter. Bumped on every dirtying mutation so
    /// a flush can detect that the bytes it captured are stale by
    /// the time it tries to mark the entry clean — in that case the
    /// page stays dirty and the next flush picks up the latest copy.
    version: u64,
}

struct CacheInner {
    /// Live page cache. `None` is never stored — pages are either
    /// in this map or absent (treated as unallocated when read
    /// without a prior load).
    pages: HashMap<PageId, CacheEntry>,
    /// LRU ring — front is most-recently-used, back is least. O(n)
    /// per touch but n is bounded by the page budget (~1024) so the
    /// constant factor is tiny.
    lru: VecDeque<PageId>,
    /// Set of dirty page ids — separate index so the flush worker
    /// doesn't walk every cached page on every tick.
    dirty: BTreeSet<PageId>,
}

impl CacheInner {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            lru: VecDeque::new(),
            dirty: BTreeSet::new(),
        }
    }

    fn lru_touch(&mut self, page_id: PageId) {
        if let Some(pos) = self.lru.iter().position(|&p| p == page_id) {
            self.lru.remove(pos);
        }
        self.lru.push_front(page_id);
    }

    fn lru_remove(&mut self, page_id: PageId) {
        if let Some(pos) = self.lru.iter().position(|&p| p == page_id) {
            self.lru.remove(pos);
        }
    }

    /// Find the LRU page id, optionally restricted to clean entries.
    /// Returns `None` if the cache is empty (or no clean entries
    /// when `clean_only`).
    fn lru_pick(&self, clean_only: bool) -> Option<PageId> {
        for &pid in self.lru.iter().rev() {
            if !clean_only {
                return Some(pid);
            }
            if matches!(
                self.pages.get(&pid),
                Some(CacheEntry {
                    state: PageState::Clean,
                    ..
                })
            ) {
                return Some(pid);
            }
        }
        None
    }
}

/// Per-volume page cache. Holds the underlying `VolumeWriter` so a
/// single boot-time construction binds the cache to its volume +
/// cloud backend; the daemon hands `Arc<PageCache>` to the SCSI
/// dispatcher in place of `Arc<VolumeWriter>`.
pub struct PageCache {
    writer: Arc<VolumeWriter>,
    inner: Mutex<CacheInner>,
    flush_notify: Notify,
    shutdown: AtomicBool,
    budget_pages: usize,
    page_size: u64,
    sector_size: u64,
    /// Monotonic count of host-visible bytes written into this volume,
    /// pre-dedup, pre-compression. Seeded from
    /// `runtime.host_bytes_written` at construction; bumped on every
    /// WRITE / committed CAW / UNMAP (UNMAP zeros count as host
    /// writes).
    host_bytes_written: AtomicU64,
    /// Monotonic count of host-visible bytes served for READs. Bumped
    /// per `read_bytes`, cache hits and cloud misses alike.
    host_bytes_read: AtomicU64,
    /// Monotonic count of bytes PUT to cloud for this volume —
    /// post-dedup, post-compression. Bumped from the daemon's upload
    /// worker as each chunk's PUT completes.
    backend_bytes_written: AtomicU64,
    /// Monotonic count of bytes fetched from cloud on a page cache
    /// miss. Bumped in `acquire_page` from `VolumeWriter::read_page`.
    backend_bytes_read: AtomicU64,
    /// `true` when any of the four counters above has advanced past
    /// the runtime sidecar's last-persisted values. Avoids rewriting
    /// the sidecar when a persist tick would be a no-op. All four
    /// counters persist together to `runtime.json` at `flush_all`,
    /// the daemon's 60 s timer, and shutdown — see
    /// `persist_runtime_if_dirty`.
    counter_dirty: AtomicBool,
    /// Upper bound on concurrent in-flight `flush_one` calls when
    /// draining the dirty set. `1` (the legacy default) preserves the
    /// pre-parallel behavior; the VSA daemon resolves the operator's
    /// `cloud.upload.max_concurrent` and passes the result via
    /// [`PageCache::with_budget_and_concurrency`]. `0` is clamped to
    /// `1` defensively.
    max_concurrent_flushes: usize,
}

impl PageCache {
    /// Construct a cache against `writer` with the default budget and
    /// **serial** flushes. Suitable for tests, smoke binaries, and
    /// other callers that don't care about flush throughput.
    pub fn new(writer: Arc<VolumeWriter>) -> Arc<Self> {
        Self::with_budget_and_concurrency(writer, DEFAULT_CACHE_BUDGET_BYTES, 1)
    }

    /// Construct a cache with an explicit byte budget and serial
    /// flushes. See [`Self::with_budget_and_concurrency`] for the
    /// parallel-flush variant.
    pub fn with_budget(writer: Arc<VolumeWriter>, budget_bytes: u64) -> Arc<Self> {
        Self::with_budget_and_concurrency(writer, budget_bytes, 1)
    }

    /// Construct a cache with an explicit byte budget and concurrency
    /// cap for the page-flush drain.
    ///
    /// `max_concurrent_flushes` is the upper bound on parallel
    /// in-flight `VolumeWriter::write_page` calls during `flush_all` /
    /// `flush_pages_in_range` / `run_flush_worker`. The VSA daemon
    /// resolves it from `cloud.upload.max_concurrent` via
    /// [`shared_cloud::UploadConfig::resolve_max_concurrent`] at boot
    /// and passes the resolved value here. `0` is clamped to `1` so
    /// the drain loop always makes forward progress.
    ///
    /// Budget rounds down to a whole number of pages; a sub-page
    /// budget floors at one page (the cache still functions and can
    /// RMW a single host write).
    pub fn with_budget_and_concurrency(
        writer: Arc<VolumeWriter>,
        budget_bytes: u64,
        max_concurrent_flushes: usize,
    ) -> Arc<Self> {
        let m = writer.manifest();
        let page_size = u64::from(m.page_size_bytes);
        let sector_size = u64::from(m.sector_bytes);
        let budget_pages = std::cmp::max(1, (budget_bytes / page_size) as usize);
        let rt = writer.runtime();
        let host_bytes_written = AtomicU64::new(rt.host_bytes_written);
        let host_bytes_read = AtomicU64::new(rt.host_bytes_read);
        let backend_bytes_written = AtomicU64::new(rt.backend_bytes_written);
        let backend_bytes_read = AtomicU64::new(rt.backend_bytes_read);
        Arc::new(Self {
            writer,
            inner: Mutex::new(CacheInner::new()),
            flush_notify: Notify::new(),
            shutdown: AtomicBool::new(false),
            budget_pages,
            page_size,
            sector_size,
            host_bytes_written,
            host_bytes_read,
            backend_bytes_written,
            backend_bytes_read,
            counter_dirty: AtomicBool::new(false),
            max_concurrent_flushes: max_concurrent_flushes.max(1),
        })
    }

    /// Current in-memory host write counter. Reflects every WRITE /
    /// committed CAW / UNMAP since boot; may lead the on-disk manifest
    /// until the next persist boundary.
    pub fn host_bytes_written(&self) -> u64 {
        self.host_bytes_written.load(Ordering::Relaxed)
    }

    /// Current in-memory host-read byte counter.
    pub fn host_bytes_read(&self) -> u64 {
        self.host_bytes_read.load(Ordering::Relaxed)
    }

    /// Current in-memory cloud-PUT byte counter.
    pub fn backend_bytes_written(&self) -> u64 {
        self.backend_bytes_written.load(Ordering::Relaxed)
    }

    /// Current in-memory cloud-fetch byte counter.
    pub fn backend_bytes_read(&self) -> u64 {
        self.backend_bytes_read.load(Ordering::Relaxed)
    }

    /// Snapshot the four live counter atomics into a [`VolumeRuntime`]
    /// — the form persisted to `runtime.json` and returned by the
    /// admin `volume info` handler for an attached volume.
    /// `sync_after` comes from the writer's live tier; `modified_at`
    /// is the current time.
    pub fn runtime_snapshot(&self) -> VolumeRuntime {
        VolumeRuntime {
            host_bytes_written: self.host_bytes_written.load(Ordering::Relaxed),
            host_bytes_read: self.host_bytes_read.load(Ordering::Relaxed),
            backend_bytes_written: self.backend_bytes_written.load(Ordering::Relaxed),
            backend_bytes_read: self.backend_bytes_read.load(Ordering::Relaxed),
            modified_at: chrono::Utc::now(),
            sync_after: self.writer.sync_after(),
        }
    }

    /// Rewrite the volume's `runtime.json` from a live counter
    /// snapshot if any counter has advanced since the last persist.
    /// Called inside `flush_all`, after an eviction-induced dirty-page
    /// flush, and by the daemon's 60 s persist timer. Idempotent — a
    /// no-op when no counter has moved.
    pub fn persist_runtime_if_dirty(&self) -> Result<(), UploaderError> {
        if !self.counter_dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        match self.writer.persist_runtime(&self.runtime_snapshot()) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Leave the dirty flag set so the next flush retries.
                self.counter_dirty.store(true, Ordering::Release);
                Err(e)
            }
        }
    }

    /// Add `n` to a counter atomic and mark the runtime sidecar
    /// dirty. Zero is a no-op — no spurious dirty flag.
    fn bump(&self, counter: &AtomicU64, n: u64) {
        if n == 0 {
            return;
        }
        counter.fetch_add(n, Ordering::Relaxed);
        self.counter_dirty.store(true, Ordering::Release);
    }

    fn bump_host_bytes(&self, n: u64) {
        self.bump(&self.host_bytes_written, n);
    }

    fn bump_host_bytes_read(&self, n: u64) {
        self.bump(&self.host_bytes_read, n);
    }

    fn bump_backend_bytes_read(&self, n: u64) {
        self.bump(&self.backend_bytes_read, n);
    }

    /// Add `n` to the cloud-PUT byte meter. `pub` because the
    /// daemon's upload worker bumps it from outside this module as
    /// each chunk's PUT completes.
    pub fn bump_backend_bytes_written(&self, n: u64) {
        self.bump(&self.backend_bytes_written, n);
    }

    pub fn manifest(&self) -> &VolumeManifest {
        self.writer.manifest()
    }

    pub fn writer(&self) -> &VolumeWriter {
        &self.writer
    }

    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    pub fn sector_size(&self) -> u64 {
        self.sector_size
    }

    pub fn budget_pages(&self) -> usize {
        self.budget_pages
    }

    /// Total volume size in bytes. Convenience accessor for the
    /// dispatcher which does range checking in byte space.
    pub fn size_bytes(&self) -> u64 {
        self.manifest().size_bytes
    }

    // -------------------------------------------------------------
    // Byte-grained API consumed by the SBC-3 data path.
    // -------------------------------------------------------------

    /// Read `len` bytes starting at `byte_offset`. Sub-page reads
    /// pull the affected page(s) into cache and slice. Unallocated
    /// pages return zero bytes per SBC-3 §5.7. Range must lie
    /// entirely within the volume — caller validates against
    /// `size_bytes()` before calling.
    pub async fn read_bytes(&self, byte_offset: u64, len: usize) -> Result<Vec<u8>, UploaderError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(len);
        let mut cursor = byte_offset;
        let mut remaining = len;
        while remaining > 0 {
            let page_id = self.page_id_for_offset(cursor)?;
            let off_in_page = (cursor % self.page_size) as usize;
            let chunk = std::cmp::min(remaining, self.page_size as usize - off_in_page);
            let bytes = self.acquire_page(page_id).await?;
            out.extend_from_slice(&bytes[off_in_page..off_in_page + chunk]);
            cursor += chunk as u64;
            remaining -= chunk;
        }
        self.bump_host_bytes_read(out.len() as u64);
        Ok(out)
    }

    /// Write `data.len()` bytes starting at `byte_offset`. Sub-page
    /// writes RMW: load the affected page (cache → pool → cloud →
    /// zero-fill if unallocated), splice in the host bytes, mark
    /// dirty. Full-page writes skip the load and install the bytes
    /// directly. Range must lie entirely within the volume.
    pub async fn write_bytes(&self, byte_offset: u64, data: &[u8]) -> Result<(), UploaderError> {
        if data.is_empty() {
            return Ok(());
        }
        let mut cursor = byte_offset;
        let mut consumed = 0usize;
        while consumed < data.len() {
            let page_id = self.page_id_for_offset(cursor)?;
            let off_in_page = (cursor % self.page_size) as usize;
            let page_bytes = self.page_size as usize;
            let chunk = std::cmp::min(data.len() - consumed, page_bytes - off_in_page);
            let slice = &data[consumed..consumed + chunk];
            if off_in_page == 0 && chunk == page_bytes {
                self.install_full_page(page_id, slice.to_vec()).await?;
            } else {
                self.modify_page(page_id, off_in_page, slice).await?;
            }
            cursor += chunk as u64;
            consumed += chunk;
        }
        self.bump_host_bytes(data.len() as u64);
        Ok(())
    }

    /// SBC-3 COMPARE AND WRITE primitive over the cache. Reads the
    /// existing bytes for `[byte_offset, byte_offset + expected.len())`,
    /// compares against `expected`; if every byte matches, splices
    /// `new` into the same range and returns `Ok(true)`. On
    /// mismatch returns `Ok(false)` and leaves the cache unchanged.
    /// Caller is responsible for serializing CAW against other CAW
    /// on the same LUN — the per-LUN `CawLocks` in the dispatcher
    /// already handles that.
    pub async fn compare_and_write_bytes(
        &self,
        byte_offset: u64,
        expected: &[u8],
        new: &[u8],
    ) -> Result<bool, UploaderError> {
        debug_assert_eq!(expected.len(), new.len());
        if expected.is_empty() {
            return Ok(true);
        }
        // Phase 1: compare. Walk the affected pages, pulling each
        // through `acquire_page`, and bail early on mismatch without
        // touching state.
        let mut cursor = byte_offset;
        let mut consumed = 0usize;
        while consumed < expected.len() {
            let page_id = self.page_id_for_offset(cursor)?;
            let off_in_page = (cursor % self.page_size) as usize;
            let chunk = std::cmp::min(
                expected.len() - consumed,
                self.page_size as usize - off_in_page,
            );
            let bytes = self.acquire_page(page_id).await?;
            if bytes[off_in_page..off_in_page + chunk] != expected[consumed..consumed + chunk] {
                return Ok(false);
            }
            cursor += chunk as u64;
            consumed += chunk;
        }
        // Phase 2: commit. Reuse `write_bytes` for the splice; the
        // cache remains mutex-serialized so a concurrent third
        // mutation is a host-side bug we don't fence. `write_bytes`
        // already bumps the host write counter, so CAW counts host
        // bytes only on successful commit — matching SBC-3 semantics.
        self.write_bytes(byte_offset, new).await?;
        Ok(true)
    }

    /// SBC-3 UNMAP primitive over the cache. Zeros the byte range
    /// `[byte_offset, byte_offset + len)`. If the range fully covers
    /// a page, the cached entry is dropped and the page-index entry
    /// is cleared synchronously (matching the previous page-aligned
    /// behavior). Sub-page UNMAP zeros the affected sectors and
    /// marks the page dirty so the next flush commits the partial
    /// erase to cloud. Range must lie within the volume.
    pub async fn unmap_bytes(&self, byte_offset: u64, len: u64) -> Result<(), UploaderError> {
        if len == 0 {
            return Ok(());
        }
        let mut cursor = byte_offset;
        let mut remaining = len;
        while remaining > 0 {
            let page_id = self.page_id_for_offset(cursor)?;
            let off_in_page = (cursor % self.page_size) as usize;
            let chunk_u64 = std::cmp::min(remaining, self.page_size - off_in_page as u64);
            let chunk = chunk_u64 as usize;
            if off_in_page == 0 && chunk == self.page_size as usize {
                self.unmap_full_page(page_id).await?;
            } else {
                let zeros = vec![0u8; chunk];
                self.modify_page(page_id, off_in_page, &zeros).await?;
            }
            cursor += chunk_u64;
            remaining -= chunk_u64;
        }
        // UNMAP zeros count as host-written bytes. The host told us
        // "these LBAs are now zero" — that's still a write from the
        // host's perspective.
        self.bump_host_bytes(len);
        Ok(())
    }

    /// SBC-3 SYNCHRONIZE CACHE primitive. Drains the pipeline to
    /// the operator-chosen [`SyncAfter`] tier (mutable via
    /// `thurvsa volume modify --sync-after <MODE>`):
    ///
    /// - [`SyncAfter::Cloud`] (default) — flush dirty cache pages
    ///   to the pool + enqueue uploads, then await the pending
    ///   tracker so every PUT for the synced range has acked. The
    ///   host's `fsync(2)` settles to "bytes are in cloud."
    /// - [`SyncAfter::Disk`] — flush dirty cache pages to the pool
    ///   only; return without waiting on the upload worker. Host
    ///   `fsync(2)` settles to "bytes are in the local pool." A
    ///   subsequent host-side crash (or daemon-host disk failure
    ///   before the worker drains) loses bytes the host believed
    ///   were durable.
    /// - [`SyncAfter::Memory`] — no-op; dirty pages stay in the
    ///   RAM cache until the periodic flush worker tick (or
    ///   eviction-induced flush) drains them. Host `fsync(2)`
    ///   returns immediately. ZFS `sync=disabled` equivalent.
    ///
    /// Ranges that cover zero pages (degenerate input) are a no-op
    /// success. The dispatcher validates that the byte range lies
    /// within the volume before calling.
    pub async fn synchronize_bytes(&self, byte_offset: u64, len: u64) -> Result<(), UploaderError> {
        // Memory mode is the "fsync is a no-op" tier — return
        // before even computing the page range. The flush worker
        // will pick up dirty pages on its next tick.
        if matches!(self.writer.sync_after(), SyncAfter::Memory) {
            return Ok(());
        }
        let (first, last) = match self.page_range_for_bytes(byte_offset, len) {
            Some(r) => r,
            None => return Ok(()),
        };
        // Phase 1 (Disk + Cloud): cache → pool + enqueue.
        self.flush_pages_in_range(first, last).await?;
        // Phase 2 (Cloud only): drain the pending-upload tracker.
        // No-op under inline dispatch (pending tracker stays
        // empty) and on already-drained ranges.
        if matches!(self.writer.sync_after(), SyncAfter::Cloud) {
            self.writer
                .pending_uploads()
                .wait_for_range(first..=last)
                .await;
        }
        Ok(())
    }

    /// Flush every dirty page in the cache. Called at shutdown so
    /// in-memory writes don't get silently lost. Persists the host
    /// write counter to `runtime.json` once all dirty pages are
    /// committed so a restart picks up the counter close to where the
    /// running daemon left it.
    ///
    /// Drains in batches of up to `max_concurrent_flushes` parallel
    /// `flush_one` calls via `buffer_unordered`. Pages re-dirtied
    /// during a batch are picked up by the next outer-loop iteration
    /// — same race-recovery shape as the previous serial loop.
    ///
    /// Awaits the pending-upload tracker for the full `0..=u32::MAX`
    /// range so async-dispatch volumes drain their upload queue at
    /// shutdown — without it, `request_shutdown` could return with
    /// pages still pool-only, surfaced as `LocalOnly` survivors that
    /// the next boot's recovery scan would have to re-enqueue.
    pub async fn flush_all(&self) -> Result<(), UploaderError> {
        self.flush_drain(|dirty, n| dirty.iter().take(n).copied().collect())
            .await?;
        self.persist_runtime_if_dirty()?;
        self.writer
            .pending_uploads()
            .wait_for_range(PageId::MIN..=PageId::MAX)
            .await;
        Ok(())
    }

    /// Drive a flush worker until shutdown is requested. The daemon
    /// spawns this future at boot; tests can poll it manually if
    /// they want background-flush behavior.
    pub async fn run_flush_worker(self: Arc<Self>) {
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return;
            }
            let _ = tokio::time::timeout(FLUSH_TICK, self.flush_notify.notified()).await;
            if self.shutdown.load(Ordering::Acquire) {
                return;
            }
            // Drain dirty pages opportunistically; ignore individual
            // flush errors (logged inside `flush_one`) so a transient
            // cloud blip doesn't kill the worker.
            let _ = self.flush_all().await;
        }
    }

    /// Request shutdown. The flush worker exits at its next wakeup;
    /// callers should still call [`Self::flush_all`] before drop to
    /// get every dirty page committed.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.flush_notify.notify_waiters();
    }

    /// Lookup a sector's page id, accounting for the volume's
    /// per-volume page size.
    fn page_id_for_offset(&self, byte_offset: u64) -> Result<PageId, UploaderError> {
        let pid = byte_offset / self.page_size;
        u32::try_from(pid).map_err(|_| UploaderError::PageOutOfRange {
            page_id: pid,
            page_size: self.page_size as u32,
            size_bytes: self.size_bytes(),
        })
    }

    fn page_range_for_bytes(&self, byte_offset: u64, len: u64) -> Option<(PageId, PageId)> {
        if len == 0 {
            return None;
        }
        let first = byte_offset / self.page_size;
        let last_byte = byte_offset.checked_add(len - 1)?;
        let last = last_byte / self.page_size;
        let first_u32 = u32::try_from(first).ok()?;
        let last_u32 = u32::try_from(last).ok()?;
        Some((first_u32, last_u32))
    }

    // -------------------------------------------------------------
    // Cache primitives (private — invariants enforced here).
    // -------------------------------------------------------------

    /// Return a clone of the page's current bytes, populating the
    /// cache from the writer if necessary. Unallocated pages
    /// materialize as a zeroed buffer (and stay marked Clean — the
    /// page index entry is still absent).
    async fn acquire_page(&self, page_id: PageId) -> Result<Vec<u8>, UploaderError> {
        // Hot path: cache hit.
        {
            let mut inner = self.inner.lock().await;
            if let Some(entry) = inner.pages.get(&page_id) {
                let bytes = entry.bytes.clone();
                inner.lru_touch(page_id);
                return Ok(bytes);
            }
        }

        // Cache miss → fetch from the writer (cloud or pool). Drop
        // the lock during the await so unrelated cache ops don't
        // block on a slow cloud read.
        let bytes = match self.writer.read_page(page_id).await? {
            Some((bytes, cloud_bytes)) => {
                // `cloud_bytes` is 0 on a local-pool hit and >0 only
                // on a real cloud fetch; the bump no-ops on 0.
                self.bump_backend_bytes_read(cloud_bytes);
                bytes
            }
            None => vec![0u8; self.page_size as usize],
        };
        if bytes.len() != self.page_size as usize {
            return Err(UploaderError::PageSizeMismatch {
                len: bytes.len(),
                page_size: self.page_size as u32,
            });
        }

        // Make room for the new entry. `evict_to_fit` releases the
        // inner lock during any required cloud upload, so concurrent
        // flushes don't stall on this path.
        self.evict_to_fit(1).await?;

        // Re-acquire the lock to commit. If another loader populated
        // the slot between our miss and our insert, prefer their
        // (possibly Dirty) bytes — those are the live state.
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.pages.get(&page_id) {
            let cached = entry.bytes.clone();
            inner.lru_touch(page_id);
            return Ok(cached);
        }
        inner.pages.insert(
            page_id,
            CacheEntry {
                bytes: bytes.clone(),
                state: PageState::Clean,
                version: 0,
            },
        );
        inner.lru.push_front(page_id);
        Ok(bytes)
    }

    /// Install a full page's worth of new bytes into the cache and
    /// mark it dirty. Skips the load step (no need to RMW a page
    /// when the host wrote the whole thing).
    async fn install_full_page(
        &self,
        page_id: PageId,
        bytes: Vec<u8>,
    ) -> Result<(), UploaderError> {
        if bytes.len() != self.page_size as usize {
            return Err(UploaderError::PageSizeMismatch {
                len: bytes.len(),
                page_size: self.page_size as u32,
            });
        }
        // Make room *outside* the inner lock first. If the page
        // turns out to already be present we'll just refresh it
        // below without consuming new budget; the (rare) wasted
        // eviction is harmless.
        let already_present = { self.inner.lock().await.pages.contains_key(&page_id) };
        if !already_present {
            self.evict_to_fit(1).await?;
        }
        let mut inner = self.inner.lock().await;
        let entry = inner.pages.entry(page_id).or_insert(CacheEntry {
            bytes: Vec::new(),
            state: PageState::Clean,
            version: 0,
        });
        entry.bytes = bytes;
        entry.state = PageState::Dirty;
        entry.version = entry.version.wrapping_add(1);
        inner.dirty.insert(page_id);
        inner.lru_touch(page_id);
        let dirty_count = inner.dirty.len();
        let budget = self.budget_pages;
        drop(inner);
        self.maybe_signal_worker(dirty_count, budget);
        Ok(())
    }

    /// Splice `bytes` into a page at `offset`, loading the page
    /// first if it isn't cached. Marks the page dirty.
    async fn modify_page(
        &self,
        page_id: PageId,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), UploaderError> {
        debug_assert!(offset + bytes.len() <= self.page_size as usize);
        // Fast path: already in cache.
        {
            let mut inner = self.inner.lock().await;
            if let Some(entry) = inner.pages.get_mut(&page_id) {
                entry.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
                entry.state = PageState::Dirty;
                entry.version = entry.version.wrapping_add(1);
                inner.dirty.insert(page_id);
                inner.lru_touch(page_id);
                let dirty_count = inner.dirty.len();
                let budget = self.budget_pages;
                drop(inner);
                self.maybe_signal_worker(dirty_count, budget);
                return Ok(());
            }
        }

        // Slow path: load + insert + splice. The `acquire_page` call
        // populates the cache; we then modify in place.
        let _ = self.acquire_page(page_id).await?;
        let mut inner = self.inner.lock().await;
        let entry = inner.pages.get_mut(&page_id).ok_or_else(|| {
            // Should be impossible — acquire_page just inserted.
            // Defensive: surface as a benign error instead of panicking.
            UploaderError::PageOutOfRange {
                page_id: u64::from(page_id),
                page_size: self.page_size as u32,
                size_bytes: self.size_bytes(),
            }
        })?;
        entry.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
        entry.state = PageState::Dirty;
        entry.version = entry.version.wrapping_add(1);
        inner.dirty.insert(page_id);
        inner.lru_touch(page_id);
        let dirty_count = inner.dirty.len();
        let budget = self.budget_pages;
        drop(inner);
        self.maybe_signal_worker(dirty_count, budget);
        Ok(())
    }

    /// UNMAP a full page: drop any cached entry (clean or dirty —
    /// the host explicitly told us to forget it) and clear the
    /// underlying page-index slot synchronously. Cloud chunks linger
    /// until `system gc` reclaims them.
    async fn unmap_full_page(&self, page_id: PageId) -> Result<(), UploaderError> {
        {
            let mut inner = self.inner.lock().await;
            if inner.pages.remove(&page_id).is_some() {
                inner.lru_remove(page_id);
                inner.dirty.remove(&page_id);
            }
        }
        self.writer.page_index().clear(page_id)?;
        Ok(())
    }

    /// Evict enough entries from the cache so that `wanted` more
    /// pages fit under the budget. Self-locked: the caller must
    /// release the inner mutex before calling, and re-acquire
    /// after. Each iteration takes the lock just long enough to
    /// pick a candidate and either drop it (clean) or snapshot its
    /// bytes (dirty); the cloud upload itself happens off-lock so
    /// concurrent cache operations — notably parallel `flush_drain`
    /// cohort members — don't stall on the eviction's PUT.
    ///
    /// The budget is a soft cap once the lock is dropped: another
    /// task may insert between our re-lock and re-check, pushing us
    /// briefly back over `budget_pages`. The loop catches up on the
    /// next iteration. Callers that need a strict invariant should
    /// hold the lock themselves (and pay the throughput cost).
    async fn evict_to_fit(&self, wanted: usize) -> Result<(), UploaderError> {
        loop {
            // Snapshot a candidate under the lock. Either consume
            // the clean drop here (no cloud IO needed) or hand back
            // a dirty-page snapshot for the off-lock flush below.
            #[allow(clippy::large_enum_variant)]
            enum Step {
                Done,
                Cleaned,
                Dirty(PageId, Vec<u8>, u64),
            }
            let step = {
                let mut inner = self.inner.lock().await;
                if inner.pages.len() + wanted <= self.budget_pages {
                    Step::Done
                } else if let Some(pid) = inner.lru_pick(true) {
                    inner.pages.remove(&pid);
                    inner.lru_remove(pid);
                    Step::Cleaned
                } else if let Some(pid) = inner.lru_pick(false) {
                    match inner.pages.get(&pid) {
                        Some(entry) => Step::Dirty(pid, entry.bytes.clone(), entry.version),
                        None => {
                            // LRU pointed at a missing entry —
                            // structural drift, clean up and retry.
                            inner.lru_remove(pid);
                            inner.dirty.remove(&pid);
                            continue;
                        }
                    }
                } else {
                    // Cache is empty after all (race with another
                    // evictor that drained it). Caller's insert
                    // path will still fit.
                    Step::Done
                }
            };
            match step {
                Step::Done => return Ok(()),
                Step::Cleaned => {
                    shared_telemetry::record::cache_eviction(self.volume_name(), "clean");
                    continue;
                }
                Step::Dirty(pid, bytes, version) => {
                    // Off-lock flush. `write_page_unsynced` leaves
                    // the page-index pwrite un-fsync'd; the next
                    // `flush_drain` / SYNCHRONIZE issues the
                    // batched `fdatasync`. Eviction itself doesn't
                    // owe the host durability — the host hasn't
                    // issued SYNC for these bytes.
                    match self.writer.write_page_unsynced(pid, &bytes).await {
                        Ok(out) => {
                            // Inline-upload path returns Some(n);
                            // async path returns None (the worker
                            // bumps the counter itself). The bump
                            // is `bump()`-guarded against 0.
                            if let Some(n) = out.put_bytes {
                                self.bump_backend_bytes_written(n);
                            }
                            shared_telemetry::record::cache_eviction(self.volume_name(), "dirty");
                            let mut inner = self.inner.lock().await;
                            match inner.pages.get(&pid) {
                                Some(entry) if entry.version == version => {
                                    inner.pages.remove(&pid);
                                    inner.lru_remove(pid);
                                    inner.dirty.remove(&pid);
                                }
                                Some(_) => {
                                    // Got rewritten between snapshot
                                    // and flush — leave it dirty for
                                    // a future flush to pick up; try
                                    // a different candidate next
                                    // iteration.
                                    tracing::debug!(
                                        page_id = pid,
                                        "thurvsa cache: dirty-page eviction lost the race; retrying"
                                    );
                                }
                                None => {
                                    // Someone else dropped it
                                    // already — fine.
                                    inner.lru_remove(pid);
                                    inner.dirty.remove(&pid);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, page_id = pid, "thurvsa cache: eviction flush failed");
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    fn volume_name(&self) -> &str {
        &self.manifest().name
    }

    /// Flush every dirty page whose id falls in `[first, last]`.
    /// Awaits each cloud upload sequentially — concurrent flushes
    /// against the same cloud backend can saturate it; thurvsa
    /// workloads don't currently need parallel sync flushes.
    async fn flush_pages_in_range(&self, first: PageId, last: PageId) -> Result<(), UploaderError> {
        self.flush_drain(move |dirty, n| dirty.range(first..=last).take(n).copied().collect())
            .await
    }

    /// Shared drain loop for `flush_all` and `flush_pages_in_range`.
    /// Each iteration snapshots up to `max_concurrent_flushes` dirty
    /// page ids under the inner lock, then drives them through
    /// `flush_one` in parallel via `buffer_unordered`. Returns the
    /// first error encountered; other in-flight tasks still complete
    /// (`buffer_unordered` doesn't cancel ready futures), matching
    /// the existing "don't kill the worker on a transient blip"
    /// behavior of `run_flush_worker`.
    ///
    /// Each `flush_one` writes via `VolumeWriter::write_page_unsynced`,
    /// so the N parallel uploads share a single trailing `fdatasync`
    /// issued here after the loop exits — one syscall per drain
    /// instead of one per page.
    async fn flush_drain<F>(&self, pick: F) -> Result<(), UploaderError>
    where
        F: Fn(&BTreeSet<PageId>, usize) -> Vec<PageId>,
    {
        let n = self.max_concurrent_flushes.max(1);
        let mut wrote_any = false;
        let mut first_err: Option<UploaderError> = None;
        'outer: loop {
            let batch = {
                let inner = self.inner.lock().await;
                pick(&inner.dirty, n)
            };
            if batch.is_empty() {
                break 'outer;
            }
            let results: Vec<Result<bool, UploaderError>> = stream::iter(batch)
                .map(|pid| self.flush_one(pid))
                .buffer_unordered(n)
                .collect()
                .await;
            for r in results {
                match r {
                    Ok(true) => wrote_any = true,
                    Ok(false) => {}
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            // On the first error, stop pulling new batches but let
            // any pages that *did* succeed contribute to the trailing
            // sync below so their durability matches their
            // in-memory-clean state.
            if first_err.is_some() {
                break 'outer;
            }
        }
        if wrote_any {
            // One fdatasync per drain, not per page.
            self.writer.page_index_sync()?;
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Flush a single page id if it is currently dirty. Returns
    /// `Ok(true)` if a pwrite was issued (a follow-up fdatasync from
    /// the caller is required for durability), `Ok(false)` if the
    /// page was already clean / absent.
    async fn flush_one(&self, page_id: PageId) -> Result<bool, UploaderError> {
        let (bytes, version) = {
            let inner = self.inner.lock().await;
            match inner.pages.get(&page_id) {
                Some(entry) if entry.state == PageState::Dirty => {
                    (entry.bytes.clone(), entry.version)
                }
                _ => return Ok(false),
            }
        };
        match self.writer.write_page_unsynced(page_id, &bytes).await {
            Ok(out) => {
                // Inline-upload path returns Some(n); async path
                // returns None (the worker bumps the counter after
                // the PUT completes).
                if let Some(n) = out.put_bytes {
                    self.bump_backend_bytes_written(n);
                }
                let mut inner = self.inner.lock().await;
                // If another writer dirtied it again while we were
                // flushing the older bytes, leave it dirty — the next
                // sync / worker tick picks up the latest copy.
                if let Some(entry) = inner.pages.get_mut(&page_id)
                    && entry.version == version
                {
                    entry.state = PageState::Clean;
                    inner.dirty.remove(&page_id);
                }
                Ok(true)
            }
            Err(e) => {
                tracing::warn!(error = %e, page_id = page_id, "thurvsa cache: flush failed");
                Err(e)
            }
        }
    }

    fn maybe_signal_worker(&self, dirty_count: usize, budget: usize) {
        let watermark = budget * DIRTY_WATERMARK_NUMERATOR / DIRTY_WATERMARK_DENOMINATOR;
        if dirty_count >= watermark {
            self.flush_notify.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use crate::{DedupScope, VolumeManifest};
    use shared_cloud::{CloudBackend, LocalBackend};
    use tempfile::TempDir;

    const PAGE: usize = DEFAULT_PAGE_SIZE_BYTES as usize;
    const SECTOR: usize = DEFAULT_SECTOR_BYTES as usize;
    const SECTORS_PER_PAGE: usize = PAGE / SECTOR;

    async fn fixture_cache(size_bytes: u64) -> (TempDir, Arc<PageCache>, Arc<VolumeWriter>) {
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let backend = LocalBackend::new(&cloud).await.unwrap();
        let backend: Arc<dyn CloudBackend> = Arc::new(backend);
        VolumeManifest::new(
            "vol1".into(),
            size_bytes,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "vol1", backend).unwrap());
        let cache = PageCache::new(writer.clone());
        (tmp, cache, writer)
    }

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add((i & 0xFF) as u8))
            .collect()
    }

    #[tokio::test]
    async fn sub_sector_write_then_read_round_trips() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        let bytes = pattern(0x5A, SECTOR);
        // LBA 0 sector → byte_offset 0
        cache.write_bytes(0, &bytes).await.unwrap();
        let got = cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn sub_sector_write_does_not_affect_neighboring_sector() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        // Seed the page with a known pattern.
        let full = pattern(0xFE, PAGE);
        cache.write_bytes(0, &full).await.unwrap();
        // Overwrite only the first sector.
        let small = vec![0xAA; SECTOR];
        cache.write_bytes(0, &small).await.unwrap();
        // Read the neighboring sector — must still hold the seed.
        let got = cache.read_bytes(SECTOR as u64, SECTOR).await.unwrap();
        assert_eq!(got, full[SECTOR..SECTOR * 2]);
        // Read the overwritten sector — must be 0xAA.
        let got = cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(got, small);
    }

    #[tokio::test]
    async fn unallocated_read_returns_zeros() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        let got = cache.read_bytes(0, PAGE).await.unwrap();
        assert!(got.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn sub_page_caw_match_commits() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        // Seed first sector with pattern A.
        let a = vec![0xAA; SECTOR];
        cache.write_bytes(0, &a).await.unwrap();
        // CAW(compare=A, write=B) at the same sector — should commit.
        let b = vec![0xBB; SECTOR];
        let ok = cache.compare_and_write_bytes(0, &a, &b).await.unwrap();
        assert!(ok);
        let got = cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(got, b);
    }

    #[tokio::test]
    async fn sub_page_caw_miscompare_leaves_state_unchanged() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        let stored = vec![0xAA; SECTOR];
        cache.write_bytes(0, &stored).await.unwrap();
        let stale = vec![0xCC; SECTOR];
        let new = vec![0xDD; SECTOR];
        let ok = cache
            .compare_and_write_bytes(0, &stale, &new)
            .await
            .unwrap();
        assert!(!ok);
        let got = cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(got, stored, "miscompare must not commit");
    }

    #[tokio::test]
    async fn sub_page_unmap_zeros_only_targeted_sectors() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        // Seed page 0 with non-zero bytes everywhere.
        let full = pattern(0x77, PAGE);
        cache.write_bytes(0, &full).await.unwrap();
        // UNMAP only the first sector.
        cache.unmap_bytes(0, SECTOR as u64).await.unwrap();
        // Sector 0 is zeros; sector 1 unchanged.
        let s0 = cache.read_bytes(0, SECTOR).await.unwrap();
        assert!(s0.iter().all(|&b| b == 0));
        let s1 = cache.read_bytes(SECTOR as u64, SECTOR).await.unwrap();
        assert_eq!(s1, full[SECTOR..SECTOR * 2]);
    }

    #[tokio::test]
    async fn synchronize_flushes_dirty_pages_to_writer() {
        let (_tmp, cache, writer) = fixture_cache(4 * (1u64 << 20)).await;
        let bytes = pattern(0x33, SECTOR);
        cache.write_bytes(0, &bytes).await.unwrap();

        // Before sync: the underlying writer may not see anything,
        // because the cache holds the page dirty. We probe via
        // page_index — the entry won't be set until the page flushes.
        assert!(writer.page_index().get(0).unwrap().is_none());

        cache.synchronize_bytes(0, PAGE as u64).await.unwrap();
        // After sync: the page index is populated.
        assert!(writer.page_index().get(0).unwrap().is_some());
    }

    #[tokio::test]
    async fn synchronize_with_clean_pages_is_noop() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        // No writes → no dirty pages → sync should still succeed.
        cache.synchronize_bytes(0, PAGE as u64).await.unwrap();
    }

    #[tokio::test]
    async fn flush_all_drains_every_dirty_page() {
        let (_tmp, cache, writer) = fixture_cache(8 * (1u64 << 20)).await;
        // Touch four distinct pages with sub-page writes.
        for i in 0..4u64 {
            cache
                .write_bytes(i * PAGE as u64, &[0x11; SECTOR])
                .await
                .unwrap();
        }
        cache.flush_all().await.unwrap();
        for i in 0..4u32 {
            assert!(
                writer.page_index().get(i).unwrap().is_some(),
                "page {i} should be persisted",
            );
        }
    }

    #[tokio::test]
    async fn full_page_unmap_drops_page_index_entry() {
        let (_tmp, cache, writer) = fixture_cache(4 * (1u64 << 20)).await;
        let full = pattern(0x88, PAGE);
        cache.write_bytes(0, &full).await.unwrap();
        cache.synchronize_bytes(0, PAGE as u64).await.unwrap();
        assert!(writer.page_index().get(0).unwrap().is_some());

        cache.unmap_bytes(0, PAGE as u64).await.unwrap();
        assert!(writer.page_index().get(0).unwrap().is_none());
        let got = cache.read_bytes(0, PAGE).await.unwrap();
        assert!(got.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn eviction_flushes_dirty_pages_when_budget_exhausted() {
        // Tiny budget — 2 pages — so a third page write forces
        // eviction.
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let backend: Arc<dyn CloudBackend> = Arc::new(LocalBackend::new(&cloud).await.unwrap());
        VolumeManifest::new(
            "vol1".into(),
            8 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "vol1", backend).unwrap());
        let cache = PageCache::with_budget(writer.clone(), 2 * PAGE as u64);

        for i in 0..3u32 {
            cache
                .write_bytes(u64::from(i) * PAGE as u64, &pattern(i as u8, PAGE))
                .await
                .unwrap();
        }
        // Page 0 was the LRU when page 2 came in — it must have
        // been flushed (and dropped from cache).
        assert!(
            writer.page_index().get(0).unwrap().is_some(),
            "page 0 must be flushed before eviction"
        );

        // Read it back through the cache — falls through to cloud,
        // pulls the page back in, returns the bytes we wrote.
        let read_back = cache.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(read_back, pattern(0, PAGE));
    }

    #[tokio::test]
    async fn write_then_read_through_cache_does_not_round_trip_to_cloud() {
        // Sanity: a write followed by a read of the same page should
        // hit the cache, not re-fetch from cloud. Indirectly tested
        // by checking the page-index entry stays absent until SYNC.
        let (_tmp, cache, writer) = fixture_cache(4 * (1u64 << 20)).await;
        let bytes = pattern(0xAB, SECTOR);
        cache.write_bytes(0, &bytes).await.unwrap();
        let read_back = cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(read_back, bytes);
        assert!(writer.page_index().get(0).unwrap().is_none());
    }

    #[tokio::test]
    async fn cross_page_write_handles_aligned_and_subpage_regions() {
        let (_tmp, cache, _w) = fixture_cache(8 * (1u64 << 20)).await;
        // Write 1.5 pages starting at sector 8 of page 0.
        let start = 8 * SECTOR as u64;
        let len = PAGE + PAGE / 2;
        let bytes = pattern(0x42, len);
        cache.write_bytes(start, &bytes).await.unwrap();
        let got = cache.read_bytes(start, len).await.unwrap();
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn sub_page_read_through_unallocated_then_partially_written_page() {
        // First read the page (unallocated → zeros). Then write a
        // sector. Re-read — the new sector shows the write, the rest
        // still reads as zeros.
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        let initial = cache.read_bytes(0, PAGE).await.unwrap();
        assert!(initial.iter().all(|&b| b == 0));

        let bytes = pattern(0xC3, SECTOR);
        cache.write_bytes(SECTOR as u64, &bytes).await.unwrap();
        let after = cache.read_bytes(0, PAGE).await.unwrap();
        assert!(after[..SECTOR].iter().all(|&b| b == 0));
        assert_eq!(&after[SECTOR..2 * SECTOR], bytes.as_slice());
        assert!(after[2 * SECTOR..].iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn full_page_sectors_per_page_caw_round_trip() {
        // Sanity for the CAW path at full-page granularity (was the
        // previous dispatcher's only allowed grain).
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        let zeros = vec![0u8; PAGE];
        let new = pattern(0x99, PAGE);
        let ok = cache
            .compare_and_write_bytes(0, &zeros, &new)
            .await
            .unwrap();
        assert!(ok);
        let got = cache.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(got, new);
    }

    #[test]
    fn page_range_for_bytes_handles_aligned_and_unaligned() {
        // Smoke-test the helper without spinning up a cloud backend.
        // We can't hold a PageCache without one, so test the math via
        // a tiny shadow type that mirrors `page_size`. The real
        // function is exercised end-to-end by the synchronize tests.
        struct Probe(u64);
        impl Probe {
            fn range(&self, off: u64, len: u64) -> Option<(u32, u32)> {
                if len == 0 {
                    return None;
                }
                let first = off / self.0;
                let last = (off + len - 1) / self.0;
                Some((first as u32, last as u32))
            }
        }
        let p = Probe(PAGE as u64);
        assert_eq!(p.range(0, PAGE as u64), Some((0, 0)));
        assert_eq!(p.range(0, (PAGE * 2) as u64), Some((0, 1)));
        assert_eq!(p.range(SECTOR as u64, SECTOR as u64), Some((0, 0)));
        // 1 byte starting at the last byte of page 0 → only page 0.
        assert_eq!(p.range(PAGE as u64 - 1, 1), Some((0, 0)));
        // Bytes spanning the boundary: last byte of page 0 + first
        // byte of page 1 → both pages.
        assert_eq!(p.range(PAGE as u64 - 1, 2), Some((0, 1)));
        assert_eq!(p.range(0, 0), None);
    }

    #[tokio::test]
    async fn host_bytes_written_counts_writes_caw_and_unmap() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        assert_eq!(cache.host_bytes_written(), 0);

        // Sub-page WRITE counts data.len().
        cache.write_bytes(0, &vec![0x11; SECTOR]).await.unwrap();
        assert_eq!(cache.host_bytes_written(), SECTOR as u64);

        // CAW commits → counts new.len(); CAW miscompare → no bump.
        let a = vec![0x11; SECTOR];
        let b = vec![0x22; SECTOR];
        let ok = cache.compare_and_write_bytes(0, &a, &b).await.unwrap();
        assert!(ok);
        assert_eq!(cache.host_bytes_written(), 2 * SECTOR as u64);

        let stale = vec![0x99; SECTOR];
        let new = vec![0xDD; SECTOR];
        let ok = cache
            .compare_and_write_bytes(0, &stale, &new)
            .await
            .unwrap();
        assert!(!ok);
        assert_eq!(cache.host_bytes_written(), 2 * SECTOR as u64);

        // UNMAP counts the zero range.
        cache.unmap_bytes(0, SECTOR as u64).await.unwrap();
        assert_eq!(cache.host_bytes_written(), 3 * SECTOR as u64);
    }

    #[tokio::test]
    async fn flush_all_persists_counter_to_runtime() {
        let (tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        cache.write_bytes(0, &vec![0x33; SECTOR]).await.unwrap();
        cache.flush_all().await.unwrap();

        // Reload the runtime sidecar from disk; counter must reflect the bump.
        let vol_dir = VolumeManifest::dir_for(tmp.path(), "vol1");
        let r = crate::runtime_state::VolumeRuntime::load(&vol_dir).unwrap();
        assert_eq!(r.host_bytes_written, SECTOR as u64);
    }

    #[tokio::test]
    async fn read_bytes_counts_host_bytes_read() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        assert_eq!(cache.host_bytes_read(), 0);
        cache.write_bytes(0, &pattern(0x10, PAGE)).await.unwrap();
        // Reads count toward host_bytes_read whether served from the
        // page cache or fetched from cloud.
        cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(cache.host_bytes_read(), SECTOR as u64);
        cache.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(cache.host_bytes_read(), (SECTOR + PAGE) as u64);
    }

    #[tokio::test]
    async fn backend_bytes_read_counts_cloud_fetch_on_pool_miss() {
        let (tmp, cache, writer) = fixture_cache(4 * (1u64 << 20)).await;
        let page = pattern(0x5A, PAGE);
        cache.write_bytes(0, &page).await.unwrap();
        cache.flush_all().await.unwrap();

        // Drop the chunk from the local pool so the next read can
        // only be satisfied from cloud.
        for (hash, _) in writer.pool().iter_chunks().unwrap() {
            writer.pool().remove(&hash).unwrap();
        }

        // A fresh cache over the same volume has an empty in-memory
        // page map, so the read falls through to read_page -> cloud.
        let cache2 = PageCache::new(writer.clone());
        assert_eq!(cache2.backend_bytes_read(), 0);
        let got = cache2.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(got, page);
        assert_eq!(cache2.backend_bytes_read(), PAGE as u64);

        // The cloud-fetch counter persists like the others.
        cache2.flush_all().await.unwrap();
        let vol_dir = VolumeManifest::dir_for(tmp.path(), "vol1");
        let r = crate::runtime_state::VolumeRuntime::load(&vol_dir).unwrap();
        assert_eq!(r.backend_bytes_read, PAGE as u64);
    }

    #[tokio::test]
    async fn flush_all_persists_all_four_counters() {
        let (tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        cache.write_bytes(0, &pattern(0x22, PAGE)).await.unwrap();
        cache.read_bytes(0, SECTOR).await.unwrap();
        // `fixture_cache` builds a `VolumeWriter` without an upload
        // sender, so `flush_all`'s `write_page_unsynced` takes the
        // inline upload path — which now bumps
        // `backend_bytes_written` by the on-wire PUT size itself.
        // No manual `bump_backend_bytes_written` needed.
        cache.flush_all().await.unwrap();

        let vol_dir = VolumeManifest::dir_for(tmp.path(), "vol1");
        let r = crate::runtime_state::VolumeRuntime::load(&vol_dir).unwrap();
        assert_eq!(r.host_bytes_written, PAGE as u64);
        assert_eq!(r.host_bytes_read, SECTOR as u64);
        // LocalBackend doesn't compress and the fixture is not
        // encrypted, so the PUT bytes equal the page size exactly.
        assert_eq!(r.backend_bytes_written, PAGE as u64);
        // No cloud miss in this test, so backend_bytes_read stays 0.
        assert_eq!(r.backend_bytes_read, 0);
    }

    #[tokio::test]
    async fn inline_upload_path_bumps_backend_bytes_written_per_page() {
        // Regression test for the "inline upload path silently
        // skipped backend_bytes_written" bug. The fix threads
        // `put_bytes` up through `WritePageOutcome` so the PageCache
        // caller can bump the counter even when no async upload
        // sender is wired (tests, CLI tools).
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        cache.write_bytes(0, &pattern(0x11, PAGE)).await.unwrap();
        cache.write_bytes(PAGE as u64, &pattern(0x22, PAGE)).await.unwrap();
        cache.flush_all().await.unwrap();
        let snap = cache.runtime_snapshot();
        // Two pages flushed, no compression / encryption — each PUT
        // is one page-sized write to LocalBackend.
        assert_eq!(snap.backend_bytes_written, 2 * PAGE as u64);
    }

    #[tokio::test]
    async fn flush_all_with_no_writes_does_not_rewrite_runtime() {
        let (tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        let runtime_path = VolumeManifest::dir_for(tmp.path(), "vol1").join("runtime.json");
        let before = std::fs::metadata(&runtime_path)
            .unwrap()
            .modified()
            .unwrap();
        // Sleep briefly so mtime resolution can distinguish.
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.flush_all().await.unwrap();
        let after = std::fs::metadata(&runtime_path)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "no-op flush must not touch runtime.json");
    }

    #[tokio::test]
    async fn write_workload_does_not_rewrite_manifest() {
        // Manifest is creation-frozen: the daemon's hot path should
        // never touch manifest.json after open. Only runtime.json
        // advances on each flush.
        let (tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        let manifest_path = VolumeManifest::dir_for(tmp.path(), "vol1").join("manifest.json");
        let before = std::fs::metadata(&manifest_path)
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.write_bytes(0, &vec![0x33; SECTOR]).await.unwrap();
        cache.flush_all().await.unwrap();
        let after = std::fs::metadata(&manifest_path)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "manifest must stay creation-frozen");
    }

    #[tokio::test]
    async fn sectors_per_page_constant_matches() {
        // Belt + suspenders: tests above assume a 16-sector page.
        // If the default ever changes, every test in this module
        // needs an audit.
        assert_eq!(SECTORS_PER_PAGE, 16);
    }

    // ─────────────────────── parallel-flush coverage ───────────────────────
    //
    // These tests build a cache with an explicit concurrency cap and a
    // `DelayingBackend` decorator that sleeps before delegating
    // `upload_chunk` to a real `LocalBackend`. The delay lets the test
    // observe whether the cache drained N pages serially (≥ N×delay
    // wall-clock) or in parallel (≈ delay wall-clock); a separate
    // counter pair records the maximum simultaneous in-flight count.
    //
    // Same fixtures the existing tests use, just with the new
    // concurrency-aware constructor and a fail-injecting backend
    // wrapper.

    use shared_cloud::cloud_backend::LockState;
    use shared_cloud::compression::CompressionAlgo;
    use shared_cloud::{CloudError, Result as CloudResult};
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// Test-only `CloudBackend` decorator. Sleeps `delay` before
    /// every `upload_chunk` call so the test can observe serial vs
    /// parallel drain timing; tracks max simultaneous in-flight
    /// `upload_chunk` callers; fails any call whose key is in the
    /// shared `fail_keys` set (mutable post-construction so tests
    /// can compute the exact cloud key from the live writer's pool
    /// and inject it). Every other trait method delegates unchanged.
    #[derive(Debug)]
    struct DelayingBackend {
        inner: Arc<dyn CloudBackend>,
        delay: Duration,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        fail_keys: Arc<std::sync::Mutex<HashSet<String>>>,
    }

    impl DelayingBackend {
        fn new(inner: Arc<dyn CloudBackend>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                inner,
                delay,
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
                fail_keys: Arc::new(std::sync::Mutex::new(HashSet::new())),
            })
        }

        fn observed_max_in_flight(&self) -> usize {
            self.max_in_flight.load(Ordering::Relaxed)
        }

        fn add_fail_key(&self, key: String) {
            self.fail_keys.lock().unwrap().insert(key);
        }
    }

    #[async_trait::async_trait]
    impl CloudBackend for DelayingBackend {
        async fn upload_chunk(
            &self,
            key: &str,
            data: &[u8],
        ) -> CloudResult<(u64, Option<u64>, Option<CompressionAlgo>)> {
            let n = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            // Update max watermark with a compare-exchange loop —
            // multiple concurrent callers may all see the same `n` and
            // race to update.
            let mut cur = self.max_in_flight.load(Ordering::Acquire);
            while n > cur {
                match self.max_in_flight.compare_exchange(
                    cur,
                    n,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(v) => cur = v,
                }
            }
            tokio::time::sleep(self.delay).await;
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            if self.fail_keys.lock().unwrap().contains(key) {
                return Err(CloudError::Other(format!("injected failure for key {key}")));
            }
            self.inner.upload_chunk(key, data).await
        }

        async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> CloudResult<u64> {
            self.inner.upload_chunk_zerocopy(key, file_path).await
        }

        async fn download_chunk(&self, key: &str) -> CloudResult<Vec<u8>> {
            self.inner.download_chunk(key).await
        }

        async fn download_chunks_parallel(&self, keys: &[String]) -> CloudResult<Vec<Vec<u8>>> {
            self.inner.download_chunks_parallel(keys).await
        }

        async fn upload_manifest(&self, key: &str, json: &str) -> CloudResult<()> {
            self.inner.upload_manifest(key, json).await
        }

        async fn download_manifest(&self, key: &str) -> CloudResult<String> {
            self.inner.download_manifest(key).await
        }

        async fn chunk_exists(&self, key: &str) -> CloudResult<bool> {
            self.inner.chunk_exists(key).await
        }

        async fn list_objects(&self, key_prefix: &str) -> CloudResult<Vec<String>> {
            self.inner.list_objects(key_prefix).await
        }

        async fn delete_object(&self, key: &str) -> CloudResult<()> {
            self.inner.delete_object(key).await
        }

        fn backend_type(&self) -> &'static str {
            "delaying"
        }

        async fn lock_state(&self) -> CloudResult<LockState> {
            self.inner.lock_state().await
        }

        async fn set_object_legal_hold(&self, key: &str, held: bool) -> CloudResult<()> {
            self.inner.set_object_legal_hold(key, held).await
        }

        async fn get_object_legal_hold(&self, key: &str) -> CloudResult<bool> {
            self.inner.get_object_legal_hold(key).await
        }

        fn clone_box(&self) -> Box<dyn CloudBackend> {
            Box::new(DelayingBackend {
                inner: Arc::clone(&self.inner),
                delay: self.delay,
                in_flight: Arc::clone(&self.in_flight),
                max_in_flight: Arc::clone(&self.max_in_flight),
                fail_keys: Arc::clone(&self.fail_keys),
            })
        }
    }

    async fn fixture_cache_with_concurrency(
        size_bytes: u64,
        max_concurrent_flushes: usize,
        delay: Duration,
    ) -> (
        TempDir,
        Arc<PageCache>,
        Arc<VolumeWriter>,
        Arc<DelayingBackend>,
    ) {
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let local = LocalBackend::new(&cloud).await.unwrap();
        let local: Arc<dyn CloudBackend> = Arc::new(local);
        let delaying = DelayingBackend::new(Arc::clone(&local), delay);
        let backend: Arc<dyn CloudBackend> = Arc::clone(&delaying) as _;
        VolumeManifest::new(
            "vol1".into(),
            size_bytes,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "vol1", backend).unwrap());
        let cache = PageCache::with_budget_and_concurrency(
            Arc::clone(&writer),
            DEFAULT_CACHE_BUDGET_BYTES,
            max_concurrent_flushes,
        );
        (tmp, cache, writer, delaying)
    }

    #[tokio::test]
    async fn flush_all_runs_pages_in_parallel() {
        // 8 dirty pages, concurrency 8, 50 ms per upload. Serial:
        // ≥ 400 ms. Parallel: ≈ 50 ms. Allow 250 ms — comfortably
        // distinguishes the two without flaking on slow CI.
        let (_tmp, cache, _w, delaying) =
            fixture_cache_with_concurrency(16 * (1u64 << 20), 8, Duration::from_millis(50)).await;
        for i in 0..8u32 {
            cache
                .write_bytes(u64::from(i) * PAGE as u64, &pattern(i as u8, PAGE))
                .await
                .unwrap();
        }
        let start = Instant::now();
        cache.flush_all().await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "parallel drain took {elapsed:?} — should be near 50 ms"
        );
        assert!(
            delaying.observed_max_in_flight() >= 2,
            "expected ≥ 2 simultaneous uploads, saw {}",
            delaying.observed_max_in_flight()
        );
    }

    #[tokio::test]
    async fn flush_all_with_n1_matches_serial_behavior() {
        // Same workload as the parallel test, but n=1 must serialize.
        let (_tmp, cache, _w, delaying) =
            fixture_cache_with_concurrency(16 * (1u64 << 20), 1, Duration::from_millis(10)).await;
        for i in 0..4u32 {
            cache
                .write_bytes(u64::from(i) * PAGE as u64, &pattern(i as u8, PAGE))
                .await
                .unwrap();
        }
        cache.flush_all().await.unwrap();
        assert_eq!(
            delaying.observed_max_in_flight(),
            1,
            "n=1 must observe at most one upload in flight"
        );
    }

    #[tokio::test]
    async fn flush_pages_in_range_parallel_only_hits_range() {
        // Dirty 16 pages, synchronize bytes covering exactly pages
        // 4..=7, assert only those four go clean.
        let (_tmp, cache, _w, _delaying) =
            fixture_cache_with_concurrency(16 * (1u64 << 20), 8, Duration::from_millis(5)).await;
        for i in 0..16u32 {
            cache
                .write_bytes(u64::from(i) * PAGE as u64, &pattern(i as u8, PAGE))
                .await
                .unwrap();
        }
        let first_byte = 4 * PAGE as u64;
        let len = 4 * PAGE as u64;
        cache.synchronize_bytes(first_byte, len).await.unwrap();
        let inner = cache.inner.lock().await;
        for pid in 4u32..=7 {
            assert!(
                !inner.dirty.contains(&pid),
                "page {pid} should be clean after synchronize_bytes"
            );
        }
        for pid in (0u32..16).filter(|p| !(4..=7).contains(p)) {
            assert!(
                inner.dirty.contains(&pid),
                "page {pid} should still be dirty"
            );
        }
    }

    #[tokio::test]
    async fn eviction_releases_lock_during_cloud_upload() {
        // Regression test for the eviction lock-drop refactor.
        //
        // Setup: tiny 2-page budget + a backend that sleeps 200 ms
        // on every upload. Pre-fill pages 0 + 1 so the cache is at
        // budget with both pages dirty. Spawn a task that writes
        // page 2 — install_full_page must evict (the LRU dirty page,
        // i.e. page 0), and the eviction's `write_page_unsynced`
        // blocks for 200 ms in the cloud upload.
        //
        // During those 200 ms the inner mutex must be released so
        // the concurrent `read_bytes(page=1)` below — which only
        // needs to take the lock long enough to clone the cached
        // bytes — can complete promptly. Old eviction held the
        // lock through the upload; that path would make this read
        // block ≥ 200 ms.
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let local: Arc<dyn CloudBackend> = Arc::new(LocalBackend::new(&cloud).await.unwrap());
        let delaying = DelayingBackend::new(Arc::clone(&local), Duration::from_millis(200));
        let backend: Arc<dyn CloudBackend> = Arc::clone(&delaying) as _;
        VolumeManifest::new(
            "vol1".into(),
            8 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "vol1", backend).unwrap());
        let cache = PageCache::with_budget_and_concurrency(Arc::clone(&writer), 2 * PAGE as u64, 1);

        // Pre-fill pages 0 and 1 (both dirty, budget exhausted).
        cache.write_bytes(0, &pattern(0, PAGE)).await.unwrap();
        cache
            .write_bytes(PAGE as u64, &pattern(1, PAGE))
            .await
            .unwrap();

        // Kick off the write that forces eviction.
        let evicting_cache = Arc::clone(&cache);
        let evicting = tokio::spawn(async move {
            evicting_cache
                .write_bytes(2 * PAGE as u64, &pattern(2, PAGE))
                .await
                .unwrap();
        });

        // Give the spawned task a moment to enter `evict_to_fit`
        // and start the upload — once it has, the inner mutex must
        // be released.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let read_start = Instant::now();
        let bytes = cache.read_bytes(PAGE as u64, PAGE).await.unwrap();
        let read_elapsed = read_start.elapsed();
        assert_eq!(bytes, pattern(1, PAGE), "cached page 1 must be readable");
        assert!(
            read_elapsed < Duration::from_millis(100),
            "read_bytes took {read_elapsed:?} — eviction must release the lock through the cloud upload, not hold it"
        );

        evicting.await.unwrap();
    }

    #[tokio::test]
    async fn flush_propagates_first_error_but_completes_in_flight_peers() {
        // Inject a failure on the chunk page 3 produces. The other
        // pages must still go clean — `buffer_unordered` doesn't
        // cancel ready futures when one returns Err.
        let (_tmp, cache, writer, delaying) =
            fixture_cache_with_concurrency(16 * (1u64 << 20), 8, Duration::from_millis(5)).await;
        // Compute the exact local-scope cloud key for page 3's
        // contents now that the writer (and its volume uuid) exist.
        let page3_hash = blake3::hash(&pattern(3, PAGE)).to_hex().to_string();
        let fail_key = writer.pool().cloud_key(&page3_hash);
        delaying.add_fail_key(fail_key);

        for i in 0..8u32 {
            cache
                .write_bytes(u64::from(i) * PAGE as u64, &pattern(i as u8, PAGE))
                .await
                .unwrap();
        }
        let result = cache.flush_all().await;
        assert!(
            result.is_err(),
            "flush_all must propagate the injected failure"
        );
        let inner = cache.inner.lock().await;
        assert!(
            inner.dirty.contains(&3),
            "page 3 must still be dirty after the injected failure"
        );
        for pid in [0u32, 1, 2, 4, 5, 6, 7] {
            assert!(
                !inner.dirty.contains(&pid),
                "page {pid} should be clean (buffer_unordered does not cancel in-flight peers)"
            );
        }
    }
}
