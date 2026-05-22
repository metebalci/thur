// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.stats` job — dedup analytics walker.
//!
//! Walks every cartridge's `chunks.idx` and groups records by
//! `(manifest.backend, namespace)`. Emits the full structured
//! `StatsReport` as a Result event; the CLI renders human / JSON
//! variants client-side.
//!
//! Walker logic mirrors what used to live in
//! `vtl/cli/src/commands/stats.rs`. Moved into the daemon so
//! a single owner of the on-disk state can answer the question
//! atomically; CLI no longer needs `--allow-running` since the
//! daemon already serializes against in-flight chunks.idx writes
//! through its own locks.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::state::DaemonState;
use core_mediachanger::chunk_index::{ChunkIndexFile, LocationTag};
use serde::Serialize;
use shared_admin_server::{JobEmitter, JobEvent};
use shared_dedup_stats::{EntityScan, compute_dedup};

#[derive(Debug, Default, Clone, Serialize)]
pub struct LocationCounts {
    pub local_only: u64,
    pub both: u64,
    pub cloud_only: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CartridgeStats {
    pub barcode: String,
    pub backend: String,
    pub scope: String,
    pub sealed_chunks: u64,
    pub logical_bytes: u64,
    pub cart_unique_bytes: u64,
    pub exclusive_bytes: u64,
    pub shared_bytes: u64,
    pub location: LocationCounts,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct BackendStats {
    pub backend: String,
    pub cartridges_global: u64,
    pub cartridges_local: u64,
    pub sealed_chunks: u64,
    pub logical_bytes: u64,
    pub unique_pool_bytes: u64,
    pub location: LocationCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsReport {
    pub backends: Vec<BackendStats>,
    pub cartridges: Vec<CartridgeStats>,
    pub skipped: Vec<SkippedCartridge>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedCartridge {
    pub barcode: String,
    pub reason: String,
}

pub async fn run(emitter: JobEmitter, _body: serde_json::Value, state: Arc<DaemonState>) {
    let data_dir = state.data_dir.clone();
    emitter
        .info(format!("Walking cartridges under {}", data_dir.display()))
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
            "Stats complete: {} backend(s), {} cartridge(s), {} skipped",
            report.backends.len(),
            report.cartridges.len(),
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

#[derive(Debug)]
struct CartScan {
    barcode: String,
    backend: String,
    scope: String,
    sealed_chunks: u64,
    logical_bytes: u64,
    location: LocationCounts,
}

fn collect_stats(data_dir: &Path) -> anyhow::Result<StatsReport> {
    let tapes_dir = data_dir.join("tapes");
    let mut skipped = Vec::new();
    if !tapes_dir.is_dir() {
        return Ok(StatsReport {
            backends: Vec::new(),
            cartridges: Vec::new(),
            skipped,
        });
    }

    let mut cart_scans: Vec<CartScan> = Vec::new();
    let mut entity_scans: Vec<EntityScan> = Vec::new();

    for entry in fs::read_dir(&tapes_dir)? {
        let entry = entry?;
        let tape_path = entry.path();
        let manifest_path = tape_path.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }

        let label = match tape_path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let json = match fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(e) => {
                skipped.push(SkippedCartridge {
                    barcode: label,
                    reason: format!("manifest read failed: {}", e),
                });
                continue;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(e) => {
                skipped.push(SkippedCartridge {
                    barcode: label,
                    reason: format!("manifest parse failed: {}", e),
                });
                continue;
            }
        };
        let backend = match v.get("backend").and_then(|s| s.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                skipped.push(SkippedCartridge {
                    barcode: label,
                    reason: "manifest missing or empty `backend` field".into(),
                });
                continue;
            }
        };
        let scope = match v.get("dedup").and_then(|d| d.as_str()) {
            Some("local") => "local",
            _ => "global",
        };
        let namespace: Option<String> = if scope == "local" {
            Some(label.clone())
        } else {
            None
        };

        let chunks_idx_path = ChunkIndexFile::path_for(&tape_path);
        if !chunks_idx_path.is_file() {
            skipped.push(SkippedCartridge {
                barcode: label,
                reason: "no chunks.idx (predates index split or corrupt)".into(),
            });
            continue;
        }
        let cif = match ChunkIndexFile::open_or_create(&tape_path) {
            Ok(f) => f,
            Err(e) => {
                skipped.push(SkippedCartridge {
                    barcode: label,
                    reason: format!("chunks.idx open failed: {}", e),
                });
                continue;
            }
        };

        let mut cart_hashes: HashMap<String, u64> = HashMap::new();
        let mut logical_bytes: u64 = 0;
        let mut sealed_chunks: u64 = 0;
        let mut location = LocationCounts::default();

        for item in cif.iter() {
            let (_id, rec) = match item {
                Ok(p) => p,
                Err(e) => {
                    skipped.push(SkippedCartridge {
                        barcode: label.clone(),
                        reason: format!("chunks.idx record decode failed: {}", e),
                    });
                    break;
                }
            };
            let Some(hash) = rec.hash.as_ref() else {
                continue;
            };
            sealed_chunks += 1;
            logical_bytes = logical_bytes.saturating_add(rec.size);
            cart_hashes
                .entry(hash.clone())
                .and_modify(|s| *s = (*s).max(rec.size))
                .or_insert(rec.size);
            match rec.location {
                LocationTag::LocalOnly => location.local_only += 1,
                LocationTag::Both => location.both += 1,
                LocationTag::CloudOnly => location.cloud_only += 1,
            }
        }

        entity_scans.push(EntityScan {
            label: label.clone(),
            backend: backend.clone(),
            namespace,
            chunks: cart_hashes,
        });
        cart_scans.push(CartScan {
            barcode: label,
            backend,
            scope: scope.to_string(),
            sealed_chunks,
            logical_bytes,
            location,
        });
    }

    let (contribs, backend_dedup) = compute_dedup(&entity_scans);

    let mut cartridges: Vec<CartridgeStats> = cart_scans
        .into_iter()
        .zip(contribs)
        .map(|(s, c)| CartridgeStats {
            barcode: s.barcode,
            backend: s.backend,
            scope: s.scope,
            sealed_chunks: s.sealed_chunks,
            logical_bytes: s.logical_bytes,
            cart_unique_bytes: c.unique_bytes,
            exclusive_bytes: c.exclusive_bytes,
            shared_bytes: c.shared_bytes,
            location: s.location,
        })
        .collect();
    cartridges.sort_by(|a, b| {
        a.backend
            .cmp(&b.backend)
            .then_with(|| a.scope.cmp(&b.scope))
            .then_with(|| a.barcode.cmp(&b.barcode))
    });

    let mut backend_map: BTreeMap<String, BackendStats> = BTreeMap::new();
    for c in &cartridges {
        let b = backend_map
            .entry(c.backend.clone())
            .or_insert_with(|| BackendStats {
                backend: c.backend.clone(),
                ..Default::default()
            });
        match c.scope.as_str() {
            "global" => b.cartridges_global += 1,
            "local" => b.cartridges_local += 1,
            _ => {}
        }
        b.sealed_chunks = b.sealed_chunks.saturating_add(c.sealed_chunks);
        b.logical_bytes = b.logical_bytes.saturating_add(c.logical_bytes);
        b.location.local_only = b.location.local_only.saturating_add(c.location.local_only);
        b.location.both = b.location.both.saturating_add(c.location.both);
        b.location.cloud_only = b.location.cloud_only.saturating_add(c.location.cloud_only);
    }
    for bd in backend_dedup {
        if let Some(b) = backend_map.get_mut(&bd.backend) {
            b.unique_pool_bytes = bd.unique_pool_bytes;
        }
    }

    Ok(StatsReport {
        backends: backend_map.into_values().collect(),
        cartridges,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::chunk_index::ChunkRec;
    use tempfile::TempDir;

    fn hex_hash(byte: u8) -> String {
        let mut s = String::with_capacity(64);
        for _ in 0..32 {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    }

    fn write_manifest(tape_dir: &Path, barcode: &str, backend: &str, dedup: &str) {
        fs::create_dir_all(tape_dir).unwrap();
        let m = serde_json::json!({
            "label": barcode,
            "backend": backend,
            "dedup": dedup,
        });
        fs::write(tape_dir.join("manifest.json"), m.to_string()).unwrap();
    }

    fn append_sealed(cif: &ChunkIndexFile, hash: &str, size: u64, location: LocationTag) {
        cif.append(&ChunkRec {
            size,
            hash: Some(hash.to_string()),
            location,
            uploaded: matches!(location, LocationTag::Both | LocationTag::CloudOnly),
            compression: None,
        })
        .unwrap();
    }

    #[test]
    fn shared_global_chunks_split_into_exclusive_and_shared() {
        let tmp = TempDir::new().unwrap();
        let dd = tmp.path();
        let tapes = dd.join("tapes");

        let t1 = tapes.join("TAPE001");
        write_manifest(&t1, "TAPE001", "primary", "global");
        let cif1 = ChunkIndexFile::open_or_create(&t1).unwrap();
        append_sealed(&cif1, &hex_hash(0xAA), 1024, LocationTag::Both);
        append_sealed(&cif1, &hex_hash(0xBB), 2048, LocationTag::LocalOnly);
        cif1.fsync().unwrap();

        let t2 = tapes.join("TAPE002");
        write_manifest(&t2, "TAPE002", "primary", "global");
        let cif2 = ChunkIndexFile::open_or_create(&t2).unwrap();
        append_sealed(&cif2, &hex_hash(0xAA), 1024, LocationTag::CloudOnly);
        append_sealed(&cif2, &hex_hash(0xCC), 4096, LocationTag::Both);
        cif2.fsync().unwrap();

        let r = collect_stats(dd).unwrap();
        let b = &r.backends[0];
        assert_eq!(b.cartridges_global, 2);
        assert_eq!(b.logical_bytes, 8192);
        assert_eq!(b.unique_pool_bytes, 7168);
        assert_eq!(b.location.both, 2);
        assert_eq!(b.location.local_only, 1);
        assert_eq!(b.location.cloud_only, 1);

        let t1_stats = r
            .cartridges
            .iter()
            .find(|c| c.barcode == "TAPE001")
            .unwrap();
        assert_eq!(t1_stats.exclusive_bytes, 2048);
        assert_eq!(t1_stats.shared_bytes, 1024);

        let t2_stats = r
            .cartridges
            .iter()
            .find(|c| c.barcode == "TAPE002")
            .unwrap();
        assert_eq!(t2_stats.exclusive_bytes, 4096);
        assert_eq!(t2_stats.shared_bytes, 1024);
    }

    #[test]
    fn cartridge_without_chunks_idx_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let dd = tmp.path();
        let tapes = dd.join("tapes");
        let t1 = tapes.join("BROKEN");
        write_manifest(&t1, "BROKEN", "primary", "global");

        let r = collect_stats(dd).unwrap();
        assert!(r.backends.is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.skipped[0].barcode, "BROKEN");
    }
}
