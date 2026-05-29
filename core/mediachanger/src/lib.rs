// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Thur VTL tape-library core (medium changer + library inventory +
//! library-wide verify). Tape-cartridge primitives
//! (cartridge, indexes, chunking, encryption, disk cache, prefetch,
//! legal-hold sentinel) live in `core-stream` after Step 5
//! Milestone 5.B.1 — this crate re-exports the same flat names so
//! existing callers (thurvtld, thurvtl, integration tests)
//! compile unchanged. The crate is about to be renamed `core-mediachanger` in
//! Milestone 5.B.2.

pub mod daemon_lock;
pub mod direct_io;
pub mod events;
pub mod io_uring;
pub mod lbp;
pub mod legal_hold;
pub mod library;
pub mod tiering;
pub mod verify;

// Tape-side primitives moved to `core-stream` in Step 5 Milestone 5.B.1
// (2026-05-10). Re-export the historical module paths so existing
// callers (`core_mediachanger::cartridge::*`, `core_mediachanger::block_index::*`,
// …) compile unchanged. The flat type re-exports below match the old
// surface byte-for-byte.
pub use core_stream::{
    block_index, cartridge, cartridge_archive, cartridge_migrate, chunk_index, chunk_store,
    dirty_pages, disk_cache, drive_state, encryption, errors, fastcdc, index_backup, lru_index,
    mode_state, prefetch, tape,
};
// `core_stream::legal_hold` carries the cartridge sentinel logic; the
// library-side `find_drive_for_loaded_cartridge` lives in
// `crate::legal_hold` (smc-side). Both are re-exported flat below so
// `core_mediachanger::legal_hold::*` resolves to the union.
pub use core_stream::legal_hold as ssc_legal_hold;

// Storage backends + compression layer were lifted into the
// `shared-object-store` crate so the sibling block-storage product
// (core-block) can consume it without depending on core-mediachanger.
// Re-export both the modules (so `core_mediachanger::object_store_backend::*`
// continues to work) and the flat type names downstream callers
// (thurvtld, thurvtl) historically pulled in via
// `core_mediachanger::ObjectStoreConfig` etc.
pub use shared_object_store::{
    AzureBackend, BackendEntry, COMPRESSION_ALGORITHM_DEFAULT, CompressionAlgo, CompressionConfig,
    DriveCompressionState, FailureKind, GcsBackend, LocalBackend, LockState, ObjectStoreBackend,
    ObjectStoreCheckStep, ObjectStoreConfig, ObjectStoreConfigError, ObjectStoreError,
    RetentionMode, S3Backend, ZSTD_DEFAULT_LEVEL, compress_data, decompress_data,
    validate_object_store_backend,
};
pub use shared_object_store::{
    azure, compression, gcs, local, object_store_backend, object_store_config,
    object_store_helpers, s3,
};

// Telemetry layer was lifted into the `shared-telemetry` crate so the
// sibling thurvsad can install its own `Telemetry` via the same
// `record::*` free-function pattern. We re-export the historical
// `core_mediachanger::metrics` module path *and* the flat type names so
// existing callers (thurvtld, internal `crate::metrics::record::*`
// in cartridge.rs / disk_cache.rs) compile unchanged.
pub use shared_telemetry as metrics;

// Audit chain + channel + ratelimiter were lifted into the
// `shared-audit` crate so the sibling thurvsad can produce
// login-phase / volume-lifecycle audit entries against the same
// chain format. Re-export the historical module paths
// (`core_mediachanger::audit::*`, `core_mediachanger::audit_channel::*`,
// `core_mediachanger::audit_ratelimit::*`) and the flat names
// downstream callers (`core_mediachanger::AuditChannel` etc.) used.
pub use core_stream::cartridge::{
    AtRestCreateParams, Cartridge, CartridgeEncryptionAlgorithm, CartridgeEncryptionMeta,
    CartridgeOpenMode, CartridgeOpenOptions, ChunkUploadOutcome, ChunkingMode, DedupScope,
    MAX_PARTITIONS, ManifestBackupOutcome, NextReadChunk, PendingPartitionLayout,
    PendingUploadPayload, generate_cartridge_uuid, lto_default_capacity_gb, upload_chunk_inert,
};
pub use core_stream::chunk_store::ChunkStore;
pub use shared_audit::{
    AUDIT_CHANNEL_CAPACITY, AuditActor, AuditChannel, AuditConfig, AuditEntry, AuditError,
    AuditLog, AuditMode, AuditRateLimitDecision, AuditRateLimitRollup, AuditRateLimiter,
    AuditResult, AuditTailCursor, AuditWriterHandle, CHAIN_STATE_FILE, GENESIS_PREV_HASH,
    PENDING_AUDIT_DIR, PendingAuditEntry, VerifyReport, compute_entry_hash, queue_pending,
    read_entries, spawn_writer as spawn_audit_writer, tail_step, verify_chain,
};
pub use shared_audit::{audit, audit_channel, audit_ratelimit};
// Storage + compression re-exports were lifted to the `shared_object_store` block above.
pub use daemon_lock::{DaemonLock, check_daemon_not_running, is_daemon_running};
pub use events::{PositionChangeReason, TapeEvent};
pub use legal_hold::find_drive_for_loaded_cartridge;
pub use library::{
    DriveInfo, LEGACY_CHASSIS_SERIAL, Library, LibraryFacade, LibraryPartition, LoadedCartridge,
    MAX_CHASSIS_SERIAL_LEN, MAX_DRIVE_MFG_SERIAL_LEN, MailSlotInfo, SlotInfo, SlotRange,
    TapeDeviceFacade, default_firmware_for_lto, generate_chassis_serial, generate_drive_mfg_serial,
    partition_serial_suffix, validate_chassis_serial, validate_firmware, validate_partitions,
};
pub use shared_telemetry::{
    Metrics, OtlpExporterConfig, OtlpProtocol, Telemetry, TelemetryConfig, TelemetryError,
};
pub use tiering::{
    CartridgeFacts, PlannedMove, PlannedMoveReport, SkippedCartridge, TieringConfig,
    TieringPlanReport, TieringPolicy, TieringPredicates, plan_moves, validate_policies,
};
// `DriveTopology` moved to `core-stream` (the trait is drive-shaped, not
// library-shaped — `core-mediachanger` keeps it as a flat re-export so existing
// `core_mediachanger::DriveTopology` callers compile unchanged).
pub use core_stream::DriveTopology;
pub use core_stream::disk_cache::{DiskCacheManager, refresh_pool_budget_from_tapes};
// `PoolBudget` + `BackpressureError` were lifted into `shared-pool` and
// are re-exported through `core_stream`; keep the flat
// `core_mediachanger::PoolBudget` surface so VTL daemon call sites compile
// unchanged.
pub use core_stream::drive_state::{DriveState, LibraryDriveState};
pub use core_stream::encryption::{
    ALGORITHM_CODE_AES_256_GCM, ALGORITHM_INDEX_AES_256_GCM, DecryptionMode, DriveEncryptionState,
    EncryptionMode, IV_LEN, KEY_LEN, KeyScope, TAG_LEN,
};
pub use core_stream::errors::{Result, SmcError};
pub use core_stream::fastcdc::{
    DEFAULT_AVG_SIZE as FASTCDC_DEFAULT_AVG, DEFAULT_MAX_SIZE as FASTCDC_DEFAULT_MAX,
    DEFAULT_MIN_SIZE as FASTCDC_DEFAULT_MIN,
};
pub use core_stream::legal_hold::{
    CartridgeKeys, HoldRunReport, PerKeyOutcome, apply_cartridge_legal_hold,
    apply_legal_hold_to_keys, collect_cartridge_keys, manifest_latest_sentinel_key,
    read_cartridge_held, read_legal_hold_for_keys,
};
pub use core_stream::mode_state::{DrivePageStore, SavedDrivePage};
pub use core_stream::prefetch::{ChunkLocationInfo, PrefetchConfig, PrefetchManager};
pub use core_stream::tape::{Block, BlockKind, Filemark};
pub use core_stream::{BackpressureError, DiskCacheBounds, DiskCacheSize, GhostList, PoolBudget};
