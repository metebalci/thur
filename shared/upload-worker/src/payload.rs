// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-upload payload + outcome types — backend-neutral so both the
//! tape and block products can construct them at their respective
//! `pending_upload_payload` boundaries without re-deriving the
//! pipeline.

use std::path::PathBuf;

use shared_object_store::{CompressionAlgo, DedupScope};

/// One chunk's worth of "ready to upload" state, decoupled from the
/// owning cartridge / volume so the daemon's upload worker can hand
/// it to a parallel task without holding a `&Cartridge` /
/// `&VolumeWriter` borrow. Constructed by per-product helpers
/// (`Cartridge::pending_upload_payload`,
/// `VolumeWriter::pending_upload_payload`).
#[derive(Debug, Clone)]
pub struct PendingUpload {
    /// Per-product identifier — tape `chunk_id` or block `page_id`.
    /// Echoed back in [`UploadOutcome::item_id`] so the caller can
    /// match outcomes to its own index records without keeping a
    /// side map.
    pub item_id: u64,
    /// BLAKE3 hex of the chunk's content. Pool path and storage key
    /// both derive from this.
    pub hash: String,
    /// On-disk pool path the upload reads from. Absolute. The
    /// uploader does `tokio::fs::read` against this — the file must
    /// be present until the upload completes.
    pub local_path: PathBuf,
    /// Storage key (already namespaced per [`DedupScope`] by the
    /// caller — pool's `object_key` / `object_key_for` helpers do this).
    /// The uploader doesn't reinterpret it; it just PUTs there.
    pub object_key: String,
    /// Source's dedup scope. Under [`DedupScope::Global`] the
    /// uploader does a storage-side HEAD probe to skip the PUT on a
    /// sibling-cartridge / sibling-volume dedup hit. Under
    /// [`DedupScope::Local`] the storage key is namespaced per
    /// cartridge / volume by construction so the HEAD is guaranteed
    /// to miss — wasted RTT, skipped. Worst-case cost on skip: a
    /// daemon crash that loses the per-product "uploaded" flag
    /// re-PUTs the same bytes on resume; correct, just a bandwidth
    /// nick.
    pub dedup: DedupScope,
    /// Storage backend name (matches the `storage.backends.<name>` key
    /// in each product's yaml). Used purely for telemetry labelling
    /// — the upload worker already routes via the backend handle, so
    /// this field is informational, not a routing input.
    pub backend_name: String,
}

/// Result of [`crate::upload_chunk_inert`], carrying enough
/// information for the owning per-product state machine to flip its
/// own index record (chunks.idx for tape, upload.idx for block) after
/// a parallel upload batch completes.
#[derive(Debug, Clone)]
pub struct UploadOutcome {
    /// Echoed from [`PendingUpload::item_id`] so the caller can
    /// match outcomes back to index records without a side map.
    pub item_id: u64,
    /// Echoed from [`PendingUpload::hash`] (BLAKE3 hex of the uploaded
    /// chunk's content). Lets the caller confirm the index record it is
    /// about to flip still references *this* chunk and wasn't superseded
    /// by a re-write between enqueue and completion (issue #113).
    pub hash: String,
    /// Echoed from [`PendingUpload::object_key`].
    pub object_key: String,
    /// True iff cross-namespace dedup fired (storage HEAD hit under
    /// `Global`) and no PUT was performed. In that case
    /// `put_compression` is unset — the existing object's compression
    /// is whatever the original PUT chose, which the new caller
    /// shouldn't claim authority over.
    pub dedup_hit: bool,
    /// Algorithm the upload worker applied for this PUT, or `None`
    /// when the storage copy is uncompressed (or `dedup_hit` is true).
    /// Block side doesn't currently track this — VSA's compression
    /// is unset; the field is preserved for future use plus VTL
    /// parity.
    pub put_compression: Option<CompressionAlgo>,
    /// On-wire bytes PUT to storage for this chunk — post-compression,
    /// i.e. the real backend storage cost. `None` when no PUT
    /// happened (`dedup_hit` is true) so the caller's backend-bytes
    /// meter doesn't count an object it never transferred. Consumed
    /// by VSA's `backend_bytes_written` counter and VTL's equivalent.
    pub put_bytes: Option<u64>,
}
