// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-cartridge runtime state — `<root>/runtime.json`.
//!
//! Sidecar to `manifest.json`. Holds everything the daemon mutates
//! while running: partition layout (mutated by FORMAT MEDIUM /
//! ERASE), active partition (mutated by LOCATE), pending partition
//! layout (staged by MODE SELECT 0x11), host-set capacity
//! proportion (SET CAPACITY), the index-backup epoch map (refreshed
//! by every cloud manifest backup), and four lifetime byte counters
//! — host/backend, written/read — that show the host-vs-backend
//! contrast (dedup + compression saving on the write side, cache
//! effectiveness on the read side).
//!
//! Splitting these out of the manifest leaves `manifest.json`
//! byte-stable after `cartridge create`, so out-of-band identity
//! mutations don't race the hot-path persist.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Partition, PendingPartitionLayout};
use crate::errors::{Result, SmcError};
use crate::index_backup::IndexEpoch;

/// One host-written MAM attribute (SCSI WRITE ATTRIBUTE 0x8D),
/// persisted in the runtime sidecar so it survives UNLOAD/reload.
/// The value bytes are hex-encoded in JSON (`mam_value_hex` below) to
/// stay human-readable and avoid serde_json's per-byte numeric-array
/// bloat — the same lowercase-hex convention the manifest uses for
/// its UUID.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub(super) struct MamAttrValue {
    /// SSC-4 attribute format code: 0 = binary, 1 = ASCII, 2 = text.
    pub format: u8,
    /// Raw attribute value as written by the host.
    #[serde(with = "mam_value_hex")]
    pub value: Vec<u8>,
}

/// Lowercase-hex serde for a MAM attribute value. `hex::encode` /
/// `hex::decode` are available without the crate's `serde` feature,
/// so this keeps `hex = "0.4"` (default features) as the only dep.
mod mam_value_hex {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).map_err(D::Error::custom)
    }
}

/// On-disk runtime state for one cartridge. Persisted alongside
/// `manifest.json` in the cartridge root; rewritten at every
/// runtime-mutating boundary (`Cartridge::persist_runtime`).
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub(super) struct Runtime {
    /// Tape partitions. `partitions[0]` is always the index/data
    /// partition for unpartitioned tapes; LTFS adds `partitions[1]`
    /// via FORMAT MEDIUM.
    #[serde(default)]
    pub partitions: Vec<Partition>,

    /// Index into `partitions` of the currently active partition.
    /// Selected by LOCATE with the CP bit; reported by READ POSITION.
    #[serde(default)]
    pub active_partition: u8,

    /// Pending partition layout staged by MODE SELECT page 0x11. Applied
    /// by FORMAT MEDIUM (FORMAT field = 0x01). Cleared after format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_partition_layout: Option<PendingPartitionLayout>,

    /// Host-set capacity proportion from SCSI SET CAPACITY (CDB 0x0B
    /// bytes 2-3). 16-bit fraction of native capacity in
    /// `[1, 65535]`; `0` and `65535` both mean "full native". The
    /// effective end-of-medium is
    /// `capacity_gb_bytes * proportion / 65535`. Defaulting to
    /// `65535` so legacy sidecars written before this field shipped
    /// read back as full-native.
    #[serde(default = "default_set_capacity_proportion")]
    pub set_capacity_proportion: u16,

    /// Per-index-file restore epoch. Stamped by the manifest-backup
    /// path after each successful upload pass for `chunks.idx` and
    /// each `blocks-p<N>.idx`. Restore reads this map to learn the
    /// page count + file size needed to stitch a pristine local copy
    /// back together — see `index_backup.rs` for the wire format and
    /// `docs/reference/SPEC.md` § Index Page Backup.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub index_epoch: BTreeMap<String, IndexEpoch>,

    /// Host-written MAM attributes (SCSI WRITE ATTRIBUTE 0x8D), keyed
    /// by attribute id. Only host-writable ids (SSC-4 ranges
    /// 0x0800-0x0BFF and 0x1400-0x17FF) land here; device/medium
    /// read-only ids are rejected at the SCSI layer and never reach
    /// this map. `BTreeMap` keeps the ascending-id order READ
    /// ATTRIBUTE requires. Survives UNLOAD/reload; cleared on ERASE /
    /// FORMAT MEDIUM. `#[serde(default)]` so pre-#60 `runtime.json`
    /// files load with an empty map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mam_attributes: BTreeMap<u16, MamAttrValue>,

    /// Lifetime count of times this volume has been mounted (loaded
    /// into any drive). Bumped once per `DriveManager::load_cartridge`;
    /// monotonic for the cartridge's whole life — survives ERASE /
    /// FORMAT MEDIUM (a mount is a physical event, not a property of
    /// the medium's contents). Surfaced via LOG SENSE 0x17 Volume
    /// Statistics (Volume Mounts) and 0x30 Tape Usage (thread count).
    /// `#[serde(default)]` so a pre-counter `runtime.json` reads back
    /// as 0.
    #[serde(default)]
    pub mount_count: u64,

    /// Lifetime host bytes written into this cartridge — pre-dedup,
    /// pre-compression, pre-encryption. Monotonic for the cartridge's
    /// life; reset to 0 on ERASE / FORMAT MEDIUM.
    #[serde(default)]
    pub host_bytes_written: u64,

    /// Lifetime plaintext bytes served to the host on READ — counted
    /// after drive-side decrypt + decompress. Counts reads satisfied
    /// from the local pool and from a cloud refetch alike.
    /// `#[serde(default)]` so a pre-counter `runtime.json` (which had
    /// only `host_bytes_written`) reads this back as 0.
    #[serde(default)]
    pub host_bytes_read: u64,

    /// Lifetime on-wire bytes PUT to cloud for this cartridge's
    /// chunks — post-dedup, post-compression, i.e. the real backend
    /// storage cost. `#[serde(default)]` for the same legacy-file
    /// reason.
    #[serde(default)]
    pub backend_bytes_written: u64,

    /// Lifetime bytes fetched from cloud on a chunk cache miss
    /// (live-session prefetch hook + the async refetch path). The
    /// downloaded chunk bytes as they land in the pool.
    /// `#[serde(default)]` for the same legacy-file reason.
    #[serde(default)]
    pub backend_bytes_read: u64,
}

fn default_set_capacity_proportion() -> u16 {
    u16::MAX
}

impl Runtime {
    pub const FILENAME: &'static str = "runtime.json";

    pub fn path_for(root: &Path) -> PathBuf {
        root.join(Self::FILENAME)
    }

    /// Build a fresh runtime sidecar for a brand-new cartridge: one
    /// data partition, full native capacity, no pending format, no
    /// byte counters recorded yet.
    pub fn new_blank() -> Self {
        Self {
            partitions: vec![Partition::default()],
            active_partition: 0,
            pending_partition_layout: None,
            set_capacity_proportion: u16::MAX,
            index_epoch: BTreeMap::new(),
            mam_attributes: BTreeMap::new(),
            mount_count: 0,
            host_bytes_written: 0,
            host_bytes_read: 0,
            backend_bytes_written: 0,
            backend_bytes_read: 0,
        }
    }

    /// Atomic write: tmp + fsync + rename, matching the manifest
    /// persistence pattern in `Cartridge::persist_runtime`.
    pub fn persist(&self, root: &Path) -> Result<()> {
        let tmp = root.join("runtime.json.tmp");
        let finalp = root.join(Self::FILENAME);
        {
            let mut f = std::io::BufWriter::with_capacity(64 * 1024, File::create(&tmp)?);
            serde_json::to_writer(&mut f, self)?;
            f.flush()?;
        }
        fs::rename(tmp, finalp)?;
        Ok(())
    }

    /// Load runtime state from `<root>/runtime.json`. Returns
    /// `SmcError::InvalidOp` if the file is missing — `manifest.json`
    /// existing without `runtime.json` means either an interrupted
    /// `cartridge create` or hand-rolled corruption; refuse rather
    /// than silently zero-initialize.
    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path_for(root);
        if !path.is_file() {
            return Err(SmcError::InvalidOp(
                "runtime.json missing alongside manifest.json — \
                 interrupted `cartridge create` or hand-rolled corruption; \
                 restore the runtime sidecar or recreate the cartridge",
            ));
        }
        let bytes = fs::read(&path)?;
        let r: Self = serde_json::from_slice(&bytes)?;
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_preserves_all_four_counters() {
        let dir = TempDir::new().unwrap();
        let mut r = Runtime::new_blank();
        r.mount_count = 7;
        r.host_bytes_written = 123_456;
        r.host_bytes_read = 789_012;
        r.backend_bytes_written = 64_000;
        r.backend_bytes_read = 32_000;
        r.persist(dir.path()).unwrap();
        let loaded = Runtime::load(dir.path()).unwrap();
        assert_eq!(loaded.mount_count, 7);
        assert_eq!(loaded.host_bytes_written, 123_456);
        assert_eq!(loaded.host_bytes_read, 789_012);
        assert_eq!(loaded.backend_bytes_written, 64_000);
        assert_eq!(loaded.backend_bytes_read, 32_000);
    }

    #[test]
    fn legacy_runtime_json_without_read_counters_defaults_to_zero() {
        // A pre-counter runtime.json carried `host_bytes_written` but
        // none of the three later counters. `#[serde(default)]` must
        // land the missing fields on 0 without failing the load.
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("runtime.json"),
            r#"{"partitions":[{"capacity_mib":0}],"active_partition":0,
                "set_capacity_proportion":65535,"host_bytes_written":42}"#,
        )
        .unwrap();
        let loaded = Runtime::load(dir.path()).unwrap();
        assert_eq!(loaded.host_bytes_written, 42);
        assert_eq!(loaded.host_bytes_read, 0);
        assert_eq!(loaded.backend_bytes_written, 0);
        assert_eq!(loaded.backend_bytes_read, 0);
        // No mount_count field on a legacy sidecar -> 0.
        assert_eq!(loaded.mount_count, 0);
        // No mam_attributes field on a legacy sidecar -> empty map.
        assert!(loaded.mam_attributes.is_empty());
    }

    #[test]
    fn mam_attributes_round_trip_as_hex() {
        let dir = TempDir::new().unwrap();
        let mut r = Runtime::new_blank();
        r.mam_attributes.insert(
            0x0801,
            MamAttrValue {
                format: 1,
                value: b"Bareos".to_vec(),
            },
        );
        // A binary value with bytes that are not valid ASCII, to prove
        // the hex round-trip is byte-exact (not lossy text coercion).
        r.mam_attributes.insert(
            0x1400,
            MamAttrValue {
                format: 0,
                value: vec![0x00, 0xFF, 0xAB, 0x10],
            },
        );
        r.persist(dir.path()).unwrap();

        // Value bytes are stored as a lowercase-hex string, not a JSON
        // numeric array.
        let raw = std::fs::read_to_string(dir.path().join("runtime.json")).unwrap();
        assert!(raw.contains("00ffab10"), "value should be hex: {raw}");

        let loaded = Runtime::load(dir.path()).unwrap();
        assert_eq!(
            loaded.mam_attributes.get(&0x0801).unwrap(),
            &MamAttrValue {
                format: 1,
                value: b"Bareos".to_vec()
            }
        );
        assert_eq!(
            loaded.mam_attributes.get(&0x1400).unwrap().value,
            vec![0x00, 0xFF, 0xAB, 0x10]
        );
    }
}
