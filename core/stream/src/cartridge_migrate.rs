// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cartridge migration — move a cartridge from one storage backend to
//! another. Same barcode, same logical identity; only the bound
//! backend changes.
//!
//! Two modes:
//!
//! - **Move** — copy every storage-referenced chunk from source to
//!   target (BLAKE3-verified inline), copy the manifest + index page
//!   backups, move the local pool files under the new backend prefix,
//!   atomically flip `manifest.backend`, then delete source objects.
//!   Source-side delete is best-effort: failures become warnings, the
//!   future GC sweep cleans up orphans. Source-side chunk deletes are
//!   skipped entirely under `Global` dedup — chunks under that scope
//!   may be referenced by other cartridges on the source backend.
//!
//! - **Rebind** — pointer-rewrite only, for operators who already run
//!   bucket-level cross-region / cross-provider replication
//!   out-of-band. Optionally HEADs every chunk + the manifest sentinel
//!   on the target first (default); operator can pass `verify: false`
//!   to skip the check and vouch for the target's contents. No data
//!   movement, no source-side mutation.
//!
//! Manifest mutation is the commit point. A crash before the manifest
//! flip leaves orphan chunks on the target backend (and source-side
//! state intact); a crash after the flip but before source delete
//! leaves orphans on the source. Both states are recoverable via
//! `system gc` on the affected backend.
//!
//! Library inventory is untouched — the cartridge stays in whatever
//! storage slot it occupied. The daemon-side gate refuses migration
//! while the cartridge is loaded in a drive (the in-memory drive state
//! still references the pre-migration backend handle).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use shared_object_store::{LockState, ObjectStoreBackend, ObjectStoreError};
use shared_pool::{ChunkPool, PoolBudget};

use crate::chunk_index::ChunkIndexFile;
use crate::errors::{Result, SmcError};
use crate::legal_hold::manifest_latest_sentinel_key;

/// What migration variant the operator asked for.
#[derive(Debug, Clone, Copy)]
pub enum MigrateMode {
    /// Copy data from source to target, flip the manifest, delete source.
    Move,
    /// Pointer rewrite. `verify=true` HEADs every chunk + the manifest
    /// sentinel on the target first; `verify=false` skips the check.
    Rebind { verify: bool },
}

impl MigrateMode {
    fn label(self) -> &'static str {
        match self {
            MigrateMode::Move => "move",
            MigrateMode::Rebind { verify: true } => "rebind",
            MigrateMode::Rebind { verify: false } => "rebind-noverify",
        }
    }
}

/// All inputs to [`run_migrate`]. Borrows are scoped to one call;
/// nothing escapes.
pub struct MigrateOptions<'a> {
    /// `<data_dir>/tapes/` — the cartridge dir lives at
    /// `tapes_dir.join(barcode)`. Pool root is derived as
    /// `tapes_dir.parent()` (matches [`crate::cartridge`]'s
    /// `derive_chunk_store`).
    pub tapes_dir: &'a Path,
    pub barcode: &'a str,
    /// Backend handles. Source and target must be distinct named
    /// backends (the operator-stated names below are validated against
    /// `manifest.backend`).
    pub source: &'a dyn ObjectStoreBackend,
    pub source_name: &'a str,
    pub target: &'a dyn ObjectStoreBackend,
    pub target_name: &'a str,
    pub mode: MigrateMode,
    /// `true` short-circuits before any mutation. The report carries
    /// the chunk inventory + sizes so callers can render the plan.
    pub dry_run: bool,
    /// Optional progress hook. Called synchronously between major
    /// phases with a short label ("copying chunks …", "deleting
    /// source …"). Daemon job workers wire this into `JobEmitter`
    /// via a sync→async forwarder; CLI-direct callers pass `None`.
    pub progress: Option<&'a (dyn Fn(&str) + Send + Sync)>,
    /// Per-backend pool budgets for the source and target. Moving a
    /// chunk file between backend pool dirs shrinks the source's
    /// on-disk pool and grows the target's, so each move releases the
    /// source budget and reserves the target's — keeping both
    /// `current_bytes()` exact for the per-backend eviction workers.
    /// `None` for CLI-direct / test callers with no live budgets.
    pub source_budget: Option<Arc<PoolBudget>>,
    pub target_budget: Option<Arc<PoolBudget>>,
}

/// Outcome of one migrate invocation. Returned regardless of mode;
/// fields irrelevant to the chosen mode stay zero.
#[derive(Debug, Default, Serialize)]
pub struct MigrateReport {
    pub barcode: String,
    /// Mode label — `"move"` | `"rebind"` | `"rebind-noverify"`.
    pub mode: String,
    pub from_backend: String,
    pub to_backend: String,
    /// Sealed chunks in `chunks.idx` (every entry with a hash).
    pub chunks_total: u64,
    /// Chunks the move mode actually PUT to the target (after skipping
    /// idempotent target-already-has-it cases).
    pub chunks_copied: u64,
    /// Chunks the rebind+verify mode HEADed and found on the target.
    pub chunks_verified: u64,
    /// Bytes pulled across the wire in move mode.
    pub bytes_copied: u64,
    /// Manifest-prefix objects copied in move mode (manifest-latest +
    /// versioned manifests + index pages).
    pub manifest_objects_copied: u64,
    /// Source objects successfully deleted in move mode. Always 0 in
    /// rebind mode.
    pub source_objects_deleted: u64,
    /// Local pool files renamed under the new backend's prefix.
    pub local_files_moved: u64,
    /// Non-fatal source-delete failures. Move mode only.
    pub source_delete_warnings: Vec<String>,
    pub dry_run: bool,
}

/// Lightweight slice of the cartridge manifest — just the fields
/// migrate needs to know. Deserialized permissively so future schema
/// additions don't break us.
#[derive(Debug, Deserialize)]
struct ManifestSlice {
    label: String,
    #[serde(default)]
    backend: String,
    #[serde(default)]
    dedup: DedupSlice,
    #[serde(default)]
    worm: bool,
}

/// Mirror of the public `DedupScope` enum, narrow on the JSON shape
/// we need. Kept private to this module so the migration path doesn't
/// depend on the full `cartridge::DedupScope` import surface.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DedupSlice {
    #[default]
    Global,
    Local,
}

impl DedupSlice {
    /// Storage-key namespace for chunks in this dedup scope.
    /// `Local` → the cartridge barcode; `Global` → none (shared pool).
    fn storage_namespace(self, barcode: &str) -> Option<&str> {
        match self {
            DedupSlice::Local => Some(barcode),
            DedupSlice::Global => None,
        }
    }
}

/// Decide whether a legal-hold read permits migration:
///   - `Ok(true)`  — held → refuse outright (no per-policy override).
///   - `Ok(false)` — not held → proceed.
///   - `Err(NotSupported)` — the source backend can't carry a hold
///     (e.g. `local`), so the cartridge is not held → proceed.
///   - any other `Err` — fail-safe: refuse to migrate when the hold
///     state cannot be confirmed (a transient storage error must not be
///     read as "not held").
fn hold_check_permits(result: std::result::Result<bool, ObjectStoreError>) -> Result<()> {
    match result {
        Ok(true) => Err(SmcError::InvalidOp(
            "cartridge is under legal hold; refusing migration",
        )),
        Ok(false) | Err(ObjectStoreError::NotSupported(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Decide whether a WORM cartridge may commit to a target with the
/// given live lock state. A WORM cartridge requires the target bucket
/// to actually enforce immutability; a non-WORM cartridge is
/// unconstrained.
fn worm_lock_permits(is_worm: bool, state: LockState) -> Result<()> {
    if is_worm && !state.is_locked() {
        return Err(SmcError::InvalidOp(
            "WORM cartridge target backend is not lock-enabled \
             (live lock_state=off); refusing migration",
        ));
    }
    Ok(())
}

/// Re-probe the target backend's *live* lock state immediately before
/// the commit point and refuse a WORM cartridge whose target does not
/// actually enforce immutability. The daemon's pre-flight gate checks
/// the YAML-declared `retention_mode`, which a scheduler — or an
/// operator who edited the conffile after boot — can drift out of sync
/// with the bucket; this is the authoritative check against the bucket
/// itself. No-op (no storage call) for non-WORM cartridges.
async fn verify_worm_target_lock(target: &dyn ObjectStoreBackend, is_worm: bool) -> Result<()> {
    if !is_worm {
        return Ok(());
    }
    let state = target.lock_state().await.map_err(storage_err)?;
    worm_lock_permits(is_worm, state)
}

/// Run a migration. Returns a populated [`MigrateReport`] on success.
///
/// Failures during the per-chunk copy stage abort the migration before
/// the manifest flip — the cartridge stays bound to the source
/// backend, no source-side mutation has happened, and the partial
/// state on the target is recoverable via `system gc` on the target.
/// Once the manifest flip commits, source-side delete failures
/// degrade to warnings: the migration succeeds and the orphans clean
/// up on the next GC sweep.
/// Bounded backend concurrency for the per-chunk copy / verify / delete
/// storms. A 12 TB LTO-8 cartridge is ~1.5M chunks at the 8 MiB FastCDC
/// average; one round trip at a time would run for days (issue #158).
/// Matches the legal-hold per-key storm.
const MIGRATE_CONCURRENCY: usize = 16;

pub async fn run_migrate(opts: MigrateOptions<'_>) -> Result<MigrateReport> {
    use futures::stream::{self, StreamExt};
    let cart_root = opts.tapes_dir.join(opts.barcode);
    let manifest_path = cart_root.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(SmcError::InvalidOp(
            "cartridge directory or manifest.json missing",
        ));
    }
    if opts.source_name == opts.target_name {
        return Err(SmcError::InvalidOp("source and target backend must differ"));
    }
    if opts.source_name.is_empty() || opts.target_name.is_empty() {
        return Err(SmcError::InvalidOp("backend names must be non-empty"));
    }

    // Parse just the fields we need from the manifest.
    let manifest_json = fs::read_to_string(&manifest_path)?;
    let slice: ManifestSlice = serde_json::from_str(&manifest_json)?;
    if slice.backend != opts.source_name {
        return Err(SmcError::InvalidOp(
            "manifest backend disagrees with operator-stated source",
        ));
    }
    if slice.label != opts.barcode {
        return Err(SmcError::InvalidOp(
            "manifest label disagrees with operator-stated barcode",
        ));
    }

    // Legal-hold gate. A held cartridge must never be relocated: the
    // hold is cloud-native (provider object-lock) with no cross-backend
    // transfer path, so moving its chunks would silently drop it. Read
    // the source-side sentinel and refuse if held. Fires before the
    // dry-run short-circuit so a preview can't claim a held cartridge
    // is movable. (The tiering planner pre-filters held cartridges; this
    // is the hard backstop shared by manual `cartridge migrate` and the
    // tiering run-now path.)
    let hold_key = manifest_latest_sentinel_key(opts.barcode);
    hold_check_permits(opts.source.get_object_legal_hold(&hold_key).await)?;

    let namespace = slice.dedup.storage_namespace(opts.barcode);

    // Walk chunks.idx, collecting hashes + sizes. We hold the index
    // file open only across this loop; the cartridge is not mutated.
    let chunk_idx = ChunkIndexFile::open_or_create(&cart_root)?;
    let mut chunk_hashes: Vec<String> = Vec::new();
    let mut chunk_keys: Vec<String> = Vec::new();
    let mut chunk_sizes: Vec<u64> = Vec::new();
    for entry in chunk_idx.iter() {
        let (_id, rec) = entry?;
        if let Some(hash) = rec.hash {
            chunk_keys.push(ChunkPool::object_key_for(namespace, &hash));
            chunk_hashes.push(hash);
            chunk_sizes.push(rec.size);
        }
    }
    drop(chunk_idx);

    let mut report = MigrateReport {
        barcode: opts.barcode.to_string(),
        mode: opts.mode.label().to_string(),
        from_backend: opts.source_name.to_string(),
        to_backend: opts.target_name.to_string(),
        chunks_total: chunk_hashes.len() as u64,
        dry_run: opts.dry_run,
        ..Default::default()
    };

    let log = |msg: &str| {
        if let Some(p) = opts.progress {
            p(msg);
        }
    };

    if opts.dry_run {
        log(&format!(
            "dry-run: would migrate {} ({} chunks) {} -> {}",
            opts.barcode,
            chunk_hashes.len(),
            opts.source_name,
            opts.target_name
        ));
        return Ok(report);
    }

    match opts.mode {
        MigrateMode::Move => {
            // Phase 1: copy chunks. Idempotent (HEAD on target first).
            log(&format!(
                "copying {} chunks {} -> {}",
                chunk_hashes.len(),
                opts.source_name,
                opts.target_name
            ));
            // Bounded-concurrency copy: each chunk independently HEADs
            // the target, then GET+verify+PUT only on a miss. Results are
            // folded into `report` after the storm drains; the first
            // error (hash mismatch / backend fault) wins.
            let source = opts.source;
            let target = opts.target;
            // Owned (hash, key) pairs per task — moving them in sidesteps
            // the higher-ranked-lifetime trap of `map`-ing borrowed refs
            // into async blocks. The clone is negligible next to a
            // backend round trip.
            let copy_pairs: Vec<(String, String)> = chunk_hashes
                .iter()
                .cloned()
                .zip(chunk_keys.iter().cloned())
                .collect();
            let copy_outcomes: Vec<Result<(bool, u64)>> = stream::iter(copy_pairs)
                .map(|(hash, key)| async move {
                    if target.chunk_exists(&key).await.map_err(storage_err)? {
                        return Ok((false, 0u64));
                    }
                    let bytes = source.download_chunk(&key).await.map_err(storage_err)?;
                    let actual = blake3_hex(&bytes);
                    if actual != hash {
                        return Err(SmcError::ContentHashMismatch {
                            expected: hash,
                            actual,
                        });
                    }
                    let size = bytes.len() as u64;
                    target.upload_chunk(&key, bytes).await.map_err(storage_err)?;
                    Ok((true, size))
                })
                .buffer_unordered(MIGRATE_CONCURRENCY)
                .collect()
                .await;
            for outcome in copy_outcomes {
                let (copied, size) = outcome?;
                if copied {
                    report.chunks_copied += 1;
                    report.bytes_copied += size;
                }
            }
            log(&format!(
                "copied {} new chunks ({} already on target)",
                report.chunks_copied,
                chunk_hashes.len() as u64 - report.chunks_copied
            ));

            // Phase 2: copy manifest backups. Source carries
            //   manifests/<barcode>/manifest-latest.json
            //   manifests/<barcode>/manifest-<ts>.json (versioned)
            //   manifests/<barcode>/chunks/page-NNNNNN.dat
            //   manifests/<barcode>/blocks-pN/page-NNNNNN.dat
            // Copy them all under the same keys.
            log("copying manifest backups");
            let manifest_prefix = format!("manifests/{}/", opts.barcode);
            let manifest_keys = opts
                .source
                .list_objects(&manifest_prefix)
                .await
                .map_err(storage_err)?;
            for key in &manifest_keys {
                copy_one_object(opts.source, opts.target, key).await?;
                report.manifest_objects_copied += 1;
            }

            // Phase 3: move local pool files under the new backend prefix.
            log("moving local pool files");
            report.local_files_moved = move_local_pool_files(
                opts.tapes_dir,
                opts.source_name,
                opts.target_name,
                namespace,
                &chunk_hashes,
                &chunk_sizes,
                opts.source_budget.as_deref(),
                opts.target_budget.as_deref(),
            )?;

            // Phase 4: commit point. Re-probe the target's live lock
            // state for WORM cartridges (authoritative over the
            // YAML-declared retention mode), then flip manifest.backend
            // atomically.
            verify_worm_target_lock(opts.target, slice.worm).await?;
            log("flipping manifest.backend");
            rewrite_manifest_backend(&cart_root, opts.target_name)?;

            // Phase 5: source-side delete. Best-effort; warnings only.
            // Under Global dedup chunks may be referenced by other
            // cartridges on the source — leave them; the future GC
            // sweep handles orphans correctly.
            let delete_chunks = matches!(slice.dedup, DedupSlice::Local);
            log(if delete_chunks {
                "deleting source objects (chunks + manifests)"
            } else {
                "deleting source manifest backups (chunks shared under Global dedup; leave for GC)"
            });
            let delete_keys: Vec<String> = if delete_chunks {
                chunk_keys
                    .iter()
                    .chain(manifest_keys.iter())
                    .cloned()
                    .collect()
            } else {
                manifest_keys.clone()
            };
            let source = opts.source;
            let delete_outcomes: Vec<std::result::Result<(), String>> = stream::iter(delete_keys)
                .map(|key| async move {
                    source
                        .delete_object(&key)
                        .await
                        .map_err(|e| format!("{}: {}", key, e))
                })
                .buffer_unordered(MIGRATE_CONCURRENCY)
                .collect()
                .await;
            for outcome in delete_outcomes {
                match outcome {
                    Ok(()) => report.source_objects_deleted += 1,
                    Err(w) => report.source_delete_warnings.push(w),
                }
            }
        }
        MigrateMode::Rebind { verify } => {
            if verify {
                log(&format!(
                    "verifying {} chunks + sentinel on target {}",
                    chunk_hashes.len(),
                    opts.target_name
                ));
                // Bounded-concurrency HEAD storm; collect existence per
                // key, then tally. (One HEAD at a time over ~1.5M chunks
                // is ~21 h of pure latency for a full LTO-8 — issue #158.)
                let target = opts.target;
                let verify_keys: Vec<String> = chunk_keys.clone();
                let verify_outcomes: Vec<Result<(String, bool)>> = stream::iter(verify_keys)
                    .map(|key| async move {
                        let exists = target.chunk_exists(&key).await.map_err(storage_err)?;
                        Ok((key, exists))
                    })
                    .buffer_unordered(MIGRATE_CONCURRENCY)
                    .collect()
                    .await;
                let mut missing: Vec<String> = Vec::new();
                for outcome in verify_outcomes {
                    let (key, exists) = outcome?;
                    if exists {
                        report.chunks_verified += 1;
                    } else if missing.len() < 16 {
                        // Cap the reported list; report.failures clamps too.
                        missing.push(key);
                    }
                }
                if missing.is_empty() {
                    let sentinel = format!("manifests/{}/manifest-latest.json", opts.barcode);
                    if !opts
                        .target
                        .chunk_exists(&sentinel)
                        .await
                        .map_err(storage_err)?
                    {
                        missing.push(sentinel);
                    }
                }
                if !missing.is_empty() {
                    return Err(SmcError::RebindTargetMissing { keys: missing });
                }
            } else {
                log("skipping verify pass (operator vouches for target)");
            }
            log("moving local pool files");
            report.local_files_moved = move_local_pool_files(
                opts.tapes_dir,
                opts.source_name,
                opts.target_name,
                namespace,
                &chunk_hashes,
                &chunk_sizes,
                opts.source_budget.as_deref(),
                opts.target_budget.as_deref(),
            )?;
            verify_worm_target_lock(opts.target, slice.worm).await?;
            log("flipping manifest.backend");
            rewrite_manifest_backend(&cart_root, opts.target_name)?;
        }
    }

    log("migration complete");
    Ok(report)
}

/// Translate `shared_object_store` errors into `SmcError` for `?` propagation.
fn storage_err(e: shared_object_store::ObjectStoreError) -> SmcError {
    SmcError::ObjectStoreError(e.to_string())
}

fn blake3_hex(bytes: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(bytes);
    hex::encode(h.finalize().as_bytes())
}

async fn copy_one_object(
    source: &dyn ObjectStoreBackend,
    target: &dyn ObjectStoreBackend,
    key: &str,
) -> Result<()> {
    // Two key shapes live under `manifests/<barcode>/`:
    //   - `manifest-latest.json` / `manifest-<ts>.json` — UTF-8 JSON,
    //     uploaded via `upload_manifest` (no compression layer).
    //   - `chunks/page-NNNNNN.dat` / `blocks-p<N>/page-NNNNNN.dat` —
    //     binary index pages, uploaded via `upload_chunk` so the
    //     backend's compression config applies.
    // Pick the right copy primitive based on the key suffix. The
    // backend's own compression handling round-trips correctly:
    // `download_chunk` decompresses on read, `upload_chunk` re-applies
    // the target's config on the way back.
    if key.ends_with(".json") {
        let body = source.download_manifest(key).await.map_err(storage_err)?;
        target
            .upload_manifest(key, &body)
            .await
            .map_err(storage_err)?;
    } else {
        // Index pages: versioned key (re-migrating the same cartridge
        // would copy potentially-different bytes under the same key).
        // `upload_versioned` bypasses the meta-cache.
        let bytes = source.download_chunk(key).await.map_err(storage_err)?;
        target
            .upload_versioned(key, &bytes)
            .await
            .map_err(storage_err)?;
    }
    Ok(())
}

/// Rewrite the `backend` field of manifest.json in place. Uses
/// `serde_json::Value` so we don't depend on the private `Manifest`
/// struct's exact shape — future schema additions stay round-tripped.
fn rewrite_manifest_backend(cart_root: &Path, new_backend: &str) -> Result<()> {
    let manifest_path = cart_root.join("manifest.json");
    let src = fs::read_to_string(&manifest_path)?;
    let mut v: serde_json::Value = serde_json::from_str(&src)?;
    let obj = v
        .as_object_mut()
        .ok_or(SmcError::InvalidOp("manifest.json root is not an object"))?;
    obj.insert(
        "backend".to_string(),
        serde_json::Value::String(new_backend.to_string()),
    );
    let body = serde_json::to_string(&v)?;
    let tmp = cart_root.join("manifest.json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &manifest_path)?;
    Ok(())
}

/// Move every local pool file referenced by the cartridge from the
/// source backend's prefix to the target's. Pool root is
/// `<tapes_dir.parent()>/chunks/<backend>/[<namespace>/]<aa>/<bb>/<hash>.dat`
/// — matches [`shared_pool::ChunkPool::pool_dir`].
///
/// Returns the count of files actually renamed (missing source files
/// are skipped silently — chunks marked `StorageOnly` in the index are
/// not expected to be local).
#[allow(clippy::too_many_arguments)]
fn move_local_pool_files(
    tapes_dir: &Path,
    source_backend: &str,
    target_backend: &str,
    namespace: Option<&str>,
    hashes: &[String],
    sizes: &[u64],
    source_budget: Option<&PoolBudget>,
    target_budget: Option<&PoolBudget>,
) -> Result<u64> {
    let parent = tapes_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let source_pool = pool_dir_for(&parent, source_backend, namespace);
    let target_pool = pool_dir_for(&parent, target_backend, namespace);
    let mut moved = 0u64;
    // Pool-budget accounting follows the actual on-disk byte movement:
    // a chunk that physically leaves the source pool releases the
    // source budget; one that physically lands as a NEW file in the
    // target pool reserves the target budget. The "target already has
    // it" branch only frees the source copy, so it releases the source
    // but does NOT reserve the target (those bytes were already
    // accounted when the target file first appeared). `force_reserve`,
    // not `try_reserve`: migration is an operator job that must not
    // block on backpressure. `namespace` is the same pool key the
    // seal/eviction paths use, so the per-namespace bucket stays exact.
    for (i, hash) in hashes.iter().enumerate() {
        let (s1, s2) = shard_pair(hash);
        let src = source_pool.join(s1).join(s2).join(format!("{hash}.dat"));
        if !src.is_file() {
            continue;
        }
        let size = sizes.get(i).copied().unwrap_or(0);
        let dst_dir = target_pool.join(s1).join(s2);
        fs::create_dir_all(&dst_dir)?;
        let dst = dst_dir.join(format!("{hash}.dat"));
        if dst.is_file() {
            // Target already has it (re-run after partial failure, or
            // dedup hit). Drop the source copy and move on — source
            // shrinks, target unchanged.
            fs::remove_file(&src)?;
            if let Some(b) = source_budget {
                b.release(size, namespace);
            }
            moved += 1;
            continue;
        }
        match fs::rename(&src, &dst) {
            Ok(()) => moved += 1,
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                fs::copy(&src, &dst)?;
                fs::remove_file(&src)?;
                moved += 1;
            }
            Err(e) => return Err(SmcError::Io(e)),
        }
        // New file landed on the target, source copy gone: move the
        // accounted bytes from the source budget to the target budget.
        if let Some(b) = source_budget {
            b.release(size, namespace);
        }
        if let Some(b) = target_budget {
            b.force_reserve(size, namespace);
        }
    }
    Ok(moved)
}

fn pool_dir_for(parent: &Path, backend: &str, namespace: Option<&str>) -> PathBuf {
    let base = parent.join("chunks").join(backend);
    match namespace {
        Some(ns) => base.join(ns),
        None => base,
    }
}

fn shard_pair(hash_hex: &str) -> (&str, &str) {
    let s1 = if hash_hex.len() >= 2 {
        &hash_hex[..2]
    } else {
        "00"
    };
    let s2 = if hash_hex.len() >= 4 {
        &hash_hex[2..4]
    } else {
        "00"
    };
    (s1, s2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_check_refuses_held_cartridge() {
        let err = hold_check_permits(Ok(true)).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(m) if m.contains("legal hold")));
    }

    #[test]
    fn hold_check_permits_unheld_cartridge() {
        assert!(hold_check_permits(Ok(false)).is_ok());
    }

    #[test]
    fn hold_check_treats_not_supported_as_unheld() {
        // A local backend can't carry a cloud-native hold.
        let r = hold_check_permits(Err(ObjectStoreError::NotSupported("local".into())));
        assert!(r.is_ok());
    }

    #[test]
    fn hold_check_fails_safe_on_other_errors() {
        // A transient read failure must NOT be read as "not held".
        let err = hold_check_permits(Err(ObjectStoreError::Network("timeout".into()))).unwrap_err();
        assert!(matches!(err, SmcError::ObjectStoreError(_)));
    }

    #[test]
    fn worm_lock_refuses_worm_onto_unlocked_target() {
        let err = worm_lock_permits(true, LockState::Off).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(m) if m.contains("lock-enabled")));
    }

    #[test]
    fn worm_lock_permits_worm_onto_locked_target() {
        assert!(worm_lock_permits(true, LockState::Governance { default_days: 30 }).is_ok());
        assert!(worm_lock_permits(true, LockState::Compliance { default_days: 365 }).is_ok());
    }

    #[test]
    fn worm_lock_ignores_non_worm_cartridges() {
        // A non-WORM cartridge is unconstrained by the target's lock state.
        assert!(worm_lock_permits(false, LockState::Off).is_ok());
    }
}
