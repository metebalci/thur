// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! core-block — SBC-3 block-storage product core.
//!
//! Sibling to `core-mediachanger`. Houses SBC-3 logic, the per-volume
//! page table, and the write-back / cloud-tier pipeline. Cloud
//! backends + retry / compression primitives come from
//! `shared-cloud`; the chunk pool is currently a lean thurvsa-side
//! mirror of thurvtl's `chunk_store` and will move to a future
//! `shared-storage` crate once both products give us evidence
//! about what really wants to be shared. See `ROADMAP.md` §
//! Workspace restructure.

pub mod cache;
pub mod chunk_pool;
pub mod disk_cache;
pub mod lru_index;
pub mod page_index;
pub mod runtime_state;
pub mod upload_index;
pub mod uploader;
pub mod verify;
pub mod volume;

pub use cache::{DEFAULT_CACHE_BUDGET_BYTES, PageCache};
pub use chunk_pool::{ChunkPool, ChunkPoolError};
// `DiskCacheSize` / `DiskCacheBounds` live in `shared-pool` so the
// YAML default and the per-entry `cloud.backends:` override can't
// drift; re-exported here so vsa-daemon doesn't need a direct
// shared-pool dep just for the type.
pub use disk_cache::{DiskCacheManager, refresh_pool_budget_from_volumes};
pub use lru_index::{LruIndexError, LruIndexFile};
pub use page_index::{PageIndex, PageIndexError};
pub use runtime_state::VolumeRuntime;
pub use shared_pool::{DiskCacheBounds, DiskCacheSize};
pub use upload_index::{UploadIndexError, UploadIndexFile, UploadState};
pub use uploader::{
    DEFAULT_BACKPRESSURE_DEADLINE, PendingUploads, UploadTask, UploaderError, VolumeWriter,
    WritePageOutcome,
};
pub use verify::{VerifyScope, VolumeVerifyReport};
pub use volume::{DedupScope, SyncAfter, VolumeError, VolumeManifest, parse_size};
