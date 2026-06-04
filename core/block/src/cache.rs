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

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::stream::{self, StreamExt};
use tokio::sync::{Mutex, Notify};

use crate::page_index::{PageId, PageIndex};
use crate::runtime_state::VolumeRuntime;
use crate::upload_index::UploadState;
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
    /// Page body behind an `Arc` so the read path (and the
    /// eviction / flush snapshots) clone a pointer, not the whole
    /// 64 KiB page. In-place mutation goes through `Arc::make_mut`:
    /// when the body is uniquely held (the common case) it mutates
    /// without copying; when a reader still holds an off-lock clone
    /// it copies-on-write, so the reader keeps a consistent
    /// pre-mutation snapshot and never sees a torn page.
    bytes: Arc<Vec<u8>>,
    state: PageState,
    /// Monotonic write counter. Bumped on every dirtying mutation so
    /// a flush can detect that the bytes it captured are stale by
    /// the time it tries to mark the entry clean — in that case the
    /// page stays dirty and the next flush picks up the latest copy.
    version: u64,
    /// Intrusive LRU links. `newer` points toward the MRU (head) end
    /// — the neighbor used more recently than this page; `older`
    /// points toward the LRU (tail) end. The head's `newer` and the
    /// tail's `older` are both `None`. A page that is present in
    /// `pages` but absent from the list (transiently, e.g. just
    /// inserted) also has both `None` and is not the head — see
    /// [`CacheInner::lru_unlink`] for how that case is distinguished.
    newer: Option<PageId>,
    older: Option<PageId>,
}

struct CacheInner {
    /// Live page cache. `None` is never stored — pages are either
    /// in this map or absent (treated as unallocated when read
    /// without a prior load). Each entry carries its own intrusive
    /// LRU links (`newer` / `older`), so the recency list is the set
    /// of entries themselves — there is no parallel structure to keep
    /// in sync or to drift.
    pages: HashMap<PageId, CacheEntry>,
    /// MRU end of the intrusive LRU list (the most-recently-touched
    /// page). `None` iff the cache is empty.
    lru_head: Option<PageId>,
    /// LRU end of the intrusive LRU list (the eviction victim). `None`
    /// iff the cache is empty.
    ///
    /// An intrusive doubly-linked list replaces the former
    /// `VecDeque<PageId>` so that touch / remove are O(1) rather than
    /// an O(n) scan-and-shift. n is bounded by the page budget, which
    /// is ~1024 at the 64 KiB-page default but scales with
    /// `cache.budget_mb`: a multi-GiB budget puts tens of thousands of
    /// pages on the list, where an O(n) touch on every cache hit would
    /// dominate the read path. The clean-only eviction pick still
    /// walks from the tail (O(k) in the number of trailing dirty
    /// pages), unchanged from before.
    lru_tail: Option<PageId>,
    /// Set of dirty page ids — separate index so the flush worker
    /// doesn't walk every cached page on every tick.
    dirty: BTreeSet<PageId>,
}

impl CacheInner {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            lru_head: None,
            lru_tail: None,
            dirty: BTreeSet::new(),
        }
    }

    /// Detach `page_id` from the intrusive LRU list, patching its
    /// neighbors and the head / tail sentinels. Safe to call on a page
    /// that is absent or already detached — it is then a no-op (a
    /// detached entry is neither the head nor carries any link, so it
    /// cannot be confused with the sole linked element, whose links
    /// are also both `None` but which *is* the head). After return the
    /// entry's own links are cleared so a later push re-links cleanly.
    fn lru_unlink(&mut self, page_id: PageId) {
        let (newer, older) = match self.pages.get(&page_id) {
            Some(e) => (e.newer, e.older),
            None => return,
        };
        // Not in the list: no links and not the head. Bail before
        // touching head/tail, which would corrupt an otherwise valid
        // list.
        if newer.is_none() && older.is_none() && self.lru_head != Some(page_id) {
            return;
        }
        match newer {
            Some(n) => {
                if let Some(e) = self.pages.get_mut(&n) {
                    e.older = older;
                }
            }
            None => self.lru_head = older,
        }
        match older {
            Some(o) => {
                if let Some(e) = self.pages.get_mut(&o) {
                    e.newer = newer;
                }
            }
            None => self.lru_tail = newer,
        }
        if let Some(e) = self.pages.get_mut(&page_id) {
            e.newer = None;
            e.older = None;
        }
    }

    /// Link `page_id` at the MRU (head) end. The entry must already be
    /// present in `pages` and currently detached (both links `None`).
    fn lru_push_front(&mut self, page_id: PageId) {
        let old_head = self.lru_head;
        match self.pages.get_mut(&page_id) {
            Some(e) => {
                e.newer = None;
                e.older = old_head;
            }
            None => return,
        }
        match old_head {
            Some(h) => {
                if let Some(e) = self.pages.get_mut(&h) {
                    e.newer = Some(page_id);
                }
            }
            // List was empty — this page is now both head and tail.
            None => self.lru_tail = Some(page_id),
        }
        self.lru_head = Some(page_id);
    }

    /// Move `page_id` to the MRU end. O(1): a no-op fast path when the
    /// page is already the head, otherwise an unlink + push-front.
    /// Correct whether the page was already linked or freshly inserted
    /// and still detached (`lru_unlink` no-ops on the latter).
    fn lru_touch(&mut self, page_id: PageId) {
        if self.lru_head == Some(page_id) {
            return;
        }
        self.lru_unlink(page_id);
        self.lru_push_front(page_id);
    }

    /// Drop a cached page entry and clear both side indexes (LRU +
    /// dirty set). Centralizes the invariant that every entry removal
    /// keeps `pages`, the LRU list, and `dirty` consistent. Unlinks
    /// before removing from `pages` because the LRU links live inside
    /// the entry. Returns whether an entry was actually present.
    fn drop_entry(&mut self, page_id: PageId) -> bool {
        if self.pages.contains_key(&page_id) {
            self.lru_unlink(page_id);
            self.pages.remove(&page_id);
            self.dirty.remove(&page_id);
            true
        } else {
            false
        }
    }

    /// Find the LRU page id, optionally restricted to clean entries.
    /// Walks from the tail (LRU end) toward the head. Returns `None`
    /// if the cache is empty (or no clean entries when `clean_only`).
    fn lru_pick(&self, clean_only: bool) -> Option<PageId> {
        let mut cur = self.lru_tail;
        while let Some(pid) = cur {
            let entry = self.pages.get(&pid)?;
            if !clean_only || entry.state == PageState::Clean {
                return Some(pid);
            }
            cur = entry.newer;
        }
        None
    }

    /// Test-only: collect the LRU order, head (MRU) first, walking via
    /// the `older` links. Lets the unit tests assert the intrusive
    /// list's shape without reaching into private link fields.
    #[cfg(test)]
    fn lru_order(&self) -> Vec<PageId> {
        let mut out = Vec::new();
        let mut cur = self.lru_head;
        // Bound the walk by the entry count so a (bug-induced) cycle
        // can't loop forever; an over-long walk yields a wrong vec
        // that the caller's assertion then flags.
        while let Some(pid) = cur {
            out.push(pid);
            if out.len() > self.pages.len() {
                break;
            }
            cur = self.pages.get(&pid).and_then(|e| e.older);
        }
        out
    }
}

/// Error from [`PageCache::resolve_range`]: an LBA range that can't be
/// mapped to a valid byte window in this volume. The SBC and NVMe data
/// paths translate it to their respective sense / status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    /// Sector size is zero — volume manifest is malformed / not ready.
    BadSectorSize,
    /// `lba + blocks` overflows u64 or runs past the end of the volume.
    OutOfRange,
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
    /// [`shared_object_store::UploadConfig::resolve_max_concurrent`] at boot
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
    /// dispatcher which does range checking in byte space. Reads the
    /// writer's live shadow, not the boot-snapshot manifest, so an
    /// online `volume resize` is visible without a restart (issue #76).
    pub fn size_bytes(&self) -> u64 {
        self.writer.size_bytes()
    }

    /// Resolve an LBA range to a `(byte_offset, byte_len)` window,
    /// overflow-safe and bounds-checked against the volume size.
    ///
    /// Centralizes the LBA -> byte-offset invariant shared by the SBC
    /// (`scsi-sbc`) and NVMe (`nvme-nvm`) data paths — both used to
    /// re-derive sizing + an overflow-safe range check independently,
    /// so a sizing bug could appear in one path but not the other. A
    /// zero-block request resolves to a zero-length window at the
    /// range's byte offset.
    pub fn resolve_range(&self, lba: u64, blocks: u64) -> Result<(u64, u64), RangeError> {
        let sector = self.sector_size();
        if sector == 0 {
            return Err(RangeError::BadSectorSize);
        }
        let total_blocks = self.size_bytes() / sector;
        let end = lba.checked_add(blocks).ok_or(RangeError::OutOfRange)?;
        if end > total_blocks {
            return Err(RangeError::OutOfRange);
        }
        // lba <= total_blocks and total_blocks * sector <= size_bytes,
        // so neither multiply can overflow; checked for clarity.
        let byte_off = lba.checked_mul(sector).ok_or(RangeError::OutOfRange)?;
        let len = blocks.checked_mul(sector).ok_or(RangeError::OutOfRange)?;
        Ok((byte_off, len))
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
                self.install_full_page(page_id, Arc::new(slice.to_vec()))
                    .await?;
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

    /// Same-volume page-aligned clone primitive — backing for the
    /// SBC-3 EXTENDED COPY (XCOPY) fast path. Copies `len` bytes
    /// from `src_byte_offset` to `dst_byte_offset`, skipping the
    /// chunk-pool round trip when the source page is clean and its
    /// chunk is already in cloud: the destination's page-index entry
    /// is rebound to the source's chunk hash, the cached destination
    /// page (if any) is invalidated, and the pool's natural dedup
    /// means no new bytes hit cloud.
    ///
    /// All three offsets and `len` must be whole multiples of
    /// [`Self::page_size`]; the caller is responsible for range
    /// validation against the volume and for ensuring the source
    /// and destination ranges do not overlap (overlap forces the
    /// SCSI layer down its bytes-copy slow path).
    ///
    /// Three per-page cases:
    /// 1. Source dirty in cache — copy the cached bytes into a new
    ///    dirty entry at the destination. One page-size memcpy, no
    ///    cloud IO.
    /// 2. Source clean and upload state is `Uploaded` — rebind
    ///    the destination's page-index entry to the source's chunk
    ///    hash (or clear it when the source is a sparse hole) and
    ///    drop the destination from cache. Zero data IO. The chunk
    ///    now has two page-index references; GC reclaims it only
    ///    once both go away.
    /// 3. Source clean but upload state is `LocalOnly` (chunk not
    ///    yet acknowledged by cloud) — fall back to a full
    ///    bytes-copy. The chunk isn't safe to alias yet because
    ///    the destination's pending-upload tracker doesn't share
    ///    the source's PUT.
    ///
    /// Counts as host-written bytes (the destination range was
    /// "written" from the host's perspective), but does not bump
    /// the cloud-PUT counter on the fast path — no new bytes
    /// crossed the pool boundary.
    pub async fn clone_page_range(
        &self,
        src_byte_offset: u64,
        dst_byte_offset: u64,
        len: u64,
    ) -> Result<(), UploaderError> {
        self.clone_page_range_into(src_byte_offset, self, dst_byte_offset, len)
            .await
    }

    /// Cross-volume variant of [`Self::clone_page_range`]: clone
    /// `len` bytes from `self` (source) into `dst` (destination)
    /// without round-tripping through host memory when every per-page
    /// case allows hash-index rebinding. The receiver is the source —
    /// `self.clone_page_range_into(src_off, &dst, dst_off, len)` reads
    /// like "self into dst at dst_off."
    ///
    /// Powers two callers:
    /// - VAAI XCOPY same-volume fast path — delegated through
    ///   [`Self::clone_page_range`] with `dst = self`.
    /// - Hyper-V ODX `WRITE USING TOKEN` cross-volume fast path —
    ///   `src` and `dst` are distinct `PageCache` instances on the
    ///   same backend.
    ///
    /// Cross-volume hash rebind is only safe when source and
    /// destination share the same chunk pool (same backend +
    /// matching dedup-scope namespace), since the destination's
    /// later reads resolve the rebound hash from `dst`'s pool. When
    /// either constraint fails (different backends, mismatched
    /// `DedupScope::Local` namespaces), the per-page logic falls
    /// back to a full bytes copy through host memory; correctness is
    /// preserved at the cost of going off-fast-path.
    ///
    /// Returns [`UploaderError::IncompatiblePageSize`] when the two
    /// caches differ on `page_size_bytes` — there is no meaningful
    /// per-page mapping in that case and the caller must shape its
    /// own bytes-copy fallback.
    pub async fn clone_page_range_into(
        &self,
        src_byte_offset: u64,
        dst: &PageCache,
        dst_byte_offset: u64,
        len: u64,
    ) -> Result<(), UploaderError> {
        if self.page_size != dst.page_size {
            return Err(UploaderError::IncompatiblePageSize {
                src: self.page_size as u32,
                dst: dst.page_size as u32,
            });
        }
        debug_assert_eq!(src_byte_offset % self.page_size, 0);
        debug_assert_eq!(dst_byte_offset % dst.page_size, 0);
        debug_assert_eq!(len % self.page_size, 0);
        if len == 0 {
            return Ok(());
        }
        let pages = len / self.page_size;
        let src_first_u64 = src_byte_offset / self.page_size;
        let dst_first_u64 = dst_byte_offset / dst.page_size;
        for i in 0..pages {
            let src_id =
                u32::try_from(src_first_u64 + i).map_err(|_| UploaderError::PageOutOfRange {
                    page_id: src_first_u64 + i,
                    page_size: self.page_size as u32,
                    size_bytes: self.size_bytes(),
                })?;
            let dst_id =
                u32::try_from(dst_first_u64 + i).map_err(|_| UploaderError::PageOutOfRange {
                    page_id: dst_first_u64 + i,
                    page_size: dst.page_size as u32,
                    size_bytes: dst.size_bytes(),
                })?;
            clone_one_page_into(self, src_id, dst, dst_id).await?;
        }
        dst.bump_host_bytes(len);
        Ok(())
    }

    /// SBC-3 SYNCHRONIZE CACHE primitive. Drains the pipeline to
    /// the operator-chosen [`SyncAfter`] tier (mutable via
    /// `thurvsa volume modify --sync-after <MODE>`):
    ///
    /// - [`SyncAfter::Storage`] (default) — flush dirty cache pages
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
        if matches!(self.writer.sync_after(), SyncAfter::Storage) {
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

    /// Freeze this volume's `pages.idx` into `dst_pages_idx` — the
    /// frozen page table a snapshot keeps so copy-on-write chunks stay
    /// reclaimable (issue #13). The snapshot references the same chunks
    /// as the parent; nothing in the pool is copied.
    ///
    /// The frozen index must reference only **cloud-uploaded** chunks:
    /// a snapshot-only chunk (one the parent later overwrites) may have
    /// its local copy evicted and be refetched from cloud on read, so
    /// it must be cloud-durable. The sequence guarantees that:
    ///
    /// 1. [`Self::flush_all`] drains dirty cache pages into the pool +
    ///    page index and awaits every pending cloud PUT, so the
    ///    snapshot captures all daemon-cached writes (crash-consistent;
    ///    the host fsyncs / fs-freezes for application consistency) and
    ///    every currently-referenced chunk is uploaded.
    /// 2. Holding the inner lock blocks new host writes and stops the
    ///    flush worker (it picks its batch under this lock), so
    ///    `pages.idx` can gain no new chunk reference while we copy.
    /// 3. A second pending-upload drain, now under the lock, covers a
    ///    flush that raced in between (1) and (2); no new flush can
    ///    start, so it terminates. Afterwards every record references
    ///    an uploaded chunk.
    /// 4. fdatasync + byte-copy. A 64-byte index record never spans a
    ///    page, so the copy stays structurally clean even against a
    ///    concurrent UNMAP `clear` (removes a reference) or ODX rebind
    ///    (binds an already-uploaded chunk) — both crash-consistent
    ///    outcomes for the snapshot.
    ///
    /// The copy runs on the blocking pool but the inner lock is held
    /// across it: the volume's host I/O pauses for the copy. `pages.idx`
    /// is sparse, so the cost scales with allocated pages, and
    /// `std::fs::copy` reflinks on btrfs/xfs/zfs.
    pub async fn snapshot_pages_idx(&self, dst_pages_idx: PathBuf) -> Result<(), UploaderError> {
        self.flush_all().await?;
        let _freeze = self.inner.lock().await;
        self.writer
            .pending_uploads()
            .wait_for_range(PageId::MIN..=PageId::MAX)
            .await;
        self.writer.page_index_sync()?;
        let src = PageIndex::path_for(&VolumeManifest::dir_for(
            self.writer.data_dir(),
            &self.manifest().name,
        ));
        tokio::task::spawn_blocking(move || copy_pages_idx(&src, &dst_pages_idx))
            .await
            .map_err(|e| UploaderError::Io(std::io::Error::other(e.to_string())))??;
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

    /// Return the page's current bytes (as a shared `Arc`), populating
    /// the cache from the writer if necessary. Cloning the returned
    /// handle is a refcount bump, not a 64 KiB copy. Unallocated pages
    /// materialize as a zeroed buffer (and stay marked Clean — the
    /// page index entry is still absent).
    async fn acquire_page(&self, page_id: PageId) -> Result<Arc<Vec<u8>>, UploaderError> {
        // Hot path: cache hit.
        {
            let mut inner = self.inner.lock().await;
            if let Some(entry) = inner.pages.get(&page_id) {
                let bytes = Arc::clone(&entry.bytes);
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
        let bytes = Arc::new(bytes);

        // Make room for the new entry. `evict_to_fit` releases the
        // inner lock during any required cloud upload, so concurrent
        // flushes don't stall on this path.
        self.evict_to_fit(1).await?;

        // Re-acquire the lock to commit. If another loader populated
        // the slot between our miss and our insert, prefer their
        // (possibly Dirty) bytes — those are the live state.
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.pages.get(&page_id) {
            let cached = Arc::clone(&entry.bytes);
            inner.lru_touch(page_id);
            return Ok(cached);
        }
        inner.pages.insert(
            page_id,
            CacheEntry {
                bytes: Arc::clone(&bytes),
                state: PageState::Clean,
                version: 0,
                newer: None,
                older: None,
            },
        );
        inner.lru_touch(page_id);
        Ok(bytes)
    }

    /// Install a full page's worth of new bytes into the cache and
    /// mark it dirty. Skips the load step (no need to RMW a page
    /// when the host wrote the whole thing).
    async fn install_full_page(
        &self,
        page_id: PageId,
        bytes: Arc<Vec<u8>>,
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
            bytes: Arc::new(Vec::new()),
            state: PageState::Clean,
            version: 0,
            newer: None,
            older: None,
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
        // Fast path: already in cache. `Arc::make_mut` mutates the
        // body in place when it is uniquely held (the common case);
        // if an off-lock reader still holds a clone it copies-on-write
        // so that reader keeps its consistent snapshot.
        {
            let mut inner = self.inner.lock().await;
            if let Some(entry) = inner.pages.get_mut(&page_id) {
                Arc::make_mut(&mut entry.bytes)[offset..offset + bytes.len()]
                    .copy_from_slice(bytes);
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
        Arc::make_mut(&mut entry.bytes)[offset..offset + bytes.len()].copy_from_slice(bytes);
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
            inner.drop_entry(page_id);
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
            // a dirty-page snapshot for the off-lock flush below. The
            // dirty snapshot is an `Arc` clone (a refcount bump), so
            // the lock is held only long enough to pick the victim.
            enum Step {
                Done,
                Cleaned,
                Dirty(PageId, Arc<Vec<u8>>, u64),
            }
            let step = {
                let mut inner = self.inner.lock().await;
                if inner.pages.len() + wanted <= self.budget_pages {
                    Step::Done
                } else if let Some(pid) = inner.lru_pick(true) {
                    inner.drop_entry(pid);
                    Step::Cleaned
                } else if let Some(pid) = inner.lru_pick(false) {
                    match inner.pages.get(&pid) {
                        Some(entry) => Step::Dirty(pid, Arc::clone(&entry.bytes), entry.version),
                        None => {
                            // LRU pointed at a missing entry —
                            // structural drift, clean up and retry.
                            inner.lru_unlink(pid);
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
                    match self.writer.write_page_unsynced(pid, bytes.as_slice()).await {
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
                                    inner.drop_entry(pid);
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
                                    // already — fine. `lru_unlink`
                                    // no-ops on an absent entry.
                                    inner.lru_unlink(pid);
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
    pub async fn flush_pages_in_range(
        &self,
        first: PageId,
        last: PageId,
    ) -> Result<(), UploaderError> {
        self.flush_drain(move |dirty, n| dirty.range(first..=last).take(n).copied().collect())
            .await
    }

    /// Drop a single cached page entry without going through the
    /// dirty-flush path. Caller asserts the page's on-disk state
    /// has been authoritatively rewritten (e.g. ODX `WRITE USING
    /// TOKEN` rebound `pages.idx[page_id]` to a different hash) and
    /// that any cached bytes would be stale.
    pub async fn invalidate_cached_page(&self, page_id: PageId) {
        let mut inner = self.inner.lock().await;
        inner.drop_entry(page_id);
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
                    (Arc::clone(&entry.bytes), entry.version)
                }
                _ => return Ok(false),
            }
        };
        match self
            .writer
            .write_page_unsynced(page_id, bytes.as_slice())
            .await
        {
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

/// Copy a frozen `pages.idx` to `dst` and fsync it (and its parent
/// directory) so a snapshot survives a crash. `std::fs::copy` uses
/// `copy_file_range(2)` where the filesystem supports it (reflink on
/// btrfs/xfs/zfs), falling back to a buffered copy elsewhere — either
/// way a single 64-byte record is copied atomically w.r.t. a
/// concurrent single-record write (records never cross a page).
fn copy_pages_idx(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst)?;
    std::fs::File::open(dst)?.sync_all()?;
    if let Some(parent) = dst.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Per-page clone shared by [`PageCache::clone_page_range`] (same
/// volume, `src` and `dst` are the same `Arc`) and the cross-volume
/// ODX path (distinct volumes on the same backend + matching pool
/// namespace). Three case branches:
///
/// 1. **Source dirty in cache** — copy the cached bytes and install
///    them as a dirty page on the destination. One memcpy, no pool
///    or backend IO.
/// 2. **Source clean and `Uploaded`, same pool namespace** — rebind
///    `dst`'s page-index slot to the source's chunk hash and mark
///    the destination's upload state `Uploaded`. Zero data IO; the
///    chunk now has two page-index references.
/// 3. **Source `LocalOnly` (pending upload) OR cross-pool mismatch**
///    — full bytes copy through `acquire_page` + `install_full_page`.
///    Cross-pool happens when the two volumes are scoped
///    `DedupScope::Local` to different namespaces; the hash isn't
///    reachable from `dst`'s pool, so a rebind would leave the
///    destination unable to read the chunk back.
async fn clone_one_page_into(
    src: &PageCache,
    src_id: PageId,
    dst: &PageCache,
    dst_id: PageId,
) -> Result<(), UploaderError> {
    if std::ptr::eq(src, dst) && src_id == dst_id {
        return Ok(());
    }
    // Case 1: source dirty in cache. The snapshot is an `Arc` clone
    // (refcount bump) shared straight into the destination entry.
    let src_dirty_bytes = {
        let inner = src.inner.lock().await;
        match inner.pages.get(&src_id) {
            Some(e) if e.state == PageState::Dirty => Some(Arc::clone(&e.bytes)),
            _ => None,
        }
    };
    if let Some(bytes) = src_dirty_bytes {
        dst.install_full_page(dst_id, bytes).await?;
        return Ok(());
    }
    // Case 3 trigger A — source not yet acked by backend; bytes copy.
    let src_upload_state = src.writer.upload_index().read(src_id)?;
    if !matches!(src_upload_state, UploadState::Uploaded) {
        let bytes = src.acquire_page(src_id).await?;
        dst.install_full_page(dst_id, bytes).await?;
        return Ok(());
    }
    // Case 3 trigger B — cross-pool: source and destination live in
    // distinct chunk pools (different backend or different Local
    // namespace), so the source's hash isn't addressable from
    // `dst.pool`. Fall back to bytes copy.
    let same_pool = src.writer.manifest().backend == dst.writer.manifest().backend
        && src.writer.manifest().pool_namespace() == dst.writer.manifest().pool_namespace();
    if !same_pool {
        let bytes = src.acquire_page(src_id).await?;
        dst.install_full_page(dst_id, bytes).await?;
        return Ok(());
    }
    // Case 2: hash rebind. Drop any cached destination entry so the
    // next host read repopulates from the freshly-bound page-index
    // entry instead of returning stale cached bytes.
    {
        let mut inner = dst.inner.lock().await;
        inner.drop_entry(dst_id);
    }
    match src.writer.page_index().get_entry(src_id)? {
        Some(entry) => {
            // Carry the source page's IV salt across with the hash
            // (issue #87): the rebound ciphertext was sealed under
            // `derive_iv(crypto_uuid, src_id, iv_salt)`, so an encrypted
            // destination that shares the source's crypto identity (a
            // clone) reads it back with the matching nonce only if the
            // salt travels with the hash.
            dst.writer
                .page_index()
                .set_salted(dst_id, &entry.hash, entry.iv_salt)?;
            dst.writer
                .upload_index()
                .set(dst_id, UploadState::Uploaded)?;
        }
        None => {
            // Source is a sparse hole; destination becomes one too.
            // SBC-3 §5.7 reads-as-zero applies.
            dst.writer.page_index().clear(dst_id)?;
            dst.writer
                .upload_index()
                .set(dst_id, UploadState::Uploaded)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use crate::{DedupScope, VolumeManifest};
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use tempfile::TempDir;

    const PAGE: usize = DEFAULT_PAGE_SIZE_BYTES as usize;
    const SECTOR: usize = DEFAULT_SECTOR_BYTES as usize;
    const SECTORS_PER_PAGE: usize = PAGE / SECTOR;

    async fn fixture_cache(size_bytes: u64) -> (TempDir, Arc<PageCache>, Arc<VolumeWriter>) {
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let backend = LocalBackend::new(&cloud).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
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

    /// Snapshot freeze (issue #13): `snapshot_pages_idx` copies the page
    /// table at a point in time, references only uploaded chunks, and
    /// stays frozen when the parent later overwrites the same page.
    #[tokio::test]
    async fn snapshot_freezes_index_and_references_uploaded_chunks() {
        let (tmp, cache, writer) = fixture_cache(4 * (1u64 << 20)).await;

        // Write page 0 (pattern A), flush.
        cache.write_bytes(0, &pattern(0xA1, PAGE)).await.unwrap();
        cache.flush_all().await.unwrap();
        let hash_a = writer.page_index().get(0).unwrap().expect("page 0 sealed");

        // Freeze the index into a snapshot directory.
        let snap_dir = tmp.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        let dst = PageIndex::path_for(&snap_dir);
        cache.snapshot_pages_idx(dst.clone()).await.unwrap();

        // Load-bearing invariant: the snapshot's chunk is cloud-uploaded
        // (so eviction may safely drop its local copy and refetch).
        assert!(
            matches!(
                writer.upload_index().read(0).unwrap(),
                UploadState::Uploaded
            ),
            "snapshot-create must leave every referenced chunk Uploaded"
        );

        // The frozen index maps page 0 to the same chunk as the live one.
        let frozen = PageIndex::open(
            &dst,
            cache.manifest().uuid,
            u64::from(cache.manifest().page_size_bytes),
        )
        .unwrap();
        assert_eq!(frozen.get(0).unwrap(), Some(hash_a));

        // Parent overwrites page 0 (pattern B): the live index moves to a
        // new chunk; the frozen snapshot index keeps the old one — this
        // is the copy-on-write divergence.
        cache.write_bytes(0, &pattern(0xB2, PAGE)).await.unwrap();
        cache.flush_all().await.unwrap();
        let hash_b = writer.page_index().get(0).unwrap().unwrap();
        assert_ne!(hash_a, hash_b, "overwrite seals a distinct chunk");
        assert_eq!(
            frozen.get(0).unwrap(),
            Some(hash_a),
            "the snapshot's frozen index does not follow the parent"
        );
    }

    #[tokio::test]
    async fn resolve_range_tracks_live_size_after_resize() {
        // 4 MiB / 4 KiB sector = 1024 blocks (LBA 0..1023).
        let (_tmp, cache, writer) = fixture_cache(4 * (1u64 << 20)).await;
        assert_eq!(cache.size_bytes(), 4 * (1u64 << 20));
        assert!(matches!(
            cache.resolve_range(1024, 1),
            Err(RangeError::OutOfRange)
        ));

        // The cache reads the writer's live shadow — a grow is visible
        // to the data-path range gate immediately, no new PageCache.
        writer.set_size(8 * (1u64 << 20)).unwrap();
        assert_eq!(cache.size_bytes(), 8 * (1u64 << 20));
        assert!(cache.resolve_range(1024, 1).is_ok());
        assert!(matches!(
            cache.resolve_range(2048, 1),
            Err(RangeError::OutOfRange)
        ));
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
        let backend: Arc<dyn ObjectStoreBackend> =
            Arc::new(LocalBackend::new(&cloud).await.unwrap());
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
        cache
            .write_bytes(PAGE as u64, &pattern(0x22, PAGE))
            .await
            .unwrap();
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

    use shared_object_store::compression::CompressionAlgo;
    use shared_object_store::object_store_backend::LockState;
    use shared_object_store::{ObjectStoreError, Result as CloudResult};
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// Test-only `ObjectStoreBackend` decorator. Sleeps `delay` before
    /// every `upload_chunk` call so the test can observe serial vs
    /// parallel drain timing; tracks max simultaneous in-flight
    /// `upload_chunk` callers; fails any call whose key is in the
    /// shared `fail_keys` set (mutable post-construction so tests
    /// can compute the exact cloud key from the live writer's pool
    /// and inject it). Every other trait method delegates unchanged.
    #[derive(Debug)]
    struct DelayingBackend {
        inner: Arc<dyn ObjectStoreBackend>,
        delay: Duration,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        fail_keys: Arc<std::sync::Mutex<HashSet<String>>>,
    }

    impl DelayingBackend {
        fn new(inner: Arc<dyn ObjectStoreBackend>, delay: Duration) -> Arc<Self> {
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
    impl ObjectStoreBackend for DelayingBackend {
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
                return Err(ObjectStoreError::Other(format!(
                    "injected failure for key {key}"
                )));
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

        fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
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
        let local: Arc<dyn ObjectStoreBackend> = Arc::new(local);
        let delaying = DelayingBackend::new(Arc::clone(&local), delay);
        let backend: Arc<dyn ObjectStoreBackend> = Arc::clone(&delaying) as _;
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
        let local: Arc<dyn ObjectStoreBackend> = Arc::new(LocalBackend::new(&cloud).await.unwrap());
        let delaying = DelayingBackend::new(Arc::clone(&local), Duration::from_millis(200));
        let backend: Arc<dyn ObjectStoreBackend> = Arc::clone(&delaying) as _;
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
        let fail_key = writer.pool().object_key(&page3_hash);
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

    // ─────────────────────── clone_page_range coverage ───────────────────────

    #[tokio::test]
    async fn clone_page_range_dirty_source_copies_bytes() {
        // Source is dirty in cache (not yet flushed); clone should
        // snapshot the cached bytes into the destination.
        let (_tmp, cache, writer) = fixture_cache(8 * (1u64 << 20)).await;
        let bytes = pattern(0x5A, PAGE);
        cache.write_bytes(0, &bytes).await.unwrap();
        // Page 0 is dirty (no SYNC). Clone page 0 → page 4.
        let dst_off = 4 * PAGE as u64;
        cache
            .clone_page_range(0, dst_off, PAGE as u64)
            .await
            .unwrap();
        // Source still readable.
        let s = cache.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(s, bytes);
        // Destination matches.
        let d = cache.read_bytes(dst_off, PAGE).await.unwrap();
        assert_eq!(d, bytes);
        // Page index entries are still empty (both pages dirty in
        // cache, neither has flushed).
        assert!(writer.page_index().get(0).unwrap().is_none());
        assert!(writer.page_index().get(4).unwrap().is_none());
    }

    #[tokio::test]
    async fn clone_page_range_clean_source_takes_hash_fast_path() {
        // Flush the source so it is clean + Uploaded; clone should
        // bind the destination's page-index entry to the same hash
        // and leave the destination uncached.
        let (_tmp, cache, writer) = fixture_cache(8 * (1u64 << 20)).await;
        let bytes = pattern(0xC3, PAGE);
        cache.write_bytes(0, &bytes).await.unwrap();
        cache.flush_all().await.unwrap();
        let src_hash = writer.page_index().get(0).unwrap().expect("seeded");
        // Clone page 0 → page 7.
        cache
            .clone_page_range(0, 7 * PAGE as u64, PAGE as u64)
            .await
            .unwrap();
        // Destination's page index points at the same chunk hash.
        let dst_hash = writer.page_index().get(7).unwrap().expect("cloned");
        assert_eq!(src_hash, dst_hash);
        // Reading the destination returns the same bytes (resolved
        // via the shared chunk).
        let d = cache.read_bytes(7 * PAGE as u64, PAGE).await.unwrap();
        assert_eq!(d, bytes);
    }

    #[tokio::test]
    async fn clone_page_range_sparse_hole_source_clears_destination() {
        // Destination starts non-empty (gets seeded then flushed);
        // cloning from an unallocated source page must clear it back
        // to a sparse hole that reads as zero.
        let (_tmp, cache, writer) = fixture_cache(8 * (1u64 << 20)).await;
        let seed = pattern(0x77, PAGE);
        cache.write_bytes(PAGE as u64, &seed).await.unwrap();
        cache.flush_all().await.unwrap();
        assert!(writer.page_index().get(1).unwrap().is_some());
        // Clone page 0 (sparse) → page 1 (seeded).
        cache
            .clone_page_range(0, PAGE as u64, PAGE as u64)
            .await
            .unwrap();
        assert!(writer.page_index().get(1).unwrap().is_none());
        let d = cache.read_bytes(PAGE as u64, PAGE).await.unwrap();
        assert!(d.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn clone_page_range_evicts_stale_destination_cache_entry() {
        // Destination had its own writes (clean in cache + indexed),
        // then gets cloned over. Subsequent reads must reflect the
        // source bytes, not the stale destination bytes the cache
        // still held.
        let (_tmp, cache, _w) = fixture_cache(8 * (1u64 << 20)).await;
        let src_bytes = pattern(0xAA, PAGE);
        let dst_bytes = pattern(0xBB, PAGE);
        cache.write_bytes(0, &src_bytes).await.unwrap();
        cache.write_bytes(PAGE as u64, &dst_bytes).await.unwrap();
        cache.flush_all().await.unwrap();
        // Warm the cache by reading the destination back.
        let dst_warm = cache.read_bytes(PAGE as u64, PAGE).await.unwrap();
        assert_eq!(dst_warm, dst_bytes);
        // Clone src → dst.
        cache
            .clone_page_range(0, PAGE as u64, PAGE as u64)
            .await
            .unwrap();
        // Destination must now read as the source bytes.
        let dst_after = cache.read_bytes(PAGE as u64, PAGE).await.unwrap();
        assert_eq!(dst_after, src_bytes);
    }

    #[tokio::test]
    async fn clone_page_range_multi_page_round_trip() {
        let (_tmp, cache, _w) = fixture_cache(16 * (1u64 << 20)).await;
        // Seed four pages with distinct patterns and flush.
        let seeded: Vec<Vec<u8>> = (0..4).map(|i| pattern(0x10 + i as u8, PAGE)).collect();
        for (i, bytes) in seeded.iter().enumerate() {
            cache
                .write_bytes((i as u64) * PAGE as u64, bytes)
                .await
                .unwrap();
        }
        cache.flush_all().await.unwrap();
        // Clone pages 0..4 → pages 8..12.
        let dst_off = 8 * PAGE as u64;
        cache
            .clone_page_range(0, dst_off, 4 * PAGE as u64)
            .await
            .unwrap();
        for (i, bytes) in seeded.iter().enumerate() {
            let read = cache
                .read_bytes(dst_off + (i as u64) * PAGE as u64, PAGE)
                .await
                .unwrap();
            assert_eq!(&read, bytes, "page {i} clone mismatch");
        }
    }

    #[tokio::test]
    async fn clone_page_range_same_src_dst_is_noop() {
        let (_tmp, cache, writer) = fixture_cache(4 * (1u64 << 20)).await;
        let bytes = pattern(0x33, PAGE);
        cache.write_bytes(0, &bytes).await.unwrap();
        cache.flush_all().await.unwrap();
        let before = writer.page_index().get(0).unwrap();
        cache.clone_page_range(0, 0, PAGE as u64).await.unwrap();
        let after = writer.page_index().get(0).unwrap();
        assert_eq!(before, after);
        let r = cache.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(r, bytes);
    }

    #[tokio::test]
    async fn clone_page_range_zero_length_is_noop() {
        let (_tmp, cache, _w) = fixture_cache(4 * (1u64 << 20)).await;
        cache.clone_page_range(0, PAGE as u64, 0).await.unwrap();
        assert_eq!(cache.host_bytes_written(), 0);
    }

    #[tokio::test]
    async fn clone_page_range_bumps_host_bytes_written() {
        let (_tmp, cache, _w) = fixture_cache(8 * (1u64 << 20)).await;
        let bytes = pattern(0x42, PAGE);
        cache.write_bytes(0, &bytes).await.unwrap();
        let before = cache.host_bytes_written();
        cache
            .clone_page_range(0, PAGE as u64, PAGE as u64)
            .await
            .unwrap();
        assert_eq!(cache.host_bytes_written(), before + PAGE as u64);
    }

    /// Build two volumes against one shared backend root + one shared
    /// `tmp/` data dir. `dedup` controls whether they share the pool
    /// (Global) or each get their own namespaced sub-pool (Local).
    async fn fixture_two_caches(
        size_bytes: u64,
        dedup: DedupScope,
    ) -> (
        TempDir,
        Arc<PageCache>,
        Arc<PageCache>,
        Arc<VolumeWriter>,
        Arc<VolumeWriter>,
    ) {
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let backend = LocalBackend::new(&cloud).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        for name in ["src_vol", "dst_vol"] {
            VolumeManifest::new(
                name.into(),
                size_bytes,
                DEFAULT_SECTOR_BYTES,
                DEFAULT_PAGE_SIZE_BYTES,
                "primary".into(),
                dedup,
                false,
                0,
            )
            .unwrap()
            .create(tmp.path())
            .unwrap();
        }
        let w_src = Arc::new(VolumeWriter::open(tmp.path(), "src_vol", backend.clone()).unwrap());
        let w_dst = Arc::new(VolumeWriter::open(tmp.path(), "dst_vol", backend).unwrap());
        let c_src = PageCache::new(w_src.clone());
        let c_dst = PageCache::new(w_dst.clone());
        (tmp, c_src, c_dst, w_src, w_dst)
    }

    #[tokio::test]
    async fn clone_page_range_into_cross_volume_global_takes_hash_fast_path() {
        // Global dedup: both volumes share the per-backend pool.
        // Cross-volume clone must rebind hashes without touching cloud
        // bytes — verified by reading the rebound page-index slot.
        let (_tmp, c_src, c_dst, w_src, w_dst) =
            fixture_two_caches(8 * (1u64 << 20), DedupScope::Global).await;
        let bytes = pattern(0x77, PAGE);
        c_src.write_bytes(0, &bytes).await.unwrap();
        c_src.flush_all().await.unwrap();
        let src_hash = w_src.page_index().get(0).unwrap().unwrap();

        c_src
            .clone_page_range_into(0, &c_dst, PAGE as u64, PAGE as u64)
            .await
            .unwrap();

        let dst_hash = w_dst.page_index().get(1).unwrap().unwrap();
        assert_eq!(
            dst_hash, src_hash,
            "Global-scope cross-vol clone must rebind to the source's hash"
        );
        let read = c_dst.read_bytes(PAGE as u64, PAGE).await.unwrap();
        assert_eq!(read, bytes);
    }

    #[tokio::test]
    async fn clone_page_range_into_cross_volume_local_falls_back_to_bytes_copy() {
        // Local dedup: each volume gets its own namespaced sub-pool,
        // so the source's hash isn't reachable from the destination's
        // pool — the per-page helper must fall back to a bytes copy
        // (correctness is what matters; the dst still ends up with
        // identical bytes, just routed through host memory).
        let (_tmp, c_src, c_dst, _w_src, _w_dst) =
            fixture_two_caches(8 * (1u64 << 20), DedupScope::Local).await;
        let bytes = pattern(0x99, PAGE);
        c_src.write_bytes(0, &bytes).await.unwrap();
        c_src.flush_all().await.unwrap();

        c_src
            .clone_page_range_into(0, &c_dst, 0, PAGE as u64)
            .await
            .unwrap();

        let read = c_dst.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(read, bytes);
    }

    // ─────────────────────── intrusive LRU coverage ───────────────────────
    //
    // Direct unit tests of the O(1) intrusive doubly-linked list in
    // `CacheInner`. The cache-level tests above exercise it indirectly
    // through eviction; these pin the data-structure invariants
    // (head/tail sentinels, link consistency on touch / drop / pick)
    // so a future refactor can't quietly corrupt the list.

    fn push_page(inner: &mut CacheInner, pid: PageId, state: PageState) {
        inner.pages.insert(
            pid,
            CacheEntry {
                bytes: Arc::new(Vec::new()),
                state,
                version: 0,
                newer: None,
                older: None,
            },
        );
        if state == PageState::Dirty {
            inner.dirty.insert(pid);
        }
        inner.lru_touch(pid);
    }

    /// Assert the cache-internal head/tail sentinels agree with the
    /// head→tail walk: head is the first element, tail the last.
    fn assert_list_consistent(inner: &CacheInner) {
        let order = inner.lru_order();
        assert_eq!(inner.lru_head, order.first().copied());
        assert_eq!(inner.lru_tail, order.last().copied());
    }

    #[test]
    fn lru_touch_orders_most_recent_first() {
        let mut inner = CacheInner::new();
        push_page(&mut inner, 1, PageState::Clean);
        push_page(&mut inner, 2, PageState::Clean);
        push_page(&mut inner, 3, PageState::Clean);
        // Inserted 1,2,3 → MRU order is 3,2,1.
        assert_eq!(inner.lru_order(), vec![3, 2, 1]);
        assert_list_consistent(&inner);

        // Touch the tail → it moves to the head.
        inner.lru_touch(1);
        assert_eq!(inner.lru_order(), vec![1, 3, 2]);
        assert_list_consistent(&inner);

        // Touch a middle node.
        inner.lru_touch(3);
        assert_eq!(inner.lru_order(), vec![3, 1, 2]);
        assert_list_consistent(&inner);

        // Touch the current head → no-op fast path.
        inner.lru_touch(3);
        assert_eq!(inner.lru_order(), vec![3, 1, 2]);
        assert_list_consistent(&inner);
    }

    #[test]
    fn lru_pick_returns_tail_and_skips_dirty_when_clean_only() {
        let mut inner = CacheInner::new();
        push_page(&mut inner, 1, PageState::Dirty);
        push_page(&mut inner, 2, PageState::Clean);
        push_page(&mut inner, 3, PageState::Dirty);
        // Order MRU→LRU: 3,2,1. Tail (LRU) is page 1.
        assert_eq!(inner.lru_pick(false), Some(1));
        // clean_only walks from the tail (1 dirty → 2 clean) and stops
        // at the least-recently-used CLEAN page.
        assert_eq!(inner.lru_pick(true), Some(2));
    }

    #[test]
    fn lru_pick_clean_only_none_when_all_dirty() {
        let mut inner = CacheInner::new();
        push_page(&mut inner, 1, PageState::Dirty);
        push_page(&mut inner, 2, PageState::Dirty);
        assert_eq!(inner.lru_pick(true), None);
        assert_eq!(inner.lru_pick(false), Some(1));
    }

    #[test]
    fn drop_entry_from_head_middle_tail_keeps_list_consistent() {
        // Drop the middle node.
        let mut inner = CacheInner::new();
        push_page(&mut inner, 1, PageState::Clean);
        push_page(&mut inner, 2, PageState::Clean);
        push_page(&mut inner, 3, PageState::Clean);
        assert!(inner.drop_entry(2));
        assert_eq!(inner.lru_order(), vec![3, 1]);
        assert_list_consistent(&inner);

        // Drop the head, then the tail.
        let mut inner = CacheInner::new();
        push_page(&mut inner, 1, PageState::Clean);
        push_page(&mut inner, 2, PageState::Clean);
        push_page(&mut inner, 3, PageState::Clean);
        assert!(inner.drop_entry(3)); // head
        assert_eq!(inner.lru_order(), vec![2, 1]);
        assert_list_consistent(&inner);
        assert!(inner.drop_entry(1)); // tail
        assert_eq!(inner.lru_order(), vec![2]);
        assert_list_consistent(&inner);
    }

    #[test]
    fn drop_entry_sole_element_empties_list() {
        let mut inner = CacheInner::new();
        push_page(&mut inner, 7, PageState::Dirty);
        assert_eq!(inner.lru_head, Some(7));
        assert_eq!(inner.lru_tail, Some(7));
        assert!(inner.drop_entry(7));
        assert!(inner.lru_order().is_empty());
        assert_eq!(inner.lru_head, None);
        assert_eq!(inner.lru_tail, None);
        assert!(!inner.dirty.contains(&7));
    }

    #[test]
    fn drop_entry_absent_returns_false_and_leaves_list_intact() {
        let mut inner = CacheInner::new();
        push_page(&mut inner, 1, PageState::Clean);
        push_page(&mut inner, 2, PageState::Clean);
        assert!(!inner.drop_entry(99));
        assert_eq!(inner.lru_order(), vec![2, 1]);
        assert_list_consistent(&inner);
    }

    #[test]
    fn lru_unlink_is_noop_for_absent_and_detached_entries() {
        let mut inner = CacheInner::new();
        push_page(&mut inner, 1, PageState::Clean);
        push_page(&mut inner, 2, PageState::Clean);
        // Absent page id: no-op.
        inner.lru_unlink(99);
        assert_eq!(inner.lru_order(), vec![2, 1]);

        // Present-but-detached entry (inserted without linking): the
        // guard must not mistake its all-None links for the sole-head
        // case and clobber the live head/tail.
        inner.pages.insert(
            3,
            CacheEntry {
                bytes: Arc::new(Vec::new()),
                state: PageState::Clean,
                version: 0,
                newer: None,
                older: None,
            },
        );
        inner.lru_unlink(3);
        assert_eq!(inner.lru_order(), vec![2, 1]);
        assert_list_consistent(&inner);
    }

    #[tokio::test]
    async fn read_updates_lru_recency_so_eviction_spares_recently_read_page() {
        // End-to-end proof that a READ updates recency through the
        // intrusive touch on the hot path. Budget = 2 pages.
        //
        //   write 0, write 1  → cache [1 MRU, 0 LRU], both dirty
        //   read 0            → 0 becomes MRU → [0 MRU, 1 LRU]
        //   write 2           → eviction victim is the LRU = page 1
        //
        // So page 1 (not page 0) gets flushed out, even though page 0
        // was written first.
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let backend: Arc<dyn ObjectStoreBackend> =
            Arc::new(LocalBackend::new(&cloud).await.unwrap());
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

        cache.write_bytes(0, &pattern(0, PAGE)).await.unwrap();
        cache
            .write_bytes(PAGE as u64, &pattern(1, PAGE))
            .await
            .unwrap();
        // Touch page 0 via a read so it is no longer the LRU.
        let _ = cache.read_bytes(0, PAGE).await.unwrap();
        // This write forces eviction of the now-LRU page (page 1).
        cache
            .write_bytes(2 * PAGE as u64, &pattern(2, PAGE))
            .await
            .unwrap();

        assert!(
            writer.page_index().get(1).unwrap().is_some(),
            "page 1 (least recently used) must have been evicted + flushed"
        );
        assert!(
            writer.page_index().get(0).unwrap().is_none(),
            "page 0 (recently read) must still be dirty in cache, not evicted"
        );
        // And page 0 still reads back correctly from cache.
        assert_eq!(cache.read_bytes(0, PAGE).await.unwrap(), pattern(0, PAGE));
    }

    // ─────────────────────── Arc copy-on-write isolation ───────────────────────

    #[tokio::test]
    async fn mutating_arc_shared_source_after_clone_does_not_corrupt_destination() {
        // Change (1)'s headline invariant. A dirty source page is
        // Arc-shared into the destination by `clone_one_page_into`
        // Case 1 (the destination entry holds an `Arc::clone` of the
        // source body, refcount 2). A later sub-page WRITE to the
        // source goes through `Arc::make_mut`, which must copy-on-write
        // — leaving the destination's view at the pre-mutation bytes.
        // A regression to an aliasing in-place mutation would corrupt
        // the destination; a regression to `Arc::get_mut().unwrap()`
        // would panic. Either way this test fails.
        let (_tmp, cache, _w) = fixture_cache(8 * (1u64 << 20)).await;
        let src = pattern(0x5A, PAGE);
        cache.write_bytes(0, &src).await.unwrap();
        let dst_off = 4 * PAGE as u64;
        // Source is dirty (not synced) → Case 1 shares the Arc.
        cache
            .clone_page_range(0, dst_off, PAGE as u64)
            .await
            .unwrap();

        // Overwrite the source's first sector AFTER the clone.
        let overwrite = vec![0xFF; SECTOR];
        cache.write_bytes(0, &overwrite).await.unwrap();

        // Destination must still equal the ORIGINAL source bytes — the
        // copy-on-write split kept the two views independent.
        let dst = cache.read_bytes(dst_off, PAGE).await.unwrap();
        assert_eq!(
            dst, src,
            "make_mut must copy-on-write; destination must not observe the source's later write"
        );
        // Source reflects the overwrite in its first sector.
        let s0 = cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(s0, overwrite);
    }

    // ─────────────────── eviction version-race recovery ───────────────────

    #[tokio::test]
    async fn eviction_losing_version_race_keeps_page_dirty_with_latest_bytes() {
        // Exercises the "Some(_) lost the race" arm of `evict_to_fit`:
        // a dirty page is snapshotted for eviction, the lock is dropped
        // for the (slow) cloud upload, and a concurrent host write
        // re-dirties that same page mid-upload (bumping its version).
        // On re-lock the eviction must NOT drop the page — it leaves it
        // dirty so the latest bytes survive, and evicts a different
        // victim instead.
        let tmp = TempDir::new().unwrap();
        let cloud = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud).unwrap();
        let local: Arc<dyn ObjectStoreBackend> = Arc::new(LocalBackend::new(&cloud).await.unwrap());
        let delaying = DelayingBackend::new(Arc::clone(&local), Duration::from_millis(200));
        let backend: Arc<dyn ObjectStoreBackend> = Arc::clone(&delaying) as _;
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

        // Pre-fill pages 0 and 1 dirty; LRU order is [1 MRU, 0 LRU] so
        // page 0 is the first eviction victim.
        cache.write_bytes(0, &pattern(0, PAGE)).await.unwrap();
        cache
            .write_bytes(PAGE as u64, &pattern(1, PAGE))
            .await
            .unwrap();

        // Writing page 2 forces eviction of the LRU (page 0); its
        // upload blocks ~200 ms with the inner lock released.
        let evicting_cache = Arc::clone(&cache);
        let evicting = tokio::spawn(async move {
            evicting_cache
                .write_bytes(2 * PAGE as u64, &pattern(2, PAGE))
                .await
                .unwrap();
        });

        // Re-dirty page 0 while its stale eviction upload is in flight:
        // bumps page 0's version and moves it to the MRU end.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let latest = vec![0xEE; SECTOR];
        cache.write_bytes(0, &latest).await.unwrap();

        evicting.await.unwrap();

        // Page 0 lost the race: it must still be dirty in cache holding
        // the latest bytes (the stale eviction did not drop it).
        {
            let inner = cache.inner.lock().await;
            assert!(
                inner.dirty.contains(&0),
                "re-dirtied page 0 must survive the lost eviction race"
            );
        }
        let s0 = cache.read_bytes(0, SECTOR).await.unwrap();
        assert_eq!(
            s0, latest,
            "page 0 must hold the re-dirtied bytes, not the evicted snapshot"
        );
        // The eviction made progress on a different victim instead:
        // page 1 was flushed out.
        assert!(
            writer.page_index().get(1).unwrap().is_some(),
            "eviction must fall through to page 1 after losing the race on page 0"
        );
    }
}
