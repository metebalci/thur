// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cloud-upload pipeline scaffold shared by the tape (core-stream /
//! thurvtl) and block (core-block / thurvsa) products.
//!
//! Two surfaces:
//!
//! - [`upload_chunk_inert`] — stateless async function that uploads a
//!   single [`PendingUpload`] to a [`CloudBackend`], doing the
//!   cloud-side dedup HEAD probe (under `DedupScope::Global`) before
//!   the PUT. Returns an [`UploadOutcome`] the caller can use to update
//!   product-specific index state. No cartridge / volume borrow held
//!   during the await; safe to run from a parallel worker task.
//! - [`run_upload_pipeline`] — drives a batch of [`PendingUpload`]s
//!   through `upload_chunk_inert` with at most `max_concurrent` PUTs
//!   in flight (`buffer_unordered`). Each completion fires a
//!   caller-supplied post-upload hook (apply-outcome, auto-hold,
//!   eviction-Notify) before yielding the outcome, so a slow PUT
//!   doesn't gate its siblings.
//!
//! What's product-specific (kept out of this crate):
//!
//! - The request type the daemon's worker receives over its `mpsc`
//!   channel (`VtlUploadRequest { tape_id, chunk_ids }` vs
//!   `VsaUploadRequest { volume_name, page_ids }`). Each daemon owns
//!   its own request struct and constructs `PendingUpload`s from it.
//! - Manifest-snapshot logic (`Cartridge::pending_upload_payload`,
//!   `VolumeWriter::pending_upload_payload`) that builds the
//!   `PendingUpload` from per-product on-disk index state.
//! - Outcome application (`Cartridge::apply_chunk_upload_outcome` for
//!   tape's `chunks.idx`; per-volume `upload.idx` for the block side)
//!   — stays on the owning state machine so it remains the sole
//!   writer to its own index.
//! - Crash-recovery scanning (tape walks `<data_dir>/tapes/`; block
//!   walks `<data_dir>/volumes/`). The scan path is the right place
//!   to re-enqueue surviving `LocalOnly` items on daemon boot.
//!
//! # Lifted from where
//!
//! - `PendingUpload` / `UploadOutcome` from `core_stream::cartridge`
//!   (formerly `PendingUploadPayload` / `ChunkUploadOutcome` —
//!   renamed to drop the tape-shaped `Chunk` prefix). The
//!   `chunk_id: u64` field is now `item_id: u64`, carrying tape
//!   chunk ids or block page ids depending on the caller.
//! - `upload_chunk_inert` from `core_stream::cartridge::mod`.
//! - `run_upload_pipeline` extracted from
//!   `vtl/daemon/src/upload_worker.rs::run_upload_pipeline` — the
//!   tape-side post-upload side effects (legal-hold re-apply,
//!   `disk_cache_evict_notify.notify_one()`) move into the hook
//!   closure the daemon passes in.

#![forbid(unsafe_code)]

pub mod inert;
pub mod payload;
pub mod pipeline;

#[cfg(test)]
mod test_support;

pub use inert::{UploadInertError, upload_chunk_inert};
pub use payload::{PendingUpload, UploadOutcome};
pub use pipeline::run_upload_pipeline;

// Re-export DedupScope for callers of this crate so the `payload`
// surface is self-contained — they don't need a parallel
// `shared-cloud` dep just to construct a `PendingUpload`.
pub use shared_cloud::DedupScope;
