// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product chunk-pool + storage verification core.
//!
//! Both products' `system verify` reduce to the same two sweeps over
//! the content-addressed chunk pool:
//!
//! * **local pool orphan sweep** — for each `(backend, namespace)`
//!   pool, which on-disk chunks are not referenced by any live entity?
//! * **storage HEAD sweep** — for each entity, are the chunks it expects
//!   in storage actually present? And which storage objects are orphans?
//!
//! A product implements [`VerifyTarget`] — its live chunk set and its
//! per-entity storage expectations — and the two sweep functions do the
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

/// Concurrency for the storage-sweep HEAD storm. Matches the upload
/// pipeline's bounded fan-out: low enough to stay under per-provider
/// rate limits, high enough to hide RTT across a multi-million-object
/// sweep.
pub const STORAGE_VERIFY_CONCURRENCY: usize = 16;

/// Live (referenced) chunk hashes bucketed by `(backend, namespace)`.
/// `namespace` is `None` for the backend-global shared pool,
/// `Some(ns)` for a local-scope per-entity pool.
pub type LiveChunkSet = HashMap<(String, Option<String>), HashSet<String>>;

/// One entity (a VTL cartridge or a VSA volume) and the chunk hashes
/// it expects to find in its storage bucket.
#[derive(Debug, Clone)]
pub struct StorageEntity {
    /// Identifies the entity in the returned [`EntityStorageResult`] —
    /// cartridge directory name / volume name.
    pub label: String,
    /// Storage backend the entity is bound to.
    pub backend: String,
    /// `None` for the shared pool, `Some(ns)` for a local-scope pool.
    pub namespace: Option<String>,
    /// Chunk hashes (hex) that should exist in the storage bucket.
    pub chunk_hashes: Vec<String>,
}

/// What a product hands the verification core.
///
/// `Send + Sync` so the storage sweep future stays `Send` — both
/// daemons run it under `tokio::spawn`.
pub trait VerifyTarget: Send + Sync {
    /// Every live chunk hash, bucketed by `(backend, namespace)`.
    /// Drives the local pool orphan sweep.
    fn live_chunks(&self) -> LiveChunkSet;

    /// Per-entity storage-expected chunks. Drives the storage HEAD sweep.
    fn storage_entities(&self) -> Vec<StorageEntity>;

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

/// A failed storage HEAD — surfaced so the caller can warn with cause.
#[derive(Debug, Clone)]
pub struct HeadFailure {
    pub hash: String,
    pub message: String,
}

/// One entity's storage chunk-presence result.
#[derive(Debug, Default, Clone)]
pub struct EntityStorageResult {
    pub label: String,
    /// Expected chunks that HEAD reported absent (or errored).
    pub chunks_missing: u64,
    /// HEAD calls that errored (also counted in `chunks_missing`).
    pub head_errors: Vec<HeadFailure>,
}

/// One backend's storage chunk sweep result.
#[derive(Debug, Default, Clone)]
pub struct StorageChunkSweep {
    pub per_entity: Vec<EntityStorageResult>,
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

/// Sweep one backend's storage bucket: HEAD every chunk each entity
/// bound to `backend_name` expects, then list `chunks/` to count
/// orphan objects.
pub async fn sweep_storage(
    target: &dyn VerifyTarget,
    backend_name: &str,
    backend: &dyn ObjectStoreBackend,
) -> StorageChunkSweep {
    let entities: Vec<StorageEntity> = target
        .storage_entities()
        .into_iter()
        .filter(|e| e.backend == backend_name)
        .collect();

    let mut per_entity: Vec<EntityStorageResult> = Vec::new();
    // Expected chunks, keyed per-namespace by the raw 32-byte digest
    // rather than the full ~79-char object-key String. At the pool's
    // documented ~60 M-chunk scale a HashSet<String> of keys is ~7 GB;
    // the per-namespace [u8;32] form is ~4x smaller and keeps namespace
    // precision (issue #167). The listing below is still the backend's
    // full Vec<String> — paged/streaming listing is a trait-wide change
    // tracked separately.
    let mut expected: HashMap<Option<String>, HashSet<[u8; 32]>> = HashMap::new();

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
        let ns_set = expected.entry(ent.namespace.clone()).or_default();
        for (_, hash) in &jobs {
            if let Some(d) = hex_to_digest(hash) {
                ns_set.insert(d);
            }
        }

        let results: Vec<_> =
            futures::stream::iter(jobs.into_iter().map(|(key, hash)| async move {
                let res = backend.chunk_exists(&key).await;
                (hash, res)
            }))
            .buffer_unordered(STORAGE_VERIFY_CONCURRENCY)
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
        per_entity.push(EntityStorageResult {
            label: ent.label.clone(),
            chunks_missing,
            head_errors,
        });
    }

    let mut sweep = StorageChunkSweep {
        per_entity,
        ..Default::default()
    };
    match backend.list_objects("chunks/").await {
        Ok(keys) => {
            for key in keys {
                sweep.chunk_objects += 1;
                // Parse `chunks/[ns/]<s1>/<s2>/<hash>.dat` into
                // (namespace, digest) and check the matching namespace's
                // expected set. An unparseable key (or one whose hash
                // isn't expected) is an orphan — same as the old
                // full-key membership test.
                let is_expected = match parse_chunk_key(&key) {
                    Some((ns, digest)) => expected
                        .get(&ns)
                        .is_some_and(|set| set.contains(&digest)),
                    None => false,
                };
                if !is_expected {
                    sweep.chunk_orphans += 1;
                }
            }
        }
        Err(e) => sweep.list_error = Some(e.to_string()),
    }
    sweep
}

/// Decode a 64-char BLAKE3 hex string into the raw 32-byte digest.
fn hex_to_digest(s: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

/// Parse a pool object key `chunks/[<ns>/]<s1>/<s2>/<hash>.dat` into its
/// `(namespace, digest)`. Returns `None` if the key doesn't match the
/// expected shape (treated as an orphan by the caller).
fn parse_chunk_key(key: &str) -> Option<(Option<String>, [u8; 32])> {
    let parts: Vec<&str> = key.split('/').collect();
    let (ns, file) = match parts.as_slice() {
        ["chunks", s1, s2, file] if s1.len() == 2 && s2.len() == 2 => (None, *file),
        ["chunks", ns, s1, s2, file] if s1.len() == 2 && s2.len() == 2 => {
            (Some((*ns).to_string()), *file)
        }
        _ => return None,
    };
    let hash = file.strip_suffix(".dat")?;
    let digest = hex_to_digest(hash)?;
    Some((ns, digest))
}

#[cfg(test)]
mod parse_key_tests {
    //! Issue #167: the storage orphan sweep parses each listed key into
    //! (namespace, digest) and checks the matching namespace's expected
    //! [u8;32] set instead of comparing full ~79-char key Strings.
    use super::{ChunkPool, hex_to_digest, parse_chunk_key};

    #[test]
    fn parse_roundtrips_object_key_for() {
        let hash = "ab".repeat(32); // 64 hex chars
        let digest = hex_to_digest(&hash).unwrap();

        // No namespace.
        let key = ChunkPool::object_key_for(None, &hash);
        assert_eq!(parse_chunk_key(&key), Some((None, digest)));

        // With namespace.
        let key = ChunkPool::object_key_for(Some("vol-42"), &hash);
        assert_eq!(
            parse_chunk_key(&key),
            Some((Some("vol-42".to_string()), digest))
        );
    }

    #[test]
    fn parse_rejects_malformed_keys() {
        assert!(parse_chunk_key("chunks/ab/cd/nothex.dat").is_none());
        assert!(parse_chunk_key("chunks/ab/cd/deadbeef").is_none()); // no .dat
        assert!(parse_chunk_key("other/ab/cd/x.dat").is_none()); // wrong root
        assert!(parse_chunk_key("chunks/toolong/cd/x.dat").is_none()); // shard not 2 chars
    }
}
