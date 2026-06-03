// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Volume manifest — per-volume metadata persisted as
//! `<data_dir>/volumes/<name>/manifest.json`.
//!
//! Symmetric to thurvtl's `cartridge/manifest.json`. The on-disk
//! schema is the durable contract between create-time tooling
//! (`thurvsa volume create`) and the future thurvsad that
//! will serve SBC-3 against it; keep it stable across thurvsa
//! versions, bump `schema_version` and add a migration step
//! when the shape changes.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current on-disk manifest version.
///
/// History:
/// - **v1** (initial) — name, uuid, size_bytes, sector_bytes,
///   page_size_bytes, backend, dedup_scope, worm, host_bytes_written,
///   created_at, modified_at.
/// - **v2** (2026-05-13) — adds optional `encryption` field for
///   per-volume AES-256-GCM at-rest encryption (key in the per-daemon
///   keystore, addressed by the volume's uuid).
/// - **v3** (2026-05-14) — splits runtime state into a sidecar
///   `runtime.json`. The manifest itself is now creation-frozen:
///   `host_bytes_written` and `modified_at` move to
///   [`crate::runtime_state::VolumeRuntime`]. `created_at` stays —
///   it's identity.
/// - **v4** (2026-05-16) — adds `lun: u64`, pinned at create time so
///   host references (by-id, by-path, fstab UUIDs aside) survive
///   sibling volume add/remove. Pre-v4 manifests deserialize with
///   the sentinel [`UNASSIGNED_LUN`]; `vsa-daemon`'s discovery layer
///   auto-assigns the smallest unused LUN on first boot and persists
///   the manifest back, so existing volumes keep their boot-time
///   alphabetical assignment.
pub const VOLUME_SCHEMA_VERSION: u32 = 4;

/// Sentinel for "this manifest predates the v4 schema and has no
/// pinned LUN yet." Discovery resolves these to real values at boot.
/// Chosen as `u64::MAX` so it can never collide with a real LUN
/// (the iSCSI LUN field is 64-bit, but NVMe NSID is 32-bit, so any
/// realistic LUN is much smaller).
pub const UNASSIGNED_LUN: u64 = u64::MAX;

/// Default sector size advertised over SBC-3 READ CAPACITY.
pub const DEFAULT_SECTOR_BYTES: u32 = 4096;

/// Default page size — the unit at which the volume is chunked,
/// uploaded to cloud, and cached locally. 64 KiB keeps random
/// 4 KiB writes at 16× amplification while keeping the cloud
/// object count manageable on a multi-TB volume.
pub const DEFAULT_PAGE_SIZE_BYTES: u32 = 64 * 1024;

/// Upper bound on operator-supplied volume names. Filesystem-safe
/// and short enough for inclusion in cloud object keys.
pub const MAX_VOLUME_NAME_LEN: usize = 64;

/// Dedup scope for the chunk pool. Mirrors thurvtl's notion: `Local`
/// namespaces every page chunk under the volume UUID (no cross-
/// volume sharing, simpler delete); `Global` joins the shared
/// per-backend pool. Default on VSA is `Local` because cross-volume
/// LBA-0 dedup hits would be coincidental — boot sectors, partition
/// tables, and filesystem superblocks differ per volume. Re-exported
/// from `shared_object_store::DedupScope` so the upload pipeline
/// (`shared-upload-worker`) carries the same enum across the
/// boundary.
pub use shared_object_store::DedupScope;

/// Parse a dedup scope string with VSA's product-specific error
/// type. Thin wrapper over `shared_object_store::DedupScope: FromStr` —
/// preserved so existing VSA call sites (CLI / volume create) keep
/// `?`-propagating through `VolumeError`.
pub fn parse_dedup_scope(s: &str) -> Result<DedupScope, VolumeError> {
    s.parse::<DedupScope>()
        .map_err(|_| VolumeError::InvalidDedupScope(s.to_string()))
}

/// What SBC-3 SYNCHRONIZE CACHE waits for before returning. Operator
/// chooses the durability floor for `fsync(2)`: the host writes a
/// page, calls fsync, and the SCSI SYNCHRONIZE CACHE handler decides
/// how far down the storage stack to drain before answering OK.
///
/// Three tiers, descending durability:
///
/// - [`SyncAfter::Storage`] (default) — SYNC blocks until every dirty
///   page in the synced range is in the cloud object store. Bytes
///   survive host-disk loss, daemon-process crash, and power loss.
///   Slowest tier; matches the operator contract that cloud-backed
///   storage exists to provide.
/// - [`SyncAfter::Disk`] — SYNC blocks until every dirty page is in
///   the local pool file on disk. Bytes survive daemon-process
///   crash and power loss; **lost** if the daemon host's disk fails
///   before the upload worker drains. For scratch volumes or
///   workloads that don't carry the value of the cloud-durability
///   guarantee.
/// - [`SyncAfter::Memory`] — SYNC is a no-op; bytes remain only in
///   the RAM `PageCache` until the periodic flush worker tick (or
///   eviction-induced flush) drains them. Bytes the host believes
///   are durable are **lost on any crash**. ZFS's `sync=disabled`
///   equivalent — for benchmarks or volumes whose loss is
///   acceptable.
///
/// Live-mutable via `thurvsa volume modify --sync-after <MODE>`.
/// A flip takes effect on the next SYNC; in-flight SYNCs finish
/// under the mode that was active when they started. The contract
/// change is **not signalled to the SCSI initiator** — a host
/// fsync-heavy workload silently gains or loses durability on a
/// flip; operators should pair flips with workload-level awareness.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncAfter {
    /// fsync = bytes durable in the storage backend. Default; safest.
    #[default]
    Storage,
    /// fsync = bytes in local disk pool. Faster; loses on disk failure.
    Disk,
    /// fsync = no-op. Fastest; loses on any crash.
    Memory,
}

impl SyncAfter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Disk => "disk",
            Self::Memory => "memory",
        }
    }

    /// On-disk encoding for the `VolumeWriter::sync_after`
    /// `AtomicU8` hot-path cache. 0/1/2; any other byte falls back
    /// to [`SyncAfter::Storage`] (safe default — preserves
    /// storage-durable semantics if the atomic is ever corrupted).
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Storage => 0,
            Self::Disk => 1,
            Self::Memory => 2,
        }
    }

    pub const fn from_u8(b: u8) -> Self {
        match b {
            1 => Self::Disk,
            2 => Self::Memory,
            _ => Self::Storage,
        }
    }
}

impl std::str::FromStr for SyncAfter {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "storage" => Ok(Self::Storage),
            "disk" => Ok(Self::Disk),
            "memory" => Ok(Self::Memory),
            other => Err(format!(
                "invalid --sync-after '{other}': expected 'storage', 'disk', or 'memory'"
            )),
        }
    }
}

impl std::fmt::Display for SyncAfter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// At-rest encryption algorithm for a volume. Today only AES-256-GCM
/// is implemented; the enum exists so future algorithms (rotation,
/// XTS, …) drop in without changing the manifest field type.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeEncryptionAlgorithm {
    Aes256Gcm,
}

impl VolumeEncryptionAlgorithm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aes256Gcm => "aes_256_gcm",
        }
    }
}

/// Per-volume at-rest encryption metadata.
///
/// The plaintext DEK is **not** in this struct. For the `local`
/// keystore backend the DEK lives in `<data_dir>/keys/<uuid_hex>.key`
/// (mode 0600) and `wrapped_dek` stays `None`. For external backends
/// (`awskms`, `vault`) the DEK is never written to disk in plaintext
/// — `wrapped_dek` carries the base64-encoded ciphertext returned by
/// the backend's `wrap` op (KMS ciphertext blob, Vault Transit
/// `vault:v1:…` string). A stolen manifest without backend
/// credentials still can't decrypt the volume: KMS / Vault enforce
/// the encryption-context binding to `volume_uuid` so the wrapped
/// blob is also useless against any other volume.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VolumeEncryptionMeta {
    pub algorithm: VolumeEncryptionAlgorithm,

    /// Name of the keystore backend entry under `keystore.backends:`
    /// in the YAML conffile that holds (or can derive) this volume's
    /// DEK. Sticky for the volume's lifetime. Default `"local"` so
    /// v2-shape manifests (pre-keystore-trait) deserialize onto the
    /// existing on-disk-keyfile path with no migration.
    #[serde(default = "default_keystore_backend")]
    pub keystore_backend: String,

    /// Backend-returned wrapped DEK, base64-encoded. `None` for
    /// `local` (the keystore sidecar IS the storage) and `Some(...)`
    /// for `awskms` / `vault`. The byte layout inside is opaque to
    /// Thur VSA — KMS returns a binary blob, Vault returns the
    /// `vault:v1:…` ciphertext string, both round-trip through the
    /// trait's `wrap` / `unwrap` calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapped_dek: Option<String>,
}

fn default_keystore_backend() -> String {
    "local".to_string()
}

fn default_unassigned_lun() -> u64 {
    UNASSIGNED_LUN
}

/// Errors returned by volume create / load / persist.
#[derive(Error, Debug)]
pub enum VolumeError {
    #[error("volume name '{0}' is invalid: {1}")]
    InvalidName(String, &'static str),

    #[error("dedup scope must be 'local' or 'global', got '{0}'")]
    InvalidDedupScope(String),

    #[error("volume size ({size_bytes} B) is not a multiple of sector size ({sector_bytes} B)")]
    SizeNotSectorAligned { size_bytes: u64, sector_bytes: u32 },

    #[error("page size ({0} B) must be a power of two and a multiple of the sector size")]
    InvalidPageSize(u32),

    #[error("volume size must be at least one page ({0} B)")]
    SizeBelowPage(u32),

    #[error("size string '{0}' could not be parsed: {1}")]
    InvalidSize(String, &'static str),

    #[error("volume directory '{0}' already exists")]
    AlreadyExists(PathBuf),

    #[error("manifest at '{0}' is missing")]
    NotFound(PathBuf),

    #[error(
        "runtime sidecar at '{0}' is missing; either an interrupted \
         `volume create` or hand-rolled corruption. Refusing to silently \
         re-initialize runtime state — `volume destroy` the volume or \
         restore the runtime.json file."
    )]
    RuntimeMissing(PathBuf),

    #[error("manifest schema version {found} not understood (expected {expected})")]
    SchemaMismatch { found: u32, expected: u32 },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("page index: {0}")]
    PageIndex(#[from] crate::page_index::PageIndexError),

    #[error("getrandom: {0}")]
    Random(String),
}

/// Inline serde for the manifest's 16-byte UUID. Stored as
/// lowercase hex in JSON, matches thurvtl's cartridge manifest.
mod uuid_serde {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(uuid: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(uuid))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(D::Error::custom)?;
        if bytes.len() != 16 {
            return Err(D::Error::custom("volume uuid must be 16 bytes"));
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&bytes);
        Ok(out)
    }
}

/// Persistent volume metadata. The page index (`page_id →
/// chunk_hash`) lives in a separate sidecar file; this manifest
/// only carries the schema-stable identity + sizing fields.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VolumeManifest {
    pub schema_version: u32,
    pub name: String,
    #[serde(with = "uuid_serde")]
    pub uuid: [u8; 16],
    /// Logical (host-visible) volume size, sector-aligned. This is the
    /// boot/persist record. The *live* size after an online `volume
    /// resize` is the writer's shadow atomic — read it via
    /// [`crate::uploader::VolumeWriter::size_bytes`] /
    /// [`crate::cache::PageCache::size_bytes`]. Reading this field on a
    /// hot path advertises a stale capacity after a resize (issue #76).
    pub size_bytes: u64,
    /// SBC-3 advertised logical-block size. 4 KiB by default.
    pub sector_bytes: u32,
    /// Page size — unit of chunk seal / cloud upload / disk cache.
    /// Power of two, multiple of `sector_bytes`.
    pub page_size_bytes: u32,
    /// Cloud backend name (matches a key under `cloud.backends` in
    /// `thurvsa.yaml`). Sticky for the volume's lifetime.
    pub backend: String,
    /// Stable host-visible LUN. Pinned at create time and persisted
    /// in the manifest so host references (`/dev/disk/by-id/...`,
    /// `/dev/disk/by-path/...-lun-N`) survive sibling volume add /
    /// remove. NSID for the NVMe-TCP transport is derived as
    /// `lun + 1` so NSID 0 stays reserved.
    ///
    /// Pre-v4 manifests deserialize via `#[serde(default)]` to
    /// [`UNASSIGNED_LUN`]; discovery resolves them on first boot and
    /// persists the manifest back so the next boot is steady-state.
    #[serde(default = "default_unassigned_lun")]
    pub lun: u64,
    pub dedup_scope: DedupScope,
    /// WORM marker. Sticky once set; the SBC-3 write paths refuse
    /// WRITE / COMPARE AND WRITE / UNMAP / WRITE SAME / XCOPY-dest with
    /// WRITE PROTECTED when `true` (see `scsi_sbc::data_path`).
    pub worm: bool,
    pub created_at: DateTime<Utc>,
    /// At-rest encryption settings. `None` is the default for both
    /// schema v1 (which had no field) and operator-created v2 volumes
    /// that didn't pass `--encrypt`. `Some(...)` flips the encrypt-
    /// before-pool-insert / decrypt-after-cloud-fetch path in
    /// [`crate::uploader::VolumeWriter`] on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<VolumeEncryptionMeta>,
}

impl VolumeManifest {
    /// Filename for the on-disk manifest within `<volume_dir>/`.
    pub const FILENAME: &'static str = "manifest.json";

    /// Subdirectory under `<data_dir>/` that holds every volume.
    pub const VOLUMES_SUBDIR: &'static str = "volumes";

    /// Resolve the on-disk volume directory for a given name.
    pub fn dir_for(data_dir: &Path, name: &str) -> PathBuf {
        data_dir.join(Self::VOLUMES_SUBDIR).join(name)
    }

    /// Resolve the manifest path for a given volume name.
    pub fn path_for(data_dir: &Path, name: &str) -> PathBuf {
        Self::dir_for(data_dir, name).join(Self::FILENAME)
    }

    /// Chunk-pool namespace for this volume: `None` under `Global`
    /// dedup (the shared per-backend pool), `Some(hex-of-uuid)` under
    /// `Local`. Keyed on the durable UUID, not the mutable name — a
    /// destroy + recreate under the same name does NOT inherit the
    /// dead volume's namespace or its orphan chunks.
    pub fn pool_namespace(&self) -> Option<String> {
        match self.dedup_scope {
            DedupScope::Global => None,
            DedupScope::Local => Some(namespace_from_uuid(&self.uuid)),
        }
    }

    /// Build a fresh manifest. Does not touch disk — call
    /// [`Self::create`] for the on-disk side. The `encryption` field
    /// is set separately via [`Self::with_encryption`]; the daemon
    /// flips it on (with a freshly minted or operator-supplied key
    /// already written to the keystore) before persisting.
    pub fn new(
        name: String,
        size_bytes: u64,
        sector_bytes: u32,
        page_size_bytes: u32,
        backend: String,
        dedup_scope: DedupScope,
        worm: bool,
        lun: u64,
    ) -> Result<Self, VolumeError> {
        validate_name(&name)?;
        validate_page_size(page_size_bytes, sector_bytes)?;
        if size_bytes < u64::from(page_size_bytes) {
            return Err(VolumeError::SizeBelowPage(page_size_bytes));
        }
        if !size_bytes.is_multiple_of(u64::from(sector_bytes)) {
            return Err(VolumeError::SizeNotSectorAligned {
                size_bytes,
                sector_bytes,
            });
        }
        Ok(Self {
            schema_version: VOLUME_SCHEMA_VERSION,
            name,
            uuid: generate_uuid()?,
            size_bytes,
            sector_bytes,
            page_size_bytes,
            backend,
            lun,
            dedup_scope,
            worm,
            created_at: Utc::now(),
            encryption: None,
        })
    }

    /// Mark this volume as encrypted. Builder-style so the existing
    /// [`Self::new`] signature stays at seven args. The key itself
    /// lives in the daemon's keystore, addressed by `self.uuid`; this
    /// only records that the volume is encrypted and with which
    /// algorithm.
    pub fn with_encryption(mut self, algorithm: VolumeEncryptionAlgorithm) -> Self {
        self.encryption = Some(VolumeEncryptionMeta {
            algorithm,
            keystore_backend: default_keystore_backend(),
            wrapped_dek: None,
        });
        self
    }

    /// Stamp the encryption metadata with the resolved keystore
    /// backend name + wrapped DEK. Daemon flow: call
    /// [`Self::with_encryption`] first (to set `algorithm`), then
    /// `with_keystore` once the backend has produced the wrapped
    /// blob. For `local` backends `wrapped_dek` is `None`.
    pub fn with_keystore(mut self, backend: String, wrapped_dek: Option<String>) -> Self {
        if let Some(meta) = self.encryption.as_mut() {
            meta.keystore_backend = backend;
            meta.wrapped_dek = wrapped_dek;
        }
        self
    }

    /// Create a new volume on disk: validate, mkdir, persist
    /// manifest, and initialize the page index. The volume is
    /// "complete" once both `manifest.json` and `pages.idx` are
    /// present in the directory; this routine guarantees both or
    /// rolls back. Refuses if the directory already exists (no
    /// overwrite — symmetric to `cartridge create`).
    pub fn create(self, data_dir: &Path) -> Result<Self, VolumeError> {
        let vol_dir = Self::dir_for(data_dir, &self.name);
        if vol_dir.exists() {
            return Err(VolumeError::AlreadyExists(vol_dir));
        }
        fs::create_dir_all(&vol_dir)?;
        match Self::create_artifacts(&self, &vol_dir) {
            Ok(()) => Ok(self),
            Err(e) => {
                let _ = fs::remove_dir_all(&vol_dir);
                Err(e)
            }
        }
    }

    /// Write the manifest + the empty page index + a zero-valued
    /// runtime sidecar into an existing volume directory. Caller
    /// cleans up the directory on failure (see [`Self::create`]).
    fn create_artifacts(this: &Self, vol_dir: &Path) -> Result<(), VolumeError> {
        this.persist(vol_dir)?;
        crate::runtime_state::VolumeRuntime::new_zero().persist(vol_dir)?;
        let idx_path = crate::page_index::PageIndex::path_for(vol_dir);
        crate::page_index::PageIndex::create(
            &idx_path,
            this.uuid,
            u64::from(this.page_size_bytes),
        )?;
        Ok(())
    }

    /// Atomic write: tmp + fsync + rename, matching the cartridge
    /// manifest persistence pattern.
    pub fn persist(&self, vol_dir: &Path) -> Result<(), VolumeError> {
        let tmp = vol_dir.join("manifest.json.tmp");
        let final_path = vol_dir.join(Self::FILENAME);
        {
            let f = fs::File::create(&tmp)?;
            let mut w = BufWriter::new(f);
            serde_json::to_writer(&mut w, self)?;
            w.flush()?;
            w.into_inner()
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .sync_all()?;
        }
        fs::rename(tmp, final_path)?;
        Ok(())
    }

    /// Load manifest from `<data_dir>/volumes/<name>/manifest.json`.
    ///
    /// Accepts every schema version this build understands and stamps
    /// the result forward to [`VOLUME_SCHEMA_VERSION`] in memory. The
    /// next [`Self::persist`] writes the upgraded manifest back to
    /// disk transparently. Identity-only callers (e.g.
    /// `volume key migrate`) are the only writers of `manifest.json`
    /// post-create — the daemon's hot path only touches
    /// `runtime.json`.
    pub fn load(data_dir: &Path, name: &str) -> Result<Self, VolumeError> {
        let path = Self::path_for(data_dir, name);
        if !path.exists() {
            return Err(VolumeError::NotFound(path));
        }
        let raw = fs::read_to_string(&path)?;
        let mut manifest: Self = serde_json::from_str(&raw)?;
        // 1..=VOLUME_SCHEMA_VERSION are the versions this build
        // understands. v1 had no `encryption` field; serde::default
        // filled it with `None`, which is correct for any pre-encryption
        // volume. Bumping the in-memory version means the next persist
        // writes v2 without an explicit migration call site.
        if manifest.schema_version == 0 || manifest.schema_version > VOLUME_SCHEMA_VERSION {
            return Err(VolumeError::SchemaMismatch {
                found: manifest.schema_version,
                expected: VOLUME_SCHEMA_VERSION,
            });
        }
        manifest.schema_version = VOLUME_SCHEMA_VERSION;
        Ok(manifest)
    }

    /// Enumerate every volume directory under `<data_dir>/volumes/`.
    /// Returns names in directory-listing order. Returns an empty
    /// vec when the parent dir is missing (no volumes yet).
    pub fn list(data_dir: &Path) -> Result<Vec<String>, VolumeError> {
        let parent = data_dir.join(Self::VOLUMES_SUBDIR);
        if !parent.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(n) = entry.file_name().to_str()
            {
                names.push(n.to_string());
            }
        }
        names.sort();
        Ok(names)
    }
}

fn validate_name(name: &str) -> Result<(), VolumeError> {
    if name.is_empty() {
        return Err(VolumeError::InvalidName(name.to_string(), "name is empty"));
    }
    if name.len() > MAX_VOLUME_NAME_LEN {
        return Err(VolumeError::InvalidName(
            name.to_string(),
            "name longer than 64 chars",
        ));
    }
    if name == "." || name == ".." {
        return Err(VolumeError::InvalidName(
            name.to_string(),
            "name is a reserved path",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(VolumeError::InvalidName(
            name.to_string(),
            "only ASCII letters, digits, '-', and '_' are allowed",
        ));
    }
    Ok(())
}

fn validate_page_size(page_size_bytes: u32, sector_bytes: u32) -> Result<(), VolumeError> {
    if page_size_bytes == 0 || !page_size_bytes.is_power_of_two() {
        return Err(VolumeError::InvalidPageSize(page_size_bytes));
    }
    if sector_bytes == 0 || !page_size_bytes.is_multiple_of(sector_bytes) {
        return Err(VolumeError::InvalidPageSize(page_size_bytes));
    }
    Ok(())
}

fn generate_uuid() -> Result<[u8; 16], VolumeError> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| VolumeError::Random(e.to_string()))?;
    Ok(buf)
}

/// Derive the `Local`-scope chunk-pool namespace from a volume UUID:
/// the 32-char lowercase hex of the 16 bytes. The single definition
/// of the namespace string — pool writers, the disk-cache walker, and
/// `system gc` all route through here so they can't drift.
pub fn namespace_from_uuid(uuid: &[u8; 16]) -> String {
    hex::encode(uuid)
}

/// Parse a human-supplied size string. Accepts a bare integer
/// (bytes), or an integer with a binary suffix:
///
///   `K` = 2¹⁰, `M` = 2²⁰, `G` = 2³⁰, `T` = 2⁴⁰, `P` = 2⁵⁰.
///
/// Suffix is case-insensitive. An optional trailing `B` or `iB`
/// is accepted but ignored (so `100GB`, `100GiB`, and `100G` all
/// parse the same). SI (decimal) units are deliberately not
/// supported — disk capacity is universally treated as binary in
/// this stack.
pub fn parse_size(input: &str) -> Result<u64, VolumeError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(VolumeError::InvalidSize(
            input.to_string(),
            "size string is empty",
        ));
    }
    let mut s = trimmed;
    if let Some(stripped) = s.strip_suffix(|c: char| c == 'b' || c == 'B') {
        s = stripped;
        if let Some(stripped2) = s.strip_suffix(|c: char| c == 'i' || c == 'I') {
            s = stripped2;
        }
    }
    let (digits, multiplier) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some(c @ ('K' | 'M' | 'G' | 'T' | 'P')) => {
            let mult: u64 = match c {
                'K' => 1u64 << 10,
                'M' => 1u64 << 20,
                'G' => 1u64 << 30,
                'T' => 1u64 << 40,
                'P' => 1u64 << 50,
                _ => unreachable!(),
            };
            (&s[..s.len() - 1], mult)
        }
        _ => (s, 1u64),
    };
    let n: u64 = digits.trim().parse().map_err(|_| {
        VolumeError::InvalidSize(input.to_string(), "leading number does not parse as u64")
    })?;
    n.checked_mul(multiplier)
        .ok_or_else(|| VolumeError::InvalidSize(input.to_string(), "size overflows u64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_size_accepts_bare_bytes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("4096").unwrap(), 4096);
    }

    #[test]
    fn parse_size_handles_binary_suffixes() {
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("1KiB").unwrap(), 1024);
        assert_eq!(parse_size("64K").unwrap(), 64 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1u64 << 30);
        assert_eq!(parse_size("1T").unwrap(), 1u64 << 40);
        assert_eq!(parse_size("2t").unwrap(), 2u64 << 40);
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("foo").is_err());
        assert!(parse_size("12X").is_err());
    }

    #[test]
    fn dedup_scope_round_trips() {
        for raw in ["local", "global", "LOCAL", "Global"] {
            let s = parse_dedup_scope(raw).unwrap();
            let json = serde_json::to_string(&s).unwrap();
            let back: DedupScope = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
        assert!(parse_dedup_scope("planetary").is_err());
    }

    #[test]
    fn sync_after_round_trips_each_tier() {
        for (raw, expected) in [
            ("storage", SyncAfter::Storage),
            ("disk", SyncAfter::Disk),
            ("memory", SyncAfter::Memory),
            ("STORAGE", SyncAfter::Storage),
            (" Memory ", SyncAfter::Memory),
        ] {
            let parsed: SyncAfter = raw.parse().unwrap();
            assert_eq!(parsed, expected, "parse {raw:?}");
            let json = serde_json::to_string(&parsed).unwrap();
            let back: SyncAfter = serde_json::from_str(&json).unwrap();
            assert_eq!(back, parsed);
        }
        assert!("strict".parse::<SyncAfter>().is_err());
        assert!("".parse::<SyncAfter>().is_err());
    }

    #[test]
    fn sync_after_default_is_cloud() {
        // Load-bearing for the contract: every new volume + every
        // legacy runtime.json without the field comes up on
        // cloud-durable.
        assert_eq!(SyncAfter::default(), SyncAfter::Storage);
    }

    #[test]
    fn sync_after_atomic_byte_roundtrip() {
        for m in [SyncAfter::Storage, SyncAfter::Disk, SyncAfter::Memory] {
            assert_eq!(SyncAfter::from_u8(m.as_u8()), m);
        }
        // Unrecognised byte → Cloud (safe default; matches the
        // doc comment on from_u8).
        assert_eq!(SyncAfter::from_u8(0xFF), SyncAfter::Storage);
    }

    #[test]
    fn create_writes_both_manifest_and_page_index() {
        let dir = TempDir::new().unwrap();
        let m = VolumeManifest::new(
            "vol1".into(),
            10 * (1u64 << 30),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap();
        let created = m.create(dir.path()).unwrap();

        let vol_dir = VolumeManifest::dir_for(dir.path(), "vol1");
        assert!(vol_dir.join("manifest.json").exists());
        let idx_path = crate::page_index::PageIndex::path_for(&vol_dir);
        assert!(idx_path.exists());

        // Page index header is bound to the volume's identity.
        let idx = crate::page_index::PageIndex::open(
            &idx_path,
            created.uuid,
            u64::from(created.page_size_bytes),
        )
        .unwrap();
        assert_eq!(idx.volume_uuid(), &created.uuid);
        assert_eq!(idx.page_size_bytes(), u64::from(created.page_size_bytes));
    }

    #[test]
    fn manifest_create_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let m = VolumeManifest::new(
            "vol1".into(),
            10 * (1u64 << 30),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap();
        let created = m.clone().create(dir.path()).unwrap();

        let loaded = VolumeManifest::load(dir.path(), "vol1").unwrap();
        assert_eq!(loaded, created);
    }

    #[test]
    fn manifest_create_refuses_duplicate() {
        let dir = TempDir::new().unwrap();
        let m1 = VolumeManifest::new(
            "vol1".into(),
            10 * (1u64 << 30),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap();
        m1.create(dir.path()).unwrap();

        let m2 = VolumeManifest::new(
            "vol1".into(),
            5 * (1u64 << 30),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap();
        let err = m2.create(dir.path()).unwrap_err();
        assert!(matches!(err, VolumeError::AlreadyExists(_)));
    }

    #[test]
    fn list_returns_sorted_names() {
        let dir = TempDir::new().unwrap();
        for name in ["zeta", "alpha", "mid"] {
            VolumeManifest::new(
                name.into(),
                1u64 << 30,
                DEFAULT_SECTOR_BYTES,
                DEFAULT_PAGE_SIZE_BYTES,
                "primary".into(),
                DedupScope::Local,
                false,
                0,
            )
            .unwrap()
            .create(dir.path())
            .unwrap();
        }
        let names = VolumeManifest::list(dir.path()).unwrap();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn list_on_missing_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        let names = VolumeManifest::list(dir.path()).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn rejects_unaligned_size() {
        let err = VolumeManifest::new(
            "vol1".into(),
            DEFAULT_PAGE_SIZE_BYTES as u64 + 1,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap_err();
        assert!(matches!(err, VolumeError::SizeNotSectorAligned { .. }));
    }

    #[test]
    fn rejects_invalid_name() {
        let cases = ["", "..", "with space", "weird/slash", "tab\there"];
        for bad in cases {
            let err = VolumeManifest::new(
                bad.into(),
                1u64 << 30,
                DEFAULT_SECTOR_BYTES,
                DEFAULT_PAGE_SIZE_BYTES,
                "primary".into(),
                DedupScope::Local,
                false,
                0,
            )
            .unwrap_err();
            assert!(matches!(err, VolumeError::InvalidName(..)), "{bad}");
        }
    }

    #[test]
    fn rejects_non_power_of_two_page_size() {
        let err = VolumeManifest::new(
            "vol1".into(),
            1u64 << 30,
            DEFAULT_SECTOR_BYTES,
            48 * 1024,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap_err();
        assert!(matches!(err, VolumeError::InvalidPageSize(_)));
    }

    #[test]
    fn fresh_manifest_has_no_encryption() {
        let m = VolumeManifest::new(
            "vol1".into(),
            1u64 << 30,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap();
        assert!(m.encryption.is_none());
    }

    #[test]
    fn with_encryption_flips_the_field() {
        let m = VolumeManifest::new(
            "vol1".into(),
            1u64 << 30,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .with_encryption(VolumeEncryptionAlgorithm::Aes256Gcm);
        let meta = m.encryption.expect("set by builder");
        assert_eq!(meta.algorithm, VolumeEncryptionAlgorithm::Aes256Gcm);
    }

    #[test]
    fn v2_with_encryption_round_trips() {
        let dir = TempDir::new().unwrap();
        let m = VolumeManifest::new(
            "vol1".into(),
            1u64 << 30,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .with_encryption(VolumeEncryptionAlgorithm::Aes256Gcm);
        let created = m.clone().create(dir.path()).unwrap();

        let loaded = VolumeManifest::load(dir.path(), "vol1").unwrap();
        assert_eq!(loaded.schema_version, VOLUME_SCHEMA_VERSION);
        assert_eq!(loaded.encryption, created.encryption);
        assert_eq!(
            loaded.encryption.unwrap().algorithm,
            VolumeEncryptionAlgorithm::Aes256Gcm,
        );
    }

    #[test]
    fn v2_persisted_omits_encryption_when_absent() {
        // `skip_serializing_if = "Option::is_none"` keeps the field
        // out of the JSON when the volume isn't encrypted, so an
        // operator inspecting the file doesn't see a stray "null".
        let dir = TempDir::new().unwrap();
        VolumeManifest::new(
            "vol1".into(),
            1u64 << 30,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(dir.path())
        .unwrap();
        let path = VolumeManifest::path_for(dir.path(), "vol1");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("encryption"),
            "unencrypted v2 manifest leaks an 'encryption' key: {raw}"
        );
    }

    #[test]
    fn pre_split_manifest_loads_with_encryption_none() {
        // Hand-craft a schema_version=1 manifest on disk (no
        // `encryption` field) and verify load() accepts it,
        // upgrades the in-memory version, and treats encryption
        // as off. Unknown fields from older shapes
        // (host_bytes_written / modified_at, now in runtime.json)
        // are silently ignored by serde.
        let dir = TempDir::new().unwrap();
        let vol_dir = VolumeManifest::dir_for(dir.path(), "vol1");
        std::fs::create_dir_all(&vol_dir).unwrap();
        let raw = serde_json::json!({
            "schema_version": 1,
            "name": "vol1",
            "uuid": "00112233445566778899aabbccddeeff",
            "size_bytes": 1_073_741_824u64,
            "sector_bytes": DEFAULT_SECTOR_BYTES,
            "page_size_bytes": DEFAULT_PAGE_SIZE_BYTES,
            "backend": "primary",
            "dedup_scope": "local",
            "worm": false,
            "created_at": "2026-05-13T00:00:00Z",
        });
        std::fs::write(vol_dir.join("manifest.json"), raw.to_string()).unwrap();
        // Page index has to exist for `open()` later but not for
        // `load()`; touch the file with the right header bytes.
        let idx_path = crate::page_index::PageIndex::path_for(&vol_dir);
        crate::page_index::PageIndex::create(
            &idx_path,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF,
            ],
            u64::from(DEFAULT_PAGE_SIZE_BYTES),
        )
        .unwrap();

        let loaded = VolumeManifest::load(dir.path(), "vol1").unwrap();
        assert_eq!(loaded.schema_version, VOLUME_SCHEMA_VERSION);
        assert!(loaded.encryption.is_none());
    }

    #[test]
    fn manifest_load_rejects_future_schema() {
        let dir = TempDir::new().unwrap();
        let vol_dir = VolumeManifest::dir_for(dir.path(), "vol1");
        std::fs::create_dir_all(&vol_dir).unwrap();
        let raw = serde_json::json!({
            "schema_version": 999,
            "name": "vol1",
            "uuid": "00112233445566778899aabbccddeeff",
            "size_bytes": 1_073_741_824u64,
            "sector_bytes": DEFAULT_SECTOR_BYTES,
            "page_size_bytes": DEFAULT_PAGE_SIZE_BYTES,
            "backend": "primary",
            "dedup_scope": "local",
            "worm": false,
            "created_at": "2026-05-13T00:00:00Z",
        });
        std::fs::write(vol_dir.join("manifest.json"), raw.to_string()).unwrap();
        let err = VolumeManifest::load(dir.path(), "vol1").unwrap_err();
        assert!(matches!(err, VolumeError::SchemaMismatch { .. }));
    }
}
