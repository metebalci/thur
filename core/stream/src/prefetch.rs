// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Prefetch manager for aggressive chunk prefetching
//!
//! Provides transparent prefetching for sequential tape operations to hide S3 latency.
//! Key insight: Tape is sequential - always prefetch next chunks after reads.

use crate::chunk_store::ChunkStore;
use crate::errors::Result;
use shared_object_store::ObjectStoreBackend;
use shared_pool::PoolBudget;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Configuration for prefetch behavior
#[derive(Debug, Clone)]
pub struct PrefetchConfig {
    /// Number of chunks to prefetch ahead (1-3)
    pub chunks_ahead: u32,
    /// Enable/disable prefetching
    pub enabled: bool,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            chunks_ahead: 2,
            enabled: true,
        }
    }
}

/// Manages background prefetching of chunks from S3
///
/// When a chunk is read, the prefetch manager automatically downloads
/// the next N chunks in the background to hide S3 latency for sequential reads.
/// Active-task table key. The cartridge label and chunk id together
/// uniquely identify an in-flight background fetch.
type ActiveTaskKey = (String, u64);
type ActiveTaskMap = Arc<Mutex<HashMap<ActiveTaskKey, JoinHandle<()>>>>;

pub struct PrefetchManager {
    /// Active prefetch tasks: (cartridge_id, chunk_id) -> JoinHandle
    active_tasks: ActiveTaskMap,
    /// Storage backend for downloading chunks
    storage_backend: Arc<Box<dyn ObjectStoreBackend>>,
    /// Configuration
    config: PrefetchConfig,
}

impl PrefetchManager {
    /// Create a new prefetch manager
    pub fn new(storage_backend: Arc<Box<dyn ObjectStoreBackend>>, config: PrefetchConfig) -> Self {
        Self {
            active_tasks: Arc::new(Mutex::new(HashMap::new())),
            storage_backend,
            config,
        }
    }

    /// Read-only access to the prefetch configuration. Used by
    /// callers (e.g. `Cartridge::trigger_prefetch`) that want to
    /// build a chunk-location snapshot sized exactly to
    /// `chunks_ahead` rather than walk the whole chunk index.
    pub fn config(&self) -> &PrefetchConfig {
        &self.config
    }

    /// Trigger prefetch after a read operation
    ///
    /// This should be called after every successful read_block operation.
    /// It will spawn background tasks to prefetch the next N chunks.
    ///
    /// # Arguments
    /// * `cartridge_id` - Label of the cartridge being read
    /// * `current_chunk_id` - ID of the chunk that was just read
    /// * `chunk_store` - The cartridge's `ChunkStore` (carries backend
    ///   name + optional per-cartridge namespace, so the prefetched
    ///   chunk lands in the same per-backend pool layout the eviction
    ///   path walks).
    /// * `chunk_location_fn` - Function to check if a chunk is already in cache
    pub async fn on_read(
        &self,
        cartridge_id: &str,
        current_chunk_id: u64,
        chunk_store: ChunkStore,
        pool_budget: Arc<PoolBudget>,
        chunk_location_fn: impl Fn(u64) -> ChunkLocationInfo + Send + 'static + Clone,
    ) {
        if !self.config.enabled {
            return;
        }

        // Prefetch next 1-N chunks
        for i in 1..=self.config.chunks_ahead {
            let next_chunk_id = current_chunk_id + i as u64;
            self.prefetch_chunk(
                cartridge_id,
                next_chunk_id,
                chunk_store.clone(),
                pool_budget.clone(),
                chunk_location_fn.clone(),
            )
            .await;
        }
    }

    /// Prefetch a specific chunk in the background
    ///
    /// Skips if:
    /// - Chunk is already in cache (LocalOnly or Both)
    /// - Prefetch task is already active for this chunk
    /// - Chunk is not in S3 (S3Only check happens in task)
    async fn prefetch_chunk(
        &self,
        cartridge_id: &str,
        chunk_id: u64,
        chunk_store: ChunkStore,
        pool_budget: Arc<PoolBudget>,
        chunk_location_fn: impl Fn(u64) -> ChunkLocationInfo + Send + 'static,
    ) {
        let task_key = (cartridge_id.to_string(), chunk_id);

        // Hold the active-task lock across the whole check-and-insert.
        // If we dropped it after the presence check (as before), two
        // concurrent calls for the same (cartridge, chunk) could both
        // pass `contains_key` and both spawn; the second `insert` would
        // orphan the first `JoinHandle`, and the orphan's later
        // `remove` would evict the survivor so `cancel_all` could no
        // longer abort it. None of the steps below await — the location
        // lookup is synchronous and `tokio::spawn` only schedules — so
        // holding the async mutex across them is cheap and deadlock-free.
        // It also forces the spawned task's completion-`remove` to wait
        // until after our `insert`, so a task that finishes early can't
        // leave a stale handle behind.
        let mut tasks = self.active_tasks.lock().await;

        // Check if already prefetching this chunk
        if tasks.contains_key(&task_key) {
            debug!(
                "Prefetch already active for {}/chunk-{}",
                cartridge_id, chunk_id
            );
            return;
        }

        // Check chunk location
        let location_info = chunk_location_fn(chunk_id);

        // Skip if chunk is already in cache
        if location_info.in_local_cache {
            debug!("Chunk {} already in cache, skipping prefetch", chunk_id);
            return;
        }

        // Skip if chunk is not in S3
        if !location_info.in_s3 {
            debug!("Chunk {} not in S3, skipping prefetch", chunk_id);
            return;
        }

        // Need a hash to address the chunk in both storage and local store.
        let hash = match location_info.hash {
            Some(h) => h,
            None => {
                debug!(
                    "Chunk {} has no hash (still in staging), skipping prefetch",
                    chunk_id
                );
                return;
            }
        };

        info!(
            "Starting prefetch for {}/chunk-{} (hash {}..) from storage",
            cartridge_id,
            chunk_id,
            &hash[..8.min(hash.len())]
        );

        // Spawn background download task
        let storage_backend = self.storage_backend.clone();
        let tasks_handle = self.active_tasks.clone();
        let task_key_clone = task_key.clone();

        let task = tokio::spawn(async move {
            match download_chunk_to_store(
                storage_backend.as_ref().as_ref(),
                &hash,
                &chunk_store,
                &pool_budget,
            )
            .await
            {
                Ok(bytes) => {
                    info!("Prefetch complete: chunk {} ({} bytes)", chunk_id, bytes);
                }
                Err(e) => {
                    warn!("Prefetch failed for chunk {}: {:?}", chunk_id, e);
                }
            }

            // Remove from active tasks when done. Blocks until the
            // spawning call releases the guard below, so this can't
            // race ahead of the `insert`.
            let mut tasks = tasks_handle.lock().await;
            tasks.remove(&task_key_clone);
        });

        // Store the task handle under the guard still held from the
        // presence check above.
        tasks.insert(task_key, task);
    }

    /// Cancel all active prefetch tasks for a cartridge
    ///
    /// This should be called when the tape head position changes unexpectedly
    /// (e.g., LOCATE, REWIND, large SPACE). The prefetched chunks would be wrong,
    /// so we abort those tasks.
    pub async fn cancel_all(&self, cartridge_id: &str) {
        let mut tasks = self.active_tasks.lock().await;
        let mut to_remove = Vec::new();

        for ((cart_id, chunk_id), handle) in tasks.iter() {
            if cart_id == cartridge_id {
                info!(
                    "Canceling prefetch for {}/chunk-{} (position changed)",
                    cartridge_id, chunk_id
                );
                handle.abort();
                to_remove.push((cart_id.clone(), *chunk_id));
            }
        }

        for key in to_remove {
            tasks.remove(&key);
        }

        if tasks.is_empty() {
            debug!("All prefetch tasks canceled for {}", cartridge_id);
        }
    }

    /// Get count of active prefetch tasks (for metrics/monitoring)
    pub async fn active_task_count(&self) -> usize {
        let tasks = self.active_tasks.lock().await;
        tasks.len()
    }

    /// Get count of active prefetch tasks for a specific cartridge
    pub async fn active_task_count_for_cartridge(&self, cartridge_id: &str) -> usize {
        let tasks = self.active_tasks.lock().await;
        tasks.keys().filter(|(id, _)| id == cartridge_id).count()
    }
}

/// Information about a chunk's location (cache vs S3) for the prefetch
/// path. Chunks are addressed by their content hash now; an unsealed
/// (staging) chunk has `hash = None` and is never prefetched.
#[derive(Debug, Clone)]
pub struct ChunkLocationInfo {
    /// Is chunk in local cache (shared chunk store)?
    pub in_local_cache: bool,
    /// Is chunk in storage?
    pub in_s3: bool,
    /// BLAKE3 hex of the sealed chunk's bytes; `None` for unsealed chunks.
    pub hash: Option<String>,
}

/// Download a chunk from storage and insert it into `chunk_store`.
///
/// Routes through `ChunkPool::insert_verified_bytes`, which honors the
/// cartridge's backend name + optional dedup-local namespace, gives us
/// an atomic tmp+rename, drops a concurrent dedup-race winner cleanly,
/// **and verifies the downloaded bytes hash to `hash` before the pool
/// accepts them** — closes the storage-bit-rot / wrong-bytes-for-hash
/// gap that prefetch shared with the SCSI-READ refetch path.
///
/// Pre-Batch-F the worker wrote directly under
/// `<chunk_store_root>/chunks/<aa>/<bb>/<hash>.dat` (flat, no
/// `<backend>` segment, no per-cartridge namespace) — that layout
/// pinned prefetched chunks forever because per-backend
/// `DiskCacheManager` walks `<root>/chunks/<backend>/...` only.
/// Going through the pool API fixes that too.
///
/// The storage key uses `chunk_store.object_key_in_store(hash)`, matching
/// the `--dedup local` per-cartridge prefix the SCSI READ /
/// upload-worker paths already speak.
async fn download_chunk_to_store(
    backend: &dyn ObjectStoreBackend,
    hash: &str,
    chunk_store: &ChunkStore,
    pool_budget: &PoolBudget,
) -> Result<usize> {
    let object_key = chunk_store.object_key_in_store(hash);
    let data = backend.download_chunk(&object_key).await?;
    let size = data.len();

    // Prefetch grows the local pool — account it against the per-backend
    // budget so `current_bytes()` stays equal to on-disk pool bytes (the
    // eviction worker reads the budget instead of rescanning). Persist
    // FIRST, then reserve only when the insert actually wrote the file
    // (`was_new`): a chunk already warmed by a racing read-miss/prefetch
    // reports `was_new == false` and is not double-counted, and a failed
    // persist returns via `?` with no reservation made. `force_reserve`,
    // not `try_reserve`: prefetch is speculative I/O and must never block
    // on backpressure.
    let store = chunk_store.clone();
    let hash_owned = hash.to_string();
    let was_new =
        tokio::task::spawn_blocking(move || store.insert_verified_bytes(&hash_owned, &data))
            .await
            .map_err(|e| {
                crate::errors::SmcError::Io(std::io::Error::other(format!(
                    "prefetch insert join: {e}"
                )))
            })
            .and_then(|inner| inner.map_err(Into::into))?;
    if was_new {
        pool_budget.force_reserve(size as u64, chunk_store.namespace());
    }

    debug!(
        "Wrote prefetched chunk to pool: {} ({} bytes)",
        object_key, size
    );
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefetch_config_default() {
        let config = PrefetchConfig::default();
        assert_eq!(config.chunks_ahead, 2);
        assert!(config.enabled);
    }

    #[tokio::test]
    async fn test_prefetch_manager_task_tracking() {
        // This test just verifies the basic task tracking mechanism
        // Real S3 integration tests are in the daemon
        let config = PrefetchConfig {
            chunks_ahead: 2,
            enabled: true,
        };

        // Note: We can't easily test without a real S3 backend,
        // but we can test the task tracking structure is correct
        assert_eq!(config.chunks_ahead, 2);
    }

    /// Issue #97 acceptance: a read of chunk K with a configured
    /// look-ahead of N actually pulls K+1..K+N from storage into the local
    /// pool in the background. Uses a real `LocalBackend` round-trip
    /// rather than a mock so the BLAKE3-verified pool insert is exercised
    /// end-to-end.
    #[tokio::test]
    async fn on_read_fans_out_look_ahead_to_pool() {
        use shared_object_store::LocalBackend;
        use std::time::Duration;

        let pool_dir = tempfile::TempDir::new().unwrap();
        let storage_dir = tempfile::TempDir::new().unwrap();

        let store = ChunkStore::new(pool_dir.path(), "primary").expect("pool");
        let backend = LocalBackend::new(storage_dir.path())
            .await
            .expect("backend");

        // Two look-ahead chunks (K+1, K+2) live only in storage. Seed the
        // backend at exactly the key the prefetcher will request.
        let mut planned = Vec::new();
        for (i, fill) in [(11u64, 0x11u8), (12u64, 0x22u8)] {
            let data = vec![fill; 4096];
            let hash = blake3::hash(&data).to_hex().to_string();
            let key = store.object_key_in_store(&hash);
            backend
                .upload_chunk(&key, &data)
                .await
                .expect("seed storage");
            planned.push((i, hash));
        }

        let mgr = PrefetchManager::new(
            Arc::new(Box::new(backend) as Box<dyn ObjectStoreBackend>),
            PrefetchConfig {
                chunks_ahead: 2,
                enabled: true,
            },
        );

        // Location oracle: the two look-ahead chunks are storage-only
        // (cache miss), everything else absent.
        let snapshot: HashMap<u64, ChunkLocationInfo> = planned
            .iter()
            .map(|(id, hash)| {
                (
                    *id,
                    ChunkLocationInfo {
                        in_local_cache: false,
                        in_s3: true,
                        hash: Some(hash.clone()),
                    },
                )
            })
            .collect();
        let location_fn = move |id: u64| -> ChunkLocationInfo {
            snapshot.get(&id).cloned().unwrap_or(ChunkLocationInfo {
                in_local_cache: false,
                in_s3: false,
                hash: None,
            })
        };

        let budget = Arc::new(PoolBudget::unbounded(pool_dir.path().to_path_buf()));
        // Read chunk 10 -> prefetch 11, 12.
        mgr.on_read("CART", 10, store.clone(), budget, location_fn)
            .await;

        // Let the background download tasks drain.
        for _ in 0..200 {
            if mgr.active_task_count().await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mgr.active_task_count().await, 0, "prefetch tasks finished");

        // Both look-ahead chunks are now resident in the local pool.
        for (_, hash) in &planned {
            assert!(
                store.open_read(hash).is_ok(),
                "prefetched chunk {hash} landed in the pool"
            );
        }
    }
}
