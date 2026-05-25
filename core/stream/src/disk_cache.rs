// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use crate::cartridge::{Cartridge, DedupScope};
use crate::chunk_index::{ChunkIndexFile, LocationTag};
use crate::chunk_store::ChunkStore;
use crate::errors::Result;
use crate::lru_index::LruIndexFile;
use shared_object_store::ObjectStoreBackend;
use shared_pool::{ChunkPool, PoolBudget};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Manages local cache eviction for cartridges with cloud backing.
/// With content-addressed shared storage, eviction must refcount-check
/// across every cartridge before deleting a chunk file from the pool —
/// the same hash may be referenced by tapes that haven't been opened
/// for read yet.
///
/// **Backend-scoped**: each DiskCacheManager handles exactly one named
/// cloud backend's pool (`<data_dir>/chunks/<backend_name>/`). With
/// per-backend pool sharding (multi_backend), every cartridge's chunks
/// live exactly under one backend, so a DiskCacheManager's scans (usage
/// calculation, eviction candidates, pinned hashes) filter manifests
/// by `manifest.backend == backend_name`. The daemon owns one
/// DiskCacheManager per configured backend; total cache budget is shared
/// at the daemon-coordinator layer above this struct.
pub struct DiskCacheManager {
    data_dir: PathBuf,
    backend_name: String,
    cache_bytes: u64,
    current_bytes: u64,
    /// Optional handle to the same backend's `PoolBudget`. When set,
    /// successful chunk evictions call `release(size)` so any
    /// `Cartridge::seal_current_chunk` blocked on backpressure wakes
    /// immediately. None for tests / standalone callers.
    pool_budget: Option<std::sync::Arc<PoolBudget>>,
    /// Soft floor on chunk "recency" below which eviction skips a
    /// candidate. See [`Self::set_recent_seal_pin_seconds`]. `0`
    /// (the default) disables the pin.
    recent_seal_pin_seconds: u64,
}

/// Metadata about a sealed chunk eligible for eviction from this
/// cartridge's perspective. The actual deletion of the pool file
/// happens only after we confirm no other cartridge still references the
/// hash with `LocalOnly` or `Both` *within the same namespace*.
///
/// `namespace` is `None` for shared per-backend pool entries
/// (`DedupScope::Global`) and `Some(barcode)` for per-cartridge
/// namespaces (`DedupScope::Local`). It selects which `ChunkStore`
/// layout the file lives in.
#[derive(Debug, Clone)]
struct EvictableChunk {
    tape_label: String,
    chunk_id: u64,
    hash: String,
    size: u64,
    last_accessed: u64,
    namespace: Option<String>,
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
        }
    }

    /// Wire the per-backend pool budget into this manager so eviction
    /// frees backpressure quota in addition to disk space. Must be the
    /// same `Arc<PoolBudget>` the cartridges of this backend hold.
    pub fn set_pool_budget(&mut self, budget: std::sync::Arc<PoolBudget>) {
        self.pool_budget = Some(budget);
    }

    /// Pin chunks whose most recent `lru.idx` touch is within the
    /// last `seconds` against eviction. Operates on the same touch
    /// timestamp the LRU sort already consults: every chunk seal
    /// and every chunk read bumps the per-cartridge `lru.idx`
    /// entry, so the window covers freshly-sealed chunks AND
    /// cache hits in the same `seconds`-wide horizon. `0` (the
    /// default) disables the pin and restores pure LRU. The
    /// `disk_cache.recent_seal_pin_seconds` YAML knob drives this
    /// from `vtl/daemon`.
    pub fn set_recent_seal_pin_seconds(&mut self, seconds: u64) {
        self.recent_seal_pin_seconds = seconds;
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

    /// Calculate cache usage for this backend's slice. With per-backend
    /// pool sharding, the cache for backend `B` is the union of:
    ///   - the shared content-addressed pool at `<data_dir>/chunks/<B>/`
    ///     (chunks from `DedupScope::Global` cartridges)
    ///   - each `DedupScope::Local` cartridge's per-cartridge namespace
    ///     at `<data_dir>/chunks/<B>/<barcode>/`
    ///   - each `manifest.backend == B` cartridge's unsealed staging
    ///     chunk at `<data_dir>/tapes/<barcode>/.staging/`
    ///
    /// Global chunks are counted once even if many cartridges of this
    /// backend reference them. Local-scope chunks are namespaced so
    /// they're inherently per-cartridge and counted exactly once.
    /// Staging chunks are counted because they take real disk space
    /// until they roll into the pool.
    pub fn calculate_usage(&mut self) -> Result<u64> {
        let mut total_bytes: u64 = 0;

        // Per-backend sealed shared pool (Global scope)
        if let Ok(store) = ChunkStore::new(&self.data_dir, &self.backend_name) {
            total_bytes += store
                .iter_chunks()?
                .into_iter()
                .map(|(_, size)| size)
                .sum::<u64>();
        }

        // Per-cartridge namespaces (Local scope) + per-cartridge staging
        let tapes_dir = self.data_dir.join("tapes");
        if tapes_dir.is_dir() {
            for entry in fs::read_dir(&tapes_dir)? {
                let entry = entry?;
                let routing = match manifest_routing_for_backend(&entry.path(), &self.backend_name)?
                {
                    Some(r) => r,
                    None => continue,
                };
                let label = match entry.path().file_name().and_then(|n| n.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };

                if routing.dedup == DedupScope::Local
                    && let Ok(ns_store) =
                        ChunkStore::new_namespaced(&self.data_dir, &self.backend_name, &label)
                {
                    total_bytes += ns_store
                        .iter_chunks()?
                        .into_iter()
                        .map(|(_, size)| size)
                        .sum::<u64>();
                }

                let staging_dir = entry.path().join(".staging");
                if !staging_dir.is_dir() {
                    continue;
                }
                for sf in fs::read_dir(&staging_dir)? {
                    let sf = sf?;
                    let meta = sf.metadata()?;
                    if meta.is_file() {
                        total_bytes += meta.len();
                    }
                }
            }
        }

        self.current_bytes = total_bytes;
        Ok(total_bytes)
    }

    /// Check if cache is over capacity
    pub fn is_over_capacity(&self) -> bool {
        self.current_bytes > self.cache_bytes
    }

    /// Evict LRU chunks until cache usage is under limit
    /// Returns the number of bytes freed
    pub async fn evict_lru_chunks(
        &mut self,
        cloud_backend: Option<&dyn ObjectStoreBackend>,
    ) -> Result<u64> {
        if self.current_bytes <= self.cache_bytes {
            debug!(
                "Cache under capacity ({} / {} bytes), no eviction needed",
                self.current_bytes, self.cache_bytes
            );
            return Ok(0);
        }

        info!(
            "Cache over capacity ({} / {} bytes), starting LRU eviction",
            self.current_bytes, self.cache_bytes
        );

        let bytes_to_free = self.current_bytes - self.cache_bytes;
        // Walk `tapes/` once and parse each manifest at most once.
        // `collect_eviction_candidates` and `collect_pinned_hashes`
        // both filter by `manifest.backend == self.backend_name`;
        // computing the (path, label, routing) triples up-front
        // halves the per-cartridge JSON parse cost on every
        // eviction pass.
        let cartridges = self.scan_backend_cartridges()?;
        let mut candidates = self.collect_eviction_candidates(&cartridges)?;
        let pinned = self.collect_pinned_hashes(&cartridges)?;

        let before_token = candidates.len();
        candidates.retain(|c| {
            !ChunkPool::is_pinned_for(&self.backend_name, c.namespace.as_deref(), &c.hash)
        });
        let pinned_token = before_token - candidates.len();
        let pinned_recent = if self.recent_seal_pin_seconds > 0 {
            let cutoff = now_unix_secs().saturating_sub(self.recent_seal_pin_seconds);
            let before = candidates.len();
            candidates.retain(|c| c.last_accessed < cutoff);
            before - candidates.len()
        } else {
            0
        };

        if candidates.is_empty() {
            if pinned_recent > 0 || pinned_token > 0 {
                warn!(
                    "All candidate chunk(s) pinned ({} by outstanding ROD token, {} by recent-seal window {}s) - eviction can't proceed until pins drop",
                    pinned_token, pinned_recent, self.recent_seal_pin_seconds,
                );
            } else {
                warn!(
                    "No chunks eligible for eviction (all chunks either not uploaded or LocalOnly)"
                );
            }
            return Ok(0);
        }

        // Sort by last_accessed (oldest first)
        candidates.sort_by_key(|c| c.last_accessed);

        info!(
            "Found {} eviction candidates ({} unique hashes, {} pinned by ROD token, {} pinned by recent-seal {}s), need to free {} bytes",
            candidates.len(),
            candidates
                .iter()
                .map(|c| c.hash.clone())
                .collect::<HashSet<_>>()
                .len(),
            pinned_token,
            pinned_recent,
            self.recent_seal_pin_seconds,
            bytes_to_free
        );

        // Group candidates by tape_label so each cartridge is opened
        // exactly once per pass. With LRU sort (stable), `label_order`
        // preserves the first-seen order, so the cartridge holding the
        // single oldest chunk is processed first; we then drain *all*
        // its candidates before moving on. Old code opened the
        // cartridge fresh per chunk, which loaded chunk_index +
        // every block_index + dirty_pages + the cloud handle for
        // every flip of one record's `location` field — for 1000
        // evictable chunks across 50 cartridges that was 1000 full
        // cartridge-opens vs 50 here.
        let mut by_label: HashMap<String, Vec<EvictableChunk>> = HashMap::new();
        let mut label_order: Vec<String> = Vec::new();
        for c in candidates {
            if !by_label.contains_key(&c.tape_label) {
                label_order.push(c.tape_label.clone());
            }
            by_label.entry(c.tape_label.clone()).or_default().push(c);
        }

        let mut bytes_freed = 0u64;
        let mut chunks_evicted = 0usize;
        let tapes_root = self.data_dir.join("tapes");

        'outer: for label in label_order {
            if self.current_bytes - bytes_freed <= self.cache_bytes {
                break;
            }
            let chunks = by_label.remove(&label).unwrap_or_default();
            // Open the cartridge once for this label.
            let cartridge = if let Some(backend) = cloud_backend {
                Cartridge::open_with_cloud(
                    &tapes_root,
                    &label,
                    crate::cartridge::CartridgeOpenMode::Open,
                    Some(backend.clone_box()),
                )
            } else {
                Cartridge::open(
                    &tapes_root,
                    &label,
                    crate::cartridge::CartridgeOpenMode::Open,
                )
            };
            let mut cartridge = match cartridge {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to open cartridge {} for eviction: {}", label, e);
                    continue;
                }
            };
            for candidate in chunks {
                if self.current_bytes - bytes_freed <= self.cache_bytes {
                    break 'outer;
                }
                match self.evict_chunk(&candidate, &mut cartridge, &pinned) {
                    Ok(freed) => {
                        bytes_freed += freed;
                        chunks_evicted += 1;
                        info!(
                            "Evicted chunk {} from {} (freed {} bytes, total freed: {} bytes)",
                            candidate.chunk_id, candidate.tape_label, freed, bytes_freed
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Failed to evict chunk {} from {}: {}",
                            candidate.chunk_id, candidate.tape_label, e
                        );
                    }
                }
            }
            // cartridge dropped at end of scope
        }

        self.current_bytes = self.current_bytes.saturating_sub(bytes_freed);

        info!(
            "LRU eviction complete: freed {} bytes across {} chunks",
            bytes_freed, chunks_evicted
        );

        Ok(bytes_freed)
    }

    /// Collect all chunks eligible for eviction *from this backend*.
    /// With content-addressed storage the same hash may appear in
    /// multiple `Global` cartridges' manifests; we need every
    /// cartridge's view (within this backend) before deciding the file
    /// is safe to delete from the shared pool. `Local`-scope candidates
    /// carry their cartridge namespace so eviction routes to the
    /// per-cartridge layout.
    ///
    /// Reads `chunks.idx` directly (per-cartridge) plus the local-only
    /// `lru.idx` sidecar for the eviction sort key — `manifest.json`
    /// no longer carries inline chunk records.
    /// Walk `tapes/` once and return one entry per cartridge whose
    /// manifest binds to this `backend_name`. The triples
    /// `(tape_path, tape_label, routing)` are reused across the
    /// per-pass collect helpers below to avoid re-parsing every
    /// manifest twice.
    fn scan_backend_cartridges(&self) -> Result<Vec<(PathBuf, String, ManifestRouting)>> {
        let tapes_dir = self.data_dir.join("tapes");
        if !tapes_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&tapes_dir)? {
            let entry = entry?;
            let tape_path = entry.path();
            if !tape_path.is_dir() {
                continue;
            }
            let routing = match manifest_routing_for_backend(&tape_path, &self.backend_name)? {
                Some(r) => r,
                None => continue,
            };
            let label = tape_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("UNKNOWN")
                .to_string();
            out.push((tape_path, label, routing));
        }
        Ok(out)
    }

    fn collect_eviction_candidates(
        &self,
        cartridges: &[(PathBuf, String, ManifestRouting)],
    ) -> Result<Vec<EvictableChunk>> {
        let mut candidates = Vec::new();

        for (tape_path, tape_label, routing) in cartridges {
            let namespace: Option<String> = match routing.dedup {
                DedupScope::Global => None,
                DedupScope::Local => Some(tape_label.clone()),
            };

            let chunk_index = match ChunkIndexFile::open_or_create(tape_path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "Cannot open chunks.idx for {}: {} - skipping",
                        tape_label, e
                    );
                    continue;
                }
            };
            let lru_index = match LruIndexFile::open_or_create(tape_path) {
                Ok(l) => l,
                Err(e) => {
                    warn!("Cannot open lru.idx for {}: {} - skipping", tape_label, e);
                    continue;
                }
            };
            for entry in chunk_index.iter() {
                let (chunk_id, rec) = match entry {
                    Ok(e) => e,
                    Err(_) => break,
                };
                if !rec.uploaded || rec.location != LocationTag::Both {
                    continue;
                }
                let hash = match rec.hash {
                    Some(h) => h,
                    None => continue,
                };
                let last_accessed = lru_index.read(chunk_id).unwrap_or(0);
                candidates.push(EvictableChunk {
                    tape_label: tape_label.clone(),
                    chunk_id,
                    hash,
                    size: rec.size,
                    last_accessed,
                    namespace: namespace.clone(),
                });
            }
        }

        Ok(candidates)
    }

    /// Build the set of hashes that any cartridge *of this backend*
    /// still wants kept on disk, keyed by namespace. A hash is "pinned"
    /// if any cartridge lists it with `location = LocalOnly`. `Both` is
    /// fine to evict (the cartridge has confirmed the cloud copy);
    /// `S3Only` already considers it gone locally.
    ///
    /// Map shape:
    ///   * `None` → hashes pinned in the shared per-backend pool
    ///     (any `Global`-scope cartridge of this backend that still
    ///     lists the chunk as `LocalOnly`).
    ///   * `Some(barcode)` → hashes pinned in that one cartridge's
    ///     `Local`-scope namespace. `Local` namespaces never share
    ///     files, so each barcode's pin set is independent.
    ///
    /// Pin lookup at eviction time is `pinned.get(&candidate.namespace)`,
    /// matching the layout the candidate was discovered under.
    fn collect_pinned_hashes(
        &self,
        cartridges: &[(PathBuf, String, ManifestRouting)],
    ) -> Result<HashMap<Option<String>, HashSet<String>>> {
        let mut pinned: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        for (tape_path, label, routing) in cartridges {
            let namespace: Option<String> = match routing.dedup {
                DedupScope::Global => None,
                DedupScope::Local => Some(label.clone()),
            };
            let chunk_index = match ChunkIndexFile::open_or_create(tape_path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Cannot open chunks.idx for {}: {} - skipping", label, e);
                    continue;
                }
            };
            let bucket = pinned.entry(namespace).or_default();
            for ent in chunk_index.iter() {
                let (_id, rec) = match ent {
                    Ok(r) => r,
                    Err(_) => break,
                };
                if rec.location != LocationTag::LocalOnly {
                    continue;
                }
                if let Some(h) = rec.hash {
                    bucket.insert(h);
                }
            }
        }
        Ok(pinned)
    }

    /// Evict a chunk from this cartridge's view. Caller has already
    /// opened the `cartridge` for `candidate.tape_label` so the same
    /// open is reused across every chunk in that cartridge's
    /// eviction batch. The pool file is only deleted if no cartridge
    /// in the same namespace still pins the hash via `LocalOnly`.
    /// Returns the bytes freed (0 if the file was kept due to
    /// refcount).
    fn evict_chunk(
        &self,
        candidate: &EvictableChunk,
        cartridge: &mut Cartridge,
        pinned: &HashMap<Option<String>, HashSet<String>>,
    ) -> Result<u64> {
        // Always update this cartridge's view to CloudOnly.
        cartridge.mark_chunk_evicted(candidate.chunk_id)?;

        // Refcount within the candidate's namespace. `Global` chunks
        // share the per-backend pool — any `Global` cartridge can pin
        // the same file. `Local` chunks live under one cartridge by
        // construction, so the pin set is just that cartridge's own
        // `LocalOnly` hashes.
        let pinned_in_ns = pinned.get(&candidate.namespace);
        if pinned_in_ns
            .map(|set| set.contains(&candidate.hash))
            .unwrap_or(false)
        {
            debug!(
                "Chunk {} (hash {}.., namespace {:?}) still pinned; keeping pool file",
                candidate.chunk_id,
                &candidate.hash[..8],
                candidate.namespace,
            );
            return Ok(0);
        }

        let store = match candidate.namespace.as_deref() {
            Some(ns) => ChunkStore::new_namespaced(&self.data_dir, &self.backend_name, ns)?,
            None => ChunkStore::new(&self.data_dir, &self.backend_name)?,
        };
        store.remove(&candidate.hash)?;
        if let Some(budget) = self.pool_budget.as_ref() {
            budget.release(candidate.size, candidate.namespace.as_deref());
        }
        debug!(
            "Deleted pool file for hash {}.. ({} bytes, namespace {:?})",
            &candidate.hash[..8],
            candidate.size,
            candidate.namespace,
        );

        Ok(candidate.size)
    }

    /// Get current cache usage (must call calculate_usage first)
    pub fn current_usage(&self) -> u64 {
        self.current_bytes
    }

    /// Get cache capacity limit
    pub fn capacity(&self) -> u64 {
        self.cache_bytes
    }

    /// Get cache usage as a percentage
    pub fn usage_percent(&self) -> f64 {
        if self.cache_bytes == 0 {
            0.0
        } else {
            (self.current_bytes as f64 / self.cache_bytes as f64) * 100.0
        }
    }
}

/// Lightweight summary of a cartridge manifest for cache scans.
/// Surfaces just the routing fields we need — the full `Manifest` is
/// heavier than this module needs and would force a dependency on the
/// cartridge module's whole serde surface. The cartridge's `backend`
/// has already been matched against `backend_name` before construction,
/// so we don't carry it here.
struct ManifestRouting {
    dedup: DedupScope,
}

/// Return `Some(routing)` iff the cartridge directory at `tape_path`
/// carries a `manifest.json` whose `backend` field equals `backend_name`.
/// Missing, unparseable, or empty-`backend` manifest → `None` (skip
/// rather than fail the scan; bad manifests get fixed at load time, not
/// by the cache layer).
fn manifest_routing_for_backend(
    tape_path: &std::path::Path,
    backend_name: &str,
) -> Result<Option<ManifestRouting>> {
    let manifest_path = tape_path.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let json = match fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let v: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let resolved = match v.get("backend").and_then(|s| s.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    if resolved != backend_name {
        return Ok(None);
    }
    let dedup = match v.get("dedup").and_then(|s| s.as_str()) {
        Some("local") => DedupScope::Local,
        // Missing / unknown / "global" → Global. Legacy manifests
        // without the field default to Global, matching `#[serde(default)]`
        // on `Manifest::dedup`.
        _ => DedupScope::Global,
    };
    Ok(Some(ManifestRouting { dedup }))
}

// ─────────────────────────────────────────────────────────────────────
// Pool budget startup walker (tape side)
// ─────────────────────────────────────────────────────────────────────
//
// `PoolBudget` itself lives in `shared-pool` — pure byte accounting,
// no tape semantics. The tape-aware startup walker is here: it scans
// `<data_dir>/chunks/<backend>/` (Global-scope chunks) plus every
// `DedupScope::Local` per-cartridge namespace and seeds the budget's
// `current_bytes` so a daemon restart doesn't silently re-grant the
// surviving on-disk bytes. The block side has its own parallel walker
// at `core/sbc/src/disk_cache.rs`.

/// Walk every chunk on disk under `backend_name` and seed
/// `budget.set_pool_buckets(per_namespace)`. Counts the shared
/// per-backend pool (bucketed under `None`) plus each
/// `DedupScope::Local` cartridge's per-cartridge namespace
/// (bucketed under `Some(label)`) — both consume the same
/// disk-cache budget.
pub fn refresh_pool_budget_from_tapes(
    budget: &PoolBudget,
    data_dir: &Path,
    backend_name: &str,
) -> Result<()> {
    let mut buckets: HashMap<Option<String>, u64> = HashMap::new();
    let store = ChunkStore::new(data_dir, backend_name)?;
    let global_sum: u64 = store
        .iter_chunks()?
        .into_iter()
        .map(|(_, sz)| sz)
        .sum::<u64>();
    if global_sum > 0 {
        buckets.insert(None, global_sum);
    }

    let tapes_dir = data_dir.join("tapes");
    if tapes_dir.is_dir() {
        for entry in fs::read_dir(&tapes_dir)? {
            let entry = entry?;
            let routing = match manifest_routing_for_backend(&entry.path(), backend_name)? {
                Some(r) => r,
                None => continue,
            };
            if routing.dedup != DedupScope::Local {
                continue;
            }
            let label = match entry.path().file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let ns_store = ChunkStore::new_namespaced(data_dir, backend_name, &label)?;
            let ns_sum: u64 = ns_store
                .iter_chunks()?
                .into_iter()
                .map(|(_, sz)| sz)
                .sum::<u64>();
            if ns_sum > 0 {
                buckets.insert(Some(label), ns_sum);
            }
        }
    }

    budget.set_pool_buckets(buckets);
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_manager_new() {
        let cache = DiskCacheManager::new(PathBuf::from("/tmp"), "primary", 1024 * 1024);
        assert_eq!(cache.capacity(), 1024 * 1024);
        assert_eq!(cache.current_usage(), 0);
        assert_eq!(cache.backend_name(), "primary");
    }

    #[test]
    fn test_is_over_capacity() {
        let mut cache = DiskCacheManager::new(PathBuf::from("/tmp"), "primary", 1000);
        assert!(!cache.is_over_capacity());

        cache.current_bytes = 1500;
        assert!(cache.is_over_capacity());
    }

    #[test]
    fn test_usage_percent() {
        let mut cache = DiskCacheManager::new(PathBuf::from("/tmp"), "primary", 1000);
        assert_eq!(cache.usage_percent(), 0.0);

        cache.current_bytes = 500;
        assert_eq!(cache.usage_percent(), 50.0);

        cache.current_bytes = 1500;
        assert_eq!(cache.usage_percent(), 150.0);
    }

    #[test]
    fn manifest_routing_for_backend_matches_only_explicit_field() {
        let tmp = tempfile::tempdir().unwrap();
        let tape_path = tmp.path().join("TAPE001");
        std::fs::create_dir_all(&tape_path).unwrap();

        // Missing manifest → None (skip in scans).
        assert!(
            manifest_routing_for_backend(&tape_path, "primary")
                .unwrap()
                .is_none()
        );

        // Empty backend field → None (a malformed manifest is not a
        // routing match for any backend).
        std::fs::write(
            tape_path.join("manifest.json"),
            r#"{"label":"X","chunks":[],"backend":""}"#,
        )
        .unwrap();
        assert!(
            manifest_routing_for_backend(&tape_path, "primary")
                .unwrap()
                .is_none()
        );

        // Missing backend key entirely → also None.
        std::fs::write(
            tape_path.join("manifest.json"),
            r#"{"label":"X","chunks":[]}"#,
        )
        .unwrap();
        assert!(
            manifest_routing_for_backend(&tape_path, "primary")
                .unwrap()
                .is_none()
        );

        // Explicit backend field matches; no dedup field defaults to Global.
        std::fs::write(
            tape_path.join("manifest.json"),
            r#"{"label":"X","chunks":[],"backend":"primary"}"#,
        )
        .unwrap();
        let r = manifest_routing_for_backend(&tape_path, "primary")
            .unwrap()
            .expect("primary match");
        assert_eq!(r.dedup, DedupScope::Global);
        assert!(
            manifest_routing_for_backend(&tape_path, "archive")
                .unwrap()
                .is_none()
        );

        // Local dedup field is parsed.
        std::fs::write(
            tape_path.join("manifest.json"),
            r#"{"label":"X","chunks":[],"backend":"primary","dedup":"local"}"#,
        )
        .unwrap();
        let r = manifest_routing_for_backend(&tape_path, "primary")
            .unwrap()
            .expect("primary local");
        assert_eq!(r.dedup, DedupScope::Local);
    }

    // PoolBudget unit tests now live in shared/pool/src/budget.rs
    // alongside the lifted impl.

    #[test]
    fn set_capacity_overwrites_the_cap() {
        let mut cache = DiskCacheManager::new(PathBuf::from("/tmp"), "primary", 1000);
        cache.set_capacity(5_000);
        assert_eq!(cache.capacity(), 5_000);
        // set_recent_seal_pin_seconds is pure local-state mutation.
        cache.set_recent_seal_pin_seconds(120);
    }

    #[test]
    fn calculate_usage_is_zero_for_an_empty_data_dir() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let mut cache = DiskCacheManager::new(tmp.path().to_path_buf(), "primary", 1 << 20);
        assert_eq!(cache.calculate_usage().expect("usage"), 0);
        assert_eq!(cache.current_usage(), 0);
    }

    #[test]
    fn calculate_usage_counts_a_cartridges_staging_chunks() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cart = tmp.path().join("tapes").join("TAPE01");
        std::fs::create_dir_all(cart.join(".staging")).expect("dirs");
        // manifest.json routes the cartridge to the "primary" backend.
        std::fs::write(
            cart.join("manifest.json"),
            r#"{"label":"TAPE01","chunks":[],"backend":"primary","dedup":"global"}"#,
        )
        .expect("manifest");
        // An unsealed staging chunk takes real disk space.
        std::fs::write(cart.join(".staging").join("chunk-0001"), vec![0u8; 4096])
            .expect("staging chunk");

        let mut cache = DiskCacheManager::new(tmp.path().to_path_buf(), "primary", 1 << 20);
        assert_eq!(cache.calculate_usage().expect("usage"), 4096);
        assert_eq!(cache.current_usage(), 4096);

        // A cartridge bound to a different backend is not counted.
        let mut other = DiskCacheManager::new(tmp.path().to_path_buf(), "archive", 1 << 20);
        assert_eq!(other.calculate_usage().expect("usage"), 0);
    }
}
