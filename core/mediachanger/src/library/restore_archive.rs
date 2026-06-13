// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl library restore-archive` driver — pull a frozen
//! archive (produced by `cartridge_archive`) back into a live
//! cartridge.
//!
//! Differences vs `library restore`:
//!   - Archive keys live under `archives/<barcode>/<label>/`, not
//!     `manifests/<barcode>/`.
//!   - Index files (`chunks.idx`, `blocks-p<N>.idx`) are single
//!     binary blobs, not delta-page sequences.
//!   - Chunks live under `archives/<barcode>/<label>/chunks/...`,
//!     not the backend's regular pool.
//!   - Restored cartridge may carry a different barcode
//!     (`--as-barcode <NEW>`), in which case it gets a fresh UUID
//!     and the manifest's `label` is rewritten.
//!
//! Restore is eager on the chunk side: every chunk is downloaded
//! from the archive prefix and inserted into the local pool. Each
//! chunk's `chunks.idx` record is rewritten to `LocalOnly,
//! uploaded=false` so the daemon's existing orphan-upload sweep
//! eventually mirrors it into the backend's regular pool prefix
//! (where the live cartridge expects it on future reads after
//! eviction). The daemon-side handler triggers that sweep
//! immediately on Ok; otherwise the next daemon boot picks it up.
//!
//! Library inventory is updated via `Library::add_or_create_tape`,
//! same as `library restore`.

use std::fs;
use std::io::Write;
use std::path::Path;

use core_stream::ChunkStore;
use core_stream::chunk_index::{ChunkIndexFile, LocationTag};
use shared_object_store::ObjectStoreBackend;

use crate::errors::{Result, SmcError};

/// Inputs to [`run_restore_archive`].
pub struct RestoreArchiveOptions<'a> {
    pub tapes_dir: &'a Path,
    /// Backend handle. The archive lives on this backend; the
    /// restored cartridge will be bound to its name.
    pub backend: &'a dyn ObjectStoreBackend,
    pub backend_name: &'a str,
    /// Source barcode the archive was created under.
    pub barcode: &'a str,
    pub label: &'a str,
    /// Rename the restored cartridge. `None` keeps the original
    /// barcode; `Some(new)` writes the cartridge dir at
    /// `<tapes_dir>/<new>/`, rewrites `manifest.label`, mints a
    /// fresh UUID, and rewires `Local`-dedup namespace.
    pub as_barcode: Option<&'a str>,
    /// Skip silently if the destination cart dir already exists.
    pub allow_existing: bool,
    pub dry_run: bool,
    pub progress: Option<&'a (dyn Fn(&str) + Send + Sync)>,
}

/// Outcome of one restore-archive invocation.
///
/// On a successful real (non-dry-run, non-skipped) restore, the
/// caller is responsible for the final `Library::add_or_create_tape`
/// seat — the primitive doesn't touch the library inventory directly
/// (it would force the caller to hold the library mutex across
/// awaits, which doesn't compose well with the daemon's job runtime).
#[derive(Debug, Default)]
pub struct RestoreArchiveReport {
    /// Source barcode the archive was created under.
    pub source_barcode: String,
    /// Local barcode the restored cartridge lives under
    /// (== source_barcode unless `--as-barcode` was set).
    pub local_barcode: String,
    pub backend: String,
    pub label: String,
    pub chunks_total: u64,
    pub chunks_downloaded: u64,
    pub bytes_downloaded: u64,
    pub index_files_downloaded: u64,
    pub skipped_existing: bool,
    pub dry_run: bool,
}

/// Pull an archive back into a live cartridge. See module docs.
pub async fn run_restore_archive(opts: RestoreArchiveOptions<'_>) -> Result<RestoreArchiveReport> {
    let local_barcode = opts.as_barcode.unwrap_or(opts.barcode);
    validate_barcode(local_barcode)?;
    if opts.label.is_empty() {
        return Err(SmcError::InvalidOp("archive label must be non-empty"));
    }

    let archive_prefix = format!("archives/{}/{}/", opts.barcode, opts.label);
    let sentinel_key = format!("{}manifest.json", archive_prefix);

    let mut report = RestoreArchiveReport {
        source_barcode: opts.barcode.to_string(),
        local_barcode: local_barcode.to_string(),
        backend: opts.backend_name.to_string(),
        label: opts.label.to_string(),
        dry_run: opts.dry_run,
        ..Default::default()
    };

    let log = |msg: &str| {
        if let Some(p) = opts.progress {
            p(msg);
        }
    };

    let cart_root = opts.tapes_dir.join(local_barcode);
    if cart_root.exists() {
        if opts.allow_existing {
            report.skipped_existing = true;
            log(&format!(
                "skipping {}: local cartridge dir already exists (--allow-existing)",
                local_barcode
            ));
            return Ok(report);
        }
        return Err(SmcError::InvalidOp(
            "local cartridge dir already exists; pass --allow-existing to skip, \
             or remove it first",
        ));
    }

    // Probe the archive sentinel up front so a typo bails before any
    // mutation. (`download_manifest` would also fail, but the error
    // shape is murkier.)
    if !opts
        .backend
        .chunk_exists(&sentinel_key)
        .await
        .map_err(storage_err)?
    {
        return Err(SmcError::InvalidOp(
            "archive manifest sentinel not found on backend; check --backend / --barcode / --label",
        ));
    }

    if opts.dry_run {
        log(&format!(
            "dry-run: would restore archives/{}/{}/ as cartridge '{}' on backend {}",
            opts.barcode, opts.label, local_barcode, opts.backend_name
        ));
        return Ok(report);
    }

    // Phase 1: download the manifest + runtime sidecar, parse for
    // dedup scope, build the rewritten manifest we'll persist.
    log("downloading archive manifest + runtime");
    let original_manifest = opts
        .backend
        .download_manifest(&sentinel_key)
        .await
        .map_err(storage_err)?;
    let runtime_key = format!("{}runtime.json", archive_prefix);
    let original_runtime = opts
        .backend
        .download_manifest(&runtime_key)
        .await
        .map_err(storage_err)?;
    let dedup_local = manifest_is_local_dedup(&original_manifest)?;
    // A rename mints a fresh manifest UUID. For an encrypted cartridge
    // that is fatal: the keystore unwrap binds the manifest UUID as AAD
    // and every per-chunk IV derives from it, so a new UUID makes both
    // the DEK unwrap and AES-GCM tag verification fail on every chunk —
    // restore-archive would report success but produce an unreadable
    // cartridge (issue #122). Refuse the rename; restore under the
    // original barcode preserves the UUID and stays decryptable.
    let is_rename = opts.as_barcode.is_some_and(|b| b != opts.barcode);
    if is_rename && manifest_is_encrypted(&original_manifest)? {
        return Err(SmcError::InvalidOp(
            "cannot rename an encrypted cartridge on restore-archive: the manifest UUID binds the \
             keystore wrap context and per-chunk IVs, so a fresh UUID would make every chunk \
             undecryptable. Restore under the original barcode (omit --as-barcode).",
        ));
    }
    let rewritten_manifest =
        rewrite_manifest_for_local(&original_manifest, local_barcode, opts.backend_name, is_rename)?;
    // Runtime fields the restored cartridge can't inherit: the
    // index-backup epoch (gets repopulated on the next manifest
    // backup pass) and the pending partition layout (a stale stage
    // from before the archive). Everything else (partitions,
    // active_partition, set_capacity_proportion, and the four
    // lifetime byte counters) carries over.
    let rewritten_runtime = rewrite_runtime_for_local(&original_runtime)?;

    // Phase 2: prepare the local cart dir, write the manifest first
    // (sentinel) then the runtime sidecar. Atomic temp+rename for
    // each. The restored cartridge needs both files to open; if we
    // crash between the two, the next open path refuses cleanly via
    // `Runtime::load`.
    fs::create_dir_all(&cart_root)?;
    write_atomic(
        &cart_root.join("manifest.json"),
        rewritten_manifest.as_bytes(),
    )?;
    write_atomic(
        &cart_root.join("runtime.json"),
        rewritten_runtime.as_bytes(),
    )?;

    // Phase 3: pull chunks.idx + blocks-p<N>.idx from the archive
    // prefix into the cart dir.
    log("downloading index files");
    let chunks_idx_key = format!("{}chunks.idx", archive_prefix);
    let chunks_idx_bytes = opts
        .backend
        .download_chunk(&chunks_idx_key)
        .await
        .map_err(storage_err)?;
    write_atomic(&ChunkIndexFile::path_for(&cart_root), &chunks_idx_bytes)?;
    report.index_files_downloaded += 1;

    // Walk archive prefix once to find every blocks-p<N>.idx
    // (operator may have multiple partitions).
    let keys_under_archive = opts
        .backend
        .list_objects(&archive_prefix)
        .await
        .map_err(storage_err)?;
    let partition_keys: Vec<String> = keys_under_archive
        .iter()
        .filter(|k| {
            k.strip_prefix(&archive_prefix)
                .and_then(|rest| rest.strip_prefix("blocks-p"))
                .and_then(|rest| rest.strip_suffix(".idx"))
                .map(|n| n.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for key in &partition_keys {
        let body = opts
            .backend
            .download_chunk(key)
            .await
            .map_err(storage_err)?;
        let filename = key.strip_prefix(&archive_prefix).ok_or_else(|| {
            SmcError::ObjectStoreError("listed key missing archive prefix".to_string())
        })?;
        write_atomic(&cart_root.join(filename), &body)?;
        report.index_files_downloaded += 1;
    }

    // Phase 4: open the freshly-downloaded chunks.idx and pull every
    // chunk from the archive prefix into the local pool. Rewrite each
    // record so its location is LocalOnly + uploaded=false. The
    // daemon's orphan-upload sweep will mirror them into the
    // backend's regular pool prefix.
    log("downloading chunks");
    let chunk_idx = ChunkIndexFile::open_or_create(&cart_root)?;
    let pool = open_local_pool(
        opts.tapes_dir,
        opts.backend_name,
        local_barcode,
        dedup_local,
    )?;
    let total = chunk_idx.next_id();
    // Build the download worklist (chunks that carry a hash). Archive
    // chunk key shape matches `cartridge_archive`'s `archive_chunk_key`.
    let mut worklist: Vec<(u64, String, String)> = Vec::new();
    for id in 0..total {
        let rec = chunk_idx.read(id)?;
        if let Some(hash) = rec.hash {
            let s1 = if hash.len() >= 2 { &hash[..2] } else { "00" };
            let s2 = if hash.len() >= 4 { &hash[2..4] } else { "00" };
            let key = format!("{}chunks/{}/{}/{}.dat", archive_prefix, s1, s2, hash);
            worklist.push((id, hash, key));
        }
    }
    // Fan the GETs out with bounded concurrency instead of one object in
    // flight at a time — a 1M-chunk cartridge is 8-22 h of pure request
    // latency serially (issue #163). The pool insert + per-record
    // chunks.idx rewrite are local and cheap; they run as each download
    // lands (buffer_unordered polls on one task, so they serialize
    // safely against each other).
    use futures::stream::{self, StreamExt};
    let backend = opts.backend;
    let pool_ref = &pool;
    let idx_ref = &chunk_idx;
    let outcomes: Vec<Result<u64>> = stream::iter(worklist)
        .map(|(id, hash, key)| async move {
            let chunk_bytes = backend.download_chunk(&key).await.map_err(storage_err)?;
            let len = chunk_bytes.len() as u64;
            pool_ref.insert_verified_bytes(&hash, &chunk_bytes)?;
            let mut rec = idx_ref.read(id)?;
            rec.location = LocationTag::LocalOnly;
            rec.uploaded = false;
            idx_ref.overwrite(id, &rec)?;
            Ok(len)
        })
        .buffer_unordered(shared_verify_core::STORAGE_VERIFY_CONCURRENCY)
        .collect()
        .await;
    let mut downloaded = 0u64;
    let mut bytes = 0u64;
    for o in outcomes {
        bytes += o?;
        downloaded += 1;
    }
    log(&format!("downloaded {downloaded}/{total} chunks"));
    chunk_idx.fsync()?;
    report.chunks_total = total;
    report.chunks_downloaded = downloaded;
    report.bytes_downloaded = bytes;

    // Slot seating is the caller's responsibility — see
    // `RestoreArchiveReport` rustdoc.
    log("restore-archive complete (caller seats into library)");
    Ok(report)
}

fn validate_barcode(barcode: &str) -> Result<()> {
    if barcode.is_empty() || barcode.len() > 64 {
        return Err(SmcError::InvalidOp("barcode must be 1-64 characters"));
    }
    for c in barcode.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(SmcError::InvalidOp(
                "barcode must be ASCII alphanumeric plus '-' or '_'",
            ));
        }
    }
    Ok(())
}

fn storage_err(e: shared_object_store::ObjectStoreError) -> SmcError {
    SmcError::ObjectStoreError(e.to_string())
}

/// Atomic temp+rename write; mirrors the cartridge module's
/// `persist_manifest` pattern.
fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let tmp = path.with_extension({
        let mut s = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        s.push_str(".tmp");
        s
    });
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn manifest_is_local_dedup(manifest_json: &str) -> Result<bool> {
    let v: serde_json::Value = serde_json::from_str(manifest_json)?;
    match v.get("dedup").and_then(|s| s.as_str()) {
        Some("local") => Ok(true),
        // "global" or absent → Global (default)
        _ => Ok(false),
    }
}

/// Build the local manifest body from the archive's by rewriting the
/// fields the new cartridge owns (identity-class):
///
/// - `label` → new barcode
/// - `backend` → restoring backend
/// - `uuid` → fresh 16-byte (ASCII hex)
///
/// The `archived_from_backend` / `archived_at` provenance fields
/// stay so an operator can see where the data came from. Runtime
/// fields (index_epoch, pending_partition_layout) live in
/// `runtime.json` and are rewritten by
/// [`rewrite_runtime_for_local`].
fn rewrite_manifest_for_local(
    archive_manifest: &str,
    new_label: &str,
    new_backend: &str,
    mint_new_uuid: bool,
) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(archive_manifest)?;
    let obj = v.as_object_mut().ok_or(SmcError::InvalidOp(
        "archive manifest root is not an object",
    ))?;
    obj.insert(
        "label".to_string(),
        serde_json::Value::String(new_label.to_string()),
    );
    obj.insert(
        "backend".to_string(),
        serde_json::Value::String(new_backend.to_string()),
    );
    // Mint a fresh UUID only on a true rename. Restoring under the
    // original barcode preserves the UUID so the keystore wrap context
    // and per-chunk IVs still resolve — minting unconditionally broke
    // every encrypted restore (issue #122). The encrypted-rename case is
    // refused by the caller before reaching here. Hex-encoded (the
    // cartridge layer's uuid_serde accepts a 32-hex string).
    if mint_new_uuid {
        obj.insert(
            "uuid".to_string(),
            serde_json::Value::String(generate_uuid_hex()),
        );
    }
    Ok(serde_json::to_string(&v)?)
}

/// True iff the archive manifest carries a non-null `encryption` stanza
/// (appliance-side at-rest encryption was on for this cartridge).
fn manifest_is_encrypted(manifest_json: &str) -> Result<bool> {
    let v: serde_json::Value = serde_json::from_str(manifest_json)?;
    Ok(v.get("encryption").is_some_and(|e| !e.is_null()))
}

/// Strip runtime fields the restored cartridge can't inherit:
///
/// - `index_epoch` → empty map (gets repopulated on next backup pass)
/// - `pending_partition_layout` → removed (stale stage from pre-archive)
///
/// Everything else carries over: partition layout, active partition,
/// host-set capacity proportion, lifetime host write counter.
fn rewrite_runtime_for_local(archive_runtime: &str) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(archive_runtime)?;
    let obj = v
        .as_object_mut()
        .ok_or(SmcError::InvalidOp("archive runtime root is not an object"))?;
    obj.insert(
        "index_epoch".to_string(),
        serde_json::Value::Object(serde_json::Map::new()),
    );
    obj.remove("pending_partition_layout");
    Ok(serde_json::to_string(&v)?)
}

fn generate_uuid_hex() -> String {
    let mut bytes = [0u8; 16];
    // Seed from the system clock + a process counter so two
    // back-to-back calls don't collide. Not cryptographic (the
    // cartridge UUID isn't a secret); just unique.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let ctr = COUNTER.fetch_add(1, Ordering::Relaxed);
    bytes[..8].copy_from_slice(&now.to_le_bytes());
    bytes[8..].copy_from_slice(&ctr.to_le_bytes());
    hex::encode(bytes)
}

fn open_local_pool(
    tapes_dir: &Path,
    backend: &str,
    barcode: &str,
    dedup_local: bool,
) -> Result<ChunkStore> {
    let parent = tapes_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let pool = if dedup_local {
        ChunkStore::new_namespaced(&parent, backend, barcode)?
    } else {
        ChunkStore::new(&parent, backend)?
    };
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_preserves_uuid_when_not_renaming() {
        // Issue #122: restoring under the original barcode must keep the
        // manifest UUID so an encrypted cartridge stays decryptable.
        let original = r#"{"label":"OLD","backend":"old","uuid":"00112233445566778899aabbccddeeff"}"#;
        let out = rewrite_manifest_for_local(original, "OLD", "primary", false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v.get("uuid").and_then(|u| u.as_str()),
            Some("00112233445566778899aabbccddeeff"),
            "UUID must be preserved when not renaming"
        );
        assert_eq!(v.get("label").and_then(|l| l.as_str()), Some("OLD"));
        assert_eq!(v.get("backend").and_then(|b| b.as_str()), Some("primary"));
    }

    #[test]
    fn rewrite_mints_fresh_uuid_on_rename() {
        let original = r#"{"label":"OLD","backend":"old","uuid":"00112233445566778899aabbccddeeff"}"#;
        let out = rewrite_manifest_for_local(original, "NEW", "primary", true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let uuid = v.get("uuid").and_then(|u| u.as_str()).unwrap();
        assert_ne!(
            uuid, "00112233445566778899aabbccddeeff",
            "rename must mint a fresh UUID"
        );
        assert_eq!(uuid.len(), 32);
    }

    #[test]
    fn manifest_is_encrypted_detects_stanza() {
        assert!(!manifest_is_encrypted(r#"{"label":"X"}"#).unwrap());
        assert!(!manifest_is_encrypted(r#"{"label":"X","encryption":null}"#).unwrap());
        assert!(
            manifest_is_encrypted(
                r#"{"label":"X","encryption":{"algorithm":"aes-256-gcm","wrapped_dek":"zz"}}"#
            )
            .unwrap()
        );
    }
}
