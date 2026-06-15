// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Library-wide consistency check.
//!
//! Read-only auditor: walks `library.json`, `inventory.json`, every
//! cartridge under `<data_dir>/tapes/`, every per-cartridge `chunks.idx`
//! / `blocks-p<N>.idx`, and every chunk file in the per-backend pool.
//! Reports three classes of finding:
//!
//! * **Errors** — invariants that must hold for the daemon to operate
//!   safely (missing referenced chunk, header-corrupt index, manifest
//!   `backend` field empty, BlockRec.chunk_id past chunks.idx tail).
//!   Verify exits non-zero.
//! * **Warnings** — soft anomalies that don't block operation (lru.idx
//!   length doesn't match chunks.idx, missing-but-recoverable sidecars,
//!   inventory references a barcode whose cartridge dir is gone).
//! * **GC hints** — orphan chunks in the pool (live set doesn't
//!   reference them) and orphan namespace dirs left by deleted
//!   cartridges. Surfaced as counts so the operator knows when running
//!   `system gc` would actually free space; verify itself never deletes
//!   anything.
//!
//! Two entry points: [`verify_local`] runs only the on-disk checks;
//! [`verify_with_storage`] does the same plus a per-backend HEAD sweep
//! against every storage bucket cartridges are bound to (chunks marked
//! StorageOnly/Both, every index-page object up to `index_epoch[label].pages`,
//! and the `manifests/<barcode>/manifest-latest.json` sentinel —
//! together that proves cold-bucket DR readiness). Local-type
//! backends are silently skipped during the storage phase.

use crate::block_index::BlockIndexFile;
use crate::chunk_index::{ChunkIndexFile, ChunkRec, LocationTag};
use crate::chunk_store::ChunkStore;
use crate::errors::Result;
use crate::object_store_backend::ObjectStoreBackend;
use crate::object_store_config::ObjectStoreConfig;
use core_stream::TAG_LEN;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use shared_verify_core::{LiveChunkSet, StorageEntity, VerifyTarget};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

/// `(chunk_id, hash, recorded_size, location_flag)` — one entry per
/// hashed chunk in chunks.idx. Used by [`verify_local_pool`] for the
/// presence/size walk and stashed verbatim into
/// [`CartridgeChunkSet::records`] for the later storage sweep.
/// The hash is the raw 32-byte BLAKE3 digest, not its 64-char hex
/// string: this set is retained for the whole verify run (including
/// the later storage sweep), so storing `[u8; 32]` inline instead of a
/// heap `String` cuts the retained footprint from ~90-120 B/chunk to
/// 32 B/chunk — gigabytes on a large library (issue #164). The
/// String-consuming boundaries (`live_chunks` / `storage_entities` /
/// `verify_local_pool`) hex-encode transiently.
type CartChunkRec = (u64, [u8; 32], u64, LocationTag);

/// Decode a 64-char BLAKE3 hex string into the raw 32-byte digest.
fn hex_to_digest(s: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

/// Manifest-derived working values [`verify_one_cartridge`] needs
/// after [`read_manifest`] has populated the per-cartridge report.
struct ManifestInfo {
    /// Raw parsed manifest.json — index_epoch is read out later.
    value: serde_json::Value,
    /// `backend` field (post-empty-string filter). `None` triggers a
    /// non-fatal error in `r` and gates the local-pool + storage sweep.
    backend: Option<String>,
    /// `<dir_name>` when manifest declares `dedup: local`, otherwise
    /// `None` (global pool — the default).
    namespace: Option<String>,
    /// Partition IDs derived from manifest `partitions[]`. At least
    /// one entry; defaults to `[0]` for pre-multi-partition manifests.
    partitions: Vec<u8>,
    /// True when the manifest carries an `encryption` stanza, i.e. the
    /// cartridge seals at-rest-encrypted chunks. Each pool object is
    /// then `plaintext + TAG_LEN` (the AES-256-GCM tag), so the
    /// local-pool size check must add that overhead to the recorded
    /// plaintext size.
    encrypted: bool,
}

/// Per-cartridge view used by both consistency checks (does the local
/// pool have this hash?) and the storage sweep (does the bucket have
/// this hash?).
#[derive(Debug, Clone)]
struct CartridgeChunkSet {
    backend: String,
    namespace: Option<String>,
    /// Manifest's `label` field — the authoritative barcode for storage
    /// keys (`manifests/<barcode>/...`). Mirrors gc.rs's
    /// `collect_live_index_pages` choice; on-disk dir name and
    /// manifest label normally match but the manifest is the contract.
    barcode_label: String,
    /// (chunk_id, hash, recorded size, location flag).
    records: Vec<CartChunkRec>,
    /// `manifest.index_epoch[label].pages` — count of pages the storage
    /// is supposed to hold per index file. Empty for cartridges
    /// predating delta-page index backup.
    index_pages: BTreeMap<String, u32>,
}

/// [`VerifyTarget`] adapter over the cartridge chunk-sets — feeds the
/// shared local-pool + storage sweeps in `shared-verify-core`. The
/// tape-specific checks (library, partitions, index pages, sentinel)
/// stay in this module; only the chunk-pool sweeps are shared.
struct TapeVerifyTarget<'a> {
    cart_sets: &'a BTreeMap<String, CartridgeChunkSet>,
}

impl VerifyTarget for TapeVerifyTarget<'_> {
    fn live_chunks(&self) -> LiveChunkSet {
        let mut out: LiveChunkSet = HashMap::new();
        for c in self.cart_sets.values() {
            let bucket = out
                .entry((c.backend.clone(), c.namespace.clone()))
                .or_default();
            for (_, h, _, _) in &c.records {
                bucket.insert(hex::encode(h));
            }
        }
        out
    }

    fn storage_entities(&self) -> Vec<StorageEntity> {
        self.cart_sets
            .iter()
            .map(|(dir, c)| StorageEntity {
                label: dir.clone(),
                backend: c.backend.clone(),
                namespace: c.namespace.clone(),
                // Only StorageOnly / Both chunks are expected in the
                // bucket; LocalOnly chunks have never been uploaded.
                chunk_hashes: c
                    .records
                    .iter()
                    .filter(|(_, _, _, loc)| {
                        matches!(loc, LocationTag::StorageOnly | LocationTag::Both)
                    })
                    .map(|(_, h, _, _)| hex::encode(h))
                    .collect(),
            })
            .collect()
    }
}

/// Top-level result. Serializable so `--json` is a one-liner.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VerifyReport {
    pub library: LibraryReport,
    pub cartridges: Vec<CartridgeReport>,
    pub pool: Vec<PoolReport>,
}

impl VerifyReport {
    /// Total error count across the whole report. Drives the CLI exit
    /// code: zero = clean, non-zero = inconsistencies.
    pub fn error_count(&self) -> usize {
        self.library.errors.len()
            + self
                .cartridges
                .iter()
                .map(|c| c.errors.len())
                .sum::<usize>()
            + self.pool.iter().map(|p| p.errors.len()).sum::<usize>()
    }

    /// Total warning count.
    pub fn warning_count(&self) -> usize {
        self.library.warnings.len()
            + self
                .cartridges
                .iter()
                .map(|c| c.warnings.len())
                .sum::<usize>()
            + self.pool.iter().map(|p| p.warnings.len()).sum::<usize>()
    }

    /// Total GC-hint orphan count across all backends.
    pub fn gc_hint_count(&self) -> usize {
        self.pool.iter().map(|p| p.gc_hints.len()).sum::<usize>()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LibraryReport {
    pub library_json_present: bool,
    pub inventory_json_present: bool,
    pub num_storage_slots: u32,
    pub num_mail_slots: u32,
    pub num_drives: u32,
    /// Barcodes referenced by inventory but with no on-disk cartridge.
    pub missing_cartridges: Vec<String>,
    /// Cartridge dirs not referenced by inventory.
    pub orphan_cartridges: Vec<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CartridgeReport {
    /// Directory name under `<data_dir>/tapes/`.
    pub dir: String,
    pub label: Option<String>,
    pub backend: Option<String>,
    pub dedup: Option<String>,
    pub manifest_ok: bool,
    pub chunks_idx_present: bool,
    pub chunks_idx_records: u64,
    pub chunks_with_hash: u64,
    pub partitions: Vec<PartitionReport>,
    /// Hashes referenced by `chunks.idx` whose pool file is missing
    /// when the location flag says it should be local
    /// (LocalOnly / Both).
    pub local_chunks_missing: u64,
    /// Hashes whose pool file size doesn't match the recorded size.
    pub local_chunks_size_mismatch: u64,
    /// Storage chunks (StorageOnly / Both) not present in the bucket on
    /// HEAD. `None` when the storage sweep was skipped.
    pub storage_chunks_missing: Option<u64>,
    /// Index-page objects under `manifests/<barcode>/<label>/page-NNNN.dat`
    /// missing in the bucket. `None` when the storage sweep was skipped.
    pub storage_index_pages_missing: Option<u64>,
    /// `Some(true)` if `manifests/<barcode>/manifest-latest.json`
    /// exists, `Some(false)` if missing, `None` if skipped.
    pub storage_sentinel_present: Option<bool>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PartitionReport {
    pub partition: u8,
    pub records: u64,
    pub data_blocks: u64,
    pub filemarks: u64,
    /// Records whose chunk_id >= chunks.idx.next_id().
    pub chunk_id_oob: u64,
    /// Records whose offset+len exceeds the recorded chunk size.
    pub offset_oob: u64,
    /// Set when an *existing* blocks-p<N>.idx failed to open (corrupt
    /// header / truncated). The caller promotes this to a hard error —
    /// a present-but-unreadable index is exactly the corruption verify
    /// exists to catch (issue #165).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_error: Option<String>,
    /// Records that failed to decode (structurally bad). Promoted to a
    /// hard error by the caller (issue #165).
    #[serde(default)]
    pub record_read_errors: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PoolReport {
    pub backend: String,
    pub shared_chunks: u64,
    pub shared_orphans: u64,
    pub shared_orphan_bytes: u64,
    pub namespaces: Vec<NamespacePoolReport>,
    pub orphan_namespace_dirs: Vec<String>,
    /// Operator-visible "running `system gc` would help" lines.
    pub gc_hints: Vec<String>,
    /// Storage-backend-side counters. `None` when the storage sweep
    /// was skipped for this backend (`--skip-storage`, local backend,
    /// or unreachable).
    pub storage: Option<StoragePoolReport>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Storage-side companion to PoolReport. Sizes aren't reported because
/// `list_objects` doesn't return them — we'd have to HEAD every key
/// just to size orphans, which doubles the request count for what's
/// already a hint.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StoragePoolReport {
    /// Total chunk objects under `chunks/` (shared + namespaced).
    pub chunk_objects: u64,
    /// Chunk objects not referenced by any cartridge bound to this
    /// backend (sum across shared pool + every Local-scope namespace).
    pub chunk_orphans: u64,
    /// Stale index-page objects past `index_epoch[label].pages` for
    /// every cartridge bound to this backend.
    pub index_page_orphans: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NamespacePoolReport {
    pub barcode: String,
    pub chunks: u64,
    pub orphans: u64,
    pub orphan_bytes: u64,
}

/// Run the local consistency pass against `data_dir`. Use this when
/// the operator passed `--skip-storage`; otherwise call
/// [`verify_with_storage`] which layers the bucket HEAD sweep on top.
pub fn verify_local(data_dir: &Path, scope: &VerifyScope) -> Result<VerifyReport> {
    let (report, _cart_sets) = verify_local_inner(data_dir, scope)?;
    Ok(report)
}

/// Local pass + storage HEAD sweep against every backend referenced by
/// at least one cartridge. `local`-type backends are silently skipped
/// (no storage surface). Errors talking to a backend are recorded as
/// PoolReport errors and verify continues — one degraded backend
/// shouldn't mask another's findings.
pub async fn verify_with_storage(
    data_dir: &Path,
    scope: &VerifyScope,
    storage_cfg: &ObjectStoreConfig,
) -> Result<VerifyReport> {
    let (mut report, cart_sets) = verify_local_inner(data_dir, scope)?;
    storage_sweep(&mut report, &cart_sets, storage_cfg).await;
    Ok(report)
}

fn verify_local_inner(
    data_dir: &Path,
    scope: &VerifyScope,
) -> Result<(VerifyReport, BTreeMap<String, CartridgeChunkSet>)> {
    let mut report = VerifyReport::default();

    // Library + inventory cross-check first — fast and gives the
    // cartridge sweep a barcode→presence map for orphan detection.
    let inventory_barcodes = verify_library(data_dir, &mut report.library)?;

    // Walk every cartridge directory and record per-cartridge chunk
    // sets along the way (the pool sweep needs them).
    let mut cart_sets: BTreeMap<String, CartridgeChunkSet> = BTreeMap::new();
    let tapes_dir = data_dir.join("tapes");
    if tapes_dir.is_dir() {
        for entry in fs::read_dir(&tapes_dir)? {
            let entry = entry?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let dir_name = match dir.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !scope.matches(&dir_name) {
                continue;
            }
            let cr = verify_one_cartridge(&dir, &dir_name, &mut cart_sets);
            report.cartridges.push(cr);
        }
    }
    // Inventory cross-check runs whether or not tapes/ exists — a fresh
    // host that's lost its tapes directory is exactly the case verify
    // is supposed to flag. Skip when the operator scoped to a barcode
    // subset; the report doesn't have full state and "missing" would
    // be misleading.
    if scope.barcodes.is_empty() {
        let on_disk: HashSet<String> = report.cartridges.iter().map(|c| c.dir.clone()).collect();
        for bc in &inventory_barcodes {
            if !on_disk.contains(bc) {
                report.library.missing_cartridges.push(bc.clone());
                report
                    .library
                    .errors
                    .push(format!("inventory references {} but no cartridge dir", bc));
            }
        }
        for c in &report.cartridges {
            if !inventory_barcodes.contains(&c.dir) {
                report.library.orphan_cartridges.push(c.dir.clone());
                report
                    .library
                    .warnings
                    .push(format!("cartridge dir {} not in inventory", c.dir));
            }
        }
    }

    // Sweep each backend's local pool for orphan chunks via the
    // shared verification core; map each sweep into a tape PoolReport.
    let target = TapeVerifyTarget {
        cart_sets: &cart_sets,
    };
    for sweep in shared_verify_core::sweep_local_pool(data_dir, &target) {
        report.pool.push(pool_report_from_sweep(sweep));
    }

    Ok((report, cart_sets))
}

/// Optional barcode filter for verify. Empty list = all cartridges.
#[derive(Debug, Default, Clone)]
pub struct VerifyScope {
    pub barcodes: Vec<String>,
}

impl VerifyScope {
    pub fn matches(&self, dir: &str) -> bool {
        self.barcodes.is_empty() || self.barcodes.iter().any(|b| b == dir)
    }
}

/// Read library.json and inventory.json. Returns the set of barcodes
/// referenced by inventory (storage_slots / mail_slots / drives).
fn verify_library(data_dir: &Path, out: &mut LibraryReport) -> Result<HashSet<String>> {
    let mut barcodes: HashSet<String> = HashSet::new();
    let lib_path = data_dir.join("library").join("library.json");
    let inv_path = data_dir.join("library").join("inventory.json");

    out.library_json_present = lib_path.is_file();
    out.inventory_json_present = inv_path.is_file();

    if out.library_json_present {
        let text = fs::read_to_string(&lib_path)?;
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                out.num_storage_slots = v
                    .get("num_storage_slots")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32;
                out.num_mail_slots = v
                    .get("num_mail_slots")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32;
                out.num_drives = v.get("num_drives").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            }
            Err(e) => out.errors.push(format!("library.json parse failed: {}", e)),
        }
    } else {
        out.errors.push(
            "library.json missing — configure `library:` in /etc/thurvtl/thurvtl.yaml and start the daemon"
                .to_string(),
        );
    }

    if out.inventory_json_present {
        let text = fs::read_to_string(&inv_path)?;
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                for key in ["storage_slots", "mail_slots", "drives"] {
                    if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
                        for item in arr {
                            if let Some(b) = item.get("barcode").and_then(|s| s.as_str())
                                && !b.is_empty()
                            {
                                barcodes.insert(b.to_string());
                            }
                        }
                    }
                }
            }
            Err(e) => out
                .errors
                .push(format!("inventory.json parse failed: {}", e)),
        }
    } else {
        out.errors.push("inventory.json missing".to_string());
    }

    Ok(barcodes)
}

/// Per-cartridge consistency pass. Records the chunk set in
/// `cart_sets` so the pool sweep can score live vs orphan after every
/// cartridge has been visited.
fn verify_one_cartridge(
    dir: &Path,
    dir_name: &str,
    cart_sets: &mut BTreeMap<String, CartridgeChunkSet>,
) -> CartridgeReport {
    let mut r = CartridgeReport {
        dir: dir_name.to_string(),
        ..Default::default()
    };

    let mi = match read_manifest(dir, dir_name, &mut r) {
        Some(x) => x,
        None => return r,
    };

    let (n, chunk_sizes, records_for_set) = match read_chunks_index(dir, &mut r) {
        Some(x) => x,
        None => return r,
    };

    verify_block_indexes(dir, &mi.partitions, n, &chunk_sizes, &mut r);

    if let Some(backend_name) = mi.backend.as_ref() {
        verify_local_pool(
            dir,
            backend_name,
            mi.namespace.as_deref(),
            mi.encrypted,
            &records_for_set,
            &mut r,
        );
        register_cart_set(
            &mi.value,
            dir_name,
            backend_name,
            mi.namespace,
            r.label.clone(),
            records_for_set,
            cart_sets,
        );
    }

    check_lru_index(dir, &mut r);

    r
}

/// Read + parse manifest.json, populate the manifest-derived fields on
/// `r`, and return the parsed value plus the working values the rest
/// of the cartridge verify pass needs.
///
/// Returns `None` on a fatal early-return (missing / unreadable /
/// unparseable manifest); the corresponding error has already been
/// pushed into `r.errors`.
fn read_manifest(dir: &Path, dir_name: &str, r: &mut CartridgeReport) -> Option<ManifestInfo> {
    let manifest_path = dir.join("manifest.json");
    if !manifest_path.is_file() {
        r.errors.push("manifest.json missing".to_string());
        return None;
    }
    let json = match fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            r.errors.push(format!("manifest.json read failed: {}", e));
            return None;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(e) => {
            r.errors.push(format!("manifest.json parse failed: {}", e));
            return None;
        }
    };
    r.manifest_ok = true;
    r.label = v
        .get("label")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let backend = v
        .get("backend")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    if backend.is_none() {
        r.errors.push("manifest backend field missing/empty".into());
    }
    r.backend = backend.clone();
    let dedup_str = v.get("dedup").and_then(|d| d.as_str()).unwrap_or("global");
    r.dedup = Some(dedup_str.to_string());
    let namespace: Option<String> = if dedup_str == "local" {
        Some(dir_name.to_string())
    } else {
        None
    };

    // Partition count from manifest — used to verify each
    // blocks-p<N>.idx file is present, no extras.
    let partitions: Vec<u8> = match v.get("partitions").and_then(|p| p.as_array()) {
        Some(arr) if !arr.is_empty() => (0..arr.len() as u8).collect(),
        _ => vec![0u8], // legacy manifests with no `partitions` field default to a single P0
    };

    // Encrypted cartridges carry an `encryption` object in the
    // manifest (absent for plaintext carts — the field is
    // skip_serializing_if = Option::is_none on the writer side). The
    // pool then holds ciphertext+tag, 16 bytes longer per chunk.
    let encrypted = v.get("encryption").map(|e| !e.is_null()).unwrap_or(false);

    Some(ManifestInfo {
        value: v,
        backend,
        namespace,
        partitions,
        encrypted,
    })
}

/// Open chunks.idx and walk every record, populating the chunk-count
/// fields on `r`. Returns `None` on a fatal early-return (missing or
/// open-failed); per-record read errors are non-fatal and the walk
/// continues.
///
/// On success returns `(next_id, chunk_sizes, records_for_set)` where
/// `records_for_set` is the subset of records with a hash (the only
/// ones the local-pool + storage-sweep care about).
fn read_chunks_index(
    dir: &Path,
    r: &mut CartridgeReport,
) -> Option<(u64, Vec<Option<u64>>, Vec<CartChunkRec>)> {
    // chunks.idx: open via the canonical type so the header magic +
    // version are validated.
    let chunks_idx_path = ChunkIndexFile::path_for(dir);
    r.chunks_idx_present = chunks_idx_path.is_file();
    if !r.chunks_idx_present {
        r.errors.push("chunks.idx missing".to_string());
        return None;
    }
    let cif = match ChunkIndexFile::open_or_create(dir) {
        Ok(f) => f,
        Err(e) => {
            r.errors.push(format!("chunks.idx open failed: {}", e));
            return None;
        }
    };
    let n = cif.next_id();
    r.chunks_idx_records = n;
    let mut chunk_sizes: Vec<Option<u64>> = vec![None; n as usize];
    let mut records_for_set: Vec<CartChunkRec> = Vec::new();
    // Batched iteration (64 KiB / 1024 records per read) instead of one
    // pread per record — ~12K reads vs 50M syscalls on a large library
    // (issue #164).
    for item in cif.iter() {
        match item {
            Ok((id, rec)) => {
                chunk_sizes[id as usize] = Some(rec.size);
                if let ChunkRec {
                    hash: Some(h),
                    location,
                    size,
                    ..
                } = &rec
                {
                    r.chunks_with_hash += 1;
                    match hex_to_digest(h) {
                        Some(d) => records_for_set.push((id, d, *size, *location)),
                        None => r
                            .errors
                            .push(format!("chunks.idx record {id} has a malformed hash")),
                    }
                }
            }
            Err(e) => r
                .errors
                .push(format!("chunks.idx record read failed: {e}")),
        }
    }
    Some((n, chunk_sizes, records_for_set))
}

/// Validate every blocks-p<N>.idx file declared by the manifest's
/// partition list. Empty-but-listed partitions get a warning rather
/// than an error (an LTFS partition isn't materialised until the
/// first write).
fn verify_block_indexes(
    dir: &Path,
    partitions: &[u8],
    chunks_next_id: u64,
    chunk_sizes: &[Option<u64>],
    r: &mut CartridgeReport,
) {
    for partition in partitions {
        let pr = verify_one_partition(dir, *partition, chunks_next_id, chunk_sizes);
        if let Some(open_err) = &pr.open_error {
            // Present-but-unreadable index: a hard error, not a warning.
            r.errors.push(open_err.clone());
        } else if pr.record_read_errors > 0 {
            r.errors.push(format!(
                "blocks-p{}.idx has {} undecodable record(s)",
                partition, pr.record_read_errors
            ));
        } else if pr.records == 0 && !BlockIndexFile::path_for(dir, *partition).is_file() {
            // Partition listed in manifest but no blocks file. For an
            // unwritten LTFS-formatted partition this is normal (it's
            // created on first write); flag it as a warning rather
            // than an error.
            r.warnings.push(format!(
                "blocks-p{}.idx missing — empty partition (no writes yet)",
                partition
            ));
        }
        r.partitions.push(pr);
    }
}

/// Local-pool presence + size sanity for every chunk the manifest
/// claims should be local (LocalOnly or Both). Counters land on
/// `r.local_chunks_missing` / `r.local_chunks_size_mismatch`.
fn verify_local_pool(
    dir: &Path,
    backend_name: &str,
    namespace: Option<&str>,
    encrypted: bool,
    records_for_set: &[CartChunkRec],
    r: &mut CartridgeReport,
) {
    // chunks.idx records the plaintext chunk size; an encrypted
    // cartridge seals each chunk as one AES-256-GCM block, so its pool
    // object is TAG_LEN bytes longer. (The IV is derived, not stored,
    // so the tag is the only on-disk overhead.)
    let pool_overhead: u64 = if encrypted { TAG_LEN as u64 } else { 0 };
    let parent = dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| Path::new("."));
    let store = match namespace {
        Some(ns) => ChunkStore::new_namespaced(parent, backend_name, ns),
        None => ChunkStore::new(parent, backend_name),
    };
    match store {
        Ok(store) => {
            for (_id, hash, size, location) in records_for_set {
                let need_local = matches!(location, LocationTag::LocalOnly | LocationTag::Both);
                if !need_local {
                    continue;
                }
                let hash_hex = hex::encode(hash);
                let path = store.store_path(&hash_hex);
                match path.metadata() {
                    Ok(meta) if meta.is_file() => {
                        let expected = *size + pool_overhead;
                        if meta.len() != expected {
                            r.local_chunks_size_mismatch += 1;
                            r.errors.push(format!(
                                "chunk {} size mismatch: chunks.idx={} expected pool={} actual pool={}",
                                short_hash(&hash_hex),
                                size,
                                expected,
                                meta.len()
                            ));
                        }
                    }
                    _ => {
                        r.local_chunks_missing += 1;
                        r.errors.push(format!(
                            "chunk {} missing from local pool ({})",
                            short_hash(&hash_hex),
                            location_str(*location)
                        ));
                    }
                }
            }
        }
        Err(e) => r.errors.push(format!("chunk store open failed: {}", e)),
    }
}

/// Stash the cartridge's chunk-set into `cart_sets` for the
/// later pool sweep. The barcode_label comes from the manifest's
/// `label` field (drives storage key prefixes); index_pages come from
/// `manifest.index_epoch[label].pages` (count of pages the storage is
/// supposed to hold per index file — empty for cartridges predating
/// delta-page index backup).
fn register_cart_set(
    manifest_v: &serde_json::Value,
    dir_name: &str,
    backend_name: &str,
    namespace: Option<String>,
    label: Option<String>,
    records_for_set: Vec<CartChunkRec>,
    cart_sets: &mut BTreeMap<String, CartridgeChunkSet>,
) {
    let barcode_label = label.unwrap_or_else(|| dir_name.to_string());
    let mut index_pages: BTreeMap<String, u32> = BTreeMap::new();
    if let Some(epoch_obj) = manifest_v.get("index_epoch").and_then(|m| m.as_object()) {
        for (label, eob) in epoch_obj {
            if let Some(pages) = eob.get("pages").and_then(|p| p.as_u64())
                && pages <= u32::MAX as u64
            {
                index_pages.insert(label.clone(), pages as u32);
            }
        }
    }
    cart_sets.insert(
        dir_name.to_string(),
        CartridgeChunkSet {
            backend: backend_name.to_string(),
            namespace,
            barcode_label,
            records: records_for_set,
            index_pages,
        },
    );
}

/// lru.idx is local-only and rebuildable; warn-only on size/header
/// mismatch. Header is 32 bytes + 8 bytes per record (u64 LE).
fn check_lru_index(dir: &Path, r: &mut CartridgeReport) {
    let lru_path = dir.join("lru.idx");
    if lru_path.is_file()
        && let Ok(meta) = lru_path.metadata()
    {
        let expected = 32 + 8 * r.chunks_idx_records;
        if meta.len() != expected {
            r.warnings.push(format!(
                "lru.idx size {} doesn't match chunks.idx ({} expected) — rebuilds on next daemon start",
                meta.len(),
                expected
            ));
        }
    }
}

fn verify_one_partition(
    dir: &Path,
    partition: u8,
    chunks_next_id: u64,
    chunk_sizes: &[Option<u64>],
) -> PartitionReport {
    let mut p = PartitionReport {
        partition,
        ..Default::default()
    };
    let path = BlockIndexFile::path_for(dir, partition);
    if !path.is_file() {
        return p;
    }
    let bif = match BlockIndexFile::open_or_create(dir, partition) {
        Ok(f) => f,
        Err(e) => {
            // The file EXISTS (checked above) but its header failed to
            // validate — corrupt magic/version or a truncated header.
            // Record it so the caller can raise a hard error; returning
            // a clean records=0 report would let `system verify` exit 0
            // on a cartridge whose every host READ will fail (issue
            // #165).
            p.open_error = Some(format!("blocks-p{partition}.idx open failed: {e}"));
            return p;
        }
    };
    let n = bif.next_lba();
    p.records = n;
    for lba in 0..n {
        let rec = match bif.read(lba) {
            Ok(r) => r,
            Err(_) => {
                // Structurally bad record — count it; the caller raises
                // a hard error (issue #165).
                p.record_read_errors += 1;
                continue;
            }
        };
        match rec.kind {
            crate::tape::BlockKind::Filemark => p.filemarks += 1,
            crate::tape::BlockKind::Data => p.data_blocks += 1,
        }
        if u64::from(rec.chunk_id) >= chunks_next_id {
            p.chunk_id_oob += 1;
            continue;
        }
        if let Some(Some(size)) = chunk_sizes.get(rec.chunk_id as usize) {
            // BlockRec offset+len must fit inside the chunk's bytes.
            // Filemarks have len=0; offset is the position the FM
            // splits the chunk at, so offset == size is fine for
            // tail-anchored filemarks.
            let end = u64::from(rec.offset) + u64::from(rec.len);
            if end > *size {
                p.offset_oob += 1;
            }
        }
    }
    p
}

/// Map a `shared-verify-core` local-pool sweep into the tape
/// `PoolReport`, layering on the tape-flavored GC-hint lines.
fn pool_report_from_sweep(sweep: shared_verify_core::PoolSweep) -> PoolReport {
    let mut p = PoolReport {
        backend: sweep.backend,
        shared_chunks: sweep.shared.chunks,
        shared_orphans: sweep.shared.orphans,
        shared_orphan_bytes: sweep.shared.orphan_bytes,
        namespaces: sweep
            .namespaces
            .into_iter()
            .map(|n| NamespacePoolReport {
                barcode: n.namespace.unwrap_or_default(),
                chunks: n.chunks,
                orphans: n.orphans,
                orphan_bytes: n.orphan_bytes,
            })
            .collect(),
        orphan_namespace_dirs: sweep.orphan_namespace_dirs,
        errors: sweep.errors,
        ..Default::default()
    };

    // GC hint summaries — counts only, no per-hash dump (verbose mode
    // can already inspect individual cartridges).
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
            "{} orphan namespace dir(s) (cartridge gone) — `system gc` would reclaim them",
            p.orphan_namespace_dirs.len()
        ));
    }

    p
}

fn short_hash(h: &str) -> String {
    let n = h.len().min(8);
    format!("{}..", &h[..n])
}

/// Run the per-backend storage HEAD sweep. For each backend referenced
/// by at least one cartridge in `cart_sets`:
///   1. HEAD every chunk that should exist in storage (StorageOnly/Both),
///      counted into the relevant cartridge's `storage_chunks_missing`.
///   2. HEAD every index page `0..pages` per `index_epoch[label]`,
///      counted into `storage_index_pages_missing`.
///   3. HEAD `manifests/<barcode>/manifest-latest.json` and record it
///      as the cartridge's `storage_sentinel_present`.
///   4. List `chunks/...` and `manifests/<barcode>/` for orphan
///      detection (GC hints, mirrors the local pool sweep).
///
/// Local-type backends are skipped — there's no storage surface, only
/// the on-disk filesystem path the local pass already covered. A
/// backend that fails to come up is recorded as a PoolReport error
/// and the sweep moves on.
async fn storage_sweep(
    report: &mut VerifyReport,
    cart_sets: &BTreeMap<String, CartridgeChunkSet>,
    storage_cfg: &ObjectStoreConfig,
) {
    // Mirror local pass: one storage sweep per backend that has at least
    // one cartridge bound to it. Cartridges bound to a backend not in
    // `storage.backends:` are recorded as a manifest-side error (config
    // drift) — surface and skip.
    let backends_in_use: HashSet<String> = cart_sets.values().map(|c| c.backend.clone()).collect();

    for backend_name in backends_in_use {
        // Skip local backends — no storage surface.
        let backend_type = match storage_cfg.backend_entry(&backend_name) {
            Ok(entry) => entry.backend_type(),
            Err(_) => {
                if let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name) {
                    pr.errors.push(format!(
                        "backend '{}' referenced by cartridge but not defined under `storage.backends:` — skipping storage sweep",
                        backend_name
                    ));
                }
                continue;
            }
        };
        if backend_type == "local" {
            continue;
        }

        let backend = match storage_cfg.create_backend_named(&backend_name).await {
            Ok(b) => b,
            Err(e) => {
                if let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name) {
                    pr.errors.push(format!(
                        "storage backend '{}' open failed: {}",
                        backend_name, e
                    ));
                }
                continue;
            }
        };

        sweep_one_backend_storage(report, cart_sets, &backend_name, &*backend).await;
    }
}

/// Per-backend storage sweep. The chunk dimension — HEAD presence plus
/// the `chunks/` orphan scan — is the cross-product `shared-verify-core`
/// sweep. The index-page and manifest-sentinel HEADs are tape-only and
/// stay here.
async fn sweep_one_backend_storage(
    report: &mut VerifyReport,
    cart_sets: &BTreeMap<String, CartridgeChunkSet>,
    backend_name: &str,
    backend: &dyn ObjectStoreBackend,
) {
    let mut storage_pool = StoragePoolReport::default();

    // Chunk dimension — the local pool orphan sweep's storage twin,
    // shared with the block product.
    let target = TapeVerifyTarget { cart_sets };
    let chunk_sweep = shared_verify_core::sweep_storage(&target, backend_name, backend).await;
    storage_pool.chunk_objects = chunk_sweep.chunk_objects;
    storage_pool.chunk_orphans = chunk_sweep.chunk_orphans;
    if let Some(e) = &chunk_sweep.list_error
        && let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name)
    {
        pr.errors
            .push(format!("storage chunks/ list failed: {}", e));
    }
    // Per-cartridge missing-chunk counts; HEAD errors become warnings.
    let mut chunk_missing: HashMap<String, u64> = HashMap::new();
    for ent in &chunk_sweep.per_entity {
        chunk_missing.insert(ent.label.clone(), ent.chunks_missing);
        if !ent.head_errors.is_empty()
            && let Some(cr) = report.cartridges.iter_mut().find(|cr| cr.dir == ent.label)
        {
            for hf in &ent.head_errors {
                cr.warnings.push(format!(
                    "storage HEAD failed for chunk {}: {}",
                    short_hash(&hf.hash),
                    hf.message
                ));
            }
        }
    }

    // Tape-only dimension: index-page + sentinel HEADs, per cartridge.
    // Collected first (cloned) so we don't borrow `report` mutably
    // across the await points.
    let cartridges_to_check: Vec<_> = cart_sets
        .iter()
        .filter(|(_, c)| c.backend == backend_name)
        .map(|(dir, c)| (dir.clone(), c.clone()))
        .collect();

    let mut per_cart_results: Vec<(String, u64, u64, bool)> = Vec::new();
    let mut all_expected_page_keys: HashSet<String> = HashSet::new();

    for (dir, c) in &cartridges_to_check {
        let missing_chunks = chunk_missing.get(dir).copied().unwrap_or(0);

        // Index-page HEADs.
        let mut page_jobs: Vec<String> = Vec::new();
        for (label, pages) in &c.index_pages {
            for page in 0..*pages {
                page_jobs.push(format!(
                    "manifests/{}/{}/page-{:06}.dat",
                    c.barcode_label, label, page
                ));
            }
        }
        for key in &page_jobs {
            all_expected_page_keys.insert(key.clone());
        }
        let page_results: Vec<_> = futures::stream::iter(
            page_jobs
                .into_iter()
                .map(|key| async move { backend.chunk_exists(&key).await }),
        )
        .buffer_unordered(shared_verify_core::STORAGE_VERIFY_CONCURRENCY)
        .collect()
        .await;
        let missing_pages: u64 = page_results
            .into_iter()
            .filter(|r| !matches!(r, Ok(true)))
            .count() as u64;

        let sentinel_key = format!("manifests/{}/manifest-latest.json", c.barcode_label);
        let sentinel_present = backend.chunk_exists(&sentinel_key).await.unwrap_or(false);

        per_cart_results.push((dir.clone(), missing_chunks, missing_pages, sentinel_present));
    }

    // Apply per-cartridge results.
    for (dir, missing_chunks, missing_pages, sentinel_present) in per_cart_results {
        if let Some(cr) = report.cartridges.iter_mut().find(|cr| cr.dir == dir) {
            cr.storage_chunks_missing = Some(missing_chunks);
            cr.storage_index_pages_missing = Some(missing_pages);
            cr.storage_sentinel_present = Some(sentinel_present);
            if missing_chunks > 0 {
                cr.errors.push(format!(
                    "{} chunk(s) missing from storage (cold-bucket DR will fail)",
                    missing_chunks
                ));
            }
            if missing_pages > 0 {
                cr.errors.push(format!(
                    "{} index-page object(s) missing from storage (cold-bucket restore can't rebuild indexes)",
                    missing_pages
                ));
            }
            if !sentinel_present {
                // Sentinel-missing can mean "never had a manifest
                // backup" (warning) or "sentinel was deleted while
                // the body remains" (worse). Verify can't tell which
                // without listing — treat it as a warning unless the
                // cartridge has expected pages, in which case it's an
                // error (we know the cartridge has been backed up).
                if !cr.partitions.is_empty()
                    && cr.chunks_with_hash > 0
                    && cr.storage_index_pages_missing.unwrap_or(0) == 0
                {
                    // The cartridge's index pages are all present in the
                    // bucket, so it demonstrably WAS backed up — a
                    // missing sentinel here means the one key
                    // `library restore` discovers cartridges by (bucket
                    // lifecycle rule / manual cleanup) is gone. Cold-bucket
                    // DR would silently skip this cartridge, so this is an
                    // error, not a warning (issue #234).
                    cr.errors.push(
                        "storage sentinel manifest-latest.json missing — cartridge is backed up (index pages present) but the DR discovery sentinel was deleted; `library restore` will not find it".into(),
                    );
                } else {
                    cr.warnings
                        .push("storage sentinel manifest-latest.json missing".into());
                }
            }
        }
    }

    // Index-page orphan sweep: list each cartridge's
    // `manifests/<barcode>/` and count stale `page-NNNNNN.dat` keys.
    // The `chunks/` orphan scan already ran inside `sweep_storage`.
    for (_, c) in &cartridges_to_check {
        let prefix = format!("manifests/{}/", c.barcode_label);
        let keys = match backend.list_objects(&prefix).await {
            Ok(k) => k,
            Err(e) => {
                if let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name) {
                    pr.errors
                        .push(format!("storage {} list failed: {}", prefix, e));
                }
                continue;
            }
        };
        for key in keys {
            // Only count `page-NNNNNN.dat` keys — the JSON manifest
            // backups (`manifest-latest.json`, `manifest-<TS>.json`)
            // are runtime state and not GC's to reclaim.
            if !key.contains("/page-") || !key.ends_with(".dat") {
                continue;
            }
            if !all_expected_page_keys.contains(&key) {
                storage_pool.index_page_orphans += 1;
            }
        }
    }

    // Surface GC hints + attach to PoolReport.
    if let Some(pr) = report.pool.iter_mut().find(|p| p.backend == backend_name) {
        if storage_pool.chunk_orphans > 0 {
            pr.gc_hints.push(format!(
                "storage has {} orphan chunk object(s) — `system gc --storage` would free them",
                storage_pool.chunk_orphans
            ));
        }
        if storage_pool.index_page_orphans > 0 {
            pr.gc_hints.push(format!(
                "storage has {} stale index-page object(s) — `system gc --storage` would free them",
                storage_pool.index_page_orphans
            ));
        }
        pr.storage = Some(storage_pool);
    }
}

fn location_str(l: LocationTag) -> &'static str {
    match l {
        LocationTag::LocalOnly => "LocalOnly",
        LocationTag::StorageOnly => "StorageOnly",
        LocationTag::Both => "Both",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_index::BlockRec;
    use crate::chunk_index::{ChunkRec, LocationTag};
    use crate::tape::BlockKind;

    /// Helper: build a cartridge dir with a manifest and a single
    /// sealed chunk whose pool file matches the recorded size.
    fn make_cart(
        data_dir: &Path,
        barcode: &str,
        backend: &str,
        dedup: &str,
        chunk_hash: &str,
        chunk_bytes: &[u8],
    ) {
        let dir = data_dir.join("tapes").join(barcode);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"label":"{barcode}","backend":"{backend}","dedup":"{dedup}","uuid":"{}"}}"#,
                "00".repeat(16)
            ),
        )
        .unwrap();
        let cif = ChunkIndexFile::open_or_create(&dir).unwrap();
        cif.append(&ChunkRec {
            size: chunk_bytes.len() as u64,
            hash: Some(chunk_hash.to_string()),
            location: LocationTag::LocalOnly,
            uploaded: false,
            compression: None,
        })
        .unwrap();
        // Block index referencing chunk_id=0, offset=0, len=chunk_bytes.len().
        let bif = BlockIndexFile::open_or_create(&dir, 0).unwrap();
        let mut rec = BlockRec::data();
        rec.chunk_id = 0;
        rec.offset = 0;
        rec.len = chunk_bytes.len() as u32;
        rec.kind = BlockKind::Data;
        bif.append(&rec).unwrap();

        // Pool file.
        let store = if dedup == "local" {
            ChunkStore::new_namespaced(data_dir, backend, barcode).unwrap()
        } else {
            ChunkStore::new(data_dir, backend).unwrap()
        };
        let path = store.store_path(chunk_hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, chunk_bytes).unwrap();
    }

    #[test]
    fn clean_library_reports_no_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        // Minimum library bootstrap.
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE001"}],"mail_slots":[],"drives":[{"drive_id":0,"barcode":null}]}"#,
        )
        .unwrap();
        let h = "a".repeat(64);
        make_cart(dd, "TAPE001", "primary", "global", &h, b"hello");

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        assert_eq!(report.error_count(), 0, "{:#?}", report);
        assert_eq!(report.cartridges.len(), 1);
        let c = &report.cartridges[0];
        assert!(c.manifest_ok);
        assert_eq!(c.chunks_idx_records, 1);
        assert_eq!(c.local_chunks_missing, 0);
        assert_eq!(c.local_chunks_size_mismatch, 0);
    }

    /// Issue #165: an existing-but-corrupt blocks-p<N>.idx (clobbered
    /// header) must be reported as an error, not silently passed. Before
    /// the fix verify returned a clean records=0 partition report and
    /// exited 0 while every host READ of that partition would fail.
    #[test]
    fn corrupt_block_index_is_reported_as_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE001"}],"mail_slots":[],"drives":[{"drive_id":0,"barcode":null}]}"#,
        )
        .unwrap();
        let h = "a".repeat(64);
        make_cart(dd, "TAPE001", "primary", "global", &h, b"hello");

        // Clobber the block-index header magic/version so an EXISTING
        // file fails to open.
        let bpath = BlockIndexFile::path_for(&dd.join("tapes").join("TAPE001"), 0);
        {
            use std::io::Write as _;
            let mut f = fs::OpenOptions::new().write(true).open(&bpath).unwrap();
            f.write_all(&[0xFFu8; 8]).unwrap();
        }

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        assert!(
            report.error_count() > 0,
            "corrupt block index must be an error: {report:#?}"
        );
        let c = &report.cartridges[0];
        assert!(
            c.errors
                .iter()
                .any(|e| e.contains("blocks-p0.idx") && e.contains("open failed")),
            "errors: {:?}",
            c.errors
        );
    }

    #[test]
    fn encrypted_cartridge_tag_overhead_is_not_a_size_mismatch() {
        // An encrypted cartridge seals each chunk as one AES-256-GCM
        // block, so its pool object is TAG_LEN (16) bytes longer than
        // the plaintext size recorded in chunks.idx. verify must add
        // that overhead before comparing pool size — otherwise every
        // encrypted chunk is wrongly flagged. (Regression for the bug
        // the monte-carlo encryption coverage surfaced.)
        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE001"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();

        let barcode = "TAPE001";
        let backend = "primary";
        let h = "c".repeat(64);
        let plaintext = b"hello, encrypted tape";

        let dir = dd.join("tapes").join(barcode);
        fs::create_dir_all(&dir).unwrap();
        // Manifest WITH an encryption stanza (what `cartridge create
        // --encrypt` writes).
        fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"label":"{barcode}","backend":"{backend}","dedup":"global","uuid":"{}","encryption":{{"algorithm":"aes_256_gcm","keystore_backend":"local"}}}}"#,
                "00".repeat(16)
            ),
        )
        .unwrap();
        // chunks.idx records the PLAINTEXT size.
        let cif = ChunkIndexFile::open_or_create(&dir).unwrap();
        cif.append(&ChunkRec {
            size: plaintext.len() as u64,
            hash: Some(h.clone()),
            location: LocationTag::LocalOnly,
            uploaded: false,
            compression: None,
        })
        .unwrap();
        let bif = BlockIndexFile::open_or_create(&dir, 0).unwrap();
        let mut rec = BlockRec::data();
        rec.chunk_id = 0;
        rec.offset = 0;
        rec.len = plaintext.len() as u32;
        rec.kind = BlockKind::Data;
        bif.append(&rec).unwrap();
        // Pool object is ciphertext+tag: plaintext + TAG_LEN bytes.
        let store = ChunkStore::new(dd, backend).unwrap();
        let path = store.store_path(&h);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut on_disk = plaintext.to_vec();
        on_disk.extend_from_slice(&[0u8; TAG_LEN]);
        fs::write(&path, &on_disk).unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        let c = &report.cartridges[0];
        assert_eq!(c.local_chunks_size_mismatch, 0, "{:#?}", report);
        assert_eq!(report.error_count(), 0, "{:#?}", report);

        // Negative control: an encrypted cart whose pool object is
        // missing the tag (plaintext-sized) must still be flagged.
        fs::write(&path, plaintext).unwrap();
        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        let c = &report.cartridges[0];
        assert_eq!(c.local_chunks_size_mismatch, 1, "{:#?}", report);
    }

    #[test]
    fn missing_chunk_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE001"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();
        let h = "b".repeat(64);
        make_cart(dd, "TAPE001", "primary", "global", &h, b"hello");
        // Delete the pool file behind verify's back.
        let store = ChunkStore::new(dd, "primary").unwrap();
        fs::remove_file(store.store_path(&h)).unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        let c = &report.cartridges[0];
        assert_eq!(c.local_chunks_missing, 1);
        assert!(report.error_count() >= 1);
    }

    #[test]
    fn orphan_pool_chunk_is_a_gc_hint_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE001"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();
        let h = "c".repeat(64);
        make_cart(dd, "TAPE001", "primary", "global", &h, b"hello");
        // Drop a stray pool file that no manifest points at.
        let store = ChunkStore::new(dd, "primary").unwrap();
        let stray = "f".repeat(64);
        let path = store.store_path(&stray);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"orphan").unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        // Orphan does NOT count as an error.
        assert_eq!(report.error_count(), 0, "{:#?}", report);
        let pool = report.pool.iter().find(|p| p.backend == "primary").unwrap();
        assert_eq!(pool.shared_orphans, 1);
        assert!(!pool.gc_hints.is_empty());
    }

    #[test]
    fn block_chunk_id_oob_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE001"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();
        let h = "d".repeat(64);
        make_cart(dd, "TAPE001", "primary", "global", &h, b"hello");

        // Append an out-of-bounds block record (chunk_id=99 with only
        // one chunk in chunks.idx).
        let bif = BlockIndexFile::open_or_create(&dd.join("tapes").join("TAPE001"), 0).unwrap();
        let mut bad = BlockRec::data();
        bad.chunk_id = 99;
        bad.offset = 0;
        bad.len = 4;
        bad.kind = BlockKind::Data;
        bif.append(&bad).unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        let c = &report.cartridges[0];
        let p = &c.partitions[0];
        assert_eq!(p.chunk_id_oob, 1);
    }

    #[test]
    fn inventory_referencing_missing_cartridge_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE_GHOST"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();

        let report = verify_local(dd, &VerifyScope::default()).unwrap();
        assert!(
            report
                .library
                .missing_cartridges
                .contains(&"TAPE_GHOST".to_string())
        );
        assert!(report.error_count() >= 1);
    }

    /// Build a Both-located cartridge plus the storage objects it
    /// references (chunk, one index page, sentinel) so the storage
    /// sweep finds a clean library.
    fn make_storage_cart(
        data_dir: &Path,
        backend_dir: &Path,
        barcode: &str,
        chunk_hash: &str,
        chunk_bytes: &[u8],
    ) {
        // Local cartridge.
        let dir = data_dir.join("tapes").join(barcode);
        fs::create_dir_all(&dir).unwrap();
        // Manifest carries an index_epoch entry for `chunks` with one page
        // — exercises the index-page presence check.
        fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"label":"{barcode}","backend":"primary","dedup":"global","uuid":"{}",
                    "index_epoch":{{"chunks":{{"pages":1,"page_size":1048576,"epoch":1,"file_size":4096}}}}}}"#,
                "00".repeat(16)
            ),
        )
        .unwrap();
        let cif = ChunkIndexFile::open_or_create(&dir).unwrap();
        cif.append(&ChunkRec {
            size: chunk_bytes.len() as u64,
            hash: Some(chunk_hash.to_string()),
            location: LocationTag::Both,
            uploaded: true,
            compression: None,
        })
        .unwrap();
        // Local pool (Both → must exist locally too).
        let store = ChunkStore::new(data_dir, "primary").unwrap();
        let path = store.store_path(chunk_hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, chunk_bytes).unwrap();

        // Pre-populate the LocalBackend's filesystem so chunk_exists
        // returns true. LocalBackend uses `<root>/<key>` layout — the
        // raw file structure mirrors what an S3 bucket would hold.
        let chunk_key = format!(
            "chunks/{}/{}/{}.dat",
            &chunk_hash[..2],
            &chunk_hash[2..4],
            chunk_hash
        );
        let chunk_obj = backend_dir.join(&chunk_key);
        fs::create_dir_all(chunk_obj.parent().unwrap()).unwrap();
        fs::write(&chunk_obj, chunk_bytes).unwrap();
        let page_key = format!("manifests/{}/chunks/page-{:06}.dat", barcode, 0);
        let page_obj = backend_dir.join(&page_key);
        fs::create_dir_all(page_obj.parent().unwrap()).unwrap();
        fs::write(&page_obj, b"page-bytes").unwrap();
        let sentinel_key = format!("manifests/{}/manifest-latest.json", barcode);
        let sentinel_obj = backend_dir.join(&sentinel_key);
        fs::create_dir_all(sentinel_obj.parent().unwrap()).unwrap();
        fs::write(&sentinel_obj, b"{}").unwrap();
    }

    #[tokio::test]
    async fn storage_sweep_clean_library() {
        use crate::local::LocalBackend;

        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE001"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();

        let backend_dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        let h = "e".repeat(64);
        make_storage_cart(dd, backend_dir.path(), "TAPE001", &h, b"hello");

        let (mut report, cart_sets) = verify_local_inner(dd, &VerifyScope::default()).unwrap();
        sweep_one_backend_storage(&mut report, &cart_sets, "primary", &backend).await;

        let c = &report.cartridges[0];
        assert_eq!(c.storage_chunks_missing, Some(0), "{:#?}", c);
        assert_eq!(c.storage_index_pages_missing, Some(0));
        assert_eq!(c.storage_sentinel_present, Some(true));
        let p = report.pool.iter().find(|p| p.backend == "primary").unwrap();
        let cp = p.storage.as_ref().expect("storage sweep ran");
        assert_eq!(cp.chunk_orphans, 0);
        assert_eq!(cp.index_page_orphans, 0);
        // No storage-side errors should bubble up to the cartridge.
        assert!(c.errors.is_empty(), "{:#?}", c.errors);
    }

    #[tokio::test]
    async fn storage_sweep_flags_missing_chunk_and_index_page() {
        use crate::local::LocalBackend;

        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE002"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();

        let backend_dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        let h = "f".repeat(64);
        make_storage_cart(dd, backend_dir.path(), "TAPE002", &h, b"data");

        // Now delete the chunk + index page from "storage" — sentinel
        // stays so we exercise the missing-chunk / missing-page paths
        // in isolation.
        fs::remove_file(backend_dir.path().join(format!(
            "chunks/{}/{}/{}.dat",
            &h[..2],
            &h[2..4],
            h
        )))
        .unwrap();
        fs::remove_file(
            backend_dir
                .path()
                .join("manifests/TAPE002/chunks/page-000000.dat"),
        )
        .unwrap();

        let (mut report, cart_sets) = verify_local_inner(dd, &VerifyScope::default()).unwrap();
        sweep_one_backend_storage(&mut report, &cart_sets, "primary", &backend).await;

        let c = &report.cartridges[0];
        assert_eq!(c.storage_chunks_missing, Some(1));
        assert_eq!(c.storage_index_pages_missing, Some(1));
        assert_eq!(c.storage_sentinel_present, Some(true));
        // Both must have produced error entries on the cartridge.
        assert!(
            c.errors
                .iter()
                .any(|e| e.contains("chunk(s) missing from storage")),
            "{:#?}",
            c.errors
        );
        assert!(
            c.errors
                .iter()
                .any(|e| e.contains("index-page object(s) missing from storage")),
            "{:#?}",
            c.errors
        );
    }

    /// Issue #234: a deleted DR sentinel on a cartridge that is
    /// demonstrably backed up (all index pages + chunks present in
    /// storage) is an ERROR, not a warning — `library restore` discovers
    /// cartridges only by that sentinel, so otherwise `system verify`
    /// reports success while cold-bucket DR would silently skip it.
    #[tokio::test]
    async fn storage_sweep_deleted_sentinel_on_backed_up_cart_is_error() {
        use crate::local::LocalBackend;

        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE003"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();

        let backend_dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        let h = "a".repeat(64);
        make_storage_cart(dd, backend_dir.path(), "TAPE003", &h, b"data");

        // Delete ONLY the sentinel — chunk + index page stay, so the
        // cartridge is demonstrably backed up.
        fs::remove_file(
            backend_dir
                .path()
                .join("manifests/TAPE003/manifest-latest.json"),
        )
        .unwrap();

        let (mut report, cart_sets) = verify_local_inner(dd, &VerifyScope::default()).unwrap();
        sweep_one_backend_storage(&mut report, &cart_sets, "primary", &backend).await;

        let c = &report.cartridges[0];
        assert_eq!(c.storage_sentinel_present, Some(false));
        assert_eq!(c.storage_index_pages_missing, Some(0));
        assert!(
            c.errors
                .iter()
                .any(|e| e.contains("sentinel manifest-latest.json missing")),
            "deleted sentinel on a backed-up cartridge must be an error: {:#?}",
            c.errors
        );
        assert!(
            !c.warnings
                .iter()
                .any(|w| w.contains("sentinel manifest-latest.json missing")),
            "must not be downgraded to a warning: {:#?}",
            c.warnings
        );
    }

    #[tokio::test]
    async fn storage_sweep_orphan_chunk_is_gc_hint_not_error() {
        use crate::local::LocalBackend;

        let tmp = tempfile::tempdir().unwrap();
        let dd = tmp.path();
        fs::create_dir_all(dd.join("library")).unwrap();
        fs::write(
            dd.join("library").join("library.json"),
            r#"{"num_storage_slots":1,"num_mail_slots":0,"num_drives":1}"#,
        )
        .unwrap();
        fs::write(
            dd.join("library").join("inventory.json"),
            r#"{"storage_slots":[{"slot_id":1,"barcode":"TAPE003"}],"mail_slots":[],"drives":[]}"#,
        )
        .unwrap();

        let backend_dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        let h = "9".repeat(64);
        make_storage_cart(dd, backend_dir.path(), "TAPE003", &h, b"hi");

        // Drop a stray storage object no manifest points at.
        let stray = "1".repeat(64);
        let stray_key = format!("chunks/{}/{}/{}.dat", &stray[..2], &stray[2..4], stray);
        let stray_obj = backend_dir.path().join(&stray_key);
        fs::create_dir_all(stray_obj.parent().unwrap()).unwrap();
        fs::write(&stray_obj, b"orphan").unwrap();

        let (mut report, cart_sets) = verify_local_inner(dd, &VerifyScope::default()).unwrap();
        sweep_one_backend_storage(&mut report, &cart_sets, "primary", &backend).await;

        let c = &report.cartridges[0];
        // The cartridge itself stays clean.
        assert_eq!(c.storage_chunks_missing, Some(0));
        assert!(c.errors.is_empty(), "{:#?}", c.errors);
        let p = report.pool.iter().find(|p| p.backend == "primary").unwrap();
        let cp = p.storage.as_ref().unwrap();
        assert_eq!(cp.chunk_orphans, 1);
        assert!(p.gc_hints.iter().any(|h| h.contains("orphan chunk object")));
    }
}
