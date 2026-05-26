// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Cloud-tiering surface (S3 chunk upload, manifest backup/restore,
// version retention) lives in cartridge/cloud.rs to keep this file
// scoped to the on-disk write/read state machine.
mod cloud;

// Chunk-seal pipeline (new_chunk, seal_current_chunk,
// roll_chunk_if_needed, seal_and_start_new_chunk,
// maybe_cdc_seal_after_write, flush_and_seal) lives in
// cartridge/chunking.rs so the on-disk write/read state machine in
// this file isn't tangled up with seal-side bookkeeping.
mod chunking;

// Block-index + chunk-index helpers (active_block_index, next_lba_of,
// active_next_lba, block_at, try_block_at, block_at_active,
// encode_block_rec, read_chunk_rec, update_chunk_rec) live in
// cartridge/indexing.rs. Read-state-machine helpers
// (maybe_decompress, maybe_decrypt, open_chunk_for_read) stay here
// pending a future reading.rs extraction.
mod indexing;

// Per-cartridge runtime sidecar (`<root>/runtime.json`): partition
// layout, lifetime byte counters, index-backup epoch map. Split out of
// the manifest in Commit 2 of plan-for-the-change-compiled-sutherland.md
// so manifest.json is creation-frozen.
mod runtime;
use runtime::Runtime;

use crate::block_index::{BlockIndexFile, BlockRec, EncryptionTag, derive_iv};
use crate::chunk_index::{ChunkIndexFile, ChunkRec, LocationTag};
use crate::chunk_store::ChunkStore;
use crate::encryption::{self, DriveEncryptionState};
use crate::errors::{Result, SmcError};
use crate::fastcdc::{self, FastCdc, StreamingChunker};
use crate::lru_index::LruIndexFile;
use crate::prefetch::{ChunkLocationInfo, PrefetchManager};
use crate::tape::{Block, BlockKind};
use blake3;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use shared_object_store::ObjectStoreBackend;
use shared_object_store::compression::{self, CompressionAlgo, DriveCompressionState};
use shared_pool::PoolBudget;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Max size of a chunk file before rolling to the next (128 MiB).
const CHUNK_ROLL_BYTES: u64 = 128 * 1024 * 1024;

/// Maximum number of partitions on an LTO tape (LTO-5+ supports 2 under LTFS).
pub const MAX_PARTITIONS: u8 = 2;

/// Default cartridge capacity in GB for a given LTO generation.
///
/// Supports LTO-7 (6 TB) and LTO-8 (12 TB) — the SPC-4 / SSC-4 /
/// SAM-5 conformance target. Returns 0 for anything outside 7..=8
/// (caller should validate before reaching here).
pub fn lto_default_capacity_gb(lto_generation: u8) -> u64 {
    match lto_generation {
        7 => 6000,
        8 => 12000,
        _ => 0,
    }
}

/// Options controlling [`Cartridge::open_with`] / [`Cartridge::open_async_with`].
///
/// Defaults: no cloud backend, default chunk size (`CHUNK_ROLL_BYTES`),
/// unlimited capacity, no LTO generation. Builder methods
/// (`with_cloud`, `with_chunk_size`, `with_capacity_gb`,
/// `with_lto_generation`) tweak individual fields.
///
/// `lto_generation` and `capacity_gb` interact: when `lto_generation`
/// is non-zero, the capacity is derived from the LTO table at open
/// time and overrides any explicit `capacity_gb`. Set
/// `with_lto_generation(0)` (the default) to use `capacity_gb`
/// directly.
#[derive(Default)]
pub struct CartridgeOpenOptions {
    cloud_backend: Option<Box<dyn ObjectStoreBackend>>,
    chunk_size_bytes: Option<u64>,
    capacity_gb: u64,
    lto_generation: u8,
    /// At-rest encryption parameters for `Create` mode: the cartridge
    /// UUID the daemon already wrapped against, the manifest metadata
    /// to stamp, and the plaintext DEK to retain in-memory for the
    /// encrypt seam. Ignored on `Open`.
    at_rest_create: Option<AtRestCreateParams>,
    /// Plaintext DEK injected for `Open` mode. Required when the
    /// manifest carries `encryption: Some(...)`; the daemon must
    /// unwrap via the named keystore backend before calling open.
    /// `None` is fine for unencrypted cartridges and for callers that
    /// only manipulate pool bytes by hash (upload worker, GC) and
    /// never touch `write_data` / `read_block`.
    at_rest_open_dek: Option<[u8; shared_crypto::KEY_LEN]>,
    /// Mark this open as a non-owning "view" handle. The upload worker
    /// (and any other caller that opens a cartridge while a primary
    /// drive-side handle is loaded) reads chunk metadata and applies
    /// upload outcomes, but must not run drop-time cleanup against the
    /// trailing staging chunk — the drive owns that. With this set,
    /// `Cartridge::drop` skips `flush_and_seal` and `runtime.persist`.
    /// The drive-side handle's drop still owns those.
    view_only: bool,
}

/// At-rest encryption parameters supplied by the daemon at create
/// time. The daemon mints the cartridge UUID, asks the chosen
/// keystore backend to `generate_and_wrap(uuid)`, and passes the
/// resulting plaintext DEK plus the manifest metadata in via
/// [`CartridgeOpenOptions::with_at_rest_for_create`]. The cartridge
/// stamps the metadata into `manifest.encryption` and retains the
/// plaintext DEK in-memory for the encrypt seam.
#[derive(Clone)]
pub struct AtRestCreateParams {
    pub uuid: [u8; 16],
    pub meta: CartridgeEncryptionMeta,
    pub plain_dek: [u8; shared_crypto::KEY_LEN],
}

impl CartridgeOpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cloud(mut self, cloud_backend: Option<Box<dyn ObjectStoreBackend>>) -> Self {
        self.cloud_backend = cloud_backend;
        self
    }

    /// Set the per-chunk roll-over size in bytes. `0` means "unlimited"
    /// (single chunk per cartridge). Leave unset to use the default
    /// `CHUNK_ROLL_BYTES` (128 MiB).
    pub fn with_chunk_size(mut self, chunk_size_bytes: u64) -> Self {
        self.chunk_size_bytes = Some(chunk_size_bytes);
        self
    }

    /// Set the cartridge capacity in GB. `0` means unlimited. Ignored
    /// when `lto_generation > 0` (the LTO table wins).
    pub fn with_capacity_gb(mut self, capacity_gb: u64) -> Self {
        self.capacity_gb = capacity_gb;
        self
    }

    /// Set the LTO generation. Non-zero values override
    /// `with_capacity_gb` — capacity is looked up in
    /// [`lto_default_capacity_gb`].
    pub fn with_lto_generation(mut self, lto_generation: u8) -> Self {
        self.lto_generation = lto_generation;
        self
    }

    /// Attach at-rest encryption parameters for `Create` mode. The
    /// daemon supplies the pre-generated UUID (already used as the
    /// keystore wrap context), the manifest metadata (algorithm +
    /// keystore backend name + wrapped DEK), and the plaintext DEK
    /// (kept in-memory only). Ignored on `Open`.
    pub fn with_at_rest_for_create(mut self, params: AtRestCreateParams) -> Self {
        self.at_rest_create = Some(params);
        self
    }

    /// Inject a plaintext DEK for `Open` mode. Required when the
    /// manifest carries `encryption: Some(...)`. The daemon must
    /// unwrap the manifest's wrapped DEK via the named keystore
    /// backend before calling open.
    pub fn with_dek_for_open(mut self, dek: [u8; shared_crypto::KEY_LEN]) -> Self {
        self.at_rest_open_dek = Some(dek);
        self
    }

    /// Mark this open as a non-owning "view" handle (upload worker, GC,
    /// any out-of-band reader). The resulting [`Cartridge`] reads chunk
    /// metadata and may apply upload outcomes via
    /// `apply_chunk_upload_outcome`, but its `Drop` skips
    /// `flush_and_seal` and `runtime.persist` so it never deletes the
    /// trailing staging chunk the drive-side primary handle is using.
    /// See issue #28 for the data-loss path this guards against.
    pub fn with_view_only(mut self) -> Self {
        self.view_only = true;
        self
    }

    /// Resolve the effective `chunk_size_bytes` and `capacity_gb`. Used
    /// internally by `open_with` / `open_async_with` to fold the
    /// `lto_generation` override and the `chunk_size_bytes` default
    /// into a single `Self` whose fields are ready to write into a
    /// fresh manifest.
    fn resolve_capacity(mut self) -> Self {
        if self.lto_generation > 0 {
            self.capacity_gb = lto_default_capacity_gb(self.lto_generation);
        }
        if self.chunk_size_bytes.is_none() {
            self.chunk_size_bytes = Some(CHUNK_ROLL_BYTES);
        }
        self
    }
}

#[derive(Debug, Clone)]
pub enum CartridgeOpenMode {
    /// Create a fresh cartridge bound to the named cloud backend.
    /// The backend name, WORM flag, and dedup scope are sticky for
    /// the cartridge's lifetime. `worm: true` enforces append-only
    /// semantics — see [`Manifest::worm`] and the SCSI WORM sense
    /// codes for details. `dedup` selects the *scope* of content-
    /// addressed dedup: `Global` uses the shared per-backend pool
    /// (cross-cartridge dedup, the headline storage feature);
    /// `Local` namespaces every chunk under the cartridge's barcode
    /// so chunks are isolated per-cartridge (compliance separation,
    /// per-cartridge cleanup). Both modes still content-address
    /// chunks by BLAKE3 — only the scope of sharing differs.
    Create {
        backend: String,
        worm: bool,
        dedup: DedupScope,
    },
    /// Open an existing cartridge. Backend, WORM flag, and dedup
    /// scope are read from the manifest.
    Open,
}

/// Scope of content-addressed dedup for a cartridge. Sticky once the
/// cartridge is created. Re-exported from `shared_object_store::DedupScope`
/// so the upload pipeline (`shared-upload-worker`) carries the same
/// enum across the boundary. The `namespace(label)` method on the
/// shared enum is what callers reach for to compute per-cartridge
/// vs shared-pool routing — the legacy `cartridge_namespace` alias
/// was removed alongside the lift.
pub use shared_object_store::DedupScope;

/// Creation-frozen identity for one cartridge. Persisted at
/// `<root>/manifest.json`; written once at `cartridge create` and
/// never rewritten by the daemon's hot path. Operator-driven
/// identity mutations (today: `cartridge_migrate::rewrite_manifest_backend`,
/// archive provenance stamping) are the only writers post-create.
///
/// Runtime-mutated state — partition layout, lifetime byte counters,
/// index-backup epoch, host-set capacity proportion — lives in the
/// sibling [`Runtime`] sidecar at `<root>/runtime.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    label: String,
    /// 16-byte cartridge identifier, sticky for the cartridge's
    /// lifetime. Generated at create time, mixed into per-block IV
    /// derivation so two cartridges loaded with the same encryption
    /// key never share an IV. Real LTO drives use a per-tape nonce
    /// in their position-based IV derivation; this is the same idea.
    #[serde(with = "uuid_serde")]
    uuid: [u8; 16],

    /// Legacy fixed-size chunk size in bytes. Kept for backwards-compat
    /// with cartridges created before content-defined chunking shipped
    /// (Stage 2). For new manifests, the authoritative source is
    /// `chunking` below; this field is set to 0 on FastCDC tapes and
    /// ignored on read.
    #[serde(default = "default_manifest_chunk_size")]
    chunk_size_bytes: u64,
    /// Per-cartridge chunking strategy. `None` on legacy manifests
    /// (interpreted as `Fixed { size_bytes: chunk_size_bytes }`); set
    /// explicitly on new cartridges. Once set, it does not change — the
    /// cartridge's existing chunks reflect the original strategy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunking: Option<ChunkingMode>,
    /// Cartridge capacity in gigabytes (0 = unlimited for backward compatibility)
    #[serde(default)]
    capacity_gb: u64,
    /// LTO generation (7 or 8) - determines capacity and cartridge type
    /// 0 = unknown/legacy (for backward compatibility)
    #[serde(default)]
    lto_generation: u8,
    /// Sticky cloud backend name this cartridge is bound to. Set at
    /// create time, persisted in the manifest, never changed for the
    /// life of the cartridge. The open path rejects manifests with an
    /// empty `backend` field.
    #[serde(default)]
    backend: String,
    /// Sticky WORM (Write Once Read Many) flag. Set at create time
    /// from `cartridge create --worm`, persisted in the manifest,
    /// never changed for the cartridge's life. When true, the
    /// cartridge enforces append-only semantics: WRITE / WRITE
    /// FILEMARKS at any LBA other than the active partition's
    /// `next_lba` (EOD) is refused, and ERASE / FORMAT MEDIUM /
    /// ALLOW OVERWRITE are refused outright. Cloud-side immutability
    /// is layered on top via the bound backend's `retention_mode`
    /// (the bucket is the contract).
    #[serde(default)]
    worm: bool,
    /// Sticky dedup scope. Set at create time from
    /// `cartridge create --dedup local|global`, persisted in the
    /// manifest, never changed for the cartridge's life. See
    /// [`DedupScope`] for the layout each variant produces.
    /// Default for legacy manifests without this field is `Global`.
    #[serde(default)]
    dedup: DedupScope,

    /// Optional appliance-side at-rest encryption metadata. `None`
    /// (the default for pre-encryption manifests) means the cartridge
    /// stores plaintext chunks in the pool, exactly like before. When
    /// `Some(...)`, every chunk seal encrypts the staged bytes with
    /// the cartridge's DEK (unwrapped at open via the named keystore
    /// backend) before pool insertion; reads decrypt before returning
    /// blocks. Independent of host-driven AME (SSC-4 SECURITY
    /// PROTOCOL); when both are on, AME runs per-block and at-rest
    /// then wraps the entire chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encryption: Option<CartridgeEncryptionMeta>,
}

/// At-rest encryption algorithm for a cartridge. Today only
/// AES-256-GCM is implemented; the enum exists so future algorithms
/// drop in without changing the manifest field type.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CartridgeEncryptionAlgorithm {
    Aes256Gcm,
}

impl CartridgeEncryptionAlgorithm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aes256Gcm => "aes_256_gcm",
        }
    }
}

/// Per-cartridge appliance-side at-rest encryption metadata.
///
/// The plaintext DEK is **not** stored in this struct. For the
/// `local` keystore backend the DEK lives in
/// `<data_dir>/keys/<cartridge_uuid_hex>.key` (mode 0600) and
/// `wrapped_dek` stays `None`. For external backends (`awskms`,
/// `vault`, `azurekv`, `gcpkms`, `kmip`) the DEK is never written to
/// disk in plaintext — `wrapped_dek` carries the base64-encoded
/// ciphertext returned by the backend's `wrap` op. The wrap context
/// is the cartridge UUID so a stolen manifest can't be unwrapped
/// against any other cartridge.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CartridgeEncryptionMeta {
    pub algorithm: CartridgeEncryptionAlgorithm,

    /// Name of the keystore backend entry under `keystore.backends:`
    /// in the YAML conffile that holds (or can derive) this
    /// cartridge's DEK. Sticky for the cartridge's lifetime — move
    /// via `thurvtl cartridge key migrate --to NEW`.
    pub keystore_backend: String,

    /// Backend-returned wrapped DEK, base64-encoded. `None` for
    /// `local` (the keystore sidecar IS the storage) and `Some(...)`
    /// for `awskms` / `vault` / `azurekv` / `gcpkms` / `kmip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapped_dek: Option<String>,
}

/// Per-cartridge chunking strategy. Fixed-size is the legacy default;
/// FastCdc is what fresh cartridges get unless the user opts out. The
/// strategy is sticky — chunks already on the tape were produced by
/// whatever strategy was active at create time, so opening a cartridge
/// preserves its original chunking.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ChunkingMode {
    /// Roll the active chunk when its size reaches `size_bytes`. Cuts
    /// are not content-aware — a one-byte shift in the input stream
    /// changes every downstream chunk's hash and tanks dedup ratio.
    Fixed { size_bytes: u64 },
    /// Content-defined chunking via FastCDC. Cuts fire when the rolling
    /// Gear hash matches the appropriate mask. Cuts are clamped to the
    /// `[min, max]` range and target an average of `avg`. Dedup
    /// survives single-byte shifts — the headline reason for stage 2.
    /// Cuts in the cartridge write path are block-aligned (cuts only
    /// happen between iSCSI blocks), so for a non-tar workload the
    /// dedup ratio is somewhat below "true" CDC.
    FastCdc { min: u64, avg: u64, max: u64 },
}

impl ChunkingMode {
    /// Default FastCDC parameters tuned for backup-target VTL workloads.
    /// 1 MiB min / 8 MiB avg / 32 MiB max is the sweet spot in the
    /// FastCDC paper for backup data — small enough to dedup tar
    /// streams across single-file changes, large enough that a 12 TB
    /// LTO-8 tape doesn't generate millions of S3 objects.
    pub fn fastcdc_default() -> Self {
        ChunkingMode::FastCdc {
            min: fastcdc::DEFAULT_MIN_SIZE as u64,
            avg: fastcdc::DEFAULT_AVG_SIZE as u64,
            max: fastcdc::DEFAULT_MAX_SIZE as u64,
        }
    }

    /// Build a fixed-size chunking strategy. `size_bytes == 0` is
    /// treated as "unlimited" downstream (single chunk per cartridge).
    pub fn fixed(size_bytes: u64) -> Self {
        ChunkingMode::Fixed { size_bytes }
    }
}

/// One tape partition. Each partition has its own LBA space (LBA 0 in
/// P0 is unrelated to LBA 0 in P1). The per-block index is *not* in
/// the manifest — it lives in `<cartridge>/blocks-p<N>.idx`, owned by
/// the runtime `Cartridge::block_indexes` vec. `next_lba` is derived
/// from that file's record count.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub(super) struct Partition {
    /// Partition size in MiB as set by MODE SELECT page 0x11. 0 means
    /// "rest of tape" (the last partition typically). Used for early-warning
    /// / capacity exceeded checks; not currently enforced beyond the
    /// cartridge-wide `capacity_gb`.
    #[serde(default)]
    pub capacity_mib: u64,
    /// ALLOW OVERWRITE barrier (CDB 0x82). Volatile drive-side state —
    /// cleared on unload (cartridge drop) and never persisted to the
    /// runtime sidecar, matching real LTO semantics. When `Some(lba)`,
    /// writes at `head_lba >= lba` succeed (overwriting), and writes
    /// elsewhere follow the normal "writes erase from here on" rule.
    #[serde(default, skip)]
    pub overwrite_barrier: Option<u64>,
}

/// Inline serde for the manifest's 16-byte UUID. Stored as lowercase hex
/// in JSON.
mod uuid_serde {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(uuid: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(uuid))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(D::Error::custom)?;
        if bytes.len() != 16 {
            return Err(D::Error::custom("manifest uuid must be 16 bytes"));
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&bytes);
        Ok(out)
    }
}

/// Partition layout staged by a successful MODE SELECT page 0x11. The
/// format only takes effect when FORMAT MEDIUM (CDB 0x04) is issued with
/// FORMAT field = 0x01. Without FORMAT MEDIUM the tape layout is
/// unchanged — this matches real-LTO + LTFS `mkltfs` flow.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PendingPartitionLayout {
    /// FDP=1: format default partitioning (drive picks sizes)
    pub fdp: bool,
    /// SDP=1: select default partitioning (revert to a single partition)
    pub sdp: bool,
    /// IDP=1: initiator-defined partitioning (use partition_sizes_mib)
    pub idp: bool,
    /// Number of additional partitions (besides P0). 0 or 1 (we cap at 1).
    pub additional_partitions: u8,
    /// Partition size unit code from page 0x11 byte 8 PSUM field
    /// (0=bytes, 1=KiB, 2=MiB, 3=GiB, 4=TiB). We accept 2 (MiB).
    pub psum: u8,
    /// Per-partition sizes in the unit indicated by `psum`. The last
    /// entry is often 0xFFFF meaning "rest of tape" — we treat that
    /// (and any 0) as "use remaining capacity".
    pub partition_sizes: Vec<u64>,
}

fn default_manifest_chunk_size() -> u64 {
    128 * 1024 * 1024 // 128 MiB default for backward compatibility
}

/// Path of the staging file for an unsealed chunk inside a cartridge root.
/// Relative path: `<root>/.staging/chunk-<id>.dat`.
///
/// `pub(super)` so the sibling `chunking` module can compute staging paths
/// for the seal pipeline; open/create paths in this file remain the
/// dominant callers.
pub(super) fn staging_path(root: &Path, chunk_id: u64) -> PathBuf {
    root.join(".staging")
        .join(format!("chunk-{}.dat", chunk_id))
}

/// Build a `ChunkStore` rooted at the parent of the given tapes_dir
/// (so `<data_dir>/tapes/...` cartridges share `<data_dir>/chunks/<backend>/`).
/// Falls back to "." if tapes_dir has no parent — keeps relative-path
/// callers and tests working without forcing them to pass a store path.
fn derive_chunk_store(
    tapes_dir: &Path,
    backend_name: &str,
    cartridge_namespace: Option<&str>,
) -> Result<ChunkStore> {
    let parent = tapes_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    match cartridge_namespace {
        Some(ns) => ChunkStore::new_namespaced(&parent, backend_name, ns).map_err(Into::into),
        None => ChunkStore::new(&parent, backend_name).map_err(Into::into),
    }
}

/// In-memory shape for a block-index record. Built from a
/// `BlockRec` (read from `BlockIndexFile`) plus the block's LBA. Not
/// serialized — every block on disk lives in `blocks-p<N>.idx`.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct BlockIndex {
    lba: u64,
    chunk_id: u64,
    offset: u64,
    /// Bytes stored on disk (compressed + 16 B AES-GCM tag if encrypted;
    /// 0 for filemark).
    len: u64,
    kind: BlockKindSerde,
    /// True iff the stored bytes are AES-256-GCM ciphertext + tag. The
    /// IV is derived at read time from
    /// `derive_iv(uuid, chunk_id, offset)`.
    encrypted: bool,
    compression: Option<CompressionAlgo>,
}

impl BlockIndex {
    fn from_rec(lba: u64, rec: &BlockRec) -> Self {
        Self {
            lba,
            chunk_id: rec.chunk_id as u64,
            offset: rec.offset as u64,
            len: rec.len as u64,
            kind: match rec.kind {
                BlockKind::Data => BlockKindSerde::Data,
                BlockKind::Filemark => BlockKindSerde::Filemark,
            },
            encrypted: rec.encrypted(),
            compression: rec.compression,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
enum BlockKindSerde {
    Data,
    Filemark,
}

impl From<BlockKind> for BlockKindSerde {
    fn from(k: BlockKind) -> Self {
        match k {
            BlockKind::Data => BlockKindSerde::Data,
            BlockKind::Filemark => BlockKindSerde::Filemark,
        }
    }
}
impl From<BlockKindSerde> for BlockKind {
    fn from(k: BlockKindSerde) -> Self {
        match k {
            BlockKindSerde::Data => BlockKind::Data,
            BlockKindSerde::Filemark => BlockKind::Filemark,
        }
    }
}

/// Directory-backed cartridge with content-addressed chunk storage and
/// optional cloud tiering.
///
/// Layout:
///   <root>/manifest.json
///   <root>/.staging/chunk-<id>.dat   (the active, unsealed chunk only)
///
/// Sealed chunks live in the shared `ChunkStore` at
/// `<chunk_store_root>/chunks/<backend>/<aa>/<bb>/<full_hash>.dat`,
/// where `<aa>` and `<bb>` are the first and second 2-hex-char shards
/// of the BLAKE3 of the chunk's bytes. The same physical file is
/// referenced by every cartridge whose manifest lists that hash — that
/// is the cross-cartridge dedup hit.
pub struct Cartridge {
    root: PathBuf,
    manifest: Manifest,
    /// Per-cartridge runtime sidecar (`runtime.json`). Holds the
    /// fields the daemon mutates on the hot path: partition layout,
    /// active partition, pending format, set-capacity proportion,
    /// index-backup epoch, lifetime byte counters. `manifest` is
    /// creation-frozen; every runtime-mutating opcode writes here
    /// instead.
    runtime: Runtime,
    /// Shared content-addressed pool used for sealed chunks. All cartridges
    /// in the same data_dir share this pool; the cartridge keeps its own
    /// handle so reads, rolls, and uploads can resolve hashes without a
    /// global registry.
    chunk_store: ChunkStore,
    /// Per-cartridge chunk index file at `<root>/chunks.idx`. Replaces
    /// the inline `Vec<ChunkMeta>` that used to live in the manifest;
    /// every per-chunk mutation (mark uploaded, transition location)
    /// is now an O(1) `pwrite_at(id * 64)`.
    chunk_index: ChunkIndexFile,
    /// Per-cartridge LRU sidecar at `<root>/lru.idx`. One u64 per
    /// chunk_id, positional, mirrored 1:1 with `chunk_index`. Holds
    /// last-accessed epoch seconds — split out of `chunks.idx` so the
    /// read path's `touch` doesn't dirty cloud-replicated metadata
    /// pages. Local-only; never uploaded; rebuilt as zeros on cold
    /// start. See `lru_index.rs`.
    lru_index: LruIndexFile,
    /// Id of the active staging chunk. Equal to the index of
    /// [`cur_chunk`] inside `chunk_index` (the file is positional, so
    /// `id` is derivable from offset). Per-cartridge monotonic 0, 1, 2,
    /// …; never reused except when a brand-new size-0 chunk is rolled
    /// off the end via truncate (see `seal_current_chunk`).
    cur_chunk_id: u64,
    /// In-memory cache of the active staging chunk's record. Persisted
    /// to `chunk_index` at chunk-roll / filemark / drop boundaries —
    /// the same cadence as the manifest persist and the block-index
    /// fsync. Sealed chunks live exclusively in `chunk_index`.
    cur_chunk: ChunkRec,
    cur_file: File,
    // tape "head": next LBA to read in the *active partition* (BOT = 0)
    head_lba: u64,
    // optional cloud backend for cloud tiering (S3, GCS, etc.)
    cloud_backend: Option<Box<dyn ObjectStoreBackend>>,
    // optional prefetch manager for aggressive prefetching
    prefetch_manager: Option<Arc<PrefetchManager>>,
    /// Per-cartridge chunking strategy. Resolved at open/create time
    /// from the manifest; sticky for the cartridge's lifetime.
    chunking: ChunkingMode,
    /// Streaming FastCDC chunker — only populated for `ChunkingMode::FastCdc`.
    /// `None` for `Fixed`. The chunker's rolling-hash state is rebuilt
    /// from the trailing staging chunk on open, so block-aligned cuts
    /// continue from where the cartridge left off.
    cdc_state: Option<StreamingChunker>,
    /// Streaming BLAKE3 of the active staging chunk's on-disk bytes.
    /// `write_data` feeds every byte it writes (post-compress, post-
    /// encrypt — i.e. exactly what hits the file) into this hasher, and
    /// `seal_current_chunk` finalizes it instead of re-reading the
    /// chunk file. Reset to an empty hasher whenever the active chunk
    /// rolls. On open of a cartridge that's already mid-chunk, the
    /// hasher is primed by replaying the trailing staging file's bytes
    /// (mirrors the FastCDC replay in `build_chunking_state`).
    cur_chunk_hasher: blake3::Hasher,
    // Drive-level encryption state (LTO Application-Managed Encryption).
    // None = no key set; reads return plaintext, writes are plaintext.
    // Cleared on UNLOAD (via Drop) or by an explicit DISABLE Set Data
    // Encryption page from the host.
    encryption: Option<DriveEncryptionState>,
    // Drive-level compression state (LTO Mode Page 0x0F DCE bit).
    // Volatile drive RAM analogue: cleared on UNLOAD (via Drop) so a
    // freshly loaded cartridge starts with whatever default the daemon
    // pushes in. Toggling DCE off does not affect read decompression of
    // blocks already on the medium — that's per-block (`BlockIndex.compressed`).
    compression: DriveCompressionState,
    /// Pool budget gate: chunk-seal calls `try_reserve` here before
    /// `insert_from_path` to apply backpressure when the local pool
    /// is at its hard cap. Constructed by the daemon (one
    /// `Arc<PoolBudget>` per backend) and shared with every
    /// cartridge bound to that backend. CLI / test paths use
    /// `PoolBudget::unbounded` (no gate). The budget itself decides
    /// the deadline (`upload.backpressure_max_wait_seconds`).
    pool_budget: Arc<PoolBudget>,
    /// Maximum time `try_reserve` is allowed to block before
    /// surfacing `SmcError::Backpressured`. Mirrors
    /// `upload.backpressure_max_wait_seconds` from the daemon
    /// config; defaulted to 60 s when no daemon is wiring this up.
    backpressure_deadline: std::time::Duration,
    /// Per-partition block-index files (`<root>/blocks-p<N>.idx`).
    /// Indexed parallel to `manifest.partitions`. Open exactly once
    /// per cartridge load; written eagerly on every block / filemark.
    block_indexes: Vec<BlockIndexFile>,
    /// Volatile legal-hold flag, snapshot of the cloud sentinel
    /// (`manifests/<barcode>/manifest-latest.json` hold state) read
    /// once at drive-load time and pinned for the cartridge's
    /// in-memory lifetime. When `true`, every host write opcode
    /// (`write_data`, `write_filemark`, `erase`, `apply_format_medium`,
    /// `set_allow_overwrite`) returns `SmcError::LegalHoldViolation`
    /// → SCSI WRITE PROTECTED 0x27/0x00. Never persisted to disk; the
    /// sentinel is always re-read on the next load.
    ///
    /// `legal-hold set` / `clear` refuse against a loaded cartridge,
    /// so this snapshot stays coherent with the bucket for the load's
    /// lifetime. Out-of-band hold changes (e.g. `aws-cli put-object-legal-hold`
    /// while the cartridge is in a drive) don't affect this flag mid-load
    /// — the auto-hold-on-upload worker (which re-reads the sentinel
    /// per upload) is the safety net for cloud-side preservation in
    /// that residual race.
    legal_held: bool,
    /// Running total of sealed-chunk bytes (everything in `chunk_index`
    /// except the active staging chunk's record). Computed once at
    /// Open by walking the index, then updated incrementally when a
    /// chunk transitions from active to sealed in
    /// `seal_and_start_new_chunk`. Combined with the in-memory
    /// `cur_chunk.size`, this gives `used_capacity_bytes()` an O(1)
    /// answer instead of an O(N) chunk-index scan per WRITE.
    /// Erase / FORMAT MEDIUM / truncate_from_head don't touch
    /// `chunk_index`, so they leave this counter alone (preserves the
    /// existing semantics where orphaned-by-LBA chunks still count as
    /// used until GC reclaims them).
    sealed_bytes: u64,
    /// Volatile early-warning latch. Set the first time a successful
    /// WRITE / WRITE FILEMARKS commits at or past
    /// `early_warning_threshold_bytes()`; cleared on rewind / locate
    /// to BOM / erase / SET CAPACITY. Mirrors real LTO behavior where
    /// EW is a one-shot signal per pass to BOM. Volatile because real
    /// drives reset it on power cycle / cartridge eject.
    early_warning_reported: bool,
    /// Appliance-side at-rest DEK, unwrapped from the keystore at
    /// open time (or freshly minted at create time). In-memory only,
    /// never persisted. `None` for cartridges with
    /// `manifest.encryption == None` and for callers that only
    /// manipulate pool bytes by hash (upload worker, GC) — those
    /// paths never touch `write_data` / `read_block`. The
    /// chunking-seal seam reads this to decide whether to encrypt
    /// the staging chunk before pool insertion; the read seam reads
    /// it to decide whether to decrypt the fetched chunk.
    pub(super) at_rest_dek: Option<[u8; shared_crypto::KEY_LEN]>,
    /// Non-owning view handle (set by
    /// [`CartridgeOpenOptions::with_view_only`]). When `true`,
    /// `Drop::drop` skips `flush_and_seal` and `runtime.persist` so
    /// the upload worker / GC / out-of-band readers can't yank the
    /// trailing staging chunk out from under the drive-side primary
    /// handle that owns it (issue #28).
    is_view_handle: bool,
    /// Per-backend ghost list of recently-evicted chunk hashes. When
    /// set, the cache-miss path consults it before each backend GET
    /// and records the eviction age (now - evicted_at) into the
    /// `cache_miss_after_eviction` histogram. None for CLI / test
    /// paths; the daemon calls `set_ghost_list` after open.
    ghost_list: Option<Arc<shared_pool::GhostList>>,
}

/// Given an open manifest plus the active staging chunk, resolve the
/// effective `ChunkingMode` and (for FastCdc) build a fresh
/// `StreamingChunker` primed with the staging chunk's bytes. Legacy
/// manifests (no `chunking` field) fall back to `Fixed` derived from
/// the manifest's `chunk_size_bytes` field.
///
/// Replaying the staging bytes through the chunker matters because
/// open of a cartridge that's already mid-chunk must continue the
/// rolling hash from where the last write_data left it — otherwise the
/// next block's CDC decision would see a bogus "fresh chunk" hash and
/// emit cuts at the wrong content offsets.
/// Open the active staging file in append mode. With `O_APPEND` the
/// kernel anchors every write to end-of-file regardless of the cursor,
/// so `write_data` can drop its per-block `seek(End(0))` syscall and
/// the redundant `flush()` (cur_file is a raw `File`, never a buffered
/// writer — `flush()` is a stdlib no-op for raw `File`). Recovery
/// boundary stays at chunk-seal: `seal_current_chunk` does an explicit
/// flush before the rename into the pool.
pub(super) fn open_staging_for_append(staging: &Path) -> Result<File> {
    Ok(OpenOptions::new().read(true).append(true).open(staging)?)
}

/// Build a streaming BLAKE3 hasher primed with the trailing staging
/// chunk's bytes so the next `seal_current_chunk` produces the same
/// hash as a from-disk re-hash. For sealed or empty chunks the hasher
/// is returned empty.
fn build_cur_chunk_hasher(
    root: &Path,
    cur_chunk_id: u64,
    cur_chunk: &ChunkRec,
) -> Result<blake3::Hasher> {
    let mut hasher = blake3::Hasher::new();
    if cur_chunk.hash.is_none() && cur_chunk.size > 0 {
        let staging = staging_path(root, cur_chunk_id);
        let f = File::open(&staging)?;
        let mut reader = std::io::BufReader::with_capacity(64 * 1024, f);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    Ok(hasher)
}

fn build_chunking_state(
    root: &Path,
    m: &Manifest,
    cur_chunk_id: u64,
    cur_chunk: &ChunkRec,
) -> Result<(ChunkingMode, Option<StreamingChunker>)> {
    let mode = m.chunking.unwrap_or(ChunkingMode::Fixed {
        size_bytes: m.chunk_size_bytes,
    });
    let cdc = match mode {
        ChunkingMode::Fixed { .. } => None,
        ChunkingMode::FastCdc { min, avg, max } => {
            let chunker = FastCdc::new(min as usize, avg as usize, max as usize);
            let mut sc = StreamingChunker::new(chunker);
            // Replay the staging file's bytes through the chunker so
            // the rolling hash continues from the right state. Cuts
            // that fired in the past are already represented by sealed
            // chunks; the staging bytes haven't seen a cut yet so we
            // ignore feed()'s return value here.
            if cur_chunk.hash.is_none() && cur_chunk.size > 0 {
                let staging = staging_path(root, cur_chunk_id);
                if let Ok(bytes) = fs::read(&staging) {
                    let _ = sc.feed(&bytes);
                }
            }
            Some(sc)
        }
    };
    Ok((mode, cdc))
}

/// Migrate a legacy (pre-partition) manifest in place: if the manifest has
/// no `partitions` entry, fold the old top-level `blocks` and `next_lba`
/// fields into `partitions[0]`. Single-partition tapes round-trip
/// identically through this migration.
/// Decompress a block's bytes if `BlockIndex.compression` is set.
/// Returns plaintext. lz4-frame and zstd self-frame their content with
/// embedded length info; for encrypted blocks the GCM tag has already
/// authenticated the ciphertext before we get here. So no
/// uncompressed-size validation is needed — we trust the codec's
/// decompressed output.
fn maybe_decompress(bi: &BlockIndex, buf: Vec<u8>) -> Result<Vec<u8>> {
    let Some(algo) = bi.compression else {
        return Ok(buf);
    };
    Ok(compression::decompress_data(algo, &buf)?)
}

/// Generate a fresh 16-byte cartridge UUID. Used at create time;
/// sticky for the cartridge's life. Mixed into per-block IV
/// derivation. Sourced from the OS CSPRNG via
/// `shared_crypto::OsRng` (a re-export of `aes_gcm::aead::OsRng`) so
/// we don't pull in a separate `rand` dependency.
pub fn generate_cartridge_uuid() -> [u8; 16] {
    use shared_crypto::{OsRng, RngCore};
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    buf
}

/// Open or create one `BlockIndexFile` per partition listed in the
/// manifest. Used at cartridge open and after FORMAT MEDIUM creates a
/// new partition layout. Returns the vector indexed parallel to
/// `manifest.partitions`.
fn open_block_indexes(root: &Path, partition_count: usize) -> Result<Vec<BlockIndexFile>> {
    (0..partition_count)
        .map(|p| BlockIndexFile::open_or_create(root, p as u8))
        .collect()
}

/// Get current timestamp in seconds since UNIX epoch.
/// `pub(super)` so the sibling `chunking` module can stamp lru-index
/// rows when it allocates / touches a chunk during the seal pipeline.
pub(super) fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Cartridge {
    /// Open a cartridge with default settings (no cloud backend, default
    /// chunk size, unlimited capacity). Convenience wrapper over
    /// [`Cartridge::open_with`] with [`CartridgeOpenOptions::default`].
    pub fn open<P: AsRef<Path>>(root: P, label: &str, mode: CartridgeOpenMode) -> Result<Self> {
        Self::open_with(root, label, mode, CartridgeOpenOptions::default())
    }

    /// Open a cartridge with a cloud backend; everything else default.
    /// Convenience wrapper over [`Cartridge::open_with`].
    ///
    /// **Sync version**: if the local manifest is missing or corrupt, the
    /// open fails — cloud-side manifest restore needs an async runtime.
    /// Use [`Cartridge::open_with_cloud_async`] for the cold-bucket DR
    /// path.
    pub fn open_with_cloud<P: AsRef<Path>>(
        root: P,
        label: &str,
        mode: CartridgeOpenMode,
        cloud_backend: Option<Box<dyn ObjectStoreBackend>>,
    ) -> Result<Self> {
        Self::open_with(
            root,
            label,
            mode,
            CartridgeOpenOptions::new().with_cloud(cloud_backend),
        )
    }

    /// Open a cartridge with the full options surface (sync). For the
    /// cold-bucket DR path (cloud-side manifest restore on a missing /
    /// corrupt local manifest), use [`Cartridge::open_async_with`] —
    /// the sync path bails on missing-local-manifest because cloud
    /// restore needs an async runtime.
    pub fn open_with<P: AsRef<Path>>(
        root: P,
        label: &str,
        mode: CartridgeOpenMode,
        opts: CartridgeOpenOptions,
    ) -> Result<Self> {
        let opts = opts.resolve_capacity();
        let tapes_dir = root.as_ref();
        let root = tapes_dir.join(label);
        match mode {
            CartridgeOpenMode::Create {
                backend,
                worm,
                dedup,
            } => Self::finalize_create(
                tapes_dir,
                root,
                label,
                backend,
                worm,
                dedup,
                opts.chunk_size_bytes.unwrap_or(CHUNK_ROLL_BYTES),
                opts.capacity_gb,
                opts.lto_generation,
                opts.cloud_backend,
                opts.at_rest_create,
            ),
            CartridgeOpenMode::Open => {
                let m = Self::load_manifest_sync(&root, &opts.cloud_backend)?;
                let runtime = Runtime::load(&root)?;
                Self::finalize_open_from_manifest(
                    tapes_dir,
                    root,
                    m,
                    runtime,
                    opts.cloud_backend,
                    opts.at_rest_open_dek,
                    opts.view_only,
                )
            }
        }
    }

    /// Async variant of [`Cartridge::open_with_cloud`]. Convenience
    /// wrapper over [`Cartridge::open_async_with`].
    pub async fn open_with_cloud_async<P: AsRef<Path>>(
        root: P,
        label: &str,
        mode: CartridgeOpenMode,
        cloud_backend: Option<Box<dyn ObjectStoreBackend>>,
    ) -> Result<Self> {
        Self::open_async_with(
            root,
            label,
            mode,
            CartridgeOpenOptions::new().with_cloud(cloud_backend),
        )
        .await
    }

    /// Variant of [`Cartridge::open_with_cloud_async`] that also
    /// injects a plaintext DEK for the at-rest encrypt/decrypt seam.
    /// Required when the manifest's `encryption` field is `Some(...)`
    /// — the daemon unwraps the DEK via the named keystore backend
    /// before calling. `dek: None` is fine for unencrypted
    /// cartridges; for encrypted cartridges it opens the cartridge
    /// without data-path access (upload worker, GC).
    pub async fn open_with_cloud_and_dek_async<P: AsRef<Path>>(
        root: P,
        label: &str,
        mode: CartridgeOpenMode,
        cloud_backend: Option<Box<dyn ObjectStoreBackend>>,
        dek: Option<[u8; shared_crypto::KEY_LEN]>,
    ) -> Result<Self> {
        let mut opts = CartridgeOpenOptions::new().with_cloud(cloud_backend);
        if let Some(d) = dek {
            opts = opts.with_dek_for_open(d);
        }
        Self::open_async_with(root, label, mode, opts).await
    }

    /// Open a cartridge with the full options surface (async). Supports
    /// cold-bucket DR: if the cartridge directory is missing locally
    /// and a cloud backend is configured, the manifest + index pages
    /// are pulled from cloud before the cartridge opens.
    pub async fn open_async_with<P: AsRef<Path>>(
        root: P,
        label: &str,
        mode: CartridgeOpenMode,
        opts: CartridgeOpenOptions,
    ) -> Result<Self> {
        let opts = opts.resolve_capacity();
        let tapes_dir = root.as_ref();
        let root = tapes_dir.join(label);
        match mode {
            CartridgeOpenMode::Create {
                backend,
                worm,
                dedup,
            } => Self::finalize_create(
                tapes_dir,
                root,
                label,
                backend,
                worm,
                dedup,
                opts.chunk_size_bytes.unwrap_or(CHUNK_ROLL_BYTES),
                opts.capacity_gb,
                opts.lto_generation,
                opts.cloud_backend,
                opts.at_rest_create,
            ),
            CartridgeOpenMode::Open => {
                let (m, runtime) =
                    Self::load_manifest_async(&root, label, &opts.cloud_backend).await?;
                Self::finalize_open_from_manifest(
                    tapes_dir,
                    root,
                    m,
                    runtime,
                    opts.cloud_backend,
                    opts.at_rest_open_dek,
                    opts.view_only,
                )
            }
        }
    }

    /// Create a fresh cartridge with an explicit `ChunkingMode`. This
    /// is the entry point used by `thurvtl cartridge create
    /// --chunking ...`. The chunking strategy is sticky for the
    /// cartridge's lifetime.
    ///
    /// Create a fresh cartridge bound to the named cloud backend. The
    /// backend name and WORM flag are sticky for the cartridge's
    /// lifetime — every chunk upload, manifest backup, prefetch, and
    /// refetch routes through this backend, never any other; WORM
    /// cartridges enforce append-only semantics.
    ///
    /// `lto_generation = 0` means unlimited capacity; otherwise capacity
    /// is derived from the LTO generation table.
    ///
    /// `backend_name` should be a valid entry in `cloud.backends` (the
    /// daemon validates this at startup). Set `worm = true` for a
    /// Write-Once-Read-Many cartridge.
    pub fn create_with_chunking<P: AsRef<Path>>(
        tapes_dir: P,
        label: &str,
        chunking: ChunkingMode,
        lto_generation: u8,
        backend_name: &str,
        worm: bool,
        dedup: DedupScope,
    ) -> Result<Self> {
        Self::create_with_chunking_and_at_rest(
            tapes_dir,
            label,
            chunking,
            lto_generation,
            backend_name,
            worm,
            dedup,
            None,
        )
    }

    /// Variant of [`Cartridge::create_with_chunking`] that also stamps
    /// an at-rest encryption metadata block into the manifest. The
    /// daemon mints the UUID, wraps a fresh DEK via the chosen
    /// keystore backend, and passes the result in via
    /// [`AtRestCreateParams`]. Plaintext cartridges still call
    /// `create_with_chunking` (which threads `None` through).
    pub fn create_with_chunking_and_at_rest<P: AsRef<Path>>(
        tapes_dir: P,
        label: &str,
        chunking: ChunkingMode,
        lto_generation: u8,
        backend_name: &str,
        worm: bool,
        dedup: DedupScope,
        at_rest: Option<AtRestCreateParams>,
    ) -> Result<Self> {
        // Fixed mode: also write legacy `chunk_size_bytes` so older readers
        // (or our own legacy-fallback path) still see the right size. CDC
        // mode: zero out the legacy field — it's no longer authoritative.
        let legacy_size = match chunking {
            ChunkingMode::Fixed { size_bytes } => size_bytes,
            ChunkingMode::FastCdc { .. } => 0,
        };
        let mut opts = CartridgeOpenOptions::new()
            .with_chunk_size(legacy_size)
            .with_lto_generation(lto_generation);
        if let Some(p) = at_rest {
            opts = opts.with_at_rest_for_create(p);
        }
        let mut cart = Self::open_with(
            tapes_dir,
            label,
            CartridgeOpenMode::Create {
                backend: backend_name.to_string(),
                worm,
                dedup,
            },
            opts,
        )?;
        cart.manifest.chunking = Some(chunking);
        // Rebuild chunking-state caches now that the manifest claims a
        // specific mode. For Fixed → cdc_state stays None; for FastCdc
        // → a fresh StreamingChunker is allocated.
        let (mode, cdc_state) = build_chunking_state(
            &cart.root,
            &cart.manifest,
            cart.cur_chunk_id,
            &cart.cur_chunk,
        )?;
        cart.chunking = mode;
        cart.cdc_state = cdc_state;
        // Identity changed — rewrite manifest.json. Runtime is
        // untouched here.
        cart.persist_identity()?;
        Ok(cart)
    }

    /// Build the on-disk skeleton for a fresh cartridge: directory,
    /// initial chunk-index / lru.idx records, first staging chunk file,
    /// then the in-memory `Cartridge` struct. Shared by sync and async
    /// open paths — no I/O on the cloud here, all sync filesystem work.
    fn finalize_create(
        tapes_dir: &Path,
        root: PathBuf,
        label: &str,
        backend: String,
        worm: bool,
        dedup: DedupScope,
        chunk_size_bytes: u64,
        capacity_gb: u64,
        lto_generation: u8,
        cloud_backend: Option<Box<dyn ObjectStoreBackend>>,
        at_rest: Option<AtRestCreateParams>,
    ) -> Result<Self> {
        if root.exists() {
            return Err(SmcError::InvalidOp("cartridge already exists"));
        }
        fs::create_dir_all(&root)?;
        // Pool layout depends on dedup: shared per-backend pool when
        // global, per-cartridge namespace when local.
        let chunk_store = derive_chunk_store(tapes_dir, &backend, dedup.namespace(label))?;
        // If the daemon supplied at-rest params, the UUID it already
        // wrapped against must become the cartridge UUID — otherwise
        // the wrapped DEK would not unwrap.
        let (uuid, encryption_meta, at_rest_dek) = match at_rest {
            Some(p) => (p.uuid, Some(p.meta), Some(p.plain_dek)),
            None => (generate_cartridge_uuid(), None, None),
        };
        let m = Manifest {
            label: label.to_string(),
            uuid,
            chunk_size_bytes,
            chunking: None,
            capacity_gb,
            lto_generation,
            backend,
            worm,
            dedup,
            encryption: encryption_meta,
        };
        let runtime = Runtime::new_blank();
        let chunk_index = ChunkIndexFile::open_or_create(&root)?;
        let lru_index = LruIndexFile::open_or_create(&root)?;
        // Append the first staging chunk record; chunk ids are
        // positional (0-based, derived from offset).
        let first = Self::new_chunk(&root, 0)?;
        let cur_chunk_id = chunk_index.append(&first)?;
        lru_index.append(now_timestamp())?;
        let f = open_staging_for_append(&staging_path(&root, cur_chunk_id))?;
        let (chunking, cdc_state) = build_chunking_state(&root, &m, cur_chunk_id, &first)?;
        let block_indexes = open_block_indexes(&root, runtime.partitions.len())?;
        let mut cart = Self {
            root,
            manifest: m,
            runtime,
            chunk_store,
            chunk_index,
            lru_index,
            cur_chunk_id,
            cur_chunk: first,
            cur_file: f,
            head_lba: 0, // BOT
            cloud_backend,
            prefetch_manager: None,
            chunking,
            cdc_state,
            cur_chunk_hasher: blake3::Hasher::new(),
            encryption: None,
            compression: DriveCompressionState::default(),
            pool_budget: Arc::new(PoolBudget::unbounded(PathBuf::from("."))),
            backpressure_deadline: std::time::Duration::from_secs(60),
            block_indexes,
            legal_held: false,
            sealed_bytes: 0,
            early_warning_reported: false,
            at_rest_dek,
            is_view_handle: false,
            ghost_list: None,
        };
        // Write manifest.json first (identity), then runtime.json. Both
        // atomic; on failure of either the caller (open_with) rolls
        // back via rmdir of the cartridge root.
        cart.persist_identity()?;
        cart.persist_runtime()?;
        Ok(cart)
    }

    /// Build the in-memory `Cartridge` from an already-loaded manifest
    /// and runtime sidecar. Shared by sync and async open paths — every
    /// step here is sync filesystem I/O; the only async piece is
    /// *getting* the manifest, which the caller has already done.
    fn finalize_open_from_manifest(
        tapes_dir: &Path,
        root: PathBuf,
        m: Manifest,
        runtime: Runtime,
        cloud_backend: Option<Box<dyn ObjectStoreBackend>>,
        at_rest_dek: Option<[u8; shared_crypto::KEY_LEN]>,
        view_only: bool,
    ) -> Result<Self> {
        if m.backend.is_empty() {
            return Err(SmcError::InvalidOp(
                "manifest is missing required `backend` field — cartridge cannot be opened",
            ));
        }
        if runtime.partitions.is_empty() {
            return Err(SmcError::InvalidOp(
                "runtime sidecar has no partitions — older cartridge format no longer supported, recreate the cartridge",
            ));
        }
        let chunk_store = derive_chunk_store(tapes_dir, &m.backend, m.dedup.namespace(&m.label))?;
        let chunk_index = ChunkIndexFile::open_or_create(&root)?;
        let lru_index = LruIndexFile::open_or_create(&root)?;
        if chunk_index.next_id() == 0 {
            return Err(SmcError::InvalidOp(
                "chunks.idx is empty — cartridge has no chunks (recreate)",
            ));
        }
        let partition_count = runtime.partitions.len();
        let (cur_chunk_id, cur_chunk, cur_file) =
            Self::resume_or_create_active(&root, partition_count, &chunk_index, &lru_index)?;
        // Bring lru.idx in lockstep with chunks.idx: cold-start restore
        // (where chunks.idx came from cloud and lru.idx is freshly
        // empty) needs zero-fill up to next_id.
        lru_index.grow_to(chunk_index.next_id())?;
        let (chunking, cdc_state) = build_chunking_state(&root, &m, cur_chunk_id, &cur_chunk)?;
        let cur_chunk_hasher = build_cur_chunk_hasher(&root, cur_chunk_id, &cur_chunk)?;
        let block_indexes = open_block_indexes(&root, partition_count)?;
        let sealed_bytes = Self::initial_sealed_bytes(&chunk_index, cur_chunk_id);
        Ok(Self {
            root,
            manifest: m,
            runtime,
            chunk_store,
            chunk_index,
            lru_index,
            cur_chunk_id,
            cur_chunk,
            cur_file,
            head_lba: 0, // start at BOT of active partition
            cloud_backend,
            prefetch_manager: None,
            chunking,
            cdc_state,
            cur_chunk_hasher,
            encryption: None,
            compression: DriveCompressionState::default(),
            pool_budget: Arc::new(PoolBudget::unbounded(PathBuf::from("."))),
            backpressure_deadline: std::time::Duration::from_secs(60),
            block_indexes,
            legal_held: false,
            sealed_bytes,
            early_warning_reported: false,
            at_rest_dek,
            is_view_handle: view_only,
            ghost_list: None,
        })
    }

    /// Load a cartridge manifest from disk. Sync variant — bails if the
    /// local manifest is missing or corrupt and a cloud backend is
    /// configured (cloud restore needs the async runtime).
    fn load_manifest_sync(
        root: &Path,
        cloud_backend: &Option<Box<dyn ObjectStoreBackend>>,
    ) -> Result<Manifest> {
        if !root.exists() && cloud_backend.is_none() {
            return Err(SmcError::InvalidOp("cartridge does not exist"));
        }
        let manifest_path = root.join("manifest.json");
        match File::open(&manifest_path) {
            Ok(mf) => match serde_json::from_reader(mf) {
                Ok(manifest) => Ok(manifest),
                Err(parse_err) => {
                    tracing::warn!(
                        "Local manifest corrupt: {}, cloud restore requires async context",
                        parse_err
                    );
                    if cloud_backend.is_some() {
                        Err(SmcError::InvalidOp(
                            "Local manifest corrupt and cloud restore requires async context. Use the async open path.",
                        ))
                    } else {
                        Err(SmcError::InvalidOp(
                            "Local manifest corrupt and no cloud backend available",
                        ))
                    }
                }
            },
            Err(_) => {
                if cloud_backend.is_some() {
                    Err(SmcError::InvalidOp(
                        "Local manifest missing and cloud restore requires async context. Use the async open path.",
                    ))
                } else {
                    Err(SmcError::InvalidOp(
                        "manifest.json not found and no cloud backend available",
                    ))
                }
            }
        }
    }

    /// Load a cartridge manifest + runtime sidecar. Async variant —
    /// supports cold-bucket DR: if either local file is missing or
    /// corrupt and a cloud backend is configured, fetch the bundle
    /// from cloud and write both files to disk before returning.
    /// Restores indexes before the local manifest so a torn upload
    /// leaves the on-disk state correctly missing rather than lying
    /// about the chunks it can't yet describe.
    async fn load_manifest_async(
        root: &Path,
        label: &str,
        cloud_backend: &Option<Box<dyn ObjectStoreBackend>>,
    ) -> Result<(Manifest, Runtime)> {
        if !root.exists() && cloud_backend.is_none() {
            return Err(SmcError::InvalidOp("cartridge does not exist"));
        }
        let manifest_path = root.join("manifest.json");
        let local_manifest: Option<Manifest> = match File::open(&manifest_path) {
            Ok(mf) => match serde_json::from_reader(mf) {
                Ok(manifest) => Some(manifest),
                Err(parse_err) => {
                    tracing::warn!("Local manifest corrupt: {parse_err}, attempting cloud restore");
                    None
                }
            },
            Err(_) => None,
        };
        let local_runtime: Option<Runtime> = match File::open(Runtime::path_for(root)) {
            Ok(_) => Runtime::load(root).ok(),
            Err(_) => None,
        };
        if let (Some(m), Some(r)) = (local_manifest, local_runtime) {
            return Ok((m, r));
        }

        // Either file missing or corrupt — try cloud restore for the bundle.
        let Some(backend) = cloud_backend else {
            return Err(SmcError::InvalidOp(
                "Local manifest or runtime missing/corrupt and no cloud backend available",
            ));
        };
        tracing::info!(
            "Local manifest or runtime not usable, attempting cloud restore for {}",
            label
        );
        let (manifest_json, runtime_json) =
            Self::restore_manifest_from_cloud(label, backend.as_ref()).await?;
        fs::create_dir_all(root)?;
        // Restore the index files first — a torn restore leaves the
        // sentinel files absent rather than lying about index state.
        // The runtime sidecar carries `index_epoch` (post-split), so
        // pass the runtime JSON to the index-restore step.
        Self::restore_indexes_from_cloud(root, label, &runtime_json, backend.as_ref()).await?;
        // Write manifest.json first, then runtime.json. A crash
        // between the two leaves runtime.json absent — Runtime::load
        // refuses with a clear error and the next open path can
        // re-restore.
        let tmp = manifest_path.with_extension("json.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(manifest_json.as_bytes())?;
        f.flush()?;
        fs::rename(tmp, &manifest_path)?;
        let runtime_path = Runtime::path_for(root);
        let tmp = runtime_path.with_extension("json.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(runtime_json.as_bytes())?;
        f.flush()?;
        fs::rename(tmp, &runtime_path)?;
        tracing::info!("Restored manifest + runtime from cloud to local files");
        let m: Manifest = serde_json::from_str(&manifest_json)?;
        let r: Runtime = serde_json::from_str(&runtime_json)?;
        Ok((m, r))
    }

    /// Resume the trailing staging chunk on Open, or create a fresh one
    /// if the last chunk in `chunk_index` is already sealed (hash set).
    /// Appends a fresh staging record to `chunk_index` when the trailing
    /// chunk is sealed.
    ///
    /// On the resume path we reconcile the in-memory chunk's `size`
    /// with the block-index files' records: chunk-index appends are
    /// debounced under the chunk-roll fsync cadence, but the block-
    /// index file is durable per-write. After a crash the chunk-index
    /// record may say the active chunk has size N while the block-
    /// index file already has records summing to N+M bytes for that
    /// chunk. Trust the block-index file: walk every record, find the
    /// maximum `(offset + len)` for `cur_chunk_id`, ftruncate the
    /// staging file to that sum so any torn-write bytes past the last
    /// recorded block are dropped.
    fn resume_or_create_active(
        root: &Path,
        partition_count: usize,
        chunk_index: &ChunkIndexFile,
        lru_index: &LruIndexFile,
    ) -> Result<(u64, ChunkRec, File)> {
        let last_id = chunk_index
            .next_id()
            .checked_sub(1)
            .ok_or(SmcError::InvalidOp("chunks.idx has no chunks"))?;
        let last = chunk_index.read(last_id)?;
        if last.hash.is_none() {
            // Trailing staging chunk — reopen the staging file and
            // reconcile its size with the block-index files.
            let true_size = Self::recovered_chunk_size(root, partition_count, last_id)?;
            let staging = staging_path(root, last_id);
            // Truncate any trailing torn-write bytes so the staging
            // file contains exactly the bytes the block-index records
            // say it does.
            let cur_len = std::fs::metadata(&staging).map(|md| md.len()).unwrap_or(0);
            if cur_len > true_size {
                let f = OpenOptions::new().write(true).open(&staging)?;
                f.set_len(true_size)?;
            }
            let f = open_staging_for_append(&staging)?;
            // Sync the size into the in-memory chunk and persist it
            // back to chunk_index so subsequent reads see the
            // reconciled size.
            let mut last = last;
            last.size = true_size;
            chunk_index.overwrite(last_id, &last)?;
            return Ok((last_id, last, f));
        }
        // All chunks sealed — create a fresh staging chunk for future writes.
        let next_id = last_id + 1;
        let newc = Self::new_chunk(root, next_id)?;
        chunk_index.append(&newc)?;
        // Keep lru.idx in lockstep with chunks.idx; the caller will
        // also `grow_to` to handle any cold-start lag, but that's
        // cheap (idempotent set_len when already at target).
        lru_index.append(now_timestamp())?;
        let f = open_staging_for_append(&staging_path(root, next_id))?;
        Ok((next_id, newc, f))
    }

    /// Walk the block-index files for every partition in `m` and find
    /// the largest (offset + len) over all records whose `chunk_id`
    /// matches `chunk_id`. Returns 0 if no record references it. Used
    /// at open to reconcile a stale chunk-index size with the durable
    /// block-index state.
    ///
    /// Walks **backward** from `next_lba() - 1`. Chunks are append-only
    /// — every record for `chunk_id` is contiguous at the tail of the
    /// block-index — so we break on the first record whose chunk_id
    /// differs. Forward walking (the original implementation) was
    /// O(blocks-on-tape) per Open; backward is O(blocks-in-trailing-chunk).
    fn recovered_chunk_size(root: &Path, partition_count: usize, chunk_id: u64) -> Result<u64> {
        let mut max_end: u64 = 0;
        for p in 0..partition_count {
            let bif = BlockIndexFile::open_or_create(root, p as u8)?;
            let n = bif.next_lba();
            // Iterate from the tail until we cross out of the target
            // chunk. Records past the trailing chunk's range belong to
            // earlier chunks and can't contribute.
            let mut lba = n;
            while lba > 0 {
                lba -= 1;
                let rec = bif.read(lba)?;
                if rec.chunk_id as u64 != chunk_id {
                    break;
                }
                let end = rec.offset as u64 + rec.len as u64;
                if end > max_end {
                    max_end = end;
                }
            }
        }
        Ok(max_end)
    }

    /// Compute the initial `sealed_bytes` running counter from an
    /// already-opened `chunk_index`. Sums every record's `size`
    /// excluding the active staging chunk (`cur_chunk_id`); the
    /// active chunk's bytes are tracked live via `cur_chunk.size`.
    /// Called once per Open. After this, `used_capacity_bytes()` is
    /// O(1) until the next chunk-roll or Open.
    fn initial_sealed_bytes(chunk_index: &ChunkIndexFile, cur_chunk_id: u64) -> u64 {
        let mut total: u64 = 0;
        for entry in chunk_index.iter() {
            match entry {
                Ok((id, rec)) => {
                    if id != cur_chunk_id {
                        total = total.saturating_add(rec.size);
                    }
                }
                Err(_) => break,
            }
        }
        total
    }

    /// Persist runtime state to `<root>/runtime.json` (atomic
    /// tmp+rename). Called at every runtime-mutating boundary:
    /// LOCATE-cross-partition, MODE SELECT 0x11, FORMAT MEDIUM,
    /// ERASE, SET CAPACITY, manifest backup (which mutates
    /// `index_epoch`). The chunk-index size of the active staging
    /// chunk is flushed at the same cadence so a crash before the
    /// next chunk-roll has up-to-date metadata.
    pub fn persist_runtime(&mut self) -> Result<()> {
        // Sync the active staging chunk's running size into chunk_index
        // so a crash before the next chunk-roll has up-to-date metadata.
        // Block-index records are the authoritative recovery source for
        // the *exact* chunk size (see `recovered_chunk_size`); this
        // write is best-effort metadata.
        self.chunk_index
            .overwrite(self.cur_chunk_id, &self.cur_chunk)?;
        self.runtime.persist(&self.root)
    }

    /// Rewrite `manifest.json` (identity only). Used at
    /// `cartridge create` time and by the legacy
    /// `create_with_chunking` path which mutates `Manifest::chunking`
    /// after construction. The daemon's hot path never calls this;
    /// post-create the manifest is creation-frozen.
    pub fn persist_identity(&mut self) -> Result<()> {
        let tmp = self.root.join("manifest.json.tmp");
        let finalp = self.root.join("manifest.json");
        let mut f = std::io::BufWriter::with_capacity(64 * 1024, File::create(&tmp)?);
        serde_json::to_writer(&mut f, &self.manifest)?;
        f.flush()?;
        fs::rename(tmp, finalp)?;
        Ok(())
    }

    /// Append a DATA block. Returns its LBA.
    /// If S3 backend is configured, marks chunk for upload (happens async in background).
    /// Returns error if cartridge capacity would be exceeded.
    ///
    /// Real-tape semantics: a write happens at the current head position, and
    /// everything from the head onward is erased. So if the head was rewound
    /// or LOCATEd into the middle of existing data, that data must be
    /// truncated before the new block is appended. The ALLOW OVERWRITE
    /// barrier (CDB 0x82) suppresses the truncate when the head is past
    /// the barrier — that's how LTFS appends index records to P0 without
    /// stomping the historical XML chain.
    pub fn write_data(&mut self, bytes: Bytes) -> Result<u64> {
        // Legal-hold + WORM gate. WORM is append-only: the head must
        // be at active-partition EOD or the write is refused.
        self.require_writable(true)?;

        // Capacity is checked against the plaintext length — that's what
        // counts against the tape's labeled capacity, regardless of
        // whether the drive then encrypts (and adds a 16-byte tag) or not.
        // Effective capacity honors any host-set SET CAPACITY proportion;
        // unlimited cartridges (`capacity_gb == 0`) skip the gate entirely.
        let plaintext_len = bytes.len() as u64;
        if let Some(effective) = self.effective_capacity_bytes() {
            let current_used = self.used_capacity_bytes();
            if current_used.saturating_add(plaintext_len) > effective {
                return Err(SmcError::EndOfMedium);
            }
        }

        self.truncate_from_head()?;

        // Drive pipeline order: compress, then encrypt. Real LTO drives do
        // the same — encrypted (high-entropy) data does not compress, so
        // compression has to run first if both are enabled. Per-block
        // GCM IV is derived from the block's recorded position
        // (cartridge_uuid, chunk_id, offset) — see
        // `block_index::derive_iv`. We need to know the chunk_id and
        // offset *before* encrypting, which means roll_chunk_if_needed
        // happens before the encrypt call below. on_disk_len isn't
        // known until after compression+encryption, but we have a
        // worst-case upper bound (plaintext_len + 16 B tag) which is
        // good enough to seal-roll on.
        //
        // Compression and encryption each allocate a new buffer when
        // they run. When neither runs (the common
        // host-driven-compression-and-encryption-off path) we serve
        // the host bytes through to disk by reference, with no extra
        // copy — the `Bytes` argument is itself refcounted, so this
        // path stays zero-allocation per write.
        let compressed_owned: Option<Vec<u8>> = if self.compression.compress_on_write() {
            let algo = self.compression.algorithm;
            Some(compression::compress_data(
                algo,
                &bytes,
                self.compression.level,
            )?)
        } else {
            None
        };
        let applied_compression = if compressed_owned.is_some() {
            Some(self.compression.algorithm)
        } else {
            None
        };
        let after_compress: &[u8] = compressed_owned.as_deref().unwrap_or(&bytes);

        // Worst-case on-disk length for seal decision.
        let max_disk_len = after_compress.len() as u64 + crate::encryption::TAG_LEN as u64;
        self.roll_chunk_if_needed(max_disk_len)?;

        let chunk_id = self.cur_chunk_id;
        let offset = self.cur_chunk.size;

        // If the drive has a key set, encrypt the (possibly compressed) bytes.
        // The IV is derived from (cartridge_uuid, chunk_id, offset) so the
        // read path can reconstruct it without storing per-block IVs.
        let encrypted_owned: Option<Vec<u8>> = match self.encryption.as_ref() {
            Some(state) if state.encrypt_on_write() => {
                let iv = derive_iv(&self.manifest.uuid, chunk_id, offset);
                Some(encryption::encrypt_block(&state.key, &iv, after_compress)?)
            }
            _ => None,
        };
        let encrypted = encrypted_owned.is_some();
        let write_bytes: &[u8] = encrypted_owned.as_deref().unwrap_or(after_compress);
        let on_disk_len = write_bytes.len() as u64;
        // cur_file is opened with O_APPEND, so the kernel anchors every
        // write to end-of-file regardless of cursor — no per-block
        // seek needed, and `flush()` on a raw File is a stdlib no-op.
        // Recovery boundary stays at chunk-seal (see seal_current_chunk).
        self.cur_file.write_all(write_bytes)?;
        // Streaming BLAKE3 of the on-disk bytes — feed exactly what
        // hit the file. Finalized in `seal_current_chunk` instead of
        // re-reading the chunk from disk for a second hash pass.
        self.cur_chunk_hasher.update(write_bytes);
        self.cur_chunk.size += on_disk_len;

        // Lifetime host-byte counter: front-end bytes the host
        // shoved at the VTL, counted before any drive-side compression
        // / encryption. Monotonic for the cartridge's life; persisted
        // with the next manifest fsync.
        self.runtime.host_bytes_written = self
            .runtime
            .host_bytes_written
            .saturating_add(plaintext_len);

        let part_idx = self.runtime.active_partition as usize;
        let rec = Self::encode_block_rec(
            chunk_id,
            offset,
            on_disk_len,
            BlockKindSerde::Data,
            encrypted,
            applied_compression,
        )?;
        let lba = self.block_indexes[part_idx].append(&rec)?;
        self.head_lba = lba + 1;

        self.lru_index.touch(self.cur_chunk_id, now_timestamp())?;
        if self.cloud_backend.is_some() {
            self.cur_chunk.location = LocationTag::LocalOnly;
            self.cur_chunk.uploaded = false;
        }

        // Per-block manifest fsync was O(N²) — every write reserialized
        // a manifest growing one BlockIndex per write. Persistence is
        // now driven at chunk-roll granularity (see
        // `seal_and_start_new_chunk` / `maybe_cdc_seal_after_write`)
        // and on cartridge flush/drop. Recovery boundary is the chunk
        // seal — same as a real LTO drive.
        //
        // FastCDC: feed the just-written ciphertext (or plaintext for
        // unencrypted writes) through the streaming chunker. The block
        // already lives in the current chunk; if the chunker fires a
        // cut, we seal the chunk *now* so the next block lands in a
        // fresh staging chunk. For Fixed mode this is a no-op.
        self.maybe_cdc_seal_after_write(write_bytes)?;

        // Early-warning latch. The write committed; now check whether
        // we just crossed the 95% threshold. EW is sticky-once-per-pass
        // until rewind / locate-to-BOM / erase / SET CAPACITY clears
        // it. Returning Err(EarlyWarning) here is intentional — it
        // surfaces a CHECK CONDITION + NoSense sense whose EOM bit
        // tells the host "data committed, but please prepare to
        // unload." Real LTO drives signal the same way.
        if self.maybe_raise_early_warning() {
            return Err(SmcError::EarlyWarning);
        }
        Ok(lba)
    }

    /// Raise the early-warning latch if the cartridge has crossed the
    /// 95% threshold and the latch is not already set. Returns
    /// `true` if the caller should surface `EarlyWarning` to the host
    /// (i.e. the latch transitioned from off to on on this call);
    /// `false` if no signaling is needed (unlimited cartridge,
    /// threshold not yet reached, or already reported once).
    fn maybe_raise_early_warning(&mut self) -> bool {
        if self.early_warning_reported {
            return false;
        }
        let Some(threshold) = self.early_warning_threshold_bytes() else {
            return false;
        };
        if self.used_capacity_bytes() >= threshold {
            self.early_warning_reported = true;
            true
        } else {
            false
        }
    }

    /// Append a FILEMARK. Returns its LBA.
    ///
    /// Same real-tape semantics as `write_data`: writing a filemark in the
    /// middle of existing data truncates everything from the head position
    /// onward.
    pub fn write_filemark(&mut self) -> Result<u64> {
        // Legal-hold + WORM gate. Filemarks count as writes, and
        // WORM treats them like data writes — append-only, EOD-only.
        self.require_writable(true)?;
        // Effective end-of-medium gate (host-set SET CAPACITY honored).
        // Filemarks are zero-byte from a tape-data perspective, but
        // real LTO drives still refuse them past EOM — keep parity.
        if let Some(effective) = self.effective_capacity_bytes()
            && self.used_capacity_bytes() >= effective
        {
            return Err(SmcError::EndOfMedium);
        }
        self.truncate_from_head()?;
        let part_idx = self.runtime.active_partition as usize;
        let rec = Self::encode_block_rec(
            self.cur_chunk_id,
            self.cur_chunk.size, // arbitrary; no bytes written
            0,
            BlockKindSerde::Filemark,
            false,
            None,
        )?;
        let lba = self.block_indexes[part_idx].append(&rec)?;
        self.head_lba = lba + 1;
        // Filemarks are application-level boundaries (`tar` and other
        // backup software flush them between files / streams), so
        // fsync block + chunk indexes here. No `persist_manifest()`:
        // filemark writes don't mutate any serialized manifest field.
        self.block_indexes[part_idx].fsync()?;
        self.chunk_index.fsync()?;
        // Early-warning surfaces the same way as in write_data — the
        // filemark commits, then EW fires once if the threshold has
        // been reached and the latch was not yet raised.
        if self.maybe_raise_early_warning() {
            return Err(SmcError::EarlyWarning);
        }
        Ok(lba)
    }

    /// Truncate the active partition at the current head position. Used by
    /// write_data and write_filemark to emulate real-tape "writes erase from
    /// here on" semantics: a real LTO drive's ALLOW OVERWRITE (CDB 0x82) only
    /// *permits* a write-in-the-middle to succeed (rather than returning
    /// CHECK CONDITION) — the data past the write head is still physically
    /// lost. Thur VTL already permits writes-in-the-middle unconditionally,
    /// so the barrier is recorded for inspection but does not change the
    /// truncate behavior.
    ///
    /// Note: we currently leave the underlying chunk file bytes in place.
    /// Discarded blocks are no longer reachable via the manifest, so they're
    /// effectively dead data; a future compaction pass can reclaim the space.
    fn truncate_from_head(&mut self) -> Result<()> {
        let part_idx = self.runtime.active_partition as usize;
        let cur_next = self.block_indexes[part_idx].next_lba();
        if self.head_lba >= cur_next {
            // Already at EOD; nothing to truncate.
            return Ok(());
        }
        // ALLOW OVERWRITE (CDB 0x82) sets a per-partition `overwrite_barrier`
        // on the active partition. While head_lba is at or past the
        // barrier, this write is host-permitted to overwrite individual
        // blocks without the usual "writes erase from here on" semantics
        // — needed for LTFS append in P0 (the index partition), where
        // the host rewrites the index block in place. Without this gate,
        // every LTFS index update truncated the rest of P0.
        let part = &self.runtime.partitions[part_idx];
        if let Some(barrier) = part.overwrite_barrier
            && self.head_lba >= barrier
        {
            return Ok(());
        }
        self.block_indexes[part_idx].truncate_to(self.head_lba)?;
        Ok(())
    }

    /// Read a block by LBA from the active partition (sync version - assumes
    /// chunk is local). For cloud-backed cartridges, use read_block_async instead.
    pub fn read_block(&mut self, lba: u64) -> Result<Block> {
        let bi = self.block_at_active(lba)?;

        let kind: BlockKind = bi.kind.into();
        if kind == BlockKind::Filemark {
            return Ok(Block {
                kind,
                data: Bytes::new(),
                lba,
            });
        }

        let chunk_id = bi.chunk_id;
        let chunk = self.read_chunk_rec(chunk_id)?;
        self.lru_index.touch(chunk_id, now_timestamp())?;

        let buf = self.read_chunk_slice(chunk_id, &chunk, bi.offset, bi.len as usize)?;

        let after_decrypt = self.maybe_decrypt(&bi, buf)?;
        let plaintext = maybe_decompress(&bi, after_decrypt)?;
        // Lifetime read counter — plaintext bytes handed back to the
        // host, the read-side mirror of `host_bytes_written`. Filemark
        // reads returned early above, so they never count here.
        self.runtime.host_bytes_read = self
            .runtime
            .host_bytes_read
            .saturating_add(plaintext.len() as u64);
        Ok(Block {
            kind,
            data: Bytes::from(plaintext),
            lba,
        })
    }

    /// Peek the chunk-index entry for an LBA without mutating any state.
    /// Returns the chunk id, hash (if sealed), and the local pool path
    /// the iSCSI READ path would resolve to. Used by the daemon's
    /// out-of-band cloud-prefetch hook so it can refetch a missing
    /// chunk *before* re-entering `read_block` (sync) — the SCSI READ
    /// opcode handler runs `read_next` which has no async surface for
    /// a cloud round-trip itself.
    ///
    /// Returns `None` if the LBA is out of range, the chunk metadata
    /// is missing, or the chunk is still in staging (no hash yet).
    /// Filemark blocks are also reported as `None` since they have no
    /// chunk bytes on disk.
    pub fn peek_chunk_for_lba(&self, lba: u64) -> Option<NextReadChunk> {
        let bi = self.try_block_at(self.runtime.active_partition, lba)?;
        let kind: BlockKind = bi.kind.into();
        if kind == BlockKind::Filemark {
            return None;
        }
        let chunk = self.chunk_index.read(bi.chunk_id).ok()?;
        let hash = chunk.hash?;
        Some(NextReadChunk {
            chunk_id: bi.chunk_id,
            hash: hash.clone(),
            store_path: self.chunk_store.store_path(&hash),
            object_key: self.chunk_store.object_key_in_store(&hash),
            backend_name: self.manifest.backend.clone(),
        })
    }

    /// Resolve a `ChunkRec` to a readable `File` handle. Sealed chunks
    /// (`hash` is `Some`) live in the shared `ChunkStore`; the active
    /// staging chunk (`hash` is `None`) lives at `<root>/.staging/chunk-<id>.dat`.
    fn open_chunk_for_read(&self, chunk_id: u64, chunk: &ChunkRec) -> Result<File> {
        match chunk.hash.as_deref() {
            Some(h) => self.chunk_store.open_read(h).map_err(Into::into),
            None => Ok(OpenOptions::new()
                .read(true)
                .open(staging_path(&self.root, chunk_id))?),
        }
    }

    /// Read a slice of one chunk's plaintext bytes. The at-rest seam
    /// lives here: when the cartridge carries a DEK, sealed chunks
    /// are stored as one AES-256-GCM envelope per chunk. AES-GCM has
    /// no random-access mode — we must authenticate the entire
    /// chunk before serving any byte — so this helper reads the
    /// whole chunk, decrypts it, and then slices.
    ///
    /// The unsealed staging chunk (`hash == None`) is never
    /// encrypted on disk: writes land plaintext into staging, and
    /// the seal step is what wraps the file. Reads from the
    /// staging chunk therefore skip the decrypt seam regardless of
    /// `at_rest_dek`.
    ///
    /// For unencrypted cartridges (the common case today) this
    /// falls back to the original seek-into-File path with no extra
    /// allocation.
    fn read_chunk_slice(
        &self,
        chunk_id: u64,
        chunk: &ChunkRec,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        match (self.at_rest_dek, chunk.hash.as_deref()) {
            // Sealed and encrypted: decrypt the whole chunk, slice.
            (Some(dek), Some(_)) => {
                let mut f = self.open_chunk_for_read(chunk_id, chunk)?;
                let mut ciphertext = Vec::with_capacity(chunk.size as usize + 16);
                f.read_to_end(&mut ciphertext)?;
                let iv = derive_iv(&self.manifest.uuid, chunk_id, 0);
                let plaintext = shared_crypto::decrypt_block(&dek, &iv, &ciphertext)
                    .map_err(|e| SmcError::EncryptionError(e.to_string()))?;
                let end = (offset as usize).saturating_add(len);
                if end > plaintext.len() {
                    return Err(SmcError::InvalidOp(
                        "block slice extends past decrypted chunk plaintext",
                    ));
                }
                Ok(plaintext[offset as usize..end].to_vec())
            }
            // Anything else (unsealed staging, or unencrypted): seek
            // + read_exact, zero-copy for the file body.
            _ => {
                let mut f = self.open_chunk_for_read(chunk_id, chunk)?;
                f.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; len];
                f.read_exact(&mut buf)?;
                Ok(buf)
            }
        }
    }

    /// If the block was encrypted at write time, decrypt with the drive's
    /// current key. Returns DataDecryptionError if the drive has no key
    /// or the GCM tag fails to verify (wrong key / tampering). For
    /// plaintext blocks, returns the bytes unchanged. The IV is derived
    /// from `(cartridge_uuid, chunk_id, offset)` — the same form used at
    /// encrypt time — so no per-block IV needs to be stored on disk.
    fn maybe_decrypt(&self, bi: &BlockIndex, buf: Vec<u8>) -> Result<Vec<u8>> {
        if !bi.encrypted {
            return Ok(buf);
        }
        let state = self
            .encryption
            .as_ref()
            .ok_or(SmcError::DataDecryptionError(
                "encrypted block read without a drive key set",
            ))?;
        if state.decryption_mode == crate::encryption::DecryptionMode::Disable {
            return Err(SmcError::DataDecryptionError(
                "drive decryption mode disabled but block is encrypted",
            ));
        }
        let iv = derive_iv(&self.manifest.uuid, bi.chunk_id, bi.offset);
        encryption::decrypt_block(&state.key, &iv, &buf)
    }

    /// Read a block by LBA from the active partition with cloud download
    /// if needed (async version). Handles cache misses by downloading from
    /// the configured cloud backend.
    pub async fn read_block_async(&mut self, lba: u64) -> Result<Block> {
        let bi = self.block_at_active(lba)?;

        let kind: BlockKind = bi.kind.into();
        if kind == BlockKind::Filemark {
            return Ok(Block {
                kind,
                data: Bytes::new(),
                lba,
            });
        }

        let chunk_id = bi.chunk_id;
        let mut chunk = self.read_chunk_rec(chunk_id)?;

        // Decide whether to fetch from cloud. Two cases trigger a fetch:
        //   1. chunk_index says CloudOnly — the chunk was evicted from
        //      the pool while this cartridge was alive (cache eviction
        //      worker).
        //   2. chunk_index says Both / LocalOnly but the pool file is
        //      gone — typical after a cold-start daemon faces a wiped
        //      chunks directory. Trust the cloud as the durable copy.
        let pool_has_chunk = chunk
            .hash
            .as_deref()
            .map(|h| self.chunk_store.exists(h))
            .unwrap_or(false);
        let needs_fetch =
            chunk.hash.is_some() && (chunk.location == LocationTag::CloudOnly || !pool_has_chunk);

        if needs_fetch {
            let hash = chunk
                .hash
                .as_ref()
                .expect("needs_fetch implies hash.is_some()")
                .clone();
            let backend = self.cloud_backend.as_ref().ok_or(SmcError::InvalidOp(
                "chunk missing from local pool and no cloud backend configured to refetch",
            ))?;
            let object_key = self.chunk_store.object_key_in_store(&hash);
            tracing::info!(
                "Cache miss: downloading chunk {} (hash {}..) from cloud",
                chunk_id,
                &hash[..8]
            );
            if let Some(gl) = self.ghost_list.as_ref() {
                let mut hash_bytes = [0u8; 32];
                if hex::decode_to_slice(&hash, &mut hash_bytes).is_ok() {
                    if let Some(age) = gl.lookup(&hash_bytes, now_timestamp()) {
                        shared_telemetry::record::cache_miss_after_eviction(
                            gl.backend(),
                            age as f64,
                        );
                    }
                }
            }
            let data = backend.download_chunk(&object_key).await?;

            // Hand the BLAKE3 verify + atomic tmp+rename to
            // `ChunkPool::insert_verified_bytes`. The verify catches
            // cloud bit-rot or a wrong-bytes-for-hash response (the
            // pool would otherwise store the bad bytes under the
            // expected filename); on mismatch the error flows out as
            // `SmcError::ContentHashMismatch` → SCSI MEDIUM ERROR
            // (0x03 / 0x11 / 0x00). For 128 MiB of data BLAKE3 +
            // write + fsync is on the order of ~80-150 ms, so the
            // `spawn_blocking` wrapper stays — runtime workers must
            // not park on this.
            let chunk_store = self.chunk_store.clone();
            let hash_for_blocking = hash.clone();
            // `data` moves into the blocking task; capture its length
            // first for the trailing log line.
            let data_len = data.len();
            tokio::task::spawn_blocking(move || -> Result<()> {
                chunk_store.insert_verified_bytes(&hash_for_blocking, &data)?;
                Ok(())
            })
            .await
            .map_err(|e| {
                SmcError::Io(std::io::Error::other(format!(
                    "spawn_blocking join error during chunk refetch persist: {e}"
                )))
            })??;

            chunk.location = LocationTag::Both;
            self.update_chunk_rec(chunk_id, &chunk)?;
            self.lru_index.touch(chunk_id, now_timestamp())?;

            // Lifetime backend-read counter — chunk bytes pulled down
            // on this cache miss.
            self.runtime.backend_bytes_read = self
                .runtime
                .backend_bytes_read
                .saturating_add(data_len as u64);

            tracing::info!(
                "Downloaded and cached chunk {} ({} bytes)",
                chunk_id,
                data_len
            );
        } else {
            self.lru_index.touch(chunk_id, now_timestamp())?;
        }

        let buf = self.read_chunk_slice(chunk_id, &chunk, bi.offset, bi.len as usize)?;

        let after_decrypt = self.maybe_decrypt(&bi, buf)?;
        let plaintext = maybe_decompress(&bi, after_decrypt)?;
        // Lifetime read counter — see `read_block`. Filemark reads
        // returned early above and never count here.
        self.runtime.host_bytes_read = self
            .runtime
            .host_bytes_read
            .saturating_add(plaintext.len() as u64);

        // Trigger prefetch for next chunks (non-blocking)
        self.trigger_prefetch(chunk_id).await;

        Ok(Block {
            kind,
            data: Bytes::from(plaintext),
            lba,
        })
    }

    /// Simple helpers
    pub fn next_lba(&self) -> u64 {
        self.active_next_lba()
    }
    pub fn label(&self) -> &str {
        &self.manifest.label
    }
    /// Sticky cloud backend name this cartridge is bound to. Set at
    /// create time, persisted in the manifest. The open path rejects
    /// manifests with a missing `backend` field.
    pub fn backend(&self) -> &str {
        &self.manifest.backend
    }
    /// Whether this cartridge is WORM (Write Once Read Many). Sticky
    /// for the cartridge's lifetime; set at create time. WORM
    /// cartridges refuse WRITE / WRITE FILEMARKS at LBAs other than
    /// EOD, plus ERASE / FORMAT MEDIUM / ALLOW OVERWRITE.
    pub fn worm(&self) -> bool {
        self.manifest.worm
    }
    /// Sticky 16-byte cartridge UUID, generated at create time.
    /// Mixed into per-block IV derivation (AME) and per-chunk IV
    /// derivation (at-rest); also the keystore wrap context.
    pub fn uuid(&self) -> [u8; 16] {
        self.manifest.uuid
    }
    /// At-rest encryption metadata from the manifest. `None` for
    /// cartridges created without `--keystore` and for all
    /// pre-encryption manifests. The daemon reads this at open time
    /// to decide whether to unwrap a DEK via the named keystore
    /// backend before calling `open_with`.
    pub fn manifest_encryption(&self) -> Option<&CartridgeEncryptionMeta> {
        self.manifest.encryption.as_ref()
    }
    /// Read a cartridge's manifest from disk without opening the
    /// cartridge. Used by the daemon at load time to peek at the
    /// `encryption` field (so it can unwrap the DEK via the named
    /// keystore backend before calling [`Cartridge::open_with`]),
    /// and by the CLI's `cartridge key {migrate, show}` daemon-down
    /// flows to mutate the manifest in place. Returns `(uuid,
    /// encryption_meta)` — the uuid is the keystore wrap context.
    pub fn read_manifest_identity(
        tapes_dir: &Path,
        label: &str,
    ) -> Result<([u8; 16], Option<CartridgeEncryptionMeta>)> {
        let manifest_path = tapes_dir.join(label).join("manifest.json");
        let f = File::open(&manifest_path)?;
        let m: Manifest = serde_json::from_reader(f)?;
        Ok((m.uuid, m.encryption))
    }

    /// Rewrite a cartridge's manifest in place to update the
    /// `encryption` block (used by daemon-down `cartridge key
    /// migrate`). Atomic tmp + rename; the daemon's hot path never
    /// touches `encryption` so this can run safely while a daemon is
    /// stopped. Re-emits the manifest with the new metadata; every
    /// other identity field is preserved verbatim.
    pub fn rewrite_manifest_encryption(
        tapes_dir: &Path,
        label: &str,
        new_meta: Option<CartridgeEncryptionMeta>,
    ) -> Result<()> {
        let cart_root = tapes_dir.join(label);
        let manifest_path = cart_root.join("manifest.json");
        let mut m: Manifest = {
            let f = File::open(&manifest_path)?;
            serde_json::from_reader(f)?
        };
        m.encryption = new_meta;
        let tmp = cart_root.join("manifest.json.tmp");
        let mut f = std::io::BufWriter::with_capacity(64 * 1024, File::create(&tmp)?);
        serde_json::to_writer(&mut f, &m)?;
        f.flush()?;
        fs::rename(tmp, manifest_path)?;
        Ok(())
    }
    /// Whether the cartridge's volatile legal-hold flag is on. Snapshot
    /// of the cloud sentinel taken at drive-load time. Drives the SCSI
    /// write-protect gate on the five mutating opcodes; never persisted.
    pub fn legal_held(&self) -> bool {
        self.legal_held
    }
    /// Set the volatile legal-hold flag. Called by the daemon's drive
    /// load path after reading the cloud sentinel
    /// (`manifest-latest.json`). Must be called before the cartridge
    /// is exposed to any SCSI write opcode; the CLI's `legal-hold
    /// set/clear` refuses against loaded cartridges so this snapshot
    /// stays coherent for the load's lifetime.
    pub fn set_legal_held(&mut self, held: bool) {
        self.legal_held = held;
    }

    /// Single gate for every host write opcode.
    ///
    /// * Legal-hold (`legal_held` flag, snapshot of the cloud sentinel)
    ///   refuses the operation outright with `LegalHoldViolation` →
    ///   plain WRITE PROTECTED 0x27/0x00 at the iSCSI layer.
    /// * WORM (`manifest.worm` sticky) refuses with `WormViolation` →
    ///   DATA PROTECT 0x07 + 0x30/0x0C at the iSCSI layer. The
    ///   `append_at_eod_ok` bit selects the WORM rule:
    ///     - `true`  — WRITE / WRITE FILEMARKS: allow only when the
    ///       head is at active-partition EOD.
    ///     - `false` — ERASE / FORMAT MEDIUM / ALLOW OVERWRITE:
    ///       refuse unconditionally.
    fn require_writable(&self, append_at_eod_ok: bool) -> Result<()> {
        if self.legal_held {
            return Err(SmcError::LegalHoldViolation);
        }
        if self.manifest.worm {
            if append_at_eod_ok && self.head_lba == self.active_next_lba() {
                // WORM appends at active-partition EOD are allowed.
            } else {
                return Err(SmcError::WormViolation);
            }
        }
        Ok(())
    }

    pub fn head_lba(&self) -> u64 {
        self.head_lba
    }
    pub fn current_chunk_id(&self) -> u32 {
        self.cur_chunk_id as u32
    }

    /// Currently active partition number (0..MAX_PARTITIONS). Used by
    /// READ POSITION and LTFS test plumbing.
    pub fn active_partition(&self) -> u8 {
        self.runtime.active_partition
    }

    /// Total number of partitions on this tape (1 for an unpartitioned
    /// tape, 2 after a successful FORMAT MEDIUM with page 0x11).
    pub fn partition_count(&self) -> u8 {
        self.runtime.partitions.len() as u8
    }

    /// Get cartridge capacity in gigabytes (0 = unlimited)
    pub fn capacity_gb(&self) -> u64 {
        self.manifest.capacity_gb
    }

    /// Get cartridge LTO generation (7 or 8; 0 = unknown/legacy).
    pub fn lto_generation(&self) -> u8 {
        self.manifest.lto_generation
    }

    /// Get current used capacity in bytes. O(1): returns the running
    /// `sealed_bytes` total (sum of every sealed chunk in
    /// `chunk_index` excluding the active staging slot) plus the
    /// live in-memory `cur_chunk.size`. The counter is computed
    /// once at Open and updated incrementally on chunk-roll, so
    /// per-WRITE capacity checks no longer scan the entire index.
    pub fn used_capacity_bytes(&self) -> u64 {
        self.sealed_bytes.saturating_add(self.cur_chunk.size)
    }

    /// Lifetime host bytes written into this cartridge — pre-dedup,
    /// pre-compression, pre-encryption. Monotonic for the cartridge's
    /// life; reset to 0 on ERASE.
    pub fn host_bytes_written(&self) -> u64 {
        self.runtime.host_bytes_written
    }

    /// Lifetime plaintext bytes served to the host on READ —
    /// post-decrypt, post-decompress. Monotonic for the cartridge's
    /// life; reset to 0 on ERASE / FORMAT MEDIUM.
    pub fn host_bytes_read(&self) -> u64 {
        self.runtime.host_bytes_read
    }

    /// Lifetime on-wire bytes PUT to cloud for this cartridge's
    /// chunks — post-dedup, post-compression.
    pub fn backend_bytes_written(&self) -> u64 {
        self.runtime.backend_bytes_written
    }

    /// Lifetime bytes fetched from cloud on a chunk cache miss.
    pub fn backend_bytes_read(&self) -> u64 {
        self.runtime.backend_bytes_read
    }

    /// Add `n` to the lifetime `backend_bytes_read` counter. Called
    /// by the daemon's live-session iSCSI prefetch hook, which
    /// downloads a missing chunk into the pool *outside* the sync
    /// read path and so cannot bump the counter inline the way
    /// [`Self::read_block_async`] does. The new value rides the next
    /// `runtime.json` persist (cartridge unload, or any
    /// `persist_runtime` boundary).
    pub fn bump_backend_bytes_read(&mut self, n: u64) {
        self.runtime.backend_bytes_read = self.runtime.backend_bytes_read.saturating_add(n);
    }

    /// Get remaining capacity in bytes (None if unlimited).
    /// Honors any host-set SET CAPACITY proportion.
    pub fn remaining_capacity_bytes(&self) -> Option<u64> {
        let effective = self.effective_capacity_bytes()?;
        Some(effective.saturating_sub(self.used_capacity_bytes()))
    }

    /// Effective end-of-medium in bytes after applying any host-set
    /// SET CAPACITY proportion. Returns `None` for unlimited
    /// cartridges (`capacity_gb == 0`).
    ///
    /// `proportion = u16::MAX` (or `0`, treated as full per SSC-5
    /// §7.13) means full native capacity. Otherwise the effective cap
    /// is `native * proportion / 65535`, rounded down. Used by
    /// WRITE / WRITE FILEMARKS to gate end-of-medium and by the
    /// early-warning threshold helper.
    pub fn effective_capacity_bytes(&self) -> Option<u64> {
        if self.manifest.capacity_gb == 0 {
            return None;
        }
        // Existing convention: GiB (1024^3) — kept for compatibility
        // with cartridges already on disk. LTO marketing capacities
        // are decimal, so an LTO-7 here is ~6.44 TiB raw; documented
        // in docs/SPEC.md.
        let native = self.manifest.capacity_gb.saturating_mul(1024 * 1024 * 1024);
        let p = self.runtime.set_capacity_proportion;
        if p == 0 || p == u16::MAX {
            return Some(native);
        }
        let effective = (native as u128 * p as u128 / u16::MAX as u128) as u64;
        Some(effective)
    }

    /// Bytes-written threshold past which the next successful WRITE /
    /// WRITE FILEMARKS sets the early-warning latch. Real LTO-7+
    /// drives signal EW at roughly the last 5% of native capacity;
    /// we use a fixed 95% trigger. Returns `None` for unlimited
    /// cartridges (no threshold to cross).
    pub fn early_warning_threshold_bytes(&self) -> Option<u64> {
        let effective = self.effective_capacity_bytes()?;
        Some(effective.saturating_mul(95) / 100)
    }

    /// Has the early-warning latch been raised on this load? Cleared
    /// on rewind / locate-to-BOM / erase / SET CAPACITY. Volatile.
    pub fn early_warning_reported(&self) -> bool {
        self.early_warning_reported
    }

    /// Persist a host-set SET CAPACITY proportion (CDB 0x0B bytes
    /// 2-3). Per SSC-5 §7.13 the operation is destructive: the medium
    /// is erased and the head reset to BOM. Clears the early-warning
    /// latch; the new effective capacity may move the threshold.
    /// `proportion = 0` is treated as `u16::MAX` (full native), per
    /// the spec's "0 reserved" wording.
    pub fn set_capacity_proportion(&mut self, proportion: u16) -> Result<()> {
        let stored = if proportion == 0 {
            u16::MAX
        } else {
            proportion
        };
        self.erase()?;
        self.runtime.set_capacity_proportion = stored;
        self.early_warning_reported = false;
        self.persist_runtime()?;
        Ok(())
    }

    /// The currently-applied SET CAPACITY proportion. `u16::MAX`
    /// means full native (the default for fresh cartridges and any
    /// cartridge that has never seen a SET CAPACITY).
    pub fn capacity_proportion(&self) -> u16 {
        self.runtime.set_capacity_proportion
    }

    /// Read a data block by LBA, treating a successful read as
    /// verification. Filemarks return an empty block.
    ///
    /// Real LTO drives don't expose a per-block hash to the host —
    /// drive-internal ECC + recorded-block CRC handle integrity. Our
    /// equivalent: chunk-level BLAKE3 (`ChunkMeta.hash`) checked at
    /// chunk seal and at fetch-from-cloud, plus AES-GCM auth tag on
    /// encrypted blocks and codec frame CRC on compressed blocks.
    /// `read_block` already runs decrypt + decompress, so reaching
    /// here with a valid `Block` means those checks passed.
    pub fn read_block_verify(&mut self, lba: u64) -> Result<Block> {
        self.read_block(lba)
    }

    /// Verify a block by reading it. No-op for filemarks (read_block
    /// returns an empty filemark `Block`). Returns `Err` if the read
    /// fails — matches the SCSI VERIFY semantic.
    pub fn verify_block(&mut self, lba: u64) -> Result<()> {
        self.read_block(lba).map(|_| ())
    }

    /// Scrub entire cartridge: read every data block across every
    /// partition. A successful read counts as verified (chunk hash,
    /// GCM tag, codec CRC are all checked inside read_block). Returns
    /// `(verified_ok, total_data_blocks)`.
    pub fn scrub_all(&mut self) -> Result<(u64, u64)> {
        let mut ok = 0u64;
        let mut total = 0u64;
        // Snapshot active partition; restore at end.
        let saved_active = self.runtime.active_partition;
        let saved_head = self.head_lba;
        for p in 0..self.runtime.partitions.len() {
            let next = self.next_lba_of(p as u8);
            // Snapshot (lba, kind) so we don't keep a borrow into the
            // block index file across the read_block call below.
            let mut lbas: Vec<(u64, BlockKindSerde)> = Vec::with_capacity(next as usize);
            for lba in 0..next {
                let bi = self.block_at(p as u8, lba)?;
                lbas.push((lba, bi.kind));
            }
            self.runtime.active_partition = p as u8;
            for (lba, kind) in lbas {
                if kind == BlockKindSerde::Filemark {
                    continue;
                }
                total += 1;
                // One read per block — read_block_verify and
                // verify_block both reduce to read_block, so we don't
                // need to call them in succession (was a double-read).
                match self.read_block(lba) {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        self.runtime.active_partition = saved_active;
                        self.head_lba = saved_head;
                        return Err(e);
                    }
                }
            }
        }
        self.runtime.active_partition = saved_active;
        self.head_lba = saved_head;
        Ok((ok, total))
    }

    /// Erase the entire cartridge: discard every block from every partition,
    /// reset head/next LBA to 0 in each. Used by SCSI ERASE (CDB 0x19).
    ///
    /// Like `truncate_from_head`, the underlying chunk file bytes are left in
    /// place — they're unreachable from the manifest and a future compaction
    /// pass can reclaim them.
    pub fn erase(&mut self) -> Result<()> {
        // Legal-hold + WORM gate. ERASE is destructive — WORM refuses
        // outright (nothing on the medium can be undone).
        self.require_writable(false)?;
        for part in self.runtime.partitions.iter_mut() {
            part.overwrite_barrier = None;
        }
        for bif in &self.block_indexes {
            bif.truncate_to(0)?;
        }
        self.runtime.active_partition = 0;
        self.head_lba = 0;
        // Erase clears the EW latch — the medium is empty again.
        self.early_warning_reported = false;
        // Erase blanks the medium, so the host's lifetime write
        // counter on this cartridge resets to 0.
        self.runtime.host_bytes_written = 0;
        self.persist_runtime()?;
        Ok(())
    }

    /// --- Tape-like sequential API ---
    /// Rewind to BOT of the active partition.
    pub fn rewind(&mut self) {
        self.head_lba = 0;
        // Real LTO: rewind clears the early-warning latch since the
        // host can write past it again. Note this only resets the
        // signaling latch — used_capacity_bytes() is unchanged because
        // the data is still on the medium.
        self.early_warning_reported = false;
    }

    /// Rewind to BOT of the active partition (async version with prefetch
    /// cancellation).
    pub async fn rewind_async(&mut self) {
        self.cancel_prefetches().await;
        self.head_lba = 0;
        self.early_warning_reported = false;
    }

    /// Current "next-read" block address within the active partition (like
    /// READ POSITION).
    pub fn position(&self) -> u64 {
        self.head_lba
    }

    /// Total blocks currently in the active partition (BOT..EOT exclusive).
    pub fn total_blocks(&self) -> u64 {
        self.active_next_lba()
    }

    /// Are we at or past EOD/EOT in the active partition?
    pub fn at_eod(&self) -> bool {
        self.head_lba >= self.active_next_lba()
    }

    /// LOCATE to an exact LBA in the active partition (like SSC LOCATE).
    pub fn locate(&mut self, lba: u64) -> Result<()> {
        if lba > self.active_next_lba() {
            return Err(SmcError::InvalidOp("LOCATE past EOT"));
        }
        self.head_lba = lba;
        Ok(())
    }

    /// LOCATE to an exact LBA in the active partition (async version with
    /// prefetch cancellation).
    pub async fn locate_async(&mut self, lba: u64) -> Result<()> {
        if lba > self.active_next_lba() {
            return Err(SmcError::InvalidOp("LOCATE past EOT"));
        }
        self.cancel_prefetches().await;
        self.head_lba = lba;
        Ok(())
    }

    /// LOCATE with partition select. Switches `active_partition` to
    /// `partition` and positions the head at `lba` within it. Used by
    /// SCSI LOCATE(10/16) when the CP (Change Partition) bit is set.
    /// Returns InvalidOp if `partition` is not a valid partition index.
    pub fn locate_partition(&mut self, partition: u8, lba: u64) -> Result<()> {
        if (partition as usize) >= self.runtime.partitions.len() {
            return Err(SmcError::InvalidOp("LOCATE: partition out of range"));
        }
        let prev = self.runtime.active_partition;
        // Switch partition first, then validate LBA against the new partition.
        self.runtime.active_partition = partition;
        if lba > self.active_next_lba() {
            self.runtime.active_partition = prev;
            return Err(SmcError::InvalidOp("LOCATE past EOT"));
        }
        self.head_lba = lba;
        if prev != partition {
            self.persist_runtime()?;
        }
        Ok(())
    }

    /// Async LOCATE with partition select.
    pub async fn locate_partition_async(&mut self, partition: u8, lba: u64) -> Result<()> {
        if (partition as usize) >= self.runtime.partitions.len() {
            return Err(SmcError::InvalidOp("LOCATE: partition out of range"));
        }
        self.cancel_prefetches().await;
        let prev = self.runtime.active_partition;
        self.runtime.active_partition = partition;
        if lba > self.active_next_lba() {
            self.runtime.active_partition = prev;
            return Err(SmcError::InvalidOp("LOCATE past EOT"));
        }
        self.head_lba = lba;
        if prev != partition {
            self.persist_runtime()?;
        }
        Ok(())
    }

    /// READ next block and advance head (no verify).
    ///
    /// Returns `EndOfData` when the head is past the last written block
    /// (or the cartridge is blank); the SCSI mapper turns that into
    /// SSC's BLANK CHECK / EOD-detected sense — the SSC-spec response
    /// for read-past-EOD on a tape — rather than the bogus
    /// IllegalRequest / INVALID OPERATION CODE that an `InvalidOp`
    /// here would map to.
    pub fn read_next(&mut self) -> Result<Block> {
        if self.at_eod() {
            return Err(SmcError::EndOfData);
        }
        let blk = self.read_block(self.head_lba)?;
        self.head_lba += 1;
        Ok(blk)
    }

    /// READ next block with verification and advance head.
    pub fn read_next_verify(&mut self) -> Result<Block> {
        if self.at_eod() {
            return Err(SmcError::EndOfData);
        }
        let blk = self.read_block_verify(self.head_lba)?;
        self.head_lba += 1;
        Ok(blk)
    }

    /// READ next block and advance head (async version with prefetch support).
    /// This version downloads from S3 if needed and triggers prefetching.
    pub async fn read_next_async(&mut self) -> Result<Block> {
        if self.at_eod() {
            return Err(SmcError::EndOfData);
        }
        let blk = self.read_block_async(self.head_lba).await?;
        self.head_lba += 1;
        Ok(blk)
    }

    /// READ next block with verification and advance head (async version with prefetch support).
    /// Verification is implicit in `read_block_async` (chunk-hash check
    /// at fetch, GCM tag on encrypted blocks, codec CRC on compressed).
    pub async fn read_next_verify_async(&mut self) -> Result<Block> {
        if self.at_eod() {
            return Err(SmcError::EndOfData);
        }
        let blk = self.read_block_async(self.head_lba).await?;
        self.head_lba += 1;
        Ok(blk)
    }

    /// SPACE over a number of **records** (blocks) in the active partition,
    /// forward (+) or backward (−). Returns the **actual** number of records
    /// moved.
    pub fn space_records(&mut self, delta: i64) -> i64 {
        let cur = self.head_lba as i64;
        let max = self.active_next_lba() as i64;
        let mut target = cur.saturating_add(delta);
        if target < 0 {
            target = 0;
        }
        if target > max {
            target = max;
        }
        let moved = target - cur;
        self.head_lba = target as u64;
        moved
    }

    /// SPACE over records in the active partition (async version with
    /// prefetch cancellation for large movements). Cancels prefetches if
    /// moving more than 2 blocks (likely not sequential anymore).
    pub async fn space_records_async(&mut self, delta: i64) -> i64 {
        // Cancel prefetches if moving more than 2 blocks
        if delta.abs() > 2 {
            self.cancel_prefetches().await;
        }

        let cur = self.head_lba as i64;
        let max = self.active_next_lba() as i64;
        let mut target = cur.saturating_add(delta);
        if target < 0 {
            target = 0;
        }
        if target > max {
            target = max;
        }
        let moved = target - cur;
        self.head_lba = target as u64;
        moved
    }

    /// SPACE over **filemarks** (EOFs) in the active partition, forward (+)
    /// or backward (−). Semantics: move the head to the block **after** the
    /// Nth filemark crossed (like SSC). Returns the number of filemarks
    /// actually crossed.
    pub fn space_filemarks(&mut self, n: i64) -> i64 {
        if n == 0 {
            return 0;
        }
        let part_idx = self.runtime.active_partition;
        let mut moved = 0i64;

        if n > 0 {
            // forward
            let mut lba = self.head_lba;
            let part_next = self.next_lba_of(part_idx);
            while lba < part_next && moved < n {
                if let Some(bi) = self.try_block_at(part_idx, lba)
                    && bi.kind == BlockKindSerde::Filemark
                {
                    moved += 1;
                }
                lba += 1;
            }
            // position is after last crossed filemark
            self.head_lba = lba.min(part_next);
            moved
        } else {
            // backward
            let mut lba: i64 = self.head_lba as i64 - 1; // start checking previous block
            while lba >= 0 && moved > n {
                // n is negative, e.g. -2; we count down
                if let Some(bi) = self.try_block_at(part_idx, lba as u64)
                    && bi.kind == BlockKindSerde::Filemark
                {
                    moved -= 1; // moving "one filemark backward"
                }
                lba -= 1;
            }
            // After moving backward over |moved| filemarks, position at
            // block **after** that filemark, i.e. (lba+1)+1 = lba+2;
            // clamp to [0..next_lba]. If `moved == 0` (no filemark
            // crossed — e.g. backward space at BOT), leave head_lba
            // alone: the old code computed `(lba + 2).max(0) as u64`
            // which advanced head from 0 to 1 in that case.
            if moved == 0 {
                return moved;
            }
            let part_next = self.next_lba_of(part_idx);
            let mut new_head = (lba + 2).max(0) as u64;
            if new_head > part_next {
                new_head = part_next;
            }
            self.head_lba = new_head;
            moved
        }
    }

    /// SPACE to end of data (EOD) in the active partition.
    pub fn space_to_eod(&mut self) {
        self.head_lba = self.active_next_lba();
    }

    /// Peek the kind at current head in the active partition (None if EOD).
    pub fn peek_kind(&self) -> Option<BlockKind> {
        self.try_block_at(self.runtime.active_partition, self.head_lba)
            .map(|bi| bi.kind.into())
    }

    // --- Prefetch Integration Methods ---

    /// Set the prefetch manager for this cartridge
    ///
    /// This should be called after cartridge creation to enable prefetching.
    /// The prefetch manager is shared across cartridges.
    pub fn set_prefetch_manager(&mut self, manager: Arc<PrefetchManager>) {
        self.prefetch_manager = Some(manager);
    }

    /// Wire the daemon's per-backend pool budget into this cartridge.
    /// Default (constructed via `open` / `create_with_chunking` without
    /// daemon scaffolding) is `PoolBudget::unbounded` — no gate, every
    /// reservation succeeds. The daemon overrides this at cartridge
    /// load with the real budget so chunk-seal applies upload
    /// backpressure when the local pool is at its hard cap.
    pub fn set_pool_budget(&mut self, budget: Arc<PoolBudget>, deadline: std::time::Duration) {
        self.pool_budget = budget;
        self.backpressure_deadline = deadline;
    }

    /// Wire the per-backend ghost list. The cache-miss path consults it
    /// on every backend GET to bucket eviction-to-refetch ages into the
    /// `cache_miss_after_eviction` histogram. Mirrors the
    /// `set_ghost_list` on `DiskCacheManager` — the same `Arc` flows
    /// through both so the read side reads what the eviction side
    /// wrote.
    pub fn set_ghost_list(&mut self, gl: Arc<shared_pool::GhostList>) {
        self.ghost_list = Some(gl);
    }

    /// Trigger prefetch after a read operation
    ///
    /// This is called internally after successful reads to prefetch upcoming chunks.
    /// The prefetch worker writes downloaded chunks into the shared
    /// `ChunkStore` (not into a per-cartridge directory) so the prefetch
    /// benefits every cartridge that ever reads the same hash.
    async fn trigger_prefetch(&self, current_chunk_id: u64) {
        if let Some(ref prefetch_mgr) = self.prefetch_manager {
            let cartridge_id = self.manifest.label.clone();
            // Hand the prefetch worker the cartridge's own ChunkStore
            // so it lands in the right per-backend / per-namespace pool
            // layout. Pre-Batch-F we passed only `root()`, which dropped
            // the `<backend>` (and `<barcode>` under `--dedup local`)
            // segments and parked prefetched chunks outside the
            // disk-cache eviction sweep.
            let chunk_store = self.chunk_store.clone();

            // Bounded snapshot: prefetch only queries the next
            // `chunks_ahead` chunk IDs (default 2, max 3 per config),
            // so we read exactly that window from chunk_index instead
            // of cloning the full map per read. Was a per-read O(N)
            // walk + N hash clones — prohibitive on a 12 TB tape with
            // 1.5M chunks.
            let ahead = prefetch_mgr.config().chunks_ahead as u64;
            let mut snapshot: std::collections::HashMap<u64, ChunkLocationInfo> =
                std::collections::HashMap::with_capacity(ahead as usize);
            for i in 1..=ahead {
                let id = current_chunk_id + i;
                if let Ok(rec) = self.chunk_index.read(id) {
                    snapshot.insert(
                        id,
                        ChunkLocationInfo {
                            in_local_cache: matches!(
                                rec.location,
                                LocationTag::LocalOnly | LocationTag::Both
                            ),
                            in_s3: matches!(
                                rec.location,
                                LocationTag::CloudOnly | LocationTag::Both
                            ),
                            hash: rec.hash,
                        },
                    );
                }
            }
            let location_fn = move |chunk_id: u64| -> ChunkLocationInfo {
                snapshot
                    .get(&chunk_id)
                    .cloned()
                    .unwrap_or(ChunkLocationInfo {
                        in_local_cache: false,
                        in_s3: false,
                        hash: None,
                    })
            };

            prefetch_mgr
                .on_read(&cartridge_id, current_chunk_id, chunk_store, location_fn)
                .await;
        }
    }

    /// Cancel all prefetches for this cartridge
    ///
    /// Called when tape position changes unexpectedly (LOCATE, REWIND, large SPACE)
    async fn cancel_prefetches(&self) {
        if let Some(ref prefetch_mgr) = self.prefetch_manager {
            prefetch_mgr.cancel_all(&self.manifest.label).await;
        }
    }

    // --- Partitioning (LTFS) ---

    /// Stage a partition layout for a future FORMAT MEDIUM. Called by the
    /// SCSI layer in response to a successful MODE SELECT page 0x11. The
    /// layout does NOT take effect until FORMAT MEDIUM (CDB 0x04) is
    /// issued — same flow as a real LTO drive under `mkltfs`.
    pub fn set_pending_partition_layout(&mut self, layout: PendingPartitionLayout) -> Result<()> {
        if layout.additional_partitions > MAX_PARTITIONS - 1 {
            return Err(SmcError::InvalidOp(
                "MODE SELECT 0x11: too many additional partitions (max 1)",
            ));
        }
        self.runtime.pending_partition_layout = Some(layout);
        self.persist_runtime()?;
        Ok(())
    }

    /// Apply the pending partition layout (or the default-single-partition
    /// layout if none is staged). Wipes all partition data — same destructive
    /// semantics as ERASE. Called by SCSI FORMAT MEDIUM (CDB 0x04).
    ///
    /// `format_field`:
    /// - `0x00` — default format: keep the current partition layout, wipe data
    /// - `0x01` — apply pending Mode Page 0x11 layout (this is what `mkltfs` issues)
    /// - `0x02` — default partition: revert to a single partition, wipe data
    pub fn apply_format_medium(&mut self, format_field: u8) -> Result<()> {
        // Legal-hold + WORM gate. FORMAT MEDIUM is destructive (every
        // format_field mutates partition layout, contents, or barriers);
        // WORM refuses regardless of the format field.
        self.require_writable(false)?;
        match format_field {
            0x00 => {
                // Default: erase, keep layout.
                self.erase()?;
                Ok(())
            }
            0x02 => {
                // Default partition: revert to single partition.
                self.runtime.partitions = vec![Partition::default()];
                self.runtime.active_partition = 0;
                self.head_lba = 0;
                self.runtime.pending_partition_layout = None;
                // Format wipes the medium, so the host write counter
                // resets to 0.
                self.runtime.host_bytes_written = 0;
                self.persist_runtime()?;
                Ok(())
            }
            0x01 => {
                let layout =
                    self.runtime
                        .pending_partition_layout
                        .clone()
                        .ok_or(SmcError::InvalidOp(
                            "FORMAT MEDIUM 0x01: no pending Mode Page 0x11 layout staged",
                        ))?;
                let part_count = if layout.sdp {
                    1
                } else {
                    1 + layout.additional_partitions as usize
                };
                if part_count == 0 || part_count > MAX_PARTITIONS as usize {
                    return Err(SmcError::InvalidOp(
                        "FORMAT MEDIUM 0x01: invalid partition count",
                    ));
                }
                let mut new_parts: Vec<Partition> = Vec::with_capacity(part_count);
                for i in 0..part_count {
                    let cap_unit_value = layout.partition_sizes.get(i).copied().unwrap_or(0);
                    let capacity_mib = match layout.psum {
                        // PSUM 0=bytes, 1=KiB, 2=MiB, 3=GiB, 4=TiB. We
                        // store MiB internally. 0xFFFF in the size field
                        // means "rest of tape" — represented here as 0.
                        _ if cap_unit_value == 0xFFFF => 0,
                        0 => cap_unit_value / (1024 * 1024),
                        1 => cap_unit_value / 1024,
                        2 => cap_unit_value,
                        3 => cap_unit_value * 1024,
                        4 => cap_unit_value * 1024 * 1024,
                        _ => 0,
                    };
                    new_parts.push(Partition {
                        capacity_mib,
                        overwrite_barrier: None,
                    });
                }
                self.runtime.partitions = new_parts;
                // Reopen block-index files: truncate the existing P0
                // file (clean slate after format) and add P1 if the
                // new layout has it. Old block-index files for
                // partitions that no longer exist are removed.
                let new_part_count = self.runtime.partitions.len();
                let old_part_count = self.block_indexes.len();
                for p in 0..new_part_count.max(old_part_count) {
                    let path = BlockIndexFile::path_for(&self.root, p as u8);
                    if p < new_part_count {
                        // Drop existing handle (if any), unlink the
                        // file, then recreate it empty so the new
                        // header is written.
                        if p < self.block_indexes.len() {
                            // Close by overwriting; Drop releases the file.
                        }
                        let _ = std::fs::remove_file(&path);
                    } else {
                        // Partition removed in new layout — unlink its index file.
                        let _ = std::fs::remove_file(&path);
                    }
                }
                self.block_indexes = open_block_indexes(&self.root, new_part_count)?;
                self.runtime.active_partition = 0;
                self.head_lba = 0;
                self.runtime.pending_partition_layout = None;
                // Format wipes the medium, so the host write counter
                // resets to 0.
                self.runtime.host_bytes_written = 0;
                self.persist_runtime()?;
                Ok(())
            }
            _other => Err(SmcError::InvalidOp(
                "FORMAT MEDIUM: unsupported FORMAT field",
            )),
        }
    }

    /// Set the ALLOW OVERWRITE barrier on the active partition. CDB 0x82
    /// allows the host to mark a position past which writes overwrite
    /// rather than truncate — this is what LTFS uses to append fresh
    /// index records to P0 without losing the prior chain.
    /// `lba == 0` clears the barrier.
    pub fn set_allow_overwrite(&mut self, partition: u8, lba: u64) -> Result<()> {
        // Legal-hold + WORM gate. ALLOW OVERWRITE stages a future
        // overwrite — WORM refuses (point of WORM is no rewrites).
        self.require_writable(false)?;
        if (partition as usize) >= self.runtime.partitions.len() {
            return Err(SmcError::InvalidOp(
                "ALLOW OVERWRITE: partition out of range",
            ));
        }
        let part = &mut self.runtime.partitions[partition as usize];
        part.overwrite_barrier = if lba == 0 { None } else { Some(lba) };
        // `overwrite_barrier` is `#[serde(skip)]` (volatile drive
        // state, like the SCSI ALLOW OVERWRITE bit on real LTO);
        // nothing on disk needs to change here.
        Ok(())
    }

    /// Clear ALLOW OVERWRITE barriers on every partition. Called when the
    /// cartridge is unloaded.
    pub fn clear_allow_overwrite_all(&mut self) {
        for part in self.runtime.partitions.iter_mut() {
            part.overwrite_barrier = None;
        }
    }

    // --- Drive-level encryption (LTO Application-Managed Encryption) ---

    /// Install or replace the drive's encryption state. Called by the
    /// SCSI layer in response to SECURITY PROTOCOL OUT (0xB5) protocol
    /// 0x20 page 0x0010 Set Data Encryption.
    pub fn set_encryption_state(&mut self, state: DriveEncryptionState) {
        self.encryption = Some(state);
    }

    /// Read-only view of the current drive encryption state, used by the
    /// SCSI layer to build SP IN Encryption Status / Next Block
    /// Encryption Status pages.
    pub fn encryption_state(&self) -> Option<&DriveEncryptionState> {
        self.encryption.as_ref()
    }

    /// Clear the drive's encryption state. Called when the host sends a
    /// DISABLE Set Data Encryption page (mode=Disable, scope=Public).
    pub fn clear_encryption(&mut self) {
        self.encryption = None;
    }

    /// True iff the block at the head position is encrypted on the
    /// medium. Used by SP IN Next Block Encryption Status (page 0x0200).
    /// Returns false at EOD or for filemarks. Looks at the active
    /// partition only — partitions have independent encryption metadata
    /// per block.
    pub fn next_block_is_encrypted(&self) -> bool {
        self.try_block_at(self.runtime.active_partition, self.head_lba)
            .map(|bi| bi.encrypted)
            .unwrap_or(false)
    }

    /// Algorithm index of the next block to be read, or 0 if plaintext / EOD.
    /// Today only AES-256-GCM is implemented (algorithm index 0x01).
    pub fn next_block_algorithm_index(&self) -> u8 {
        if self.next_block_is_encrypted() {
            crate::encryption::ALGORITHM_INDEX_AES_256_GCM
        } else {
            0
        }
    }

    // --- Drive-level compression (LTO Mode Page 0x0F DCE bit) ---

    /// Set drive-side compression state. Called by the SCSI layer when
    /// MODE SELECT(6/10) flips DCE on page 0x0F, or by the daemon to
    /// install the daemon-config default at cartridge load. Toggling DCE
    /// off does not affect read decompression of blocks already on the
    /// medium — that's per-block (`BlockIndex.compressed`).
    ///
    /// `algorithm = Sldc` with `dce = true` is rejected here and
    /// silently rewritten to LZ4: the SLDC encoder/decoder isn't shipped
    /// (`compression::compress_data` returns CompressionError), and
    /// without this rewrite an operator that misconfigured
    /// `drive.compression.algorithm: sldc` would see *every* host
    /// write trap with CHECK CONDITION until UNLOAD. Falling back at
    /// the activation boundary makes the failure recoverable: every
    /// new write proceeds with LZ4, and the operator sees the warning
    /// in the daemon log instead of a phantom-broken cartridge.
    pub fn set_compression_state(&mut self, mut state: DriveCompressionState) {
        if state.dce && state.algorithm == CompressionAlgo::Sldc {
            tracing::warn!(
                "drive compression: algorithm=sldc requested with dce=true, but the SLDC codec is not shipped - falling back to lz4. Update drive.compression.algorithm in the daemon config."
            );
            state.algorithm = CompressionAlgo::Lz4;
        }
        self.compression = state;
    }

    /// Read-only view of the current drive compression state, used by
    /// MODE SENSE page 0x0F to report the current DCE bit truthfully.
    pub fn compression_state(&self) -> DriveCompressionState {
        self.compression
    }
}

/// Sealed chunk metadata for the upcoming SCSI READ at a given LBA.
/// Returned by [`Cartridge::peek_chunk_for_lba`] for the iSCSI
/// daemon's prefetch hook so it can refetch a missing chunk from
/// cloud (async) before re-entering the sync read path that doesn't
/// have its own cloud-fallback surface.
#[derive(Debug, Clone)]
pub struct NextReadChunk {
    pub chunk_id: u64,
    pub hash: String,
    /// Local pool path the read path will resolve to.
    pub store_path: PathBuf,
    /// Cloud key the prefetch hook should fetch — already namespaced
    /// per the source cartridge's dedup policy.
    pub object_key: String,
    /// Sticky cloud backend the cartridge is bound to, drawn from
    /// `cloud.backends` in the daemon config. The prefetch hook
    /// resolves this name through its backend registry.
    pub backend_name: String,
}

/// Snapshot describing one chunk to upload — re-exported from
/// `shared_upload_worker::PendingUpload` (lifted alongside
/// `upload_chunk_inert` so the block product can share the same
/// payload shape). Built via [`Cartridge::pending_upload_payload`].
/// Field-name note: the historical `chunk_id` is `item_id` on the
/// shared struct (tape passes a chunk id, block passes a page id).
pub use shared_upload_worker::PendingUpload as PendingUploadPayload;

/// Outcome of [`Cartridge::backup_manifest_to_cloud`]: every cloud key
/// that was freshly PUT during the pass — versioned manifest backup,
/// the `manifest-latest.json` sentinel, and the per-file index page
/// objects (`manifests/<barcode>/<label>/page-<NNNNNN>.dat`). The
/// daemon's auto-hold-on-upload worker uses these so a held cartridge's
/// freshly-PUT objects inherit the per-object hold even when set/clear
/// is racing the daemon (out-of-band hold against a loaded cartridge —
/// the residual case after `legal-hold set/clear` started refusing
/// against loaded cartridges).
#[derive(Debug, Clone)]
pub struct ManifestBackupOutcome {
    /// Versioned manifest backup
    /// (`manifests/<barcode>/manifest-<TIMESTAMP>.json`).
    pub versioned_key: String,
    /// Refreshed `manifests/<barcode>/manifest-latest.json` sentinel.
    /// Sentinel-last in the apply order, so it must be held LAST.
    pub latest_key: String,
    /// Index-page objects freshly PUT this pass. May be empty if no
    /// pages were dirty since the previous backup. Order is unspecified.
    pub index_page_keys: Vec<String>,
}

/// Outcome of an [`upload_chunk_inert`] call — re-exported from
/// `shared_upload_worker::UploadOutcome`. Field-name note: historical
/// `chunk_id` is `item_id` on the shared struct.
pub use shared_upload_worker::UploadOutcome as ChunkUploadOutcome;

/// Stateless companion to [`Cartridge::upload_chunk_to_cloud`].
/// Re-exported from `shared_upload_worker::upload_chunk_inert` so the
/// tape and block products share the cloud-side dedup probe + PUT
/// logic. See that crate's docs for the per-step rationale.
pub use shared_upload_worker::upload_chunk_inert;

impl Drop for Cartridge {
    fn drop(&mut self) {
        // View handles (upload worker, GC, any out-of-band reader
        // opened while a drive-side primary handle holds the
        // cartridge loaded) must not touch the trailing staging
        // chunk on drop. The empty-chunk branch of `flush_and_seal`
        // unlinks `.staging/chunk-<id>.dat` and truncates the
        // chunk_index slot — yanking the staging file out from
        // under the primary's open `cur_file` and surfacing as a
        // post-write ENOENT on the next read (issue #28). Runtime
        // sidecar persist is skipped too: the primary owns those
        // counters and would clobber its in-flight updates.
        if self.is_view_handle {
            return;
        }

        // Best-effort: seal (or clean up, if empty) the trailing staging
        // chunk so the on-disk state matches what's reachable via the
        // manifest. Failures are logged but never panic — Drop must not
        // unwind. Crash-after-seal-before-persist is an accepted edge
        // case; GC and a manual reopen recover from it.
        if self.cur_chunk.hash.is_none()
            && let Err(e) = self.flush_and_seal()
        {
            tracing::warn!(
                "Cartridge::drop: failed to seal trailing chunk for {}: {}",
                self.manifest.label,
                e
            );
        }

        // Persist the runtime sidecar so the four lifetime byte
        // counters survive a cartridge unload (the natural tape
        // boundary — MOVE MEDIUM drops the loaded `Cartridge`). The
        // hot SCSI path's `persist_runtime` boundaries — LOCATE,
        // FORMAT, ERASE, manifest backup — never fire on a pure
        // sequential read, so without this an unloaded cartridge's
        // read counters would be lost. `Runtime::persist` is
        // independent of the chunk index, so it is safe to call
        // after `flush_and_seal` regardless of seal state. A daemon
        // crash still loses counter movement since the last persist.
        if let Err(e) = self.runtime.persist(&self.root) {
            tracing::warn!(
                "Cartridge::drop: failed to persist runtime sidecar for {}: {}",
                self.manifest.label,
                e
            );
        }
    }
}

#[cfg(test)]
mod media_helper_tests {
    use super::*;

    #[test]
    fn capacity_table() {
        assert_eq!(lto_default_capacity_gb(7), 6000);
        assert_eq!(lto_default_capacity_gb(8), 12000);
        // Unknown generations: 0.
        assert_eq!(lto_default_capacity_gb(0), 0);
        assert_eq!(lto_default_capacity_gb(9), 0);
    }
}
