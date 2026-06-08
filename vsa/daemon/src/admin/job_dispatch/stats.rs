// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.stats` job — dedup analytics walker (block side).
//!
//! Block-side parallel of `vtl/daemon/src/admin/job_dispatch/stats.rs`.
//! Walks every volume's `pages.idx`, collects the distinct chunk
//! hashes each volume references, and sizes them against the chunk
//! pool. The dedup math itself — the exclusive/shared split and the
//! per-backend unique pool bytes — is the cross-product
//! `shared_dedup_stats::compute_dedup`.
//!
//! Two differences from the tape side:
//!
//! - Entities are volumes (`pages.idx`), not cartridges
//!   (`chunks.idx`); the report carries no `location` column because
//!   `pages.idx` records no local/storage tag.
//! - `pages.idx` records carry no chunk size, so sizes come from the
//!   pool's `iter_chunks()`. A chunk a volume references but that has
//!   been evicted to storage-only is not locally sizeable — it is
//!   excluded from the byte figures (the page still counts toward
//!   `allocated_pages`). `system stats` is a local-pool tuning view.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use core_block::{ChunkPool, DedupScope, PageIndex, VolumeManifest};
use serde::Serialize;
use shared_admin_server::{JobEmitter, JobEvent};
use shared_dedup_stats::{EntityScan, compute_dedup};

use crate::admin::handlers::AdminState;

#[derive(Debug, Clone, Serialize)]
pub struct VolumeStats {
    pub volume: String,
    pub backend: String,
    pub scope: String,
    pub allocated_pages: u64,
    pub logical_bytes: u64,
    pub volume_unique_bytes: u64,
    pub exclusive_bytes: u64,
    pub shared_bytes: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct BackendStats {
    pub backend: String,
    pub volumes_global: u64,
    pub volumes_local: u64,
    pub allocated_pages: u64,
    pub logical_bytes: u64,
    pub unique_pool_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsReport {
    pub backends: Vec<BackendStats>,
    pub volumes: Vec<VolumeStats>,
    pub skipped: Vec<SkippedVolume>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedVolume {
    pub volume: String,
    pub reason: String,
}

pub async fn run(emitter: JobEmitter, _body: serde_json::Value, state: AdminState) {
    let data_dir = state.data_dir.clone();
    emitter
        .info(format!("Walking volumes under {}", data_dir.display()))
        .await;

    let report = match tokio::task::spawn_blocking(move || collect_stats(&data_dir)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("stats failed: {}", e)))
                .await;
            return;
        }
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("stats panicked: {}", e),
                ))
                .await;
            return;
        }
    };

    emitter
        .info(format!(
            "Stats complete: {} backend(s), {} volume(s), {} skipped",
            report.backends.len(),
            report.volumes.len(),
            report.skipped.len(),
        ))
        .await;
    match serde_json::to_value(&report) {
        Ok(v) => emitter.emit(JobEvent::result(v)).await,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("serialize stats: {}", e),
                ))
                .await;
            return;
        }
    }
    emitter.emit(JobEvent::done(0)).await;
}

/// One scanned volume — `pages.idx` walked, dedup math not yet run.
#[derive(Debug)]
struct VolScan {
    volume: String,
    backend: String,
    namespace: Option<String>,
    scope: String,
    allocated_pages: u64,
    logical_bytes: u64,
    hashes: HashSet<String>,
}

fn collect_stats(data_dir: &Path) -> anyhow::Result<StatsReport> {
    let mut skipped: Vec<SkippedVolume> = Vec::new();
    let mut vol_scans: Vec<VolScan> = Vec::new();

    for name in VolumeManifest::list(data_dir)? {
        let manifest = match VolumeManifest::load(data_dir, &name) {
            Ok(m) => m,
            Err(e) => {
                skipped.push(SkippedVolume {
                    volume: name,
                    reason: format!("manifest load failed: {}", e),
                });
                continue;
            }
        };
        let vol_dir = VolumeManifest::dir_for(data_dir, &name);
        let page_index = match PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        ) {
            Ok(p) => p,
            Err(e) => {
                skipped.push(SkippedVolume {
                    volume: name,
                    reason: format!("pages.idx open failed: {}", e),
                });
                continue;
            }
        };

        let mut hashes: HashSet<String> = HashSet::new();
        let mut allocated_pages: u64 = 0;
        let mut decode_error: Option<String> = None;
        for record in page_index.iter() {
            match record {
                Ok((_page_id, hash)) => {
                    allocated_pages += 1;
                    hashes.insert(hex::encode(hash));
                }
                Err(e) => {
                    decode_error = Some(format!("pages.idx iteration failed: {}", e));
                    break;
                }
            }
        }
        if let Some(reason) = decode_error {
            skipped.push(SkippedVolume {
                volume: name,
                reason,
            });
            continue;
        }

        let scope = match manifest.dedup_scope {
            DedupScope::Local => "local",
            DedupScope::Global => "global",
        };
        let logical_bytes = allocated_pages.saturating_mul(u64::from(manifest.page_size_bytes));
        vol_scans.push(VolScan {
            volume: name,
            backend: manifest.backend.clone(),
            namespace: manifest.pool_namespace(),
            scope: scope.to_string(),
            allocated_pages,
            logical_bytes,
            hashes,
        });
    }

    // Size every referenced chunk against its pool. `pages.idx` has
    // no inline size, so `iter_chunks()` is the only local size
    // source — one pass per distinct `(backend, namespace)` pool.
    let mut pool_sizes: HashMap<(String, Option<String>), HashMap<String, u64>> = HashMap::new();
    for vs in &vol_scans {
        let key = (vs.backend.clone(), vs.namespace.clone());
        if pool_sizes.contains_key(&key) {
            continue;
        }
        let pool = match &vs.namespace {
            Some(ns) => ChunkPool::new_namespaced(data_dir, &vs.backend, ns)?,
            None => ChunkPool::new(data_dir, &vs.backend)?,
        };
        let map: HashMap<String, u64> = pool.iter_chunks()?.into_iter().collect();
        pool_sizes.insert(key, map);
    }

    let entity_scans: Vec<EntityScan> = vol_scans
        .iter()
        .map(|vs| {
            let pool_map = pool_sizes
                .get(&(vs.backend.clone(), vs.namespace.clone()))
                .expect("every scanned volume's pool was sized above");
            let chunks: HashMap<String, u64> = vs
                .hashes
                .iter()
                .filter_map(|h| pool_map.get(h).map(|&sz| (h.clone(), sz)))
                .collect();
            EntityScan {
                label: vs.volume.clone(),
                backend: vs.backend.clone(),
                namespace: vs.namespace.clone(),
                chunks,
            }
        })
        .collect();

    let (contribs, backend_dedup) = compute_dedup(&entity_scans);

    let mut volumes: Vec<VolumeStats> = vol_scans
        .into_iter()
        .zip(contribs)
        .map(|(vs, c)| VolumeStats {
            volume: vs.volume,
            backend: vs.backend,
            scope: vs.scope,
            allocated_pages: vs.allocated_pages,
            logical_bytes: vs.logical_bytes,
            volume_unique_bytes: c.unique_bytes,
            exclusive_bytes: c.exclusive_bytes,
            shared_bytes: c.shared_bytes,
        })
        .collect();
    volumes.sort_by(|a, b| {
        a.backend
            .cmp(&b.backend)
            .then_with(|| a.scope.cmp(&b.scope))
            .then_with(|| a.volume.cmp(&b.volume))
    });

    let mut backend_map: BTreeMap<String, BackendStats> = BTreeMap::new();
    for v in &volumes {
        let b = backend_map
            .entry(v.backend.clone())
            .or_insert_with(|| BackendStats {
                backend: v.backend.clone(),
                ..Default::default()
            });
        match v.scope.as_str() {
            "global" => b.volumes_global += 1,
            "local" => b.volumes_local += 1,
            _ => {}
        }
        b.allocated_pages = b.allocated_pages.saturating_add(v.allocated_pages);
        b.logical_bytes = b.logical_bytes.saturating_add(v.logical_bytes);
    }
    for bd in backend_dedup {
        if let Some(b) = backend_map.get_mut(&bd.backend) {
            b.unique_pool_bytes = bd.unique_pool_bytes;
        }
    }

    Ok(StatsReport {
        backends: backend_map.into_values().collect(),
        volumes,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use tempfile::TempDir;

    fn make_volume(data_dir: &Path, name: &str, backend: &str) -> VolumeManifest {
        VolumeManifest::new(
            name.to_string(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            backend.to_string(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(data_dir)
        .unwrap()
    }

    /// Two pages mapped to two distinct sealed chunks: the walker
    /// counts both pages, sizes both chunks from the pool, and the
    /// solo volume owns every chunk exclusively.
    #[test]
    fn volume_stats_count_pages_and_size_chunks() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary");
        let ns = manifest.pool_namespace().unwrap();

        let pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (h1, _) = pool.insert_bytes(&[0x11; 4096]).unwrap();
        let (h2, _) = pool.insert_bytes(&[0x22; 2048]).unwrap();

        let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
        let pages = PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )
        .unwrap();
        let b1: [u8; 32] = hex::decode(&h1).unwrap().try_into().unwrap();
        let b2: [u8; 32] = hex::decode(&h2).unwrap().try_into().unwrap();
        pages.set(0, &b1).unwrap();
        pages.set(1, &b2).unwrap();

        let report = collect_stats(data_dir).unwrap();
        assert_eq!(report.volumes.len(), 1);
        let v = &report.volumes[0];
        assert_eq!(v.allocated_pages, 2);
        assert_eq!(v.logical_bytes, 2 * u64::from(DEFAULT_PAGE_SIZE_BYTES));
        // Two distinct chunks, both resident locally: 4096 + 2048.
        assert_eq!(v.volume_unique_bytes, 6144);
        // Solo volume — every chunk is exclusive, none shared.
        assert_eq!(v.exclusive_bytes, 6144);
        assert_eq!(v.shared_bytes, 0);

        assert_eq!(report.backends.len(), 1);
        assert_eq!(report.backends[0].unique_pool_bytes, 6144);
        assert_eq!(report.backends[0].volumes_local, 1);
        assert_eq!(report.backends[0].allocated_pages, 2);
    }
}
