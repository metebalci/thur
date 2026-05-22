// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Volume-wide consistency check — the block-storage analogue of
//! `core_mediachanger::verify`.
//!
//! Read-only auditor. For every volume under `<data_dir>/volumes/`:
//!
//! * **page-index integrity** — `pages.idx` opens cleanly and its
//!   header binds to the volume (magic / schema version / record size
//!   / `volume_uuid` / `page_size`). [`PageIndex::open`] performs the
//!   binding checks; a failure is a hard error.
//! * **chunk presence** — every chunk hash the page table references
//!   resolves to a pool file. A referenced chunk absent from the local
//!   pool is a *warning* (it may simply be evicted to cloud-only); the
//!   optional cloud sweep is what turns genuine loss into an error.
//! * **local pool orphan sweep** — chunks in the pool that no live
//!   volume references. Surfaced as GC hints, never errors.
//!
//! The two pool sweeps (local orphan scan, cloud HEAD sweep) are the
//! cross-product `shared-verify-core` primitives — the same code the
//! tape verifier runs. What stays here is block-specific: the
//! page-table integrity check and the report shape.
//!
//! Two entry points mirror the tape side: [`verify_local`] runs only
//! the on-disk checks; [`verify_with_cloud`] layers a per-backend HEAD
//! sweep on top.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use shared_cloud::CloudConfig;
use shared_verify_core::{CloudEntity, LiveChunkSet, VerifyTarget};

use crate::chunk_pool::ChunkPool;
use crate::page_index::PageIndex;
use crate::volume::{DedupScope, VolumeError, VolumeManifest};

/// Optional volume-name filter. Empty = every volume.
#[derive(Debug, Default, Clone)]
pub struct VerifyScope {
    pub volumes: Vec<String>,
}

impl VerifyScope {
    fn matches(&self, name: &str) -> bool {
        self.volumes.is_empty() || self.volumes.iter().any(|v| v == name)
    }
}

/// Top-level result. Serializable so the daemon can ship it to the
/// CLI as a single job `Result` event.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VolumeVerifyReport {
    pub volumes: Vec<VolumeReport>,
    pub pool: Vec<PoolReport>,
}

impl VolumeVerifyReport {
    /// Total errors — drives the CLI exit code.
    pub fn error_count(&self) -> usize {
        self.volumes.iter().map(|v| v.errors.len()).sum::<usize>()
            + self.pool.iter().map(|p| p.errors.len()).sum::<usize>()
    }

    /// Total warnings.
    pub fn warning_count(&self) -> usize {
        self.volumes.iter().map(|v| v.warnings.len()).sum::<usize>()
            + self.pool.iter().map(|p| p.warnings.len()).sum::<usize>()
    }

    /// Total GC-hint count across all backends.
    pub fn gc_hint_count(&self) -> usize {
        self.pool.iter().map(|p| p.gc_hints.len()).sum::<usize>()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VolumeReport {
    pub volume: String,
    pub backend: Option<String>,
    /// `"local"` or `"global"` dedup scope.
    pub scope: Option<String>,
    /// `pages.idx` opened and its header bound to the volume.
    pub pages_idx_ok: bool,
    pub allocated_pages: u64,
    /// Distinct chunk hashes the page table references whose pool file
    /// is absent locally. A warning, not an error — eviction to
    /// cloud-only is normal; the cloud sweep confirms genuine loss.
    pub local_chunks_missing: u64,
    /// Chunks absent from the cloud bucket on HEAD. `None` when the
    /// cloud sweep was skipped.
    pub cloud_chunks_missing: Option<u64>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PoolReport {
    pub backend: String,
    pub shared_chunks: u64,
    pub shared_orphans: u64,
    pub shared_orphan_bytes: u64,
    pub namespaces: Vec<NamespaceReport>,
    pub orphan_namespace_dirs: Vec<String>,
    pub gc_hints: Vec<String>,
    /// Cloud-side counters. `None` when the cloud sweep was skipped.
    pub cloud: Option<CloudReport>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NamespaceReport {
    pub namespace: String,
    pub chunks: u64,
    pub orphans: u64,
    pub orphan_bytes: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CloudReport {
    /// Total chunk objects under `chunks/`.
    pub chunk_objects: u64,
    /// Chunk objects no live volume references.
    pub chunk_orphans: u64,
}

/// One scanned volume reduced to what the shared sweeps consume.
struct VolEntity {
    volume: String,
    backend: String,
    namespace: Option<String>,
    /// Distinct chunk hashes (hex) the page table references.
    hashes: Vec<String>,
}

/// [`VerifyTarget`] adapter over the scanned volumes.
struct BlockVerifyTarget {
    entities: Vec<VolEntity>,
}

impl VerifyTarget for BlockVerifyTarget {
    fn live_chunks(&self) -> LiveChunkSet {
        let mut out: LiveChunkSet = HashMap::new();
        for e in &self.entities {
            let bucket = out
                .entry((e.backend.clone(), e.namespace.clone()))
                .or_default();
            for h in &e.hashes {
                bucket.insert(h.clone());
            }
        }
        out
    }

    fn cloud_entities(&self) -> Vec<CloudEntity> {
        self.entities
            .iter()
            .map(|e| CloudEntity {
                label: e.volume.clone(),
                backend: e.backend.clone(),
                namespace: e.namespace.clone(),
                chunk_hashes: e.hashes.clone(),
            })
            .collect()
    }
}

/// Local-only consistency pass. Use when the operator passed
/// `--skip-cloud`; otherwise call [`verify_with_cloud`].
pub fn verify_local(
    data_dir: &Path,
    scope: &VerifyScope,
) -> Result<VolumeVerifyReport, VolumeError> {
    let (report, _target) = verify_inner(data_dir, scope)?;
    Ok(report)
}

/// Local pass plus a per-backend cloud HEAD sweep. Backend errors are
/// recorded into the report and the sweep continues — one degraded
/// backend shouldn't mask another's findings.
pub async fn verify_with_cloud(
    data_dir: &Path,
    scope: &VerifyScope,
    cloud_cfg: &CloudConfig,
) -> Result<VolumeVerifyReport, VolumeError> {
    let (mut report, target) = verify_inner(data_dir, scope)?;
    cloud_sweep(&mut report, &target, cloud_cfg).await;
    Ok(report)
}

fn verify_inner(
    data_dir: &Path,
    scope: &VerifyScope,
) -> Result<(VolumeVerifyReport, BlockVerifyTarget), VolumeError> {
    let mut report = VolumeVerifyReport::default();
    let mut entities: Vec<VolEntity> = Vec::new();

    for name in VolumeManifest::list(data_dir)? {
        if !scope.matches(&name) {
            continue;
        }
        let mut vr = VolumeReport {
            volume: name.clone(),
            ..Default::default()
        };

        let manifest = match VolumeManifest::load(data_dir, &name) {
            Ok(m) => m,
            Err(e) => {
                vr.errors.push(format!("manifest load failed: {}", e));
                report.volumes.push(vr);
                continue;
            }
        };
        vr.backend = Some(manifest.backend.clone());
        let scope_str = match manifest.dedup_scope {
            DedupScope::Local => "local",
            DedupScope::Global => "global",
        };
        vr.scope = Some(scope_str.to_string());
        let namespace = manifest.pool_namespace();

        // Page-index integrity: `open` validates magic / schema /
        // record size and binds the index to the volume's uuid +
        // page size. A failure here is a hard error.
        let vol_dir = VolumeManifest::dir_for(data_dir, &name);
        let page_index = match PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        ) {
            Ok(p) => {
                vr.pages_idx_ok = true;
                p
            }
            Err(e) => {
                vr.errors
                    .push(format!("pages.idx integrity check failed: {}", e));
                report.volumes.push(vr);
                continue;
            }
        };

        // Walk the page table; collect distinct referenced hashes.
        let mut hash_set: HashSet<String> = HashSet::new();
        let mut iter_failed = false;
        for record in page_index.iter() {
            match record {
                Ok((_page_id, hash)) => {
                    vr.allocated_pages += 1;
                    hash_set.insert(hex::encode(hash));
                }
                Err(e) => {
                    vr.errors.push(format!("pages.idx iteration failed: {}", e));
                    iter_failed = true;
                    break;
                }
            }
        }
        if iter_failed {
            report.volumes.push(vr);
            continue;
        }

        // Chunk presence: every referenced hash should resolve to a
        // pool file. Absence is a warning — it may just be evicted.
        let pool = match &namespace {
            Some(ns) => ChunkPool::new_namespaced(data_dir, &manifest.backend, ns),
            None => ChunkPool::new(data_dir, &manifest.backend),
        };
        match pool {
            Ok(pool) => {
                for h in &hash_set {
                    if !pool.exists(h) {
                        vr.local_chunks_missing += 1;
                    }
                }
            }
            Err(e) => vr.errors.push(format!("chunk pool open failed: {}", e)),
        }
        if vr.local_chunks_missing > 0 {
            vr.warnings.push(format!(
                "{} referenced chunk(s) absent from the local pool — evicted to \
                 cloud-only, or lost; the cloud sweep confirms which",
                vr.local_chunks_missing
            ));
        }

        entities.push(VolEntity {
            volume: name.clone(),
            backend: manifest.backend.clone(),
            namespace,
            hashes: hash_set.into_iter().collect(),
        });
        report.volumes.push(vr);
    }

    // Local pool orphan sweep — shared with the tape verifier.
    let target = BlockVerifyTarget { entities };
    for sweep in shared_verify_core::sweep_local_pool(data_dir, &target) {
        report.pool.push(pool_report_from_sweep(sweep));
    }

    Ok((report, target))
}

/// Map a `shared-verify-core` local-pool sweep into the block
/// `PoolReport`, layering on the GC-hint lines.
fn pool_report_from_sweep(sweep: shared_verify_core::PoolSweep) -> PoolReport {
    let mut p = PoolReport {
        backend: sweep.backend,
        shared_chunks: sweep.shared.chunks,
        shared_orphans: sweep.shared.orphans,
        shared_orphan_bytes: sweep.shared.orphan_bytes,
        namespaces: sweep
            .namespaces
            .into_iter()
            .map(|n| NamespaceReport {
                namespace: n.namespace.unwrap_or_default(),
                chunks: n.chunks,
                orphans: n.orphans,
                orphan_bytes: n.orphan_bytes,
            })
            .collect(),
        orphan_namespace_dirs: sweep.orphan_namespace_dirs,
        errors: sweep.errors,
        ..Default::default()
    };

    if p.shared_orphans > 0 {
        p.gc_hints.push(format!(
            "shared pool has {} orphan chunks ({} bytes) — `system gc` would free them",
            p.shared_orphans, p.shared_orphan_bytes
        ));
    }
    let ns_orphan_total: u64 = p.namespaces.iter().map(|n| n.orphans).sum();
    let ns_orphan_bytes: u64 = p.namespaces.iter().map(|n| n.orphan_bytes).sum();
    if ns_orphan_total > 0 {
        p.gc_hints.push(format!(
            "{} namespace(s) hold {} orphan chunks ({} bytes) — `system gc` would free them",
            p.namespaces.iter().filter(|n| n.orphans > 0).count(),
            ns_orphan_total,
            ns_orphan_bytes
        ));
    }
    if !p.orphan_namespace_dirs.is_empty() {
        p.gc_hints.push(format!(
            "{} orphan namespace dir(s) (volume gone) — `system gc` would reclaim them",
            p.orphan_namespace_dirs.len()
        ));
    }

    p
}

/// Per-backend cloud HEAD sweep. Local-type backends are skipped.
async fn cloud_sweep(
    report: &mut VolumeVerifyReport,
    target: &BlockVerifyTarget,
    cloud_cfg: &CloudConfig,
) {
    let backends: HashSet<String> = target.entities.iter().map(|e| e.backend.clone()).collect();

    for backend_name in backends {
        match cloud_cfg.backend_entry(&backend_name) {
            Ok(entry) => {
                if entry.backend_type() == "local" {
                    continue;
                }
            }
            Err(_) => {
                if let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name) {
                    pr.errors.push(format!(
                        "backend '{}' referenced by a volume but not defined under \
                         `cloud.backends:` — skipping cloud sweep",
                        backend_name
                    ));
                }
                continue;
            }
        }

        let backend = match cloud_cfg.create_backend_named(&backend_name).await {
            Ok(b) => b,
            Err(e) => {
                if let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name) {
                    pr.errors.push(format!(
                        "cloud backend '{}' open failed: {}",
                        backend_name, e
                    ));
                }
                continue;
            }
        };

        let sweep = shared_verify_core::sweep_cloud(target, &backend_name, &*backend).await;
        if let Some(e) = &sweep.list_error
            && let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name)
        {
            pr.errors.push(format!("cloud chunks/ list failed: {}", e));
        }

        for ent in &sweep.per_entity {
            if let Some(vr) = report.volumes.iter_mut().find(|v| v.volume == ent.label) {
                vr.cloud_chunks_missing = Some(ent.chunks_missing);
                for hf in &ent.head_errors {
                    vr.warnings.push(format!(
                        "cloud HEAD failed for chunk {}: {}",
                        short_hash(&hf.hash),
                        hf.message
                    ));
                }
                if ent.chunks_missing > 0 {
                    vr.errors.push(format!(
                        "{} chunk(s) missing from cloud (cold-bucket DR will fail)",
                        ent.chunks_missing
                    ));
                }
            }
        }

        if let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name) {
            let cloud = CloudReport {
                chunk_objects: sweep.chunk_objects,
                chunk_orphans: sweep.chunk_orphans,
            };
            if cloud.chunk_orphans > 0 {
                pr.gc_hints.push(format!(
                    "cloud has {} orphan chunk object(s) — `system gc --cloud` would free them",
                    cloud.chunk_orphans
                ));
            }
            pr.cloud = Some(cloud);
        }
    }
}

fn short_hash(h: &str) -> String {
    let n = h.len().min(8);
    format!("{}..", &h[..n])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
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

    fn open_pages(data_dir: &Path, name: &str, m: &VolumeManifest) -> PageIndex {
        let vol_dir = VolumeManifest::dir_for(data_dir, name);
        PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            m.uuid,
            u64::from(m.page_size_bytes),
        )
        .unwrap()
    }

    #[test]
    fn clean_volume_reports_no_errors() {
        let tmp = TempDir::new().unwrap();
        let dd = tmp.path();
        let m = make_volume(dd, "vol-a", "primary");
        let ns = m.pool_namespace().unwrap();

        let pool = ChunkPool::new_namespaced(dd, "primary", &ns).unwrap();
        let (h, _) = pool.insert_bytes(&[0x11; 4096]).unwrap();
        let bytes: [u8; 32] = hex::decode(&h).unwrap().try_into().unwrap();
        open_pages(dd, "vol-a", &m).set(0, &bytes).unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        assert_eq!(report.error_count(), 0, "{:#?}", report);
        assert_eq!(report.volumes.len(), 1);
        let v = &report.volumes[0];
        assert!(v.pages_idx_ok);
        assert_eq!(v.allocated_pages, 1);
        assert_eq!(v.local_chunks_missing, 0);
    }

    #[test]
    fn referenced_chunk_absent_is_a_warning_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let dd = tmp.path();
        let m = make_volume(dd, "vol-a", "primary");

        // Map a page to a hash that was never sealed into the pool.
        let phantom = [0xABu8; 32];
        open_pages(dd, "vol-a", &m).set(0, &phantom).unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        let v = &report.volumes[0];
        assert_eq!(v.local_chunks_missing, 1);
        assert_eq!(report.error_count(), 0, "absence is a warning");
        assert!(report.warning_count() >= 1);
    }

    #[test]
    fn orphan_pool_chunk_is_a_gc_hint() {
        let tmp = TempDir::new().unwrap();
        let dd = tmp.path();
        let m = make_volume(dd, "vol-a", "primary");
        let ns = m.pool_namespace().unwrap();

        // Seal a chunk no page references.
        let pool = ChunkPool::new_namespaced(dd, "primary", &ns).unwrap();
        pool.insert_bytes(&[0x22; 2048]).unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        assert_eq!(report.error_count(), 0, "{:#?}", report);
        let p = report.pool.iter().find(|p| p.backend == "primary").unwrap();
        let ns_orphans: u64 = p.namespaces.iter().map(|n| n.orphans).sum();
        assert_eq!(ns_orphans, 1);
        assert!(!p.gc_hints.is_empty());
    }
}
