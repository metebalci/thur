// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Storage-tiering surface on [`Cartridge`].
//!
//! Lifted out of `cartridge/mod.rs` (was a ~3000-line single `impl
//! Cartridge`) so the S3-tiering + manifest-backup paths read on
//! their own. Public method names are unchanged — call sites compile
//! verbatim. Logic is a straight move; no behaviour difference.
//!
//! Covers:
//! - chunk-upload pipeline (`get_pending_uploads`,
//!   `upload_chunk_to_storage`, `pending_upload_payload`,
//!   `apply_chunk_upload_outcome`, `mark_chunk_evicted`)
//! - hash / eviction snapshots
//!   (`referenced_chunk_hashes`, `evictable_chunks`,
//!   `has_storage_backend`, `root_path`)
//! - manifest backup / restore + version retention
//!   (`backup_manifest_to_storage`, `restore_manifest_from_storage`,
//!   `restore_indexes_from_storage`, `cleanup_old_manifest_versions`)

use std::fs;
use std::path::{Path, PathBuf};

use shared_object_store::ObjectStoreBackend;

use super::{
    BlockIndexFile, Cartridge, ChunkIndexFile, ChunkUploadOutcome, LocationTag,
    ManifestBackupOutcome, PendingUploadPayload, Result, SmcError, now_timestamp,
    upload_chunk_inert,
};

impl Cartridge {
    // --- S3 Tiering Methods ---

    /// Get list of sealed chunks that need to be uploaded.
    /// Returns (chunk_id, hash, local_store_path) tuples. Unsealed
    /// (active staging) chunks are excluded — only sealed chunks have a
    /// stable content hash and a stable storage key.
    pub fn get_pending_uploads(&self) -> Vec<(u64, String, PathBuf)> {
        let mut pending = Vec::new();
        for entry in self.chunk_index.iter() {
            let Ok((id, chunk)) = entry else { break };
            if chunk.uploaded {
                continue;
            }
            if let Some(hash) = chunk.hash {
                let local_path = self.chunk_store.store_path(&hash);
                pending.push((id, hash, local_path));
            }
        }
        pending
    }

    /// Upload a specific chunk to storage storage (called by background worker).
    /// Skips the upload if another cartridge already pushed an object with
    /// the same hash — this is the cross-cartridge dedup hit on the storage
    /// side. Either way, the cartridge's manifest is updated to reflect
    /// the chunk is now safe to evict.
    ///
    /// Returns the storage key the chunk lives at on success — the caller
    /// (upload worker) needs this to honor an active legal hold by
    /// re-applying the per-object hold flag after the PUT (the PUT
    /// creates a fresh object version on lock-enabled buckets, and the
    /// hold is per-version).
    pub async fn upload_chunk_to_storage(&mut self, chunk_id: u64) -> Result<String> {
        let payload = match self.pending_upload_payload(chunk_id) {
            Some(p) => p,
            None => {
                // Either the chunk doesn't exist, is unsealed, or is
                // already marked uploaded. The unsealed case is a
                // caller bug; the already-uploaded case is benign and
                // we just hand back the key so the caller can reapply
                // legal hold if needed.
                let chunk = self.read_chunk_rec(chunk_id)?;
                let hash = chunk.hash.as_ref().ok_or(SmcError::InvalidOp(
                    "chunk has no hash (still in staging) — seal before uploading",
                ))?;
                return Ok(self.chunk_store.object_key_in_store(hash));
            }
        };

        let backend = self
            .storage_backend
            .as_deref()
            .ok_or(SmcError::InvalidOp("no storage backend configured"))?;

        let outcome = upload_chunk_inert(backend, &payload).await?;
        self.apply_chunk_upload_outcome(&outcome);
        if !outcome.dedup_hit {
            tracing::info!("Successfully uploaded chunk {} to storage", chunk_id);
        }
        Ok(outcome.object_key)
    }

    /// Snapshot of everything an external caller needs to upload a
    /// single chunk without holding a `&Cartridge` reference: the
    /// chunk id, the content hash, and the local pool path. Returns
    /// None if the chunk doesn't exist, is unsealed, or is already
    /// marked uploaded. The daemon's upload worker uses this to drive
    /// parallel uploads from a single owning Cartridge — see
    /// [`upload_chunk_inert`] / [`apply_chunk_upload_outcome`].
    pub fn pending_upload_payload(&self, chunk_id: u64) -> Option<PendingUploadPayload> {
        let c = self.read_chunk_rec(chunk_id).ok()?;
        if c.uploaded {
            return None;
        }
        let hash = c.hash.as_ref()?;
        Some(PendingUploadPayload {
            item_id: chunk_id,
            hash: hash.clone(),
            local_path: self.chunk_store.store_path(hash),
            object_key: self.chunk_store.object_key_in_store(hash),
            dedup: self.manifest.dedup,
            backend_name: self.manifest.backend.clone(),
        })
    }

    /// Apply the outcome of a [`upload_chunk_inert`] call to this
    /// cartridge's chunk-index: flip `uploaded = true`, set
    /// `location = Both`, capture storage-side compression info on a
    /// fresh PUT. Also bumps the lifetime `backend_bytes_written`
    /// counter by the on-wire bytes PUT (skipped on a dedup hit,
    /// where `put_bytes` is `None` — nothing was transferred). The
    /// chunk-index update is no-op if the chunk is no longer in the
    /// index (e.g. raced with GC); the byte counter still moves
    /// because the PUT did happen. The chunk-index pwrite is durable
    /// on return — no separate `persist_manifest()` is needed for
    /// chunk state.
    pub fn apply_chunk_upload_outcome(&mut self, outcome: &ChunkUploadOutcome) {
        if let Some(n) = outcome.put_bytes {
            self.runtime.backend_bytes_written =
                self.runtime.backend_bytes_written.saturating_add(n);
        }
        let Ok(mut c) = self.read_chunk_rec(outcome.item_id) else {
            return;
        };
        c.uploaded = true;
        c.location = LocationTag::Both;
        if !outcome.dedup_hit {
            c.compression = outcome.put_compression;
        }
        let _ = self.update_chunk_rec(outcome.item_id, &c);
    }

    /// Mark a chunk as evicted in this cartridge's view. With shared
    /// content-addressed storage, the actual local file may still be
    /// referenced by other cartridges; the cache manager is responsible
    /// for refcount-checking before deleting from the shared pool.
    /// This method only updates this manifest's per-cartridge `location`.
    pub fn mark_chunk_evicted(&mut self, chunk_id: u64) -> Result<()> {
        let mut chunk = self.read_chunk_rec(chunk_id)?;

        if chunk.hash.is_none() {
            return Err(SmcError::InvalidOp(
                "cannot evict an unsealed (staging) chunk",
            ));
        }
        if !chunk.uploaded {
            return Err(SmcError::InvalidOp(
                "cannot evict chunk that is not uploaded",
            ));
        }
        if chunk.location != LocationTag::Both {
            return Err(SmcError::InvalidOp(
                "cannot evict chunk that is not in Both state",
            ));
        }

        chunk.location = LocationTag::StorageOnly;
        self.update_chunk_rec(chunk_id, &chunk)?;
        tracing::debug!("Marked chunk {} as evicted (StorageOnly)", chunk_id);
        Ok(())
    }

    /// Iterate over hashes of every sealed chunk. Used by the cache
    /// manager to build a global reference set across all cartridges
    /// before deleting any file from the shared pool.
    pub fn referenced_chunk_hashes(&self) -> Vec<String> {
        self.chunk_index
            .iter()
            .filter_map(|entry| entry.ok().and_then(|(_, c)| c.hash))
            .collect()
    }

    /// Snapshot of sealed-chunk eviction metadata for this cartridge.
    /// Returns `(chunk_id, hash, size, last_accessed)` for chunks that
    /// are uploaded and currently `Both` (i.e., evictable from this
    /// cartridge's perspective). Excludes unsealed staging chunks.
    /// `last_accessed` is read from the local-only `lru.idx` sidecar;
    /// 0 if the slot was never touched (cold-start fallback — sorts
    /// oldest-first, which is what eviction wants).
    pub fn evictable_chunks(&self) -> Vec<(u64, String, u64, u64)> {
        self.chunk_index
            .iter()
            .filter_map(|entry| {
                let (id, c) = entry.ok()?;
                if !c.uploaded || c.location != LocationTag::Both {
                    return None;
                }
                let h = c.hash?;
                let last = self.lru_index.read(id).unwrap_or(0);
                Some((id, h, c.size, last))
            })
            .collect()
    }

    /// Check if S3 backend is configured
    pub fn has_storage_backend(&self) -> bool {
        self.storage_backend.is_some()
    }

    /// Get the root path of this cartridge
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    // --- Manifest Backup/Restore Methods ---

    /// Backup manifest to storage storage with versioning. Three layers:
    ///
    /// 1. Ship every dirty page of `chunks.idx` and each
    ///    `blocks-p<N>.idx` to
    ///    `manifests/<label>/<file_label>/page-<NNNNNN>.dat`. Pages
    ///    are content-addressed by sequence (overwrite on the next
    ///    mutation); the dirty bitmap drives the delta. Without this
    ///    layer a cold-bucket DR has no way to map LBA → chunk hash.
    /// 2. Stamp each file's `IndexEpoch { pages, page_size, epoch,
    ///    file_size }` into the manifest's `index_epoch` map so
    ///    restore knows what to fetch.
    /// 3. PUT the JSON manifest twice: a versioned backup, then the
    ///    `manifest-latest.json` sentinel last (mirrors legal-hold
    ///    sentinel-last ordering — a torn upload leaves the sentinel
    ///    pointing at the previous consistent epoch).
    ///
    /// Returns `(versioned_key, latest_key)` so the upload worker can
    /// extend an active legal hold over the freshly-PUT manifest
    /// objects.
    pub async fn backup_manifest_to_storage(&mut self) -> Result<ManifestBackupOutcome> {
        let backend = self
            .storage_backend
            .as_ref()
            .ok_or(SmcError::InvalidOp("no storage backend configured"))?
            .clone();

        let label = self.manifest.label.clone();

        // 1+2. Index pages first, sentinel-last for the same reason
        // legal-hold uses sentinel-last: if we crash after pages but
        // before sentinel, restore reads the previous epoch which is
        // still pages-consistent.
        let mut index_page_keys: Vec<String> = Vec::new();
        // chunks.idx
        {
            let file_ref = crate::index_backup::chunk_index_file_ref(&self.chunk_index)?;
            let (epoch, page_keys) =
                crate::index_backup::upload_one_index(&label, &file_ref, &*backend).await?;
            self.runtime.index_epoch.insert("chunks".to_string(), epoch);
            index_page_keys.extend(page_keys);
        }
        // blocks-p<N>.idx
        for (partition, bif) in self.block_indexes.iter().enumerate() {
            let file_ref = crate::index_backup::block_index_file_ref(bif, partition as u8)?;
            let key = format!("blocks-p{}", partition);
            let (epoch, page_keys) =
                crate::index_backup::upload_one_index(&label, &file_ref, &*backend).await?;
            self.runtime.index_epoch.insert(key, epoch);
            index_page_keys.extend(page_keys);
        }

        // Bundle manifest + runtime into one storage sentinel object.
        // Identity stays creation-frozen on disk, but cold-bucket DR
        // needs both halves; the alternative (two storage objects) costs
        // a HEAD per restore and an extra failure mode on PUT.
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "manifest": &self.manifest,
            "runtime": &self.runtime,
        }))?;
        let timestamp = now_timestamp();

        // 3a. Versioned backup.
        let versioned_key = format!("manifests/{}/manifest-{}.json", label, timestamp);
        backend.upload_manifest(&versioned_key, &json).await?;
        tracing::debug!("Backed up manifest bundle to S3: {}", versioned_key);

        // 3b. Latest sentinel — last write of the pass.
        let latest_key = format!("manifests/{}/manifest-latest.json", label);
        backend.upload_manifest(&latest_key, &json).await?;
        tracing::debug!("Updated latest manifest bundle in S3: {}", latest_key);

        // Persist the (now updated) `index_epoch` map locally. Without
        // this, a crash between storage upload and the next unrelated
        // persist leaves on-disk and storage manifests out of sync —
        // cold-bucket DR could read stale local epochs and miss the
        // page-uploads we just shipped.
        self.persist_runtime()?;

        Ok(ManifestBackupOutcome {
            versioned_key,
            latest_key,
            index_page_keys,
        })
    }

    /// Restore a `manifest-latest.json` bundle from storage. Returns
    /// the manifest + runtime JSON halves (`(manifest_json,
    /// runtime_json)`) for the caller to persist locally. Bundle
    /// shape: `{"manifest": {...identity...}, "runtime": {...sidecar...}}`.
    pub async fn restore_manifest_from_storage(
        label: &str,
        backend: &dyn ObjectStoreBackend,
    ) -> Result<(String, String)> {
        let latest_key = format!("manifests/{}/manifest-latest.json", label);
        tracing::info!(
            "Attempting to restore manifest bundle from S3: {}",
            latest_key
        );

        let body = backend.download_manifest(&latest_key).await?;
        tracing::info!(
            "Successfully restored manifest bundle from S3 ({} bytes)",
            body.len()
        );
        let bundle: serde_json::Value = serde_json::from_str(&body)?;
        let manifest_v = bundle.get("manifest").cloned().ok_or(SmcError::InvalidOp(
            "storage manifest bundle missing 'manifest' field",
        ))?;
        let runtime_v = bundle.get("runtime").cloned().ok_or(SmcError::InvalidOp(
            "storage manifest bundle missing 'runtime' field",
        ))?;
        let manifest_json = serde_json::to_string(&manifest_v)?;
        let runtime_json = serde_json::to_string(&runtime_v)?;
        Ok((manifest_json, runtime_json))
    }

    /// Restore the per-cartridge index files (`chunks.idx` +
    /// `blocks-p<N>.idx`) from storage by replaying the page sequence
    /// recorded in the runtime sidecar's `index_epoch` map. Used in
    /// the cold-bucket DR path immediately after
    /// `restore_manifest_from_storage` and before
    /// `BlockIndexFile::open_or_create` / `ChunkIndexFile::open_or_create`.
    ///
    /// `cart_root` is the per-cartridge directory (e.g.
    /// `<data_dir>/tapes/<barcode>/`). Pre-existing index files at
    /// those paths are overwritten — the storage copy is authoritative
    /// in this code path. Empty `index_epoch` map (legacy bundle
    /// written before delta-page index backup shipped) is a no-op
    /// returning Ok — the caller's open path will then create empty
    /// index files; correctness in that case is the operator's
    /// problem (the data is unrecoverable without per-block / per-
    /// chunk metadata, but that's a pre-existing gap).
    pub async fn restore_indexes_from_storage(
        cart_root: &Path,
        label: &str,
        runtime_json: &str,
        backend: &dyn ObjectStoreBackend,
    ) -> Result<()> {
        // Parse just enough of the runtime sidecar to read
        // index_epoch. We don't trust the existing on-disk file
        // because the local copy may be missing entirely.
        let runtime: super::Runtime = serde_json::from_str(runtime_json)?;
        if runtime.index_epoch.is_empty() {
            tracing::warn!(
                "runtime {} has no index_epoch - skipping index restore (legacy bundle or backup before index pages shipped)",
                label
            );
            return Ok(());
        }
        fs::create_dir_all(cart_root)?;
        for (file_label, epoch) in &runtime.index_epoch {
            let dest = if file_label == "chunks" {
                ChunkIndexFile::path_for(cart_root)
            } else if let Some(p_str) = file_label.strip_prefix("blocks-p") {
                let partition: u8 = p_str.parse().map_err(|_| {
                    SmcError::InvalidOp(
                        "manifest index_epoch key has invalid blocks-p<N> partition number",
                    )
                })?;
                BlockIndexFile::path_for(cart_root, partition)
            } else {
                tracing::warn!(
                    "manifest {} has unknown index_epoch label '{}' - skipping",
                    label,
                    file_label
                );
                continue;
            };
            tracing::info!(
                "Restoring index file '{}' for {} from storage ({} pages, file_size={} B)",
                file_label,
                label,
                epoch.pages,
                epoch.file_size,
            );
            crate::index_backup::restore_one_index(label, file_label, &dest, epoch, backend)
                .await?;
        }
        Ok(())
    }

    /// Cleanup old manifest versions, keeping only the last N versions
    pub async fn cleanup_old_manifest_versions(&self, keep_count: usize) -> Result<usize> {
        let backend = self
            .storage_backend
            .as_ref()
            .ok_or(SmcError::InvalidOp("no storage backend configured"))?;

        let label = &self.manifest.label;
        let prefix = format!("manifests/{}/", label);

        // List all manifest objects
        let keys = backend.list_objects(&prefix).await?;

        // Filter for versioned manifests (manifest-{timestamp}.json)
        let mut versioned: Vec<String> = keys
            .iter()
            .filter(|k| {
                k.starts_with(&format!("manifests/{}/manifest-", label))
                    && k.ends_with(".json")
                    && !k.ends_with("manifest-latest.json")
            })
            .cloned()
            .collect();

        // Sort by name (timestamp is in name, so lexicographic sort works)
        versioned.sort();
        versioned.reverse(); // Newest first

        // If we have more than keep_count, delete the old ones
        let deleted_count = if versioned.len() > keep_count {
            let to_delete = &versioned[keep_count..];
            tracing::info!(
                "Cleaning up {} old manifest versions for {} (keeping {})",
                to_delete.len(),
                label,
                keep_count
            );

            for key in to_delete {
                tracing::debug!("Deleting old manifest: {}", key);
                backend.delete_object(key).await?;
            }

            to_delete.len()
        } else {
            0
        };

        Ok(deleted_count)
    }
}
