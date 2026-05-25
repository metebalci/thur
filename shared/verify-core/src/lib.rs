// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product chunk-pool + cloud verification core.
//!
//! Both products' `system verify` reduce to the same two sweeps over
//! the content-addressed chunk pool:
//!
//! * **local pool orphan sweep** — for each `(backend, namespace)`
//!   pool, which on-disk chunks are not referenced by any live entity?
//! * **cloud HEAD sweep** — for each entity, are the chunks it expects
//!   in cloud actually present? And which cloud objects are orphans?
//!
//! A product implements [`VerifyTarget`] — its live chunk set and its
//! per-entity cloud expectations — and the two sweep functions do the
//! rest. Everything *around* the sweeps (tape library/partition checks,
//! block page-table integrity, the product's `VerifyReport` shape, the
//! gc-hint wording) stays per-product. The tape side additionally HEADs
//! index-page objects and the manifest sentinel; those have no block
//! analogue and stay in `core-mediachanger`.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use futures::stream::StreamExt;
use shared_object_store::ObjectStoreBackend;
use shared_pool::ChunkPool;

/// Concurrency for the cloud-sweep HEAD storm. Matches the upload
/// pipeline's bounded fan-out: low enough to stay under per-provider
/// rate limits, high enough to hide RTT across a multi-million-object
/// sweep.
pub const CLOUD_VERIFY_CONCURRENCY: usize = 16;

/// Live (referenced) chunk hashes bucketed by `(backend, namespace)`.
/// `namespace` is `None` for the backend-global shared pool,
/// `Some(ns)` for a local-scope per-entity pool.
pub type LiveChunkSet = HashMap<(String, Option<String>), HashSet<String>>;

/// One entity (a VTL cartridge or a VSA volume) and the chunk hashes
/// it expects to find in its cloud bucket.
#[derive(Debug, Clone)]
pub struct CloudEntity {
    /// Identifies the entity in the returned [`EntityCloudResult`] —
    /// cartridge directory name / volume name.
    pub label: String,
    /// Cloud backend the entity is bound to.
    pub backend: String,
    /// `None` for the shared pool, `Some(ns)` for a local-scope pool.
    pub namespace: Option<String>,
    /// Chunk hashes (hex) that should exist in the cloud bucket.
    pub chunk_hashes: Vec<String>,
}

/// What a product hands the verification core.
///
/// `Send + Sync` so the cloud sweep future stays `Send` — both
/// daemons run it under `tokio::spawn`.
pub trait VerifyTarget: Send + Sync {
    /// Every live chunk hash, bucketed by `(backend, namespace)`.
    /// Drives the local pool orphan sweep.
    fn live_chunks(&self) -> LiveChunkSet;

    /// Per-entity cloud-expected chunks. Drives the cloud HEAD sweep.
    fn cloud_entities(&self) -> Vec<CloudEntity>;

    /// Distinct backends in use. Default: the backends named in
    /// [`Self::live_chunks`].
    fn backends(&self) -> Vec<String> {
        let mut b: Vec<String> = self
            .live_chunks()
            .keys()
            .map(|(backend, _)| backend.clone())
            .collect();
        b.sort();
        b.dedup();
        b
    }
}

/// One pool's orphan tally — the shared pool or one local namespace.
#[derive(Debug, Default, Clone)]
pub struct NamespaceSweep {
    /// `None` for the shared pool, `Some(ns)` for a namespace pool.
    pub namespace: Option<String>,
    /// Total chunk files in the pool.
    pub chunks: u64,
    /// Chunks not referenced by any live entity.
    pub orphans: u64,
    /// Sum of orphan chunk sizes.
    pub orphan_bytes: u64,
}

/// One backend's local pool sweep result.
#[derive(Debug, Default, Clone)]
pub struct PoolSweep {
    pub backend: String,
    /// The backend-global shared pool.
    pub shared: NamespaceSweep,
    /// Every local-scope namespace pool, sorted by namespace name.
    pub namespaces: Vec<NamespaceSweep>,
    /// Namespace dirs present on disk but referenced by no live
    /// entity — the whole dir is reclaimable.
    pub orphan_namespace_dirs: Vec<String>,
    /// Pool open / scan failures.
    pub errors: Vec<String>,
}

/// A failed cloud HEAD — surfaced so the caller can warn with cause.
#[derive(Debug, Clone)]
pub struct HeadFailure {
    pub hash: String,
    pub message: String,
}

/// One entity's cloud chunk-presence result.
#[derive(Debug, Default, Clone)]
pub struct EntityCloudResult {
    pub label: String,
    /// Expected chunks that HEAD reported absent (or errored).
    pub chunks_missing: u64,
    /// HEAD calls that errored (also counted in `chunks_missing`).
    pub head_errors: Vec<HeadFailure>,
}

/// One backend's cloud chunk sweep result.
#[derive(Debug, Default, Clone)]
pub struct CloudChunkSweep {
    pub per_entity: Vec<EntityCloudResult>,
    /// Total objects under `chunks/`.
    pub chunk_objects: u64,
    /// `chunks/` objects referenced by no entity bound to this backend.
    pub chunk_orphans: u64,
    /// Set when the `chunks/` listing itself failed.
    pub list_error: Option<String>,
}

/// Sweep every `(backend, namespace)` pool for orphan chunks. One
/// [`PoolSweep`] per backend, sorted by backend name.
pub fn sweep_local_pool(data_dir: &Path, target: &dyn VerifyTarget) -> Vec<PoolSweep> {
    let live = target.live_chunks();
    let mut backends: Vec<String> = live.keys().map(|(b, _)| b.clone()).collect();
    backends.sort();
    backends.dedup();
    backends
        .iter()
        .map(|backend| sweep_one_backend_pool(data_dir, backend, &live))
        .collect()
}

fn sweep_one_backend_pool(data_dir: &Path, backend: &str, live: &LiveChunkSet) -> PoolSweep {
    let mut errors: Vec<String> = Vec::new();
    let empty: HashSet<String> = HashSet::new();

    // Shared (backend-global) pool.
    let shared_live = live.get(&(backend.to_string(), None)).unwrap_or(&empty);
    let shared = match ChunkPool::new(data_dir, backend) {
        Ok(pool) => sweep_pool_chunks(&pool, shared_live, None, "shared pool", &mut errors),
        Err(e) => {
            errors.push(format!("shared pool open failed: {}", e));
            NamespaceSweep::default()
        }
    };

    // Namespaces to sweep: those named in the live set, plus any
    // namespace dir still on disk (its entity may be gone).
    let mut live_namespaces: BTreeSet<String> = BTreeSet::new();
    for (b, ns) in live.keys() {
        if b == backend
            && let Some(name) = ns
        {
            live_namespaces.insert(name.clone());
        }
    }
    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    let pool_root = data_dir.join("chunks").join(backend);
    if pool_root.is_dir()
        && let Ok(rd) = fs::read_dir(&pool_root)
    {
        for entry in rd.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            // 2-hex dirs are the shared pool's shard dirs.
            if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                on_disk.insert(name);
            }
        }
    }
    let orphan_namespace_dirs: Vec<String> = on_disk
        .iter()
        .filter(|n| !live_namespaces.contains(*n))
        .cloned()
        .collect();

    let mut namespaces: Vec<NamespaceSweep> = Vec::new();
    for ns in live_namespaces.union(&on_disk) {
        let ns_live = live
            .get(&(backend.to_string(), Some(ns.clone())))
            .unwrap_or(&empty);
        match ChunkPool::new_namespaced(data_dir, backend, ns) {
            Ok(pool) => namespaces.push(sweep_pool_chunks(
                &pool,
                ns_live,
                Some(ns.clone()),
                &format!("namespace '{}'", ns),
                &mut errors,
            )),
            Err(e) => errors.push(format!("namespace '{}' open failed: {}", ns, e)),
        }
    }

    PoolSweep {
        backend: backend.to_string(),
        shared,
        namespaces,
        orphan_namespace_dirs,
        errors,
    }
}

fn sweep_pool_chunks(
    pool: &ChunkPool,
    live: &HashSet<String>,
    namespace: Option<String>,
    context: &str,
    errors: &mut Vec<String>,
) -> NamespaceSweep {
    let mut sweep = NamespaceSweep {
        namespace,
        ..Default::default()
    };
    match pool.iter_chunks() {
        Ok(items) => {
            for (hash, size) in items {
                sweep.chunks += 1;
                if !live.contains(&hash) {
                    sweep.orphans += 1;
                    sweep.orphan_bytes += size;
                }
            }
        }
        Err(e) => errors.push(format!("{} scan failed: {}", context, e)),
    }
    sweep
}

/// Sweep one backend's cloud bucket: HEAD every chunk each entity
/// bound to `backend_name` expects, then list `chunks/` to count
/// orphan objects.
pub async fn sweep_storage(
    target: &dyn VerifyTarget,
    backend_name: &str,
    backend: &dyn ObjectStoreBackend,
) -> CloudChunkSweep {
    let entities: Vec<CloudEntity> = target
        .cloud_entities()
        .into_iter()
        .filter(|e| e.backend == backend_name)
        .collect();

    let mut per_entity: Vec<EntityCloudResult> = Vec::new();
    let mut expected: HashSet<String> = HashSet::new();

    for ent in &entities {
        let jobs: Vec<(String, String)> = ent
            .chunk_hashes
            .iter()
            .map(|h| {
                (
                    ChunkPool::object_key_for(ent.namespace.as_deref(), h),
                    h.clone(),
                )
            })
            .collect();
        for (key, _) in &jobs {
            expected.insert(key.clone());
        }

        let results: Vec<_> =
            futures::stream::iter(jobs.into_iter().map(|(key, hash)| async move {
                let res = backend.chunk_exists(&key).await;
                (hash, res)
            }))
            .buffer_unordered(CLOUD_VERIFY_CONCURRENCY)
            .collect()
            .await;

        let mut chunks_missing: u64 = 0;
        let mut head_errors: Vec<HeadFailure> = Vec::new();
        for (hash, res) in results {
            match res {
                Ok(true) => {}
                Ok(false) => chunks_missing += 1,
                Err(e) => {
                    chunks_missing += 1;
                    head_errors.push(HeadFailure {
                        hash,
                        message: e.to_string(),
                    });
                }
            }
        }
        per_entity.push(EntityCloudResult {
            label: ent.label.clone(),
            chunks_missing,
            head_errors,
        });
    }

    let mut sweep = CloudChunkSweep {
        per_entity,
        ..Default::default()
    };
    match backend.list_objects("chunks/").await {
        Ok(keys) => {
            for key in keys {
                sweep.chunk_objects += 1;
                if !expected.contains(&key) {
                    sweep.chunk_orphans += 1;
                }
            }
        }
        Err(e) => sweep.list_error = Some(e.to_string()),
    }
    sweep
}
