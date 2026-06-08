// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Sequential-access (SSC-4 / LTO) device-type core.
//!
//! Tape-side primitives consumed by `thurvtl` (SMC LUN 0 + N SSC
//! drives — composes this crate via `core-mediachanger`). Extracted from
//! `core-mediachanger`.

pub mod block_index;
pub mod cartridge;
pub mod cartridge_archive;
pub mod cartridge_migrate;
pub mod chunk_index;
pub mod chunk_store;
mod compression_codec;
pub mod dirty_pages;
pub mod disk_cache;
pub mod drive_state;
pub mod drive_topology;
pub mod encryption;
pub mod errors;
pub mod fastcdc;
pub mod index_backup;
pub mod legal_hold;
pub mod lru_index;
pub mod mode_state;
pub mod prefetch;
pub mod tape;

pub use cartridge::{
    Cartridge, CartridgeOpenMode, CartridgeOpenOptions, ChunkUploadOutcome, ChunkingMode,
    DedupScope, MAX_PARTITIONS, ManifestBackupOutcome, NextReadChunk, PendingPartitionLayout,
    PendingUploadPayload, PrefetchWindow, lto_default_capacity_gb, upload_chunk_inert,
};
pub use chunk_store::ChunkStore;
pub use disk_cache::{DiskCacheManager, refresh_pool_budget_from_tapes};
// `PoolBudget` + `BackpressureError` were lifted into `shared-pool` so
// the block side can share the same gate. Re-exported here so VTL call
// sites that referenced `core_stream::PoolBudget` (and the
// `core_mediachanger::PoolBudget` alias in `core/smc/src/lib.rs`) compile
// unchanged.
pub use drive_state::{DriveState, LibraryDriveState};
pub use drive_topology::DriveTopology;
pub use encryption::{
    ALGORITHM_CODE_AES_256_GCM, ALGORITHM_INDEX_AES_256_GCM, DecryptionMode, DriveEncryptionState,
    EncryptionMode, IV_LEN, KEY_LEN, KeyScope, TAG_LEN,
};
pub use errors::{Result, SmcError};
pub use fastcdc::{
    DEFAULT_AVG_SIZE as FASTCDC_DEFAULT_AVG, DEFAULT_MAX_SIZE as FASTCDC_DEFAULT_MAX,
    DEFAULT_MIN_SIZE as FASTCDC_DEFAULT_MIN,
};
pub use legal_hold::{
    CartridgeKeys, HoldRunReport, PerKeyOutcome, apply_cartridge_legal_hold,
    apply_legal_hold_to_keys, collect_cartridge_keys, manifest_latest_sentinel_key,
    read_cartridge_held, read_legal_hold_for_keys,
};
pub use mode_state::{DrivePageStore, SavedDrivePage};
pub use prefetch::{ChunkLocationInfo, PrefetchConfig, PrefetchManager};
pub use shared_pool::{BackpressureError, DiskCacheBounds, DiskCacheSize, GhostList, PoolBudget};
pub use tape::{Block, BlockKind, Filemark};
