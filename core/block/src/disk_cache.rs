// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Block-side parallel of `core_stream::disk_cache`. Two surfaces:
//!
//! - [`refresh_pool_budget_from_volumes`] — daemon-startup walker
//!   that seeds `PoolBudget::current_bytes` from whatever survived a
//!   previous run, so a restart doesn't silently re-grant the
//!   on-disk bytes.
//! - [`DiskCacheManager`] — per-backend eviction state used by the
//!   daemon's eviction worker. Mirrors `core_stream::DiskCacheManager`
//!   structurally and shares the same refcount-safe contract:
//!   eviction must skip any chunk whose pages still have a pending
//!   storage upload, because dropping a pool entry that doesn't yet
//!   have a storage copy is data loss. The per-volume `upload.idx`
//!   sidecar records `LocalOnly` vs `Uploaded`; the eviction filter
//!   `OR`s across every referencing page so a chunk shared by an
//!   uploaded page and a pending-upload page stays pinned until the
//!   pending PUT acks.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use shared_pool::PoolBudget;
use tracing::{debug, info, warn};

use crate::chunk_pool::{ChunkPool, ChunkPoolError};
use crate::lru_index::LruIndexFile;
use crate::page_index::PageIndex;
use crate::upload_index::{UploadIndexFile, UploadState};
use crate::volume::{DedupScope, VolumeManifest, namespace_from_uuid};

/// Walk every chunk on disk under `backend_name` and seed
/// `budget.set_pool_buckets(per_namespace)`. Counts the shared
/// per-backend pool (bucketed under `None`) plus each
/// `DedupScope::Local` volume's per-volume namespace (bucketed under
/// `Some(uuid_hex)`) — both consume the same disk-cache budget. The
/// returned value is the backend total, summed across buckets, for
/// the caller's log line.
///
/// Mirrors `core_stream::disk_cache::refresh_pool_budget_from_tapes`
/// modulo terminology: VSA walks `<data_dir>/volumes/<name>/manifest.json`
/// instead of `<data_dir>/tapes/<barcode>/manifest.json`, and there
/// is no `.staging/` directory to account for — `VolumeWriter` goes
/// straight from RAM (`PageCache`) to the chunk pool without a
/// disk-staging step.
pub fn refresh_pool_budget_from_volumes(
    budget: &PoolBudget,
    data_dir: &Path,
    backend_name: &str,
) -> Result<u64, ChunkPoolError> {
    let mut buckets: HashMap<Option<String>, u64> = HashMap::new();

    let pool = ChunkPool::new(data_dir, backend_name)?;
    let global_sum: u64 = pool
        .iter_chunks()?
        .into_iter()
        .map(|(_, sz)| sz)
        .sum::<u64>();
    if global_sum > 0 {
        buckets.insert(None, global_sum);
    }

    let volumes_dir = data_dir.join(VolumeManifest::VOLUMES_SUBDIR);
    if volumes_dir.is_dir() {
        // A snapshot/clone family shares one namespace (issue #13), so
        // several volumes can map to the same `ns` — scan each
        // namespace once.
        let mut seen_ns: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in fs::read_dir(&volumes_dir)? {
            let entry = entry?;
            let routing = match manifest_routing_for_backend(&entry.path(), backend_name) {
                Some(r) => r,
                None => continue,
            };
            if routing.dedup_scope != DedupScope::Local {
                continue;
            }
            let ns = namespace_from_uuid(&routing.namespace_uuid);
            if !seen_ns.insert(ns.clone()) {
                continue;
            }
            let ns_pool = ChunkPool::new_namespaced(data_dir, backend_name, &ns)?;
            let ns_sum: u64 = ns_pool
                .iter_chunks()?
                .into_iter()
                .map(|(_, sz)| sz)
                .sum::<u64>();
            if ns_sum > 0 {
                buckets.insert(Some(ns), ns_sum);
            }
        }
    }

    let total: u64 = buckets.values().sum();
    budget.set_pool_buckets(buckets);
    Ok(total)
}

/// Per-backend eviction state. The daemon owns one
/// `DiskCacheManager` per configured storage backend; total cache
/// budget is shared at the daemon-coordinator layer (per-backend
/// `PoolBudget` map).
///
/// Eviction is LRU-keyed via the per-volume `lru.idx` sidecar
/// ([`LruIndexFile`]) and refcount-gated by the per-volume
/// `upload.idx` sidecar ([`UploadIndexFile`]). For each pool
/// chunk, the manager walks every volume's `pages.idx` whose
/// `manifest.backend == backend_name`, takes the max touch
/// timestamp across any page that references the hash, ANDs the
/// upload state across the same set, and evicts oldest-first
/// among the **uploaded** chunks until usage is back under cap.
/// A chunk pinned by even a single `LocalOnly` reference stays
/// resident until the upload worker drains — dropping it before
/// the storage PUT acks would be the only-copy data-loss path the
/// pre-async-upload era didn't have to worry about. `read_page`
/// refetches on pool miss transparently.
pub struct DiskCacheManager {
    data_dir: PathBuf,
    backend_name: String,
    cache_bytes: u64,
    current_bytes: u64,
    /// Optional handle to the same backend's `PoolBudget`. When
    /// set, successful chunk evictions call `release(size)` so any
    /// `VolumeWriter::write_page` blocked on backpressure wakes
    /// immediately. None for tests / standalone callers.
    pool_budget: Option<Arc<PoolBudget>>,
    /// Soft floor on chunk "recency" below which eviction skips a
    /// candidate. See [`Self::set_recent_seal_pin_seconds`]. `0`
    /// (the default) disables the pin.
    recent_seal_pin_seconds: u64,
    /// Per-backend ghost list of recently-evicted chunk hashes. When
    /// set, every successful unlink calls `insert` with the chunk's
    /// hash + the current wall-clock; the read-path miss site reads
    /// this same list to bucket re-fetch ages into the
    /// `cache_miss_after_eviction` histogram. None for tests.
    ghost_list: Option<Arc<shared_pool::GhostList>>,
}

/// A sealed pool chunk eligible for eviction. `namespace` selects
/// which `ChunkPool` layout the file lives in: `None` for shared
/// per-backend pool entries (`DedupScope::Global`), `Some(volume)`
/// for per-volume namespaces (`DedupScope::Local`). `uploaded` is
/// `false` iff at least one page referencing this hash still has
/// `upload.idx[page_id] == LocalOnly`; `evict_lru_chunks` filters
/// non-uploaded candidates out before sorting so a pending PUT can
/// never lose its only copy.
#[derive(Debug, Clone)]
struct EvictableChunk {
    hash: String,
    size: u64,
    last_accessed: u64,
    namespace: Option<String>,
    uploaded: bool,
}

impl DiskCacheManager {
    /// Create a cache manager scoped to the named backend's pool.
    pub fn new(data_dir: PathBuf, backend_name: &str, cache_bytes: u64) -> Self {
        Self {
            data_dir,
            backend_name: backend_name.to_string(),
            cache_bytes,
            current_bytes: 0,
            pool_budget: None,
            recent_seal_pin_seconds: 0,
            ghost_list: None,
        }
    }

    /// Wire the per-backend pool budget into this manager so
    /// eviction frees backpressure quota in addition to disk space.
    /// Must be the same `Arc<PoolBudget>` the volumes of this
    /// backend hold.
    pub fn set_pool_budget(&mut self, budget: Arc<PoolBudget>) {
        self.pool_budget = Some(budget);
    }

    /// Pin chunks whose most recent `lru.idx` touch is within the
    /// last `seconds` against eviction. Operates on the same touch
    /// timestamp the LRU sort already consults: every
    /// `write_page_unsynced` and every `read_page` bumps the
    /// referencing page's `lru.idx` entry to `now_unix_secs()`, so
    /// the window covers freshly-sealed chunks AND cache hits in
    /// the same `seconds`-wide horizon. `0` (the default) disables
    /// the pin and restores pure LRU. The
    /// `disk_cache.recent_seal_pin_seconds` YAML knob drives this
    /// from `vsa/daemon`.
    pub fn set_recent_seal_pin_seconds(&mut self, seconds: u64) {
        self.recent_seal_pin_seconds = seconds;
    }

    /// Wire the per-backend ghost list. Every successful unlink in
    /// `evict_lru_chunks` will insert the evicted chunk's hash so the
    /// read-path miss site can bucket re-fetch ages.
    pub fn set_ghost_list(&mut self, gl: Arc<shared_pool::GhostList>) {
        self.ghost_list = Some(gl);
    }

    /// Backend name this manager is scoped to.
    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// Overwrite the cache cap. The eviction worker calls this on
    /// every tick when the operator picked `disk_cache.size_gb:
    /// auto` so external disk pressure shrinks the effective cap
    /// reactively (clamped by `min_size_gb` / `max_size_gb`). Cheap
    /// to invoke on every tick — pure local-state mutation, no
    /// scan.
    pub fn set_capacity(&mut self, bytes: u64) {
        self.cache_bytes = bytes;
    }

    /// Cache capacity limit.
    pub fn capacity(&self) -> u64 {
        self.cache_bytes
    }

    /// Current cache usage (must call [`Self::calculate_usage`] first
    /// to refresh).
    pub fn current_usage(&self) -> u64 {
        self.current_bytes
    }

    /// Seed `current_bytes` directly from the authoritative per-backend
    /// `PoolBudget` (O(1)) instead of the full [`Self::calculate_usage`]
    /// pool walk. The eviction worker calls this every tick now that the
    /// budget is exact across every pool mutation site (seal, eviction,
    /// GC, read-miss refetch) — see issue #49. `evict_lru_chunks` reads
    /// this value to size how much it must free; the candidate
    /// enumeration walk still runs, but only when over cap.
    pub fn set_current_usage(&mut self, bytes: u64) {
        self.current_bytes = bytes;
    }

    /// Is the cache currently over capacity?
    pub fn is_over_capacity(&self) -> bool {
        self.current_bytes > self.cache_bytes
    }

    /// Cache usage as a percentage.
    pub fn usage_percent(&self) -> f64 {
        if self.cache_bytes == 0 {
            0.0
        } else {
            (self.current_bytes as f64 / self.cache_bytes as f64) * 100.0
        }
    }

    /// Recompute usage by walking every chunk in this backend's
    /// slice of the pool. Sums the Global per-backend pool plus
    /// each Local-scope volume's per-volume namespace. Updates the
    /// internal `current_bytes` and returns the total.
    pub fn calculate_usage(&mut self) -> Result<u64, ChunkPoolError> {
        let chunks = self.scan_chunks()?;
        let total = chunks.iter().map(|c| c.size).sum::<u64>();
        self.current_bytes = total;
        Ok(total)
    }

    /// Evict LRU chunks until usage is under cap. Returns the
    /// number of bytes freed. On every successful pool-file delete
    /// the per-backend `PoolBudget` is released, so any
    /// `VolumeWriter` parked on backpressure wakes immediately.
    ///
    /// Filters out any chunk whose pages still have a `LocalOnly`
    /// upload-state record (storage PUT not yet acked). The chunk
    /// stays pinned until the upload worker flips the sidecar back
    /// to `Uploaded` — at which point the next eviction tick (or
    /// the upload-completion `Notify`) considers it again.
    pub fn evict_lru_chunks(&mut self) -> Result<u64, ChunkPoolError> {
        if self.current_bytes <= self.cache_bytes {
            debug!(
                "Cache pool '{}' under capacity ({} / {} bytes), no eviction needed",
                self.backend_name, self.current_bytes, self.cache_bytes
            );
            return Ok(0);
        }

        let bytes_to_free = self.current_bytes - self.cache_bytes;
        let mut chunks = self.scan_chunks()?;
        let (touches, uploaded) = self.collect_lru_touches_and_upload_state()?;
        for c in chunks.iter_mut() {
            // The walk keys by the raw 32-byte hash; decode the
            // chunk's hex name once (cheap — chunk count, not page
            // count). A malformed name (shouldn't happen) leaves the
            // defaults below.
            let key = hex_to_blake3(&c.hash);
            // Per-namespace lookup: Global chunks have None key,
            // Local-scope chunks key by their owning volume.
            c.last_accessed = key
                .and_then(|k| touches.get(&c.namespace).and_then(|m| m.get(&k)))
                .copied()
                .unwrap_or(0);
            // Default uploaded = true when no record exists: a
            // legacy chunk written before async upload landed (no
            // upload.idx entry) was synchronously uploaded under
            // the old write path, so dropping it from the pool is
            // safe — `read_page` will refetch from storage on miss.
            c.uploaded = key
                .and_then(|k| uploaded.get(&c.namespace).and_then(|m| m.get(&k)))
                .copied()
                .unwrap_or(true);
        }
        let total_candidates = chunks.len();
        chunks.retain(|c| c.uploaded);
        let pinned_localonly = total_candidates - chunks.len();
        let before_token = chunks.len();
        chunks.retain(|c| {
            !ChunkPool::is_pinned_for(&self.backend_name, c.namespace.as_deref(), &c.hash)
        });
        let pinned_token = before_token - chunks.len();
        let pinned_recent = if self.recent_seal_pin_seconds > 0 {
            let cutoff = now_unix_secs().saturating_sub(self.recent_seal_pin_seconds);
            let before = chunks.len();
            chunks.retain(|c| c.last_accessed < cutoff);
            before - chunks.len()
        } else {
            0
        };
        chunks.sort_by_key(|c| c.last_accessed);

        if chunks.is_empty() {
            if pinned_localonly > 0 || pinned_recent > 0 || pinned_token > 0 {
                warn!(
                    "Backend '{}': all {} candidate chunk(s) pinned ({} pending upload, {} held by outstanding ROD token, {} within recent-seal window {}s) - eviction can't proceed",
                    self.backend_name,
                    pinned_localonly + pinned_recent + pinned_token,
                    pinned_localonly,
                    pinned_token,
                    pinned_recent,
                    self.recent_seal_pin_seconds,
                );
            } else {
                warn!(
                    "No chunks eligible for eviction on backend '{}'",
                    self.backend_name
                );
            }
            return Ok(0);
        }

        info!(
            "Backend '{}' over budget: {} candidates ({} pinned by pending upload, {} pinned by outstanding ROD token, {} pinned by recent-seal {}s), need to free {} bytes",
            self.backend_name,
            chunks.len(),
            pinned_localonly,
            pinned_token,
            pinned_recent,
            self.recent_seal_pin_seconds,
            bytes_to_free
        );

        let mut freed = 0u64;
        for c in chunks {
            if self.current_bytes - freed <= self.cache_bytes {
                break;
            }
            let pool = match c.namespace.as_deref() {
                Some(ns) => ChunkPool::new_namespaced(&self.data_dir, &self.backend_name, ns)?,
                None => ChunkPool::new(&self.data_dir, &self.backend_name)?,
            };
            match pool.remove(&c.hash) {
                Ok(()) => {
                    freed += c.size;
                    if let Some(budget) = self.pool_budget.as_ref() {
                        budget.release(c.size, c.namespace.as_deref());
                    }
                    if let Some(gl) = self.ghost_list.as_ref()
                        && let Some(hash_bytes) = hex_to_blake3(&c.hash)
                    {
                        gl.insert(hash_bytes, now_unix_secs());
                    }
                    debug!(
                        "Evicted chunk {}.. ({} B, ns {:?}) from backend '{}'",
                        &c.hash[..16.min(c.hash.len())],
                        c.size,
                        c.namespace,
                        self.backend_name
                    );
                }
                Err(e) => {
                    warn!(
                        "Eviction failed on chunk {}.. (backend '{}'): {}",
                        &c.hash[..16.min(c.hash.len())],
                        self.backend_name,
                        e
                    );
                }
            }
        }

        self.current_bytes = self.current_bytes.saturating_sub(freed);
        info!(
            "Eviction pass on backend '{}' freed {} bytes",
            self.backend_name, freed
        );
        Ok(freed)
    }

    /// List every pool chunk under this backend with its namespace
    /// (None = Global shared pool, Some(volume) = per-volume Local
    /// namespace) and on-disk size. Used by [`Self::calculate_usage`]
    /// and as the candidate set for eviction.
    fn scan_chunks(&self) -> Result<Vec<EvictableChunk>, ChunkPoolError> {
        let mut out = Vec::new();

        // Global per-backend pool (DedupScope::Global volumes share
        // these chunks).
        let pool = ChunkPool::new(&self.data_dir, &self.backend_name)?;
        for (hash, size) in pool.iter_chunks()? {
            out.push(EvictableChunk {
                hash,
                size,
                last_accessed: 0,
                namespace: None,
                uploaded: true,
            });
        }

        // Local-scope per-volume namespaces. A snapshot/clone family
        // shares one namespace (issue #13), so dedup across volumes —
        // scanning the same namespace twice would double-count its
        // chunks (and queue each for eviction twice).
        let volumes_dir = self.data_dir.join(VolumeManifest::VOLUMES_SUBDIR);
        if volumes_dir.is_dir() {
            let mut seen_ns: std::collections::HashSet<String> = std::collections::HashSet::new();
            for entry in fs::read_dir(&volumes_dir)? {
                let entry = entry?;
                let routing = match manifest_routing_for_backend(&entry.path(), &self.backend_name)
                {
                    Some(r) => r,
                    None => continue,
                };
                if routing.dedup_scope != DedupScope::Local {
                    continue;
                }
                let ns = namespace_from_uuid(&routing.namespace_uuid);
                if !seen_ns.insert(ns.clone()) {
                    continue;
                }
                let ns_pool = ChunkPool::new_namespaced(&self.data_dir, &self.backend_name, &ns)?;
                for (hash, size) in ns_pool.iter_chunks()? {
                    out.push(EvictableChunk {
                        hash,
                        size,
                        last_accessed: 0,
                        namespace: Some(ns.clone()),
                        uploaded: true,
                    });
                }
            }
        }

        Ok(out)
    }

    /// Walk every volume's `pages.idx` + `lru.idx` + `upload.idx`
    /// in one pass and build two `namespace → hash → ...` maps:
    ///
    /// - touches: `namespace → hash → max(last_accessed)` — drives
    ///   the eviction LRU sort.
    /// - uploaded: `namespace → hash → AND(is_uploaded for every
    ///   referencing page)` — `false` iff at least one page
    ///   referencing the hash still has `upload.idx[page_id] ==
    ///   LocalOnly`, which pins the chunk against eviction.
    ///
    /// For Local-scope volumes the namespace is the family UUID hex
    /// ([`VolumeManifest::pool_namespace`], which resolves a clone's
    /// inherited `dedup_namespace`); for Global-scope volumes the
    /// namespace is `None` (shared pool). Errors on a single volume's
    /// index files are logged + skipped rather than propagated — a
    /// corrupt or in-flux index should not stall the whole eviction
    /// pass.
    ///
    /// Snapshots (issue #13) are deliberately **not** walked here. A
    /// snapshot's frozen `pages.idx` only references chunks that were
    /// already `Uploaded` at snapshot time (snapshot-create quiesces:
    /// it flushes dirty pages to the pool and awaits storage acks before
    /// freezing), and content-addressed chunks are immutable, so a
    /// snapshot-only chunk is always storage-durable. With no entry here
    /// it falls to the safe defaults in `evict_lru_chunks`
    /// (`uploaded = true`, `last_accessed = 0`) — evictable as a cold,
    /// already-uploaded chunk, exactly right. Walking snapshots would
    /// change no eviction outcome.
    #[allow(clippy::type_complexity)]
    fn collect_lru_touches_and_upload_state(
        &self,
    ) -> Result<
        (
            HashMap<Option<String>, HashMap<[u8; 32], u64>>,
            HashMap<Option<String>, HashMap<[u8; 32], bool>>,
        ),
        ChunkPoolError,
    > {
        // Keyed by the raw 32-byte BLAKE3 hash, not its 64-char hex
        // form: `pages.idx` already yields the bytes, so hex-encoding
        // per page allocated a transient `String` per allocated page
        // (10^8+ at documented scale) and inflated each map entry from
        // 32 inline bytes to a heap `String` (issue #152).
        let mut touches: HashMap<Option<String>, HashMap<[u8; 32], u64>> = HashMap::new();
        let mut uploaded: HashMap<Option<String>, HashMap<[u8; 32], bool>> = HashMap::new();
        let volumes_dir = self.data_dir.join(VolumeManifest::VOLUMES_SUBDIR);
        if !volumes_dir.is_dir() {
            return Ok((touches, uploaded));
        }
        for entry in fs::read_dir(&volumes_dir)? {
            let entry = entry?;
            let vol_path = entry.path();
            // Backend-match filter only — the namespace below comes
            // from the loaded manifest's UUID, not this routing probe.
            if manifest_routing_for_backend(&vol_path, &self.backend_name).is_none() {
                continue;
            }
            let name = match vol_path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Load manifest to recover the uuid + page_size_bytes
            // PageIndex::open needs to validate the header.
            let manifest = match VolumeManifest::load(&self.data_dir, &name) {
                Ok(m) => m,
                Err(e) => {
                    warn!("LRU walk: load manifest '{}' failed: {}", name, e);
                    continue;
                }
            };
            let idx_path = PageIndex::path_for(&vol_path);
            let page_index = match PageIndex::open(
                &idx_path,
                manifest.uuid,
                u64::from(manifest.page_size_bytes),
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("LRU walk: open pages.idx for '{}' failed: {}", name, e);
                    continue;
                }
            };
            let lru_index = match LruIndexFile::open_or_create(&vol_path) {
                Ok(l) => l,
                Err(e) => {
                    warn!("LRU walk: open lru.idx for '{}' failed: {}", name, e);
                    continue;
                }
            };
            // Optional sidecar. `open_or_create` creates the file
            // when it's absent, so a genuinely missing sidecar
            // (legacy volume from the synchronous-seal era) returns
            // `Ok` full of zeros → every page reads as Uploaded,
            // which is honest. An `Err` therefore means the file is
            // *present but unreadable* (or uncreatable) — a real IO
            // / permission fault, not the legacy case. We must not
            // treat those pages as evictable: a momentary fault
            // could otherwise delete pool chunks whose storage PUT
            // hasn't landed (later 404 on read). Pin conservatively
            // — mark every page of this volume LocalOnly for this
            // pass (which also preserves the cross-volume AND pin
            // for chunks shared via Global dedup). The eviction
            // worker is periodic; the next pass re-reads.
            let mut sidecar_unreadable = false;
            let upload_idx: Option<UploadIndexFile> = match UploadIndexFile::open_or_create(
                &vol_path,
            ) {
                Ok(u) => Some(u),
                Err(e) => {
                    warn!(
                        "LRU walk: open upload.idx for '{}' failed: {} (pinning this volume's chunks against eviction for this pass)",
                        name, e
                    );
                    sidecar_unreadable = true;
                    None
                }
            };
            // One sequential read of each sidecar per volume instead of
            // a random `pread` per allocated page (issue #152). `lru.idx`
            // is a best-effort recency hint: a read failure degrades to
            // "all pages oldest", never pinning. `upload.idx` is a
            // safety gate: a read failure pins the whole volume for this
            // pass, exactly like the open failure handled above.
            let lru_ts: Vec<u64> = lru_index.read_all().unwrap_or_default();
            let upload_states: Vec<u8> = if sidecar_unreadable {
                Vec::new()
            } else {
                match upload_idx.as_ref().map(UploadIndexFile::read_all) {
                    Some(Ok(v)) => v,
                    Some(Err(e)) => {
                        warn!(
                            "LRU walk: read upload.idx for '{}' failed: {} (pinning this volume's chunks against eviction for this pass)",
                            name, e
                        );
                        sidecar_unreadable = true;
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            };
            let namespace_key: Option<String> = manifest.pool_namespace();
            let bucket_t = touches.entry(namespace_key.clone()).or_default();
            let bucket_u = uploaded.entry(namespace_key).or_default();
            for record in page_index.iter() {
                let (page_id, hash) = match record {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("LRU walk: iterate pages.idx for '{}' failed: {}", name, e);
                        break;
                    }
                };
                let ts = lru_ts.get(page_id as usize).copied().unwrap_or(0);
                let is_uploaded = if sidecar_unreadable {
                    // Sidecar unreadable — can't verify upload state for
                    // any page; pin conservatively.
                    false
                } else {
                    // A page id past the sidecar's length reads as
                    // Uploaded (legacy default, matching
                    // `UploadIndexFile::read`); an in-range byte decodes
                    // via `UploadState::from_byte`, where unknown bytes
                    // also fall back to Uploaded.
                    match upload_states.get(page_id as usize) {
                        None => true,
                        Some(&b) => UploadState::from_byte(b) == UploadState::Uploaded,
                    }
                };
                bucket_t
                    .entry(hash)
                    .and_modify(|cur| {
                        if ts > *cur {
                            *cur = ts;
                        }
                    })
                    .or_insert(ts);
                // AND across every reference: a single LocalOnly
                // page pins the chunk against eviction.
                bucket_u
                    .entry(hash)
                    .and_modify(|cur| *cur &= is_uploaded)
                    .or_insert(is_uploaded);
            }
        }
        Ok((touches, uploaded))
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decode a 64-character BLAKE3 hex string into the 32-byte ghost-list
/// key shape. Returns `None` on any malformed input — caller treats
/// that as "skip the ghost-list insert" (the eviction still happens).
fn hex_to_blake3(hex_str: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut out).ok()?;
    Some(out)
}

/// Lightweight summary of a volume manifest for cache scans —
/// matches `core_stream::disk_cache::ManifestRouting` in shape. The
/// volume's `backend` has already been matched against
/// `backend_name` before construction, so we don't carry it here.
/// `namespace_uuid` is the durable identity the `Local`-scope
/// namespace is keyed on (see [`namespace_from_uuid`]): the inherited
/// `dedup_namespace` for a snapshot/clone family member (issue #13),
/// else the volume's own `uuid`. A whole family thus resolves to one
/// namespace — callers must dedup namespaces across volumes.
struct ManifestRouting {
    dedup_scope: DedupScope,
    namespace_uuid: [u8; 16],
}

/// Return `Some(routing)` iff the volume directory at `vol_path`
/// carries a `manifest.json` whose `backend` field equals
/// `backend_name`. Missing / unparseable / empty-`backend` / missing
/// or malformed `uuid` manifest → `None` (skip rather than fail the
/// scan; bad manifests get fixed at load time, not by the cache
/// layer).
///
/// `namespace_uuid` is read from `dedup_namespace` when present
/// (snapshot/clone family member — chunks live in the family pool),
/// falling back to `uuid` (a fresh volume is its own family root). This
/// mirrors [`VolumeManifest::dedup_namespace_uuid`] but reads the raw
/// JSON so the cache layer doesn't pay a full manifest deserialize per
/// volume per scan.
fn manifest_routing_for_backend(vol_path: &Path, backend_name: &str) -> Option<ManifestRouting> {
    let manifest_path = vol_path.join(VolumeManifest::FILENAME);
    if !manifest_path.is_file() {
        return None;
    }
    let json = fs::read_to_string(&manifest_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let resolved = v.get("backend").and_then(|s| s.as_str())?;
    if resolved.is_empty() || resolved != backend_name {
        return None;
    }
    let dedup_scope = match v.get("dedup_scope").and_then(|s| s.as_str()) {
        Some("global") => DedupScope::Global,
        // Missing / unknown / "local" → Local. Matches `Default` impl
        // on `DedupScope` (local is the volume-side default).
        _ => DedupScope::Local,
    };
    // Family namespace (issue #13): dedup_namespace when set, else uuid.
    let key_hex = v
        .get("dedup_namespace")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| v.get("uuid").and_then(|s| s.as_str()))?;
    let namespace_uuid: [u8; 16] = hex::decode(key_hex).ok()?.try_into().ok()?;
    Some(ManifestRouting {
        dedup_scope,
        namespace_uuid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_pool::ChunkPool;
    use crate::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use tempfile::TempDir;

    fn make_volume(
        data_dir: &Path,
        name: &str,
        backend: &str,
        scope: DedupScope,
    ) -> VolumeManifest {
        VolumeManifest::new(
            name.to_string(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            backend.to_string(),
            scope,
            false,
            0,
        )
        .unwrap()
        .create(data_dir)
        .unwrap()
    }

    #[test]
    fn refresh_empty_pool_seeds_zero() {
        let tmp = TempDir::new().unwrap();
        let budget = PoolBudget::unbounded(tmp.path().to_path_buf());
        let total = refresh_pool_budget_from_volumes(&budget, tmp.path(), "primary").unwrap();
        assert_eq!(total, 0);
        assert_eq!(budget.current_bytes(), 0);
    }

    #[test]
    fn refresh_counts_global_pool_chunks() {
        let tmp = TempDir::new().unwrap();
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        pool.insert_bytes(&[0xAB; 1024]).unwrap();
        pool.insert_bytes(&[0xCD; 2048]).unwrap();
        let budget = PoolBudget::unbounded(tmp.path().to_path_buf());
        let total = refresh_pool_budget_from_volumes(&budget, tmp.path(), "primary").unwrap();
        assert_eq!(total, 1024 + 2048);
        assert_eq!(budget.current_bytes(), 1024 + 2048);
    }

    #[test]
    fn refresh_counts_local_namespace_chunks() {
        let tmp = TempDir::new().unwrap();
        let manifest = make_volume(tmp.path(), "vol-a", "primary", DedupScope::Local);
        let ns = manifest.pool_namespace().unwrap();
        let ns_pool = ChunkPool::new_namespaced(tmp.path(), "primary", &ns).unwrap();
        ns_pool.insert_bytes(&[0x11; 4096]).unwrap();
        let budget = PoolBudget::unbounded(tmp.path().to_path_buf());
        let total = refresh_pool_budget_from_volumes(&budget, tmp.path(), "primary").unwrap();
        assert_eq!(total, 4096);
    }

    #[test]
    fn refresh_skips_other_backends() {
        let tmp = TempDir::new().unwrap();
        // Chunk under "primary" only.
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        pool.insert_bytes(&[0x77; 512]).unwrap();
        // Volume under "archive" has its own namespace — must not be
        // counted when refreshing the "primary" budget.
        let manifest = make_volume(tmp.path(), "vol-archive", "archive", DedupScope::Local);
        let ns = manifest.pool_namespace().unwrap();
        let ns_archive = ChunkPool::new_namespaced(tmp.path(), "archive", &ns).unwrap();
        ns_archive.insert_bytes(&[0x88; 4096]).unwrap();

        let budget = PoolBudget::unbounded(tmp.path().to_path_buf());
        let total = refresh_pool_budget_from_volumes(&budget, tmp.path(), "primary").unwrap();
        assert_eq!(total, 512);
    }

    #[test]
    fn refresh_skips_global_scope_volumes_for_namespace_walk() {
        // A Global-scope volume's chunks live in the shared pool
        // (already counted), not in a per-volume namespace. The walker
        // must not try to open a non-existent namespace dir for it.
        let tmp = TempDir::new().unwrap();
        make_volume(tmp.path(), "vol-global", "primary", DedupScope::Global);
        let pool = ChunkPool::new(tmp.path(), "primary").unwrap();
        pool.insert_bytes(&[0x55; 8192]).unwrap();
        let budget = PoolBudget::unbounded(tmp.path().to_path_buf());
        let total = refresh_pool_budget_from_volumes(&budget, tmp.path(), "primary").unwrap();
        assert_eq!(total, 8192);
    }

    /// A destroy + recreate under the same volume name must NOT make
    /// the new volume inherit the dead one's namespace. The namespace
    /// is keyed on the durable UUID, so a fresh `create` mints a fresh
    /// UUID → fresh namespace → zero inherited chunks. The orphan
    /// chunks from the dead volume linger under the old namespace
    /// until `system gc` reclaims them.
    #[test]
    fn recreate_same_name_does_not_inherit_namespace() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        let first = make_volume(data_dir, "vol-a", "primary", DedupScope::Local);
        let ns_first = first.pool_namespace().unwrap();
        let pool_first = ChunkPool::new_namespaced(data_dir, "primary", &ns_first).unwrap();
        pool_first.insert_bytes(&[0x42; 4096]).unwrap();
        assert_eq!(pool_first.iter_chunks().unwrap().len(), 1);

        // Destroy the volume. `volume destroy` removes the manifest +
        // page index but leaves the pool chunks (the orphans GC
        // reclaims) — model that by dropping only the volume dir.
        fs::remove_dir_all(VolumeManifest::dir_for(data_dir, "vol-a")).unwrap();

        // Recreate under the same name → fresh UUID → fresh namespace.
        let second = make_volume(data_dir, "vol-a", "primary", DedupScope::Local);
        let ns_second = second.pool_namespace().unwrap();
        assert_ne!(
            ns_first, ns_second,
            "recreate-same-name must mint a distinct namespace"
        );

        let pool_second = ChunkPool::new_namespaced(data_dir, "primary", &ns_second).unwrap();
        assert!(
            pool_second.iter_chunks().unwrap().is_empty(),
            "new volume must not inherit the dead volume's chunks"
        );
        // The dead volume's chunk still sits under the old namespace,
        // an orphan until GC sweeps it.
        assert_eq!(
            ChunkPool::new_namespaced(data_dir, "primary", &ns_first)
                .unwrap()
                .iter_chunks()
                .unwrap()
                .len(),
            1,
            "dead volume's chunk lingers as an orphan until GC"
        );
    }

    /// Family namespace (issue #13): a clone carries
    /// `dedup_namespace = family root`, so it shares the parent's pool
    /// namespace. The cache walkers must (a) resolve that family
    /// namespace from the clone's manifest, not the clone's own uuid,
    /// and (b) scan it only once even though two volumes map to it —
    /// otherwise usage double-counts.
    #[test]
    fn family_namespace_scanned_once_not_per_volume() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();

        // Parent (family root) + a clone that inherits its namespace.
        let parent = make_volume(data_dir, "parent", "primary", DedupScope::Local);
        let family = parent.dedup_namespace_uuid();
        let clone = VolumeManifest::new(
            "clone".into(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            1,
        )
        .unwrap()
        .with_dedup_namespace(family)
        .create(data_dir)
        .unwrap();
        // Both resolve to the same on-disk pool namespace.
        assert_eq!(parent.pool_namespace(), clone.pool_namespace());

        // One 4 KiB chunk in the shared family namespace.
        let ns = parent.pool_namespace().unwrap();
        ChunkPool::new_namespaced(data_dir, "primary", &ns)
            .unwrap()
            .insert_bytes(&[0x9; 4096])
            .unwrap();

        // calculate_usage must count it once, not twice.
        let mut mgr = DiskCacheManager::new(data_dir.to_path_buf(), "primary", 1 << 30);
        assert_eq!(mgr.calculate_usage().unwrap(), 4096);

        // Budget refresh must also bucket it once.
        let budget = PoolBudget::unbounded(data_dir.to_path_buf());
        let total = refresh_pool_budget_from_volumes(&budget, data_dir, "primary").unwrap();
        assert_eq!(total, 4096);
    }

    /// Pin-against-eviction contract: a chunk whose owning page is
    /// marked `LocalOnly` in `upload.idx` must not be evicted, even
    /// when it's the only candidate and the cache is over cap.
    /// `Uploaded` chunks under the same volume are still evictable.
    /// Mirrors `core_stream::disk_cache`'s `collect_pinned_hashes`
    /// filter — same load-bearing safety property both products
    /// rely on.
    #[test]
    fn evict_lru_chunks_skips_localonly_pinned_pages() {
        use crate::page_index::PageIndex;
        use crate::upload_index::{UploadIndexFile, UploadState};

        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary", DedupScope::Local);
        let ns = manifest.pool_namespace().unwrap();

        // Seed two pool chunks under the per-volume namespace.
        let ns_pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (pinned_hash, _) = ns_pool.insert_bytes(&[0xAA; 2048]).unwrap();
        let (evictable_hash, _) = ns_pool.insert_bytes(&[0xBB; 4096]).unwrap();

        // Map both into pages.idx so the LRU walker sees them as
        // referenced by the volume.
        let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
        let pages = PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )
        .unwrap();
        let pinned_bytes: [u8; 32] = hex::decode(&pinned_hash).unwrap().try_into().unwrap();
        let evictable_bytes: [u8; 32] = hex::decode(&evictable_hash).unwrap().try_into().unwrap();
        pages.set(0, &pinned_bytes).unwrap();
        pages.set(1, &evictable_bytes).unwrap();

        // Mark page 0 LocalOnly (upload still pending); page 1 is
        // the default Uploaded (legacy / sidecar-empty).
        let upload_idx = UploadIndexFile::open_or_create(&vol_dir).unwrap();
        upload_idx.set(0, UploadState::LocalOnly).unwrap();

        // Tiny cap forces eviction: 4 KiB cap vs ~6 KiB on disk.
        let mut mgr = DiskCacheManager::new(data_dir.to_path_buf(), "primary", 4 * 1024);
        let used = mgr.calculate_usage().unwrap();
        assert_eq!(used, 2048 + 4096);
        assert!(mgr.is_over_capacity());

        let freed = mgr.evict_lru_chunks().unwrap();
        // Evictable chunk goes; pinned chunk stays.
        assert_eq!(freed, 4096);
        assert!(
            !ns_pool.exists(&evictable_hash),
            "evictable chunk should be gone"
        );
        assert!(
            ns_pool.exists(&pinned_hash),
            "LocalOnly chunk must NOT be evicted (would lose the only copy)"
        );
    }

    /// `recent_seal_pin_seconds > 0` pins chunks whose most recent
    /// `lru.idx` touch is inside the window, even when LRU would
    /// otherwise evict them. The ancient chunk goes; the freshly-
    /// touched one stays. Mirrors the policy the
    /// `disk_cache.recent_seal_pin_seconds` YAML knob drives.
    #[test]
    fn evict_lru_chunks_pins_recently_touched() {
        use crate::lru_index::LruIndexFile;
        use crate::page_index::PageIndex;

        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary", DedupScope::Local);
        let ns = manifest.pool_namespace().unwrap();

        let ns_pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (recent_hash, _) = ns_pool.insert_bytes(&[0xDD; 2048]).unwrap();
        let (ancient_hash, _) = ns_pool.insert_bytes(&[0xEE; 4096]).unwrap();

        let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
        let pages = PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )
        .unwrap();
        let recent_bytes: [u8; 32] = hex::decode(&recent_hash).unwrap().try_into().unwrap();
        let ancient_bytes: [u8; 32] = hex::decode(&ancient_hash).unwrap().try_into().unwrap();
        pages.set(0, &recent_bytes).unwrap();
        pages.set(1, &ancient_bytes).unwrap();

        let lru = LruIndexFile::open_or_create(&vol_dir).unwrap();
        let now = now_unix_secs();
        lru.touch(0, now).unwrap();
        lru.touch(1, now.saturating_sub(3600)).unwrap();

        // Cap 4 KiB vs ~6 KiB on disk; needs to free ~2 KiB. A
        // 300-second pin window covers page 0, leaves page 1 evictable.
        let mut mgr = DiskCacheManager::new(data_dir.to_path_buf(), "primary", 4 * 1024);
        mgr.set_recent_seal_pin_seconds(300);
        mgr.calculate_usage().unwrap();
        assert!(mgr.is_over_capacity());

        let freed = mgr.evict_lru_chunks().unwrap();
        assert_eq!(freed, 4096, "ancient chunk should be evicted");
        assert!(
            ns_pool.exists(&recent_hash),
            "chunk touched within pin window must stay resident"
        );
        assert!(
            !ns_pool.exists(&ancient_hash),
            "chunk older than pin window should be gone"
        );
    }

    /// Mixed-reference pin: a chunk referenced by both an Uploaded
    /// page and a LocalOnly page stays pinned (the AND across refs
    /// in `collect_lru_touches_and_upload_state` enforces this).
    #[test]
    fn evict_lru_chunks_pins_shared_chunk_if_any_ref_is_localonly() {
        use crate::page_index::PageIndex;
        use crate::upload_index::{UploadIndexFile, UploadState};

        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary", DedupScope::Local);
        let ns = manifest.pool_namespace().unwrap();

        // One chunk referenced by two pages.
        let ns_pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (shared_hash, _) = ns_pool.insert_bytes(&[0xCC; 8192]).unwrap();

        let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
        let pages = PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )
        .unwrap();
        let raw: [u8; 32] = hex::decode(&shared_hash).unwrap().try_into().unwrap();
        pages.set(0, &raw).unwrap();
        pages.set(1, &raw).unwrap();

        // Page 0 = Uploaded, Page 1 = LocalOnly. The chunk pins.
        let upload_idx = UploadIndexFile::open_or_create(&vol_dir).unwrap();
        upload_idx.set(0, UploadState::Uploaded).unwrap();
        upload_idx.set(1, UploadState::LocalOnly).unwrap();

        let mut mgr = DiskCacheManager::new(data_dir.to_path_buf(), "primary", 0);
        mgr.calculate_usage().unwrap();
        let freed = mgr.evict_lru_chunks().unwrap();
        assert_eq!(freed, 0);
        assert!(ns_pool.exists(&shared_hash));
    }

    /// Sidecar-unreadable pin: if `upload.idx` is *present but cannot
    /// be opened* (IO / permission fault — distinct from a legacy
    /// *absent* sidecar, which `open_or_create` would create fresh and
    /// read as all-Uploaded), the eviction pass must pin every page of
    /// that volume rather than treat them as Uploaded → evictable.
    /// Otherwise a momentary fault could delete a pool chunk whose
    /// storage PUT hasn't landed (later 404 on read).
    #[test]
    fn evict_lru_chunks_pins_when_sidecar_unreadable() {
        use crate::page_index::PageIndex;
        use crate::upload_index::UploadIndexFile;

        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary", DedupScope::Local);
        let ns = manifest.pool_namespace().unwrap();

        let ns_pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (hash, _) = ns_pool.insert_bytes(&[0xAB; 4096]).unwrap();

        let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
        let pages = PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )
        .unwrap();
        let raw: [u8; 32] = hex::decode(&hash).unwrap().try_into().unwrap();
        pages.set(0, &raw).unwrap();

        // No LocalOnly marker: under a readable sidecar this page would
        // read as Uploaded and be evictable. Make the sidecar present
        // but unopenable by planting a directory at its path — open
        // fails with EISDIR, the open-failure branch under test.
        let sidecar = UploadIndexFile::path_for(&vol_dir);
        fs::create_dir(&sidecar).unwrap();

        let mut mgr = DiskCacheManager::new(data_dir.to_path_buf(), "primary", 0);
        mgr.calculate_usage().unwrap();
        assert!(mgr.is_over_capacity());

        let freed = mgr.evict_lru_chunks().unwrap();
        assert_eq!(freed, 0, "unreadable sidecar must pin the volume's chunks");
        assert!(
            ns_pool.exists(&hash),
            "chunk must survive when its upload state can't be verified"
        );
    }

    /// End-to-end: an `evict_lru_chunks` pass that actually unlinks a
    /// chunk must populate the wired ghost list with that chunk's
    /// hash. This is the seam the cache-miss read path will later
    /// query.
    #[test]
    fn eviction_populates_ghost_list() {
        use crate::page_index::PageIndex;
        use shared_pool::GhostList;
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary", DedupScope::Local);
        let ns = manifest.pool_namespace().unwrap();

        let ns_pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (evictable_hash, _) = ns_pool.insert_bytes(&[0x42; 4096]).unwrap();

        let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
        let pages = PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )
        .unwrap();
        let hash_bytes: [u8; 32] = hex::decode(&evictable_hash).unwrap().try_into().unwrap();
        pages.set(0, &hash_bytes).unwrap();

        let gl = Arc::new(GhostList::new("primary", 1024));
        let mut mgr = DiskCacheManager::new(data_dir.to_path_buf(), "primary", 0);
        mgr.set_ghost_list(gl.clone());
        mgr.calculate_usage().unwrap();
        let freed = mgr.evict_lru_chunks().unwrap();
        assert_eq!(freed, 4096, "chunk should have been evicted");
        // Same hash bytes the cache-miss path would compute from the
        // page_index entry — lookup must hit.
        assert!(
            gl.lookup(&hash_bytes, now_unix_secs() + 1).is_some(),
            "ghost list should carry the evicted chunk"
        );
    }
}
