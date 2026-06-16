// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cartridge archive — snapshot a cartridge's full state to a
//! different storage backend as a self-contained, frozen blob with no
//! live cartridge representation.
//!
//! Unlike [`crate::cartridge_migrate`], archive does not mutate the
//! source cartridge: the source's manifest, indexes, local pool, and
//! storage objects are unchanged. The archive is a parallel object
//! tree on the target backend keyed by `(barcode, label)`. Multiple
//! archives of the same cartridge can coexist under distinct labels.
//!
//! # Object layout on the target backend
//!
//! ```text
//! archives/<barcode>/<label>/manifest.json
//! archives/<barcode>/<label>/chunks.idx
//! archives/<barcode>/<label>/blocks-p<N>.idx
//! archives/<barcode>/<label>/chunks/<s1>/<s2>/<hash>.dat
//! ```
//!
//! Self-contained: chunks live under the archive prefix (not the
//! target backend's regular chunk pool), so each archive is a single
//! deletable subtree and an archive of cartridge X doesn't collide
//! with a live cartridge X on the same backend.
//!
//! The archive's `manifest.json` is the source manifest plus two
//! stamped fields:
//!   - `archived_from_backend: String` — the source cartridge's
//!     bound backend
//!   - `archived_at: String` — ISO-8601 UTC timestamp at archive time
//!
//! # What restores look like
//!
//! `library restore-archive` (separate verb) walks the archive
//! prefix, downloads the four metadata files + lazily-fetches chunks
//! through the existing cold-bucket read path. The restored cartridge
//! is a brand-new live cartridge directory, bound to whichever
//! backend the operator names (typically the archive's host backend,
//! but `--rebind-to-backend X` is a knob if the operator wants to
//! immediately migrate the restored cartridge to a third backend).

use std::fs;
use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use shared_object_store::ObjectStoreBackend;
use shared_pool::ChunkPool;

use crate::chunk_index::ChunkIndexFile;
use crate::errors::{Result, SmcError};

/// Inputs to [`run_archive`]. Borrowed for the call's lifetime.
pub struct ArchiveOptions<'a> {
    /// `<data_dir>/tapes/`.
    pub tapes_dir: &'a Path,
    pub barcode: &'a str,
    /// The source cartridge's bound storage backend (handle), used to
    /// fetch chunks marked `StorageOnly` that aren't in the local pool.
    /// May be the same `ObjectStoreBackend` impl as `target` if the operator
    /// is archiving back to the cartridge's own bucket under a
    /// different prefix.
    pub source: &'a dyn ObjectStoreBackend,
    /// Where the archive bytes land.
    pub target: &'a dyn ObjectStoreBackend,
    pub target_name: &'a str,
    /// 1-64-char alphanumeric (`-` and `_` allowed). Pre-validated by
    /// the daemon-side handler; the primitive re-validates anyway.
    pub label: &'a str,
    pub dry_run: bool,
    /// Same shape as [`crate::cartridge_migrate::MigrateOptions::progress`].
    pub progress: Option<&'a (dyn Fn(&str) + Send + Sync)>,
}

/// Outcome of one archive invocation.
#[derive(Debug, Default, Serialize)]
pub struct ArchiveReport {
    pub barcode: String,
    pub from_backend: String,
    pub to_backend: String,
    pub label: String,
    pub archived_at: String,
    pub chunks_total: u64,
    pub chunks_uploaded: u64,
    pub chunks_from_local_pool: u64,
    pub chunks_from_source_storage: u64,
    pub bytes_uploaded: u64,
    /// Index files captured: `chunks.idx` + every `blocks-p<N>.idx`.
    pub index_files_uploaded: u64,
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
struct ManifestSlice {
    label: String,
    #[serde(default)]
    backend: String,
    #[serde(default)]
    dedup: DedupSlice,
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DedupSlice {
    #[default]
    Global,
    Local,
}

impl DedupSlice {
    fn storage_namespace(self, barcode: &str) -> Option<&str> {
        match self {
            DedupSlice::Local => Some(barcode),
            DedupSlice::Global => None,
        }
    }
}

/// Run an archive operation. The source cartridge is read-only; only
/// the target backend is mutated (under the `archives/<barcode>/<label>/`
/// prefix).
///
/// Pre-flight checks:
///   - `manifest.json` exists for the named barcode
///   - `manifest.backend` is non-empty
///   - the archive sentinel (manifest.json under the archive prefix)
///     doesn't already exist on the target — refuses re-archive under
///     the same label rather than silently overwriting
///
/// Failure mid-run leaves a partial archive on the target. There is
/// no cleanup: a follow-up `system gc` on the target would not see
/// the orphans (archive objects live outside the normal `chunks/`
/// and `manifests/` prefixes). Operator's recourse is to delete the
/// archive prefix manually and re-archive under a fresh label.
pub async fn run_archive(opts: ArchiveOptions<'_>) -> Result<ArchiveReport> {
    validate_label(opts.label)?;
    if opts.target_name.is_empty() {
        return Err(SmcError::InvalidOp("target_name must be non-empty"));
    }

    let cart_root = opts.tapes_dir.join(opts.barcode);
    let manifest_path = cart_root.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(SmcError::InvalidOp(
            "cartridge directory or manifest.json missing",
        ));
    }
    let manifest_json = fs::read_to_string(&manifest_path)?;
    let slice: ManifestSlice = serde_json::from_str(&manifest_json)?;
    // Read the runtime sidecar too — restore-archive needs it back to
    // open the restored cartridge. Archive provenance lives on the
    // manifest side (identity-class); runtime travels as opaque
    // bytes.
    let runtime_path = cart_root.join("runtime.json");
    if !runtime_path.is_file() {
        return Err(SmcError::InvalidOp(
            "cartridge runtime.json missing — `cartridge create` was interrupted, or the runtime sidecar was hand-removed",
        ));
    }
    let runtime_json = fs::read_to_string(&runtime_path)?;
    if slice.label != opts.barcode {
        return Err(SmcError::InvalidOp(
            "manifest label disagrees with operator-stated barcode",
        ));
    }
    if slice.backend.is_empty() {
        return Err(SmcError::InvalidOp(
            "manifest has no `backend` field — cartridge not bound",
        ));
    }
    let namespace = slice.dedup.storage_namespace(opts.barcode);

    // Archive collision check. Refuse if the target already has an
    // archive at this prefix — operator must pick a different label.
    let archive_sentinel = format!("archives/{}/{}/manifest.json", opts.barcode, opts.label);
    if opts
        .target
        .chunk_exists(&archive_sentinel)
        .await
        .map_err(storage_err)?
    {
        return Err(SmcError::InvalidOp(
            "archive with this label already exists on the target — pick a different label",
        ));
    }

    // Walk chunks.idx, gather hashes. Same shape as migrate.
    let chunk_idx = ChunkIndexFile::open_or_create(&cart_root)?;
    let mut hashes: Vec<String> = Vec::new();
    for entry in chunk_idx.iter() {
        let (_id, rec) = entry?;
        if let Some(h) = rec.hash {
            hashes.push(h);
        }
    }
    drop(chunk_idx);

    let archived_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut report = ArchiveReport {
        barcode: opts.barcode.to_string(),
        from_backend: slice.backend.clone(),
        to_backend: opts.target_name.to_string(),
        label: opts.label.to_string(),
        archived_at: archived_at.clone(),
        chunks_total: hashes.len() as u64,
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
            "dry-run: would archive {} ({} chunks) -> {}: archives/{}/{}/",
            opts.barcode,
            hashes.len(),
            opts.target_name,
            opts.barcode,
            opts.label,
        ));
        return Ok(report);
    }

    // Phase 1: copy chunks. Prefer local pool when present (avoids
    // a source-storage round-trip); fall back to source storage.
    log(&format!(
        "archiving {} chunks to {}: archives/{}/{}/",
        hashes.len(),
        opts.target_name,
        opts.barcode,
        opts.label
    ));
    let local_pool = open_local_pool(opts.tapes_dir, &slice.backend, namespace)?;
    for (i, hash) in hashes.iter().enumerate() {
        let src_key = ChunkPool::object_key_for(namespace, hash);
        let dst_key = archive_chunk_key(opts.barcode, opts.label, hash);
        let (bytes, from_local) = if local_pool.exists(hash) {
            (local_pool.read_bytes(hash)?, true)
        } else {
            (
                opts.source
                    .download_chunk(&src_key)
                    .await
                    .map_err(storage_err)?,
                false,
            )
        };
        // Sanity: BLAKE3-verify. Defends against on-disk bit rot for
        // the local-pool branch (the storage-refetch branch is already
        // covered by the storage-integrity guard in the source's
        // download_chunk path, but defense in depth is cheap).
        let actual = blake3_hex(&bytes);
        if &actual != hash {
            return Err(SmcError::ContentHashMismatch {
                expected: hash.clone(),
                actual,
            });
        }
        let size = bytes.len() as u64;
        opts.target
            .upload_chunk(&dst_key, bytes)
            .await
            .map_err(storage_err)?;
        report.chunks_uploaded += 1;
        report.bytes_uploaded += size;
        if from_local {
            report.chunks_from_local_pool += 1;
        } else {
            report.chunks_from_source_storage += 1;
        }
        if (i + 1).is_multiple_of(64) {
            log(&format!("archived {}/{} chunks", i + 1, hashes.len()));
        }
    }

    // Phase 2: snapshot the cartridge's index files (chunks.idx +
    // blocks-p<N>.idx for every partition). Read full binary contents
    // from disk; upload via `upload_chunk` so the backend's
    // compression config can squeeze on the way out (same shape as
    // index-page-backup pages — same kind of binary blob).
    log("uploading index files");
    let chunks_idx_path = cart_root.join("chunks.idx");
    if chunks_idx_path.is_file() {
        let bytes = fs::read(&chunks_idx_path)?;
        let key = format!("archives/{}/{}/chunks.idx", opts.barcode, opts.label);
        // Versioned: re-archiving the same barcode+label overwrites
        // the same key with new index contents. `upload_versioned`
        // bypasses the meta-cache so the second archive's bytes
        // actually reach storage.
        opts.target
            .upload_versioned(&key, &bytes)
            .await
            .map_err(storage_err)?;
        report.index_files_uploaded += 1;
    }
    // Each partition's block index file. Don't depend on knowing how
    // many partitions exist — just walk the cartridge dir.
    for entry in fs::read_dir(&cart_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(rest) = name_str.strip_prefix("blocks-p") else {
            continue;
        };
        let Some(num) = rest.strip_suffix(".idx") else {
            continue;
        };
        if !num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        let key = format!("archives/{}/{}/{}", opts.barcode, opts.label, name_str);
        // Versioned (same rationale as chunks.idx above).
        opts.target
            .upload_versioned(&key, &bytes)
            .await
            .map_err(storage_err)?;
        report.index_files_uploaded += 1;
    }

    // Phase 3a: upload runtime.json *before* the manifest sentinel.
    // The manifest is what callers HEAD to discover the archive
    // (sentinel-last), so the runtime must already be in place when
    // a restore-archive run finds the sentinel.
    log("uploading runtime");
    let runtime_key = format!("archives/{}/{}/runtime.json", opts.barcode, opts.label);
    opts.target
        .upload_manifest(&runtime_key, &runtime_json)
        .await
        .map_err(storage_err)?;

    // Phase 3b: stamp the manifest with archive provenance and upload
    // last (sentinel-last; the manifest object is what callers HEAD
    // to discover an archive). Use upload_manifest for the JSON path
    // — compression doesn't apply, and the body is small.
    log("uploading manifest (sentinel-last)");
    let stamped = stamp_archive_provenance(&manifest_json, &slice.backend, &archived_at)?;
    opts.target
        .upload_manifest(&archive_sentinel, &stamped)
        .await
        .map_err(storage_err)?;

    log("archive complete");
    Ok(report)
}

fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > 64 {
        return Err(SmcError::InvalidOp("archive label must be 1-64 characters"));
    }
    for c in label.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(SmcError::InvalidOp(
                "archive label must be ASCII alphanumeric plus '-' or '_'",
            ));
        }
    }
    Ok(())
}

fn archive_chunk_key(barcode: &str, label: &str, hash: &str) -> String {
    let s1 = if hash.len() >= 2 { &hash[..2] } else { "00" };
    let s2 = if hash.len() >= 4 { &hash[2..4] } else { "00" };
    format!(
        "archives/{}/{}/chunks/{}/{}/{}.dat",
        barcode, label, s1, s2, hash
    )
}

fn open_local_pool(tapes_dir: &Path, backend: &str, namespace: Option<&str>) -> Result<ChunkPool> {
    let parent = tapes_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let pool = match namespace {
        Some(ns) => ChunkPool::new_namespaced(&parent, backend, ns)?,
        None => ChunkPool::new(&parent, backend)?,
    };
    Ok(pool)
}

fn storage_err(e: shared_object_store::ObjectStoreError) -> SmcError {
    SmcError::ObjectStoreError(e.to_string())
}

fn blake3_hex(bytes: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(bytes);
    hex::encode(h.finalize().as_bytes())
}

/// Insert `archived_from_backend` + `archived_at` fields into the
/// manifest. Schema-agnostic — uses `serde_json::Value` so future
/// manifest additions round-trip.
fn stamp_archive_provenance(
    manifest_json: &str,
    from_backend: &str,
    archived_at: &str,
) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(manifest_json)?;
    let obj = v
        .as_object_mut()
        .ok_or(SmcError::InvalidOp("manifest.json root is not an object"))?;
    obj.insert(
        "archived_from_backend".to_string(),
        serde_json::Value::String(from_backend.to_string()),
    );
    obj.insert(
        "archived_at".to_string(),
        serde_json::Value::String(archived_at.to_string()),
    );
    Ok(serde_json::to_string(&v)?)
}
