// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// The ten public inventory-mutation verbs (add_or_create_tape,
// load_to_drive, unload_from_drive, move_cartridge, export/import,
// load/unload, remove_from_slot) live in library/inventory.rs to
// keep this file scoped to library bring-up + accessors.
mod inventory;

// Partition lookup/replace verbs and the `resize` slot-renumbering
// state machine (with the three private fan-out helpers
// `take_evicted_payloads` / `count_empty_storage` /
// `pour_into_storage` it owns) live in library/partitions.rs.
mod partitions;

// Declared-vs-persisted topology reconciliation. `open_or_materialize`
// is the daemon's single library bring-up entry point under the
// chassis-into-YAML refactor; `compute_bounds` powers
// `thurvtl library bounds`. The four SMC element bases + the
// `MAIL_SLOT_COUNT = 1` constant live here as private constants —
// no operator surface for those values anywhere.
pub mod reconcile;

// Cross-region DR restore driver — discover cartridges in a storage
// bucket and reconstruct the local cartridge directories via the
// existing single-cartridge cold-bucket path. The CLI's
// `library restore` verb wires this with the YAML conffile's
// `storage.backends:` block + an inventory rebuild.
pub mod restore;

// Pull a frozen archive (produced by `cartridge_archive`) back into a
// live cartridge. Layered on top of the same library inventory
// primitives.
pub mod restore_archive;

use crate::cartridge::Cartridge;
use crate::errors::{Result, SmcError};
use core_stream::DriveTopology;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

/// Per-product abstraction over "what tape device am I serving SCSI for?".
///
/// Bundles the read-only identity / topology surface that the shared
/// SSC drive-LUN dispatcher needs:
/// - chassis + per-drive serial strings (INQUIRY VPDs `0x80` / `0xB1` / `0xB3`),
/// - LTO generation + firmware revision (INQUIRY VPDs `0xB0` / `0xB2`,
///   LOG SENSE drive-status pages),
/// - partition membership (LUN-level partition fence in `dispatch_scsi`,
///   VPD `0x80` `_LLNN` partition suffix, REPORT LUNS partition filter).
///
/// `thurvtl` implements this on a wrapper around
/// `Arc<Mutex<Library>>` that locks internally per call (multi-drive,
/// optionally partitioned). SMC-side methods (READ ELEMENT STATUS,
/// MOVE MEDIUM, …) stay on `Library`'s inherent impl — they're
/// library-only by design.
///
/// Methods return *owned* data (`String`, `Vec<u32>`) so the
/// `Library`-backed impl can lock briefly per call without leaking
/// guards through borrows. The trait surface is small and not on a
/// hot path (drive-LUN identity calls are sub-millisecond INQUIRY /
/// LOG SENSE / REPORT LUNS work; long-running WRITE / READ never
/// touches it), so the per-call allocation cost is negligible.
pub trait TapeDeviceFacade: DriveTopology + Send + Sync {
    /// Stable chassis (automation-device) serial. Surfaced via
    /// INQUIRY VPD `0xB3` on every drive LUN, and as the prefix of
    /// VPD `0x80` Unit Serial Number on the changer LUN.
    fn chassis_serial(&self) -> String;

    /// Manufacturer-assigned per-drive serial. Surfaced via INQUIRY
    /// VPD `0xB1` (drive LUN) and LOG SENSE page `0x14` parameter
    /// `0x0040`. `None` when the drive doesn't exist.
    fn drive_mfg_serial(&self, drive_id: u32) -> Option<String>;

    /// LTO generation (1-9) for the drive. Used to gate cartridge
    /// generation compatibility (`Library::set_library_lto_generation`)
    /// and to select per-LTO defaults in INQUIRY / mode pages.
    fn lto_generation(&self) -> u8;

    /// 4-byte revision string reported in INQUIRY byte 32..36. Library
    /// returns the operator-set firmware override or the per-LTO
    /// default.
    fn drive_firmware(&self) -> String;

    /// One-based partition index used in the SCSI VPD `0x80`
    /// `_LLNN` Unit Serial Number suffix. Returns `1` for unpartitioned
    /// libraries (non-partitioned chassis report as Partition 1).
    fn partition_index_one_based(&self, partition_name: Option<&str>) -> u8 {
        let _ = partition_name;
        1
    }

    /// Drive ids belonging to the named partition. `None` when the
    /// partition doesn't exist. The default impl returns `None`
    /// (no partitioning) — overridden by `Library`.
    fn partition_drive_ids(&self, partition_name: &str) -> Option<Vec<u32>> {
        let _ = partition_name;
        None
    }

    /// Drive ids visible to the session, optionally filtered by the
    /// session-bound partition. Default impl returns empty
    /// (changer-only chassis with no drives). LUN-byte assembly is the
    /// consumer's concern.
    fn drive_ids_in_partition(&self, partition_name: Option<&str>) -> Vec<u32> {
        let _ = partition_name;
        Vec::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotInfo {
    pub id: u32,
    pub barcode: Option<String>, // e.g., "TAPE001L8"
    pub occupied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailSlotInfo {
    pub id: u32,
    pub barcode: Option<String>,
    pub occupied: bool,
    pub accessible: bool, // false = closed, true = open for import/export
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveInfo {
    pub id: u32,
    pub barcode: Option<String>,
    pub occupied: bool,
    pub home_slot: Option<u16>, // Where cartridge was loaded from
    /// Manufacturer-assigned serial number, surfaced via INQUIRY
    /// VPD `0xB1` and LOG SENSE page `0x14` parameter `0x0040`.
    /// Generated once at library init / when a drive is added via
    /// `library modify`; persisted across daemon restarts so backup-
    /// software catalogs stay stable. None on libraries that pre-
    /// date the field — the daemon falls back to a deterministic
    /// per-LUN literal in that case (legacy behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mfg_serial: Option<String>,
}

// Old single-file manifest (for migration)
#[derive(Debug, Serialize, Deserialize)]
struct OldLibraryManifest {
    storage_slots: Vec<SlotInfo>,
    mail_slots: Vec<MailSlotInfo>,
    drives: Vec<DriveInfo>,
    tapes_dir: String,
    lto_generation: u8,
}

// New two-file architecture

/// Operator-declared chassis intent, deserialized from the
/// `library:` block of `thurvtl.yaml`. Mirror of the YAML schema; the
/// daemon stores this verbatim in `library.json`'s `declared` stanza
/// at successful reconcile and diffs YAML against it on subsequent
/// starts.
///
/// Independent from `LibraryTopology` (which also carries the minted
/// chassis_serial + four element bases that the operator never sees).
/// Phase 1 of the chassis-into-YAML refactor moves this struct into
/// `library/reconcile.rs` along with the diff / materialize entry
/// points; Phase 0 leaves it here so the type is in tree without yet
/// being wired to any caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredTopology {
    pub num_storage_slots: u32,
    pub num_drives: u32,
    pub lto_generation: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
}

/// library.json - Static topology (immutable at runtime)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryTopology {
    pub version: u32, // File format version (currently 1)
    pub num_storage_slots: u32,
    pub num_mail_slots: u32,
    pub num_drives: u32,
    pub lto_generation: u8,
    /// Optional firmware revision string reported in INQUIRY byte
    /// 32..36. None → use the per-LTO default
    /// (`default_firmware_for_lto`). 1-4 ASCII chars; padded with
    /// spaces to fill the 4-byte SCSI revision field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// Stable chassis (automation-device) serial. Persisted at
    /// `library init` time so the same string survives daemon
    /// restarts and is distinct across deployments. Surfaced via
    /// INQUIRY VPD `0xB3` on every drive LUN, and as the prefix of
    /// the per-partition VPD `0x80` Unit Serial Number on the
    /// changer LUN. None on legacy libraries that pre-date this
    /// field — `chassis_serial()` falls back to the historical
    /// literal in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chassis_serial: Option<String>,
    /// Logical partitions carved out of the chassis. Empty = legacy
    /// single-partition library (every slot/drive belongs to the
    /// implicit "default" partition; no SCSI-layer fence). When
    /// non-empty every storage slot, mail slot, and drive must
    /// belong to exactly one partition; an iSCSI session bound to a
    /// partition (via CHAP user) is fenced to its slot/drive set in
    /// MOVE MEDIUM and READ ELEMENT STATUS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<LibraryPartition>,
    /// SMC-3 element addresses, set at `library init` and immutable
    /// thereafter — barcoded inventory entries reference these,
    /// changing them would orphan every loaded element. Validated
    /// at init for u16 overflow and pairwise non-overlap across
    /// transport / storage / mail / drives.
    pub transport_base: u16,
    pub storage_base: u16,
    pub import_export_base: u16,
    pub data_transfer_base: u16,
}

/// Half-open `[start, end)` element-id range. `start <= end`; an empty
/// range (`start == end`) is permitted (e.g. a partition with no mail
/// slots).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SlotRange {
    pub start: u32,
    pub end: u32,
}

impl SlotRange {
    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }
    pub fn contains(&self, id: u32) -> bool {
        id >= self.start && id < self.end
    }
    pub fn overlaps(&self, other: &SlotRange) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.start < other.end && other.start < self.end
    }
}

/// A single logical partition. Slot/mail-slot ranges are half-open
/// `[start, end)` bands of the chassis-level address space; `drives`
/// is an explicit set (drive subsets are not contiguous in
/// general). Partition names are unique within a library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryPartition {
    pub name: String,
    pub storage_slots: SlotRange,
    #[serde(default)]
    pub mail_slots: SlotRange,
    pub drives: Vec<u32>,
}

/// Validate a partition layout against a library topology. Returns
/// `Ok(())` if every partition is well-formed and the union covers
/// every slot/drive exactly once. Empty `partitions` slice is OK
/// (legacy single-partition mode).
pub fn validate_partitions(
    partitions: &[LibraryPartition],
    num_storage_slots: u32,
    num_mail_slots: u32,
    num_drives: u32,
) -> Result<()> {
    if partitions.is_empty() {
        return Ok(());
    }

    use std::collections::HashSet;
    let mut names: HashSet<&str> = HashSet::new();
    let mut storage_seen: Vec<bool> = vec![false; num_storage_slots as usize];
    let mut mail_seen: Vec<bool> = vec![false; num_mail_slots as usize];
    let mut drive_seen: Vec<bool> = vec![false; num_drives as usize];

    for p in partitions {
        let trimmed = p.name.trim();
        if trimmed.is_empty() {
            return Err(SmcError::LibraryConfig(
                "partition name must be non-empty".into(),
            ));
        }
        if trimmed.len() > 64 {
            return Err(SmcError::LibraryConfig(format!(
                "partition name '{}' too long (max 64 chars)",
                trimmed
            )));
        }
        if !names.insert(trimmed) {
            return Err(SmcError::LibraryConfig(format!(
                "duplicate partition name '{}'",
                trimmed
            )));
        }

        if p.storage_slots.start > p.storage_slots.end {
            return Err(SmcError::LibraryConfig(format!(
                "partition '{}' storage range start > end",
                p.name
            )));
        }
        if p.storage_slots.end > num_storage_slots {
            return Err(SmcError::LibraryConfig(format!(
                "partition '{}' storage range [{}, {}) exceeds {} storage slots",
                p.name, p.storage_slots.start, p.storage_slots.end, num_storage_slots
            )));
        }
        for id in p.storage_slots.start..p.storage_slots.end {
            let cell = &mut storage_seen[id as usize];
            if *cell {
                return Err(SmcError::LibraryConfig(format!(
                    "partition '{}' storage slot {} overlaps another partition",
                    p.name, id
                )));
            }
            *cell = true;
        }

        if p.mail_slots.start > p.mail_slots.end {
            return Err(SmcError::LibraryConfig(format!(
                "partition '{}' mail range start > end",
                p.name
            )));
        }
        if p.mail_slots.end > num_mail_slots {
            return Err(SmcError::LibraryConfig(format!(
                "partition '{}' mail range [{}, {}) exceeds {} mail slots",
                p.name, p.mail_slots.start, p.mail_slots.end, num_mail_slots
            )));
        }
        for id in p.mail_slots.start..p.mail_slots.end {
            let cell = &mut mail_seen[id as usize];
            if *cell {
                return Err(SmcError::LibraryConfig(format!(
                    "partition '{}' mail slot {} overlaps another partition",
                    p.name, id
                )));
            }
            *cell = true;
        }

        for &d in &p.drives {
            if d >= num_drives {
                return Err(SmcError::LibraryConfig(format!(
                    "partition '{}' references drive {} but only {} drives configured",
                    p.name, d, num_drives
                )));
            }
            let cell = &mut drive_seen[d as usize];
            if *cell {
                return Err(SmcError::LibraryConfig(format!(
                    "partition '{}' drive {} overlaps another partition",
                    p.name, d
                )));
            }
            *cell = true;
        }
    }

    if let Some(missing) = storage_seen.iter().position(|s| !s) {
        return Err(SmcError::LibraryConfig(format!(
            "storage slot {} is not covered by any partition (full coverage required when partitions are defined)",
            missing
        )));
    }
    if let Some(missing) = mail_seen.iter().position(|s| !s) {
        return Err(SmcError::LibraryConfig(format!(
            "mail slot {} is not covered by any partition",
            missing
        )));
    }
    if let Some(missing) = drive_seen.iter().position(|s| !s) {
        return Err(SmcError::LibraryConfig(format!(
            "drive {} is not covered by any partition",
            missing
        )));
    }

    Ok(())
}

/// Default INQUIRY revision string for a given LTO generation. The
/// VTL reports a distinctive `TVL<gen>` code by default rather than a
/// real drive-vendor firmware revision, on the principle that we
/// shouldn't claim to be firmware we are not — that would inherit any
/// reputation / CVEs / known-bug workarounds the real revision
/// carries. Operators who need a specific firmware string for a
/// backup product's compatibility matrix can override via
/// `library init --firmware <CODE>`.
pub fn default_firmware_for_lto(lto_generation: u8) -> &'static str {
    match lto_generation {
        7 => "TVL7",
        8 => "TVL8",
        _ => "TVL0",
    }
}

/// Legacy chassis serial, used when a pre-existing `library.json` has
/// no `chassis_serial` field. Matches the literal that was hardcoded
/// in `vtl/daemon/src/iscsi/protocol.rs` before the field landed.
pub const LEGACY_CHASSIS_SERIAL: &str = "THUR-CHG-001";

/// Maximum chassis-serial length. A 14-byte chassis serial + `_LLNN`
/// partition suffix = 19 bytes total in VPD `0x80`. Real
/// backup-software cataloging tends to
/// assume serials ≤ 20 chars; a longer chassis serial would push the
/// partition-suffixed VPD `0x80` past that threshold and risk
/// truncation in third-party tools.
pub const MAX_CHASSIS_SERIAL_LEN: usize = 14;

/// Maximum drive manufacturer-serial length. VPD `0xB1` is
/// 32-byte ASCII but for interop we keep
/// drive serials ≤ 14 bytes — same shape as the chassis serial.
pub const MAX_DRIVE_MFG_SERIAL_LEN: usize = 14;

/// Hex-encode the first `nibbles` nibbles of a BLAKE3 digest of
/// CSPRNG-sampled entropy. Used for chassis + drive serial generation
/// so all three derivations (chassis, drive, partition) share one
/// hashing primitive.
fn random_blake3_hex(nibbles: usize) -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let entropy: [u8; 32] = rng.random();
    let h = blake3::hash(&entropy);
    let bytes = h.as_bytes();
    let bytes_needed = nibbles.div_ceil(2);
    let mut s = String::with_capacity(bytes_needed * 2);
    for b in &bytes[..bytes_needed] {
        use std::fmt::Write;
        write!(&mut s, "{:02X}", b).expect("write to String never fails");
    }
    s.truncate(nibbles);
    s
}

/// Generate a fresh chassis serial. Format: `TVLxxxxxxxxxxx` where
/// `TVL` is the VTL serial prefix (uppercase 3-char alphanumeric, no
/// separator) and the remaining 11 chars are uppercase hex from
/// BLAKE3 of a CSPRNG sample (44 bits of effective entropy). 14 chars
/// total, leaving room for the `_LLNN` partition suffix in VPD `0x80`.
pub fn generate_chassis_serial() -> String {
    format!("TVL{}", random_blake3_hex(11))
}

/// Generate a fresh drive manufacturer serial. Format: `TVLxxxxxxx`
/// (10 chars total: 3-char serial prefix + 7 hex chars). Random per
/// drive so two VTL deployments running the same chassis topology
/// don't collide on identical drive serials in backup-software
/// catalogs.
pub fn generate_drive_mfg_serial() -> String {
    format!("TVL{}", random_blake3_hex(7))
}

/// Validate an operator-supplied chassis serial. 1 to
/// `MAX_CHASSIS_SERIAL_LEN` printable-ASCII characters.
pub fn validate_chassis_serial(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > MAX_CHASSIS_SERIAL_LEN {
        return Err(SmcError::InvalidOp(
            "chassis_serial must be 1 to 14 ASCII characters",
        ));
    }
    if !s.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err(SmcError::InvalidOp(
            "chassis_serial must contain only printable ASCII characters",
        ));
    }
    Ok(())
}

/// Derive the SCSI Unit Serial Number suffix for a logical partition.
/// Returns 6 uppercase hex chars derived from a BLAKE3 hash of
/// `chassis_serial || partition_name`. Stable across daemon restarts
/// and reproducible — two Thur VTL processes with the same
/// `library.json` derive the same serials.
pub fn partition_serial_suffix(chassis_serial: &str, partition_name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(chassis_serial.as_bytes());
    hasher.update(b"|");
    hasher.update(partition_name.as_bytes());
    let h = hasher.finalize();
    let bytes = h.as_bytes();
    format!("{:02X}{:02X}{:02X}", bytes[0], bytes[1], bytes[2])
}

/// Validate a firmware revision string for SCSI INQUIRY use.
/// Must be 1-4 ASCII printable characters. Returns an error otherwise.
pub fn validate_firmware(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 4 {
        return Err(SmcError::InvalidOp(
            "firmware must be 1 to 4 ASCII characters",
        ));
    }
    if !s.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err(SmcError::InvalidOp(
            "firmware must contain only printable ASCII characters",
        ));
    }
    Ok(())
}

/// Validate that the four SMC-3 element-type address ranges
/// (transport, storage, import/export, data transfer) fit in u16 and
/// do not overlap each other. Called from `Library::initialize`.
pub fn validate_element_address_layout(
    transport_base: u16,
    storage_base: u16,
    num_storage_slots: u32,
    import_export_base: u16,
    num_mail_slots: u32,
    data_transfer_base: u16,
    num_drives: u32,
) -> Result<()> {
    // Half-open [base, base+count) bounded in u32 to detect u16 overflow
    // up front. Transport is a single address (count = 1).
    fn range(name: &str, base: u16, count: u32) -> Result<(u32, u32, &str)> {
        let end = base as u32 + count;
        if end > u16::MAX as u32 + 1 {
            return Err(SmcError::LibraryConfig(format!(
                "{name} element range {}..{} exceeds u16 address space",
                base, end
            )));
        }
        Ok((base as u32, end, name))
    }
    let ranges = [
        range("transport", transport_base, 1)?,
        range("storage", storage_base, num_storage_slots)?,
        range("mail", import_export_base, num_mail_slots)?,
        range("drives", data_transfer_base, num_drives)?,
    ];
    for i in 0..ranges.len() {
        for j in (i + 1)..ranges.len() {
            let (a_start, a_end, a_name) = ranges[i];
            let (b_start, b_end, b_name) = ranges[j];
            if a_start < b_end && b_start < a_end {
                return Err(SmcError::LibraryConfig(format!(
                    "element address ranges overlap: {a_name} {a_start}..{a_end} and {b_name} {b_start}..{b_end}"
                )));
            }
        }
    }
    Ok(())
}

/// inventory.json - Dynamic inventory (mutable state)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryInventory {
    pub version: u32, // File format version (currently 1)
    pub storage_slots: Vec<SlotInfo>,
    pub mail_slots: Vec<MailSlotInfo>,
    pub drives: Vec<DriveInfo>,
}

pub struct Library {
    root: PathBuf,      // <data_dir>/library
    tapes_dir: PathBuf, // <data_dir>/tapes
    topology: LibraryTopology,
    inventory: LibraryInventory,
}

pub struct LoadedCartridge {
    pub slot_id: u32,
    pub barcode: String,
    pub cartridge: Cartridge,
}

impl Library {
    /// Open an existing library (manifest must exist).
    /// Use `initialize()` to create a new library.
    /// Layout:
    ///   <root>/library.json (topology)
    ///   <root>/inventory.json (state)
    ///   tapes live under `tapes_dir`
    ///
    /// Automatically migrates old single-file format to two-file format.
    pub fn open<P: AsRef<Path>>(root: P, tapes_dir: P) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let tapes_dir = tapes_dir.as_ref().to_path_buf();

        let lib_path = root.join("library.json");
        let inv_path = root.join("inventory.json");

        if !lib_path.exists() {
            return Err(SmcError::InvalidOp(
                "library not materialized: configure `library:` in /etc/thurvtl/thurvtl.yaml and start the daemon (the daemon materializes library.json on first start from the YAML block)",
            ));
        }

        // Check if we need to migrate from old format
        if lib_path.exists() && !inv_path.exists() {
            tracing::info!("Migrating old library.json to two-file format");
            Self::migrate_old_format(&root, &lib_path, &inv_path)?;
            tracing::info!("Migration complete");
        }

        // Load two-file format. Schema versioning: v1 is the
        // pre-refactor flat shape (matches `LibraryTopology` directly);
        // v2 is the `declared`/`minted` split written by
        // `reconcile::open_or_materialize`. Daemon-down callers stay
        // working under v2 by flattening through `topology_from_disk`.
        let raw_topology = fs::read_to_string(&lib_path)?;
        let probe: serde_json::Value = serde_json::from_str(&raw_topology)?;
        let version = probe.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
        let topology: LibraryTopology = if version == reconcile::DISK_SCHEMA_VERSION as u64 {
            let disk: reconcile::DiskV2 = serde_json::from_str(&raw_topology)?;
            reconcile::topology_from_disk(disk)
        } else {
            serde_json::from_str(&raw_topology)?
        };
        let inventory: LibraryInventory = serde_json::from_str(&fs::read_to_string(&inv_path)?)?;

        Ok(Self {
            root,
            tapes_dir,
            topology,
            inventory,
        })
    }

    /// Migrate old single-file library.json to two-file format
    fn migrate_old_format(_root: &Path, lib_path: &Path, inv_path: &Path) -> Result<()> {
        let txt = fs::read_to_string(lib_path)?;
        let old: OldLibraryManifest = serde_json::from_str(&txt)?;

        // Create topology. No chassis_serial on migrated topologies —
        // `chassis_serial()` falls back to LEGACY_CHASSIS_SERIAL so
        // existing backup-software catalogs see the same value they
        // saw before. Operator can override via `library modify
        // --chassis-serial` to mint a fresh one. Element bases default
        // to the historical values that this build used to
        // hardcode (transport=0, storage=1001, mail=101, drives=1) so
        // a migrated library reports the same element addresses
        // backup software previously saw.
        let topology = LibraryTopology {
            version: 1,
            num_storage_slots: old.storage_slots.len() as u32,
            num_mail_slots: old.mail_slots.len() as u32,
            num_drives: old.drives.len() as u32,
            lto_generation: old.lto_generation,
            firmware: None,
            chassis_serial: None,
            partitions: Vec::new(),
            transport_base: 0,
            storage_base: 1001,
            import_export_base: 101,
            data_transfer_base: 1,
        };

        // Create inventory
        let inventory = LibraryInventory {
            version: 1,
            storage_slots: old.storage_slots,
            mail_slots: old.mail_slots,
            drives: old.drives,
        };

        // Write new files with locking
        Self::write_locked(lib_path, &serde_json::to_string_pretty(&topology)?)?;
        Self::write_locked(inv_path, &serde_json::to_string_pretty(&inventory)?)?;

        Ok(())
    }

    /// Initialize a new library with specified topology.
    /// This should only be called via CLI, not from daemon.
    /// A fresh random chassis serial is minted internally and
    /// persisted to `library.json`; operators with a rare migration
    /// need can edit the file directly afterward.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize<P: AsRef<Path>>(
        root: P,
        tapes_dir: P,
        num_storage_slots: u32,
        num_mail_slots: u32,
        num_drives: u32,
        lto_generation: u8,
        firmware: Option<String>,
        transport_base: u16,
        storage_base: u16,
        import_export_base: u16,
        data_transfer_base: u16,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let tapes_dir = tapes_dir.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&tapes_dir)?;

        let lib_path = root.join("library.json");
        let inv_path = root.join("inventory.json");

        if lib_path.exists() {
            return Err(SmcError::InvalidOp(
                "Library already initialized. Use 'thurvtl library modify' to change configuration.",
            ));
        }

        // Caps reflect the spec ceilings, not arbitrary sanity bounds.
        // Storage + mail slots max out at 65535 because SMC-3 element
        // addresses are 16-bit; `validate_element_address_layout`
        // below additionally enforces that the per-type ranges fit
        // and don't overlap within that 0..=65535 space. Drives cap
        // at 255 because the iSCSI transport currently parses LUNs
        // via single-byte peripheral-device addressing (SAM Method 0,
        // `pdu.lun[1]` in shared/iscsi/src/transport.rs) — 256 LUNs
        // total, LUN 0 reserved for the changer, leaving 1..=255 for
        // drive LUNs. Lifting that ceiling needs flat-space LUN
        // encoding in the transport, not just a knob here.
        if !(1..=65535).contains(&num_storage_slots) {
            return Err(SmcError::InvalidOp(
                "num_storage_slots must be between 1 and 65535",
            ));
        }
        if num_mail_slots > 65535 {
            return Err(SmcError::InvalidOp(
                "num_mail_slots must be between 0 and 65535",
            ));
        }
        if !(1..=255).contains(&num_drives) {
            return Err(SmcError::InvalidOp("num_drives must be between 1 and 255"));
        }
        // VTL ships as a clean LTO-8 drive. We keep the LTO-7
        // descriptor in REPORT DENSITY SUPPORT (matching real LTO-8
        // drive backwards-read advertisement) but don't model
        // LTO-7 cartridge creation. See docs/reference/CONFORMANCE_SCSI.md.
        if lto_generation != 8 {
            return Err(SmcError::InvalidOp("lto_generation must be 8"));
        }
        if let Some(ref fw) = firmware {
            validate_firmware(fw)?;
        }
        validate_element_address_layout(
            transport_base,
            storage_base,
            num_storage_slots,
            import_export_base,
            num_mail_slots,
            data_transfer_base,
            num_drives,
        )?;

        // Create topology
        let topology = LibraryTopology {
            version: 1,
            num_storage_slots,
            num_mail_slots,
            num_drives,
            lto_generation,
            firmware,
            chassis_serial: Some(generate_chassis_serial()),
            partitions: Vec::new(),
            transport_base,
            storage_base,
            import_export_base,
            data_transfer_base,
        };

        // Create empty inventory
        let mut storage_slots = Vec::with_capacity(num_storage_slots as usize);
        for i in 0..num_storage_slots {
            storage_slots.push(SlotInfo {
                id: i,
                barcode: None,
                occupied: false,
            });
        }

        let mut mail_slots = Vec::with_capacity(num_mail_slots as usize);
        for i in 0..num_mail_slots {
            mail_slots.push(MailSlotInfo {
                id: i,
                barcode: None,
                occupied: false,
                accessible: true, // Always accessible in virtual library
            });
        }

        let mut drives = Vec::with_capacity(num_drives as usize);
        for i in 0..num_drives {
            drives.push(DriveInfo {
                id: i,
                barcode: None,
                occupied: false,
                home_slot: None,
                mfg_serial: Some(generate_drive_mfg_serial()),
            });
        }

        let inventory = LibraryInventory {
            version: 1,
            storage_slots,
            mail_slots,
            drives,
        };

        // Persist both files with locking
        Self::write_locked(&lib_path, &serde_json::to_string_pretty(&topology)?)?;
        Self::write_locked(&inv_path, &serde_json::to_string_pretty(&inventory)?)?;

        Ok(Self {
            root,
            tapes_dir,
            topology,
            inventory,
        })
    }

    /// Helper function to write a file with exclusive locking and atomic rename.
    ///
    /// Pre-Batch-F this locked `path` itself, but `fs::rename` replaces
    /// the inode under the locked file descriptor — the new inode
    /// inherits no lock. Two CLI processes could each `lock_exclusive`
    /// the *current* file (succeeding because each holds a different
    /// inode after the first rename), then both rename, with the
    /// second silently overwriting the first. Locking a separate
    /// sentinel file (`<basename>.lock`) that *no one ever renames*
    /// gives a stable inode for the whole read-modify-write window
    /// across both processes.
    fn write_locked(path: &Path, content: &str) -> Result<()> {
        let tmp_path = path.with_extension("tmp");

        let lock_path = {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let basename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("library");
            parent.join(format!(".{basename}.lock"))
        };

        // Create the sentinel lockfile. It is never the rename target,
        // so its inode is stable for the lifetime of the directory —
        // every concurrent writer locks the same inode and serializes
        // properly. The lockfile is left in place after the write
        // (next run reuses it); it carries no payload.
        let lock_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        lock_file.lock_exclusive()?;

        // Write to temporary file
        fs::write(&tmp_path, content)?;

        // Atomic rename (replaces target file)
        fs::rename(&tmp_path, path)?;

        // Lock automatically released on drop
        Ok(())
    }

    /// Persist inventory (slot assignments) to disk
    /// Note: topology is immutable at runtime, so we only persist inventory
    fn persist(&self) -> Result<()> {
        let inv_path = self.root.join("inventory.json");
        let txt = serde_json::to_string_pretty(&self.inventory)?;
        Self::write_locked(&inv_path, &txt)
    }

    pub fn storage_slots(&self) -> &[SlotInfo] {
        &self.inventory.storage_slots
    }

    pub fn mail_slots(&self) -> &[MailSlotInfo] {
        &self.inventory.mail_slots
    }

    pub fn drives(&self) -> &[DriveInfo] {
        &self.inventory.drives
    }

    pub fn lto_generation(&self) -> u8 {
        self.topology.lto_generation
    }

    pub fn transport_base(&self) -> u16 {
        self.topology.transport_base
    }

    pub fn storage_base(&self) -> u16 {
        self.topology.storage_base
    }

    pub fn import_export_base(&self) -> u16 {
        self.topology.import_export_base
    }

    pub fn data_transfer_base(&self) -> u16 {
        self.topology.data_transfer_base
    }

    // Partition lookup/replace verbs (partitions, partition_for_*,
    // get_partition, partition_index_one_based, set_partitions) live
    // in library/partitions.rs.

    /// Configured INQUIRY revision string for the loaded LTO generation.
    /// Falls back to `default_firmware_for_lto` when the operator hasn't
    /// overridden via `library init --firmware` / `library modify --firmware`.
    pub fn drive_firmware(&self) -> &str {
        self.topology
            .firmware
            .as_deref()
            .unwrap_or_else(|| default_firmware_for_lto(self.topology.lto_generation))
    }

    /// Stable chassis (automation-device) serial. Surfaced via INQUIRY
    /// VPD `0xB3` on every drive LUN and as the prefix of VPD `0x80`
    /// Unit Serial Number on the changer LUN. Falls back to
    /// `LEGACY_CHASSIS_SERIAL` for libraries that pre-date the field
    /// — those continue to look the same to backup-software catalogs.
    pub fn chassis_serial(&self) -> &str {
        self.topology
            .chassis_serial
            .as_deref()
            .unwrap_or(LEGACY_CHASSIS_SERIAL)
    }

    /// Manufacturer-assigned serial for a tape-drive LUN. Surfaced
    /// via INQUIRY VPD `0xB1` and LOG SENSE page `0x14` parameter
    /// `0x0040`. Returns `None` when the drive doesn't exist; falls
    /// back to a deterministic per-drive literal when the inventory
    /// pre-dates the `mfg_serial` field (legacy compatibility).
    pub fn drive_mfg_serial(&self, drive_id: u32) -> Option<String> {
        let drive = self.get_drive(drive_id)?;
        Some(drive.mfg_serial.clone().unwrap_or_else(|| {
            // Legacy pre-field libraries: mirror the historical
            // `format!("THUR-MFG-{:03}", lun)` shape so that backup-
            // software catalogs that recorded those serials still
            // match what we report today. LUN = drive_id + 1 by the
            // standard Thur VTL mapping.
            format!("THUR-MFG-{:03}", drive_id + 1)
        }))
    }

    // `Library::resize` (the slot-renumbering state machine that
    // mutates both topology and inventory under the partition
    // coverage invariant) lives in library/partitions.rs alongside
    // the partition verbs it has to honor.

    pub fn get_storage_slot(&self, slot_id: u32) -> Option<&SlotInfo> {
        self.inventory
            .storage_slots
            .iter()
            .find(|s| s.id == slot_id)
    }

    pub fn get_mail_slot(&self, slot_id: u32) -> Option<&MailSlotInfo> {
        self.inventory.mail_slots.iter().find(|s| s.id == slot_id)
    }

    pub fn get_drive(&self, drive_id: u32) -> Option<&DriveInfo> {
        self.inventory.drives.iter().find(|d| d.id == drive_id)
    }

    fn storage_slot_mut(&mut self, id: u32) -> Result<&mut SlotInfo> {
        self.inventory
            .storage_slots
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(SmcError::InvalidOp("invalid cartridge slot id"))
    }

    fn mail_slot_mut(&mut self, id: u32) -> Result<&mut MailSlotInfo> {
        self.inventory
            .mail_slots
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(SmcError::InvalidOp("invalid mail slot id"))
    }

    fn drive_slot_mut(&mut self, id: u32) -> Result<&mut DriveInfo> {
        self.inventory
            .drives
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or(SmcError::InvalidOp("invalid drive id"))
    }

    /// Reload inventory from disk (called by INITIALIZE ELEMENT STATUS).
    /// Drops any slot/drive entries whose ids are outside the current topology bounds —
    /// protects against a stale or hand-edited inventory.json with extra entries.
    /// Returns (occupied_slots, loaded_drives) for logging/events.
    pub fn reload_inventory(&mut self, data_dir: &Path) -> Result<(usize, usize)> {
        let inv_path = data_dir.join("library/inventory.json");
        let mut inventory: LibraryInventory = serde_json::from_str(&fs::read_to_string(inv_path)?)?;

        let max_storage = self.topology.num_storage_slots;
        let max_mail = self.topology.num_mail_slots;
        let max_drives = self.topology.num_drives;

        let storage_before = inventory.storage_slots.len();
        inventory.storage_slots.retain(|s| s.id < max_storage);
        if inventory.storage_slots.len() != storage_before {
            tracing::warn!(
                "Dropped {} out-of-bounds storage slot(s) (topology={})",
                storage_before - inventory.storage_slots.len(),
                max_storage
            );
        }

        let mail_before = inventory.mail_slots.len();
        inventory.mail_slots.retain(|s| s.id < max_mail);
        if inventory.mail_slots.len() != mail_before {
            tracing::warn!(
                "Dropped {} out-of-bounds mail slot(s) (topology={})",
                mail_before - inventory.mail_slots.len(),
                max_mail
            );
        }

        let drives_before = inventory.drives.len();
        inventory.drives.retain(|d| d.id < max_drives);
        if inventory.drives.len() != drives_before {
            tracing::warn!(
                "Dropped {} out-of-bounds drive(s) (topology={})",
                drives_before - inventory.drives.len(),
                max_drives
            );
        }

        self.inventory = inventory;

        // Return counts for logging/events
        let occupied_slots = self
            .inventory
            .storage_slots
            .iter()
            .filter(|s| s.occupied)
            .count();
        let loaded_drives = self.inventory.drives.iter().filter(|d| d.occupied).count();

        Ok((occupied_slots, loaded_drives))
    }
}

impl DriveTopology for Library {
    fn drive_count(&self) -> usize {
        self.inventory.drives.len()
    }

    fn drive_ids(&self) -> Vec<u32> {
        self.inventory.drives.iter().map(|d| d.id).collect()
    }

    fn partition_for_drive(&self, drive_id: u32) -> Option<String> {
        Library::partition_for_drive(self, drive_id).map(str::to_string)
    }
}

/// Send/Sync facade wrapping `Arc<Mutex<Library>>`. thurvtld
/// owns its `Library` behind a single mutex (admin-socket commands and
/// SCSI dispatch share it); this wrapper presents that mutex as
/// [`TapeDeviceFacade`] without leaking the lock guard into the
/// dispatcher's call sites. Each method locks briefly, copies what it
/// needs, and drops the lock — long-running SCSI ops (WRITE parking on
/// `PoolBudget`) never hold it.
#[derive(Clone)]
pub struct LibraryFacade {
    inner: std::sync::Arc<std::sync::Mutex<Library>>,
}

impl LibraryFacade {
    pub fn new(inner: std::sync::Arc<std::sync::Mutex<Library>>) -> Self {
        Self { inner }
    }

    fn with_library<R>(&self, f: impl FnOnce(&Library) -> R) -> R {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&guard)
    }
}

impl DriveTopology for LibraryFacade {
    fn drive_count(&self) -> usize {
        self.with_library(DriveTopology::drive_count)
    }

    fn drive_ids(&self) -> Vec<u32> {
        self.with_library(DriveTopology::drive_ids)
    }

    fn partition_for_drive(&self, drive_id: u32) -> Option<String> {
        self.with_library(|lib| Library::partition_for_drive(lib, drive_id).map(str::to_string))
    }
}

impl TapeDeviceFacade for LibraryFacade {
    fn chassis_serial(&self) -> String {
        self.with_library(|lib| lib.chassis_serial().to_string())
    }

    fn drive_mfg_serial(&self, drive_id: u32) -> Option<String> {
        self.with_library(|lib| lib.drive_mfg_serial(drive_id))
    }

    fn lto_generation(&self) -> u8 {
        self.with_library(Library::lto_generation)
    }

    fn drive_firmware(&self) -> String {
        self.with_library(|lib| lib.drive_firmware().to_string())
    }

    fn partition_index_one_based(&self, partition_name: Option<&str>) -> u8 {
        self.with_library(|lib| lib.partition_index_one_based(partition_name))
    }

    fn partition_drive_ids(&self, partition_name: &str) -> Option<Vec<u32>> {
        self.with_library(|lib| lib.get_partition(partition_name).map(|p| p.drives.clone()))
    }

    fn drive_ids_in_partition(&self, partition_name: Option<&str>) -> Vec<u32> {
        self.with_library(|lib| match partition_name {
            Some(name) => lib
                .get_partition(name)
                .map(|p| p.drives.clone())
                .unwrap_or_default(),
            None => lib.drives().iter().map(|d| d.id).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_two_file_architecture() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");

        // Initialize library
        let library =
            Library::initialize(&lib_root, &tapes_root, 10, 2, 2, 8, None, 0, 1001, 101, 1)
                .unwrap();

        // Verify both files exist
        assert!(lib_root.join("library.json").exists());
        assert!(lib_root.join("inventory.json").exists());

        // Verify topology
        assert_eq!(library.storage_slots().len(), 10);
        assert_eq!(library.mail_slots().len(), 2);
        assert_eq!(library.drives().len(), 2);
        assert_eq!(library.lto_generation(), 8);

        // Verify all slots are empty
        assert!(library.storage_slots().iter().all(|s| !s.occupied));
        assert!(library.mail_slots().iter().all(|s| !s.occupied));
        assert!(library.drives().iter().all(|d| !d.occupied));
    }

    /// Issue #120: a MOVE / LOAD to an occupied destination must fail
    /// with the cartridge still in its source element, not vanish from
    /// inventory.
    #[test]
    fn failed_move_to_occupied_destination_leaves_source_intact() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");
        let mut library =
            Library::initialize(&lib_root, &tapes_root, 5, 0, 2, 8, None, 0, 1001, 101, 1).unwrap();
        let src_id = library.add_or_create_tape("TAPEAAAL8", "primary").unwrap();
        let dst_id = library.add_or_create_tape("TAPEBBBL8", "primary").unwrap();
        assert_ne!(src_id, dst_id);

        // Move onto an occupied destination slot — must fail.
        assert!(
            library.move_cartridge(src_id, dst_id).is_err(),
            "move to occupied destination must fail"
        );
        let src = library
            .storage_slots()
            .iter()
            .find(|s| s.id == src_id)
            .unwrap();
        assert!(src.occupied, "source slot still occupied after failed move");
        assert_eq!(
            src.barcode.as_deref(),
            Some("TAPEAAAL8"),
            "source barcode intact after failed move"
        );
    }

    /// Issue #120: a LOAD to a drive that already holds a cartridge must
    /// leave the second tape in its storage slot.
    #[test]
    fn failed_load_to_occupied_drive_leaves_source_intact() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");
        let mut library =
            Library::initialize(&lib_root, &tapes_root, 5, 0, 2, 8, None, 0, 1001, 101, 1).unwrap();
        let first = library.add_or_create_tape("TAPEAAAL8", "primary").unwrap();
        let second = library.add_or_create_tape("TAPEBBBL8", "primary").unwrap();
        let drive_id = library.drives()[0].id;

        library.load_to_drive(first, drive_id).unwrap();
        // Drive is now occupied; loading the second tape must fail.
        assert!(
            library.load_to_drive(second, drive_id).is_err(),
            "load into occupied drive must fail"
        );
        let src = library
            .storage_slots()
            .iter()
            .find(|s| s.id == second)
            .unwrap();
        assert!(
            src.occupied,
            "second tape's slot still occupied after failed load"
        );
        assert_eq!(src.barcode.as_deref(), Some("TAPEBBBL8"));
    }

    #[test]
    fn test_library_reopen() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");

        // Initialize and add a tape
        {
            let mut library =
                Library::initialize(&lib_root, &tapes_root, 5, 0, 1, 8, None, 0, 1001, 101, 1)
                    .unwrap();
            library.add_or_create_tape("TAPE001L8", "primary").unwrap();
        }

        // Reopen and verify state persisted
        let library = Library::open(&lib_root, &tapes_root).unwrap();
        assert_eq!(library.storage_slots().len(), 5);
        assert_eq!(
            library
                .storage_slots()
                .iter()
                .filter(|s| s.occupied)
                .count(),
            1
        );
        assert_eq!(
            library.storage_slots()[0].barcode,
            Some("TAPE001L8".to_string())
        );
    }

    #[test]
    fn test_reload_inventory() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");
        let data_dir = temp_dir.path();

        // Initialize library
        let mut library =
            Library::initialize(&lib_root, &tapes_root, 10, 0, 2, 8, None, 0, 1001, 101, 1)
                .unwrap();
        library.add_or_create_tape("TAPE001L8", "primary").unwrap();

        // Simulate CLI adding another tape by directly modifying inventory.json
        let mut inventory: LibraryInventory =
            serde_json::from_str(&fs::read_to_string(lib_root.join("inventory.json")).unwrap())
                .unwrap();
        inventory.storage_slots[1].occupied = true;
        inventory.storage_slots[1].barcode = Some("TAPE002L8".to_string());
        fs::write(
            lib_root.join("inventory.json"),
            serde_json::to_string_pretty(&inventory).unwrap(),
        )
        .unwrap();

        // Reload inventory (simulates INITIALIZE ELEMENT STATUS)
        let (occupied, _) = library.reload_inventory(data_dir).unwrap();
        assert_eq!(occupied, 2);
        assert_eq!(
            library.storage_slots()[1].barcode,
            Some("TAPE002L8".to_string())
        );
    }

    #[test]
    fn test_file_locking() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");

        // Initialize library
        let mut library =
            Library::initialize(&lib_root, &tapes_root, 5, 0, 1, 8, None, 0, 1001, 101, 1).unwrap();

        // Add tape (triggers persist with file locking)
        library.add_or_create_tape("TAPE001L8", "primary").unwrap();

        // Verify files exist and are valid JSON
        let lib_json = fs::read_to_string(lib_root.join("library.json")).unwrap();
        let inv_json = fs::read_to_string(lib_root.join("inventory.json")).unwrap();

        let _: LibraryTopology = serde_json::from_str(&lib_json).unwrap();
        let _: LibraryInventory = serde_json::from_str(&inv_json).unwrap();
    }

    fn part(name: &str, ss: (u32, u32), ms: (u32, u32), drives: &[u32]) -> LibraryPartition {
        LibraryPartition {
            name: name.to_string(),
            storage_slots: SlotRange {
                start: ss.0,
                end: ss.1,
            },
            mail_slots: SlotRange {
                start: ms.0,
                end: ms.1,
            },
            drives: drives.to_vec(),
        }
    }

    #[test]
    fn validate_partitions_empty_is_legacy() {
        // No partitions = legacy single-partition library; always OK
        // even when sizes are weird.
        assert!(validate_partitions(&[], 40, 5, 3).is_ok());
    }

    #[test]
    fn validate_partitions_full_coverage() {
        let parts = vec![
            part("alpha", (0, 20), (0, 2), &[0, 1]),
            part("bravo", (20, 40), (2, 5), &[2]),
        ];
        assert!(validate_partitions(&parts, 40, 5, 3).is_ok());
    }

    #[test]
    fn validate_partitions_overlap_storage() {
        let parts = vec![
            part("alpha", (0, 25), (0, 5), &[0, 1, 2]),
            part("bravo", (20, 40), (0, 0), &[]),
        ];
        let err = validate_partitions(&parts, 40, 5, 3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("overlaps"));
    }

    #[test]
    fn validate_partitions_overlap_drive() {
        let parts = vec![
            part("alpha", (0, 20), (0, 5), &[0, 1]),
            part("bravo", (20, 40), (0, 0), &[1, 2]),
        ];
        let err = validate_partitions(&parts, 40, 5, 3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("drive 1 overlaps"));
    }

    #[test]
    fn validate_partitions_storage_gap_rejected() {
        // Storage slot 19 left uncovered.
        let parts = vec![
            part("alpha", (0, 19), (0, 5), &[0, 1]),
            part("bravo", (20, 40), (0, 0), &[2]),
        ];
        let err = validate_partitions(&parts, 40, 5, 3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("storage slot 19 is not covered"));
    }

    #[test]
    fn element_address_layout_defaults_pass() {
        assert!(
            validate_element_address_layout(0, 1001, 40, 101, 5, 1, 3).is_ok(),
            "historical default element bases must not overlap"
        );
    }

    #[test]
    fn element_address_layout_overlap_storage_mail_rejected() {
        // storage [100..300) overlaps mail [150..151)
        let err = validate_element_address_layout(0, 100, 200, 150, 1, 1, 1)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("overlap") && err.contains("storage") && err.contains("mail"),
            "expected overlap error mentioning storage/mail, got: {err}"
        );
    }

    #[test]
    fn element_address_layout_u16_overflow_rejected() {
        // storage_base 65000 + 1000 slots = 66000 > u16::MAX (65535).
        let err = validate_element_address_layout(0, 65000, 1000, 101, 0, 1, 1)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("u16") && err.contains("storage"),
            "expected u16-overflow error on storage, got: {err}"
        );
    }

    #[test]
    fn element_address_layout_zero_mail_count_ok() {
        // mail [101..101) is empty; transport=0 lives outside.
        assert!(validate_element_address_layout(0, 1001, 40, 101, 0, 1, 3).is_ok());
    }

    #[test]
    fn validate_partitions_drive_gap_rejected() {
        let parts = vec![part("alpha", (0, 40), (0, 5), &[0, 1])];
        let err = validate_partitions(&parts, 40, 5, 3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("drive 2 is not covered"));
    }

    #[test]
    fn validate_partitions_duplicate_name_rejected() {
        let parts = vec![
            part("alpha", (0, 20), (0, 5), &[0, 1]),
            part("alpha", (20, 40), (0, 0), &[2]),
        ];
        let err = validate_partitions(&parts, 40, 5, 3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate partition name"));
    }

    #[test]
    fn validate_partitions_out_of_bounds_rejected() {
        let parts = vec![part("alpha", (0, 50), (0, 5), &[0, 1, 2])];
        let err = validate_partitions(&parts, 40, 5, 3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds 40 storage slots"));
    }

    #[test]
    fn validate_partitions_empty_mail_range_ok() {
        // Partition with no mail slots — alpha gets the whole 5-slot
        // mail bank, bravo claims none.
        let parts = vec![
            part("alpha", (0, 20), (0, 5), &[0, 1]),
            part("bravo", (20, 40), (0, 0), &[2]),
        ];
        assert!(validate_partitions(&parts, 40, 5, 3).is_ok());
    }

    #[test]
    fn library_set_partitions_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");

        let mut library =
            Library::initialize(&lib_root, &tapes_root, 40, 5, 3, 8, None, 0, 1001, 101, 1)
                .unwrap();
        assert!(library.partitions().is_empty());

        let parts = vec![
            part("alpha", (0, 20), (0, 2), &[0, 1]),
            part("bravo", (20, 40), (2, 5), &[2]),
        ];
        library.set_partitions(parts.clone()).unwrap();

        // The on-disk schema must stay v2 (declared/minted split): the
        // daemon's open_or_materialize hard-refuses a v1 flat file, so a
        // flat rewrite would brick the daemon (issue #121).
        let raw = std::fs::read_to_string(lib_root.join("library.json")).unwrap();
        let disk: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            disk.get("version").and_then(|x| x.as_u64()),
            Some(2),
            "set_partitions must persist the v2 schema"
        );
        assert!(
            disk.get("minted").is_some(),
            "v2 minted stanza must be preserved"
        );

        // Reopen and verify persistence.
        let reloaded = Library::open(&lib_root, &tapes_root).unwrap();
        assert_eq!(reloaded.partitions().len(), 2);
        assert_eq!(reloaded.partition_for_drive(0), Some("alpha"));
        assert_eq!(reloaded.partition_for_drive(2), Some("bravo"));
        assert_eq!(reloaded.partition_for_storage_slot(15), Some("alpha"));
        assert_eq!(reloaded.partition_for_storage_slot(35), Some("bravo"));
        assert_eq!(reloaded.partition_for_mail_slot(0), Some("alpha"));
        assert_eq!(reloaded.partition_for_mail_slot(3), Some("bravo"));
    }

    #[test]
    fn library_set_partitions_rejects_invalid() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");

        let mut library =
            Library::initialize(&lib_root, &tapes_root, 40, 5, 3, 8, None, 0, 1001, 101, 1)
                .unwrap();
        // Missing drive 2.
        let parts = vec![part("alpha", (0, 40), (0, 5), &[0, 1])];
        assert!(library.set_partitions(parts).is_err());
        // State unchanged.
        assert!(library.partitions().is_empty());
    }

    #[test]
    fn chassis_serial_random_format() {
        let s = generate_chassis_serial();
        assert_eq!(s.len(), 14);
        assert!(s.starts_with("TVL"));
        assert!(s[3..].chars().all(|c| c.is_ascii_hexdigit()));
        // Two calls produce different serials with overwhelming probability.
        assert_ne!(s, generate_chassis_serial());
    }

    #[test]
    fn drive_mfg_serial_random_format() {
        let s = generate_drive_mfg_serial();
        assert_eq!(s.len(), 10);
        assert!(s.starts_with("TVL"));
        assert!(s[3..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(s, generate_drive_mfg_serial());
    }

    #[test]
    fn validate_chassis_serial_accepts_legacy_and_default() {
        assert!(validate_chassis_serial(LEGACY_CHASSIS_SERIAL).is_ok());
        assert!(validate_chassis_serial(&generate_chassis_serial()).is_ok());
    }

    #[test]
    fn validate_chassis_serial_rejects_too_long_and_non_ascii() {
        let too_long = "A".repeat(MAX_CHASSIS_SERIAL_LEN + 1);
        assert!(validate_chassis_serial(&too_long).is_err());
        assert!(validate_chassis_serial("").is_err());
        assert!(validate_chassis_serial("foo\nbar").is_err());
    }

    #[test]
    fn partition_serial_suffix_is_stable_and_distinct() {
        let a = partition_serial_suffix("TVLDEADBEEF42", "alpha");
        let b = partition_serial_suffix("TVLDEADBEEF42", "alpha");
        let c = partition_serial_suffix("TVLDEADBEEF42", "bravo");
        let d = partition_serial_suffix("TVLDEADBEEF99", "alpha");
        assert_eq!(a, b, "same inputs → same suffix");
        assert_ne!(a, c, "different partition → different suffix");
        assert_ne!(a, d, "different chassis → different suffix");
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn library_initialize_persists_chassis_serial() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");
        let library =
            Library::initialize(&lib_root, &tapes_root, 5, 0, 2, 8, None, 0, 1001, 101, 1).unwrap();
        let serial = library.chassis_serial().to_string();
        assert!(serial.starts_with("TVL"));
        assert_eq!(serial.len(), 14);

        // Survives reopen.
        let reloaded = Library::open(&lib_root, &tapes_root).unwrap();
        assert_eq!(reloaded.chassis_serial(), serial);

        // Each drive gets its own random serial that survives reopen.
        let drive0 = library.drive_mfg_serial(0).unwrap();
        let drive1 = library.drive_mfg_serial(1).unwrap();
        assert!(drive0.starts_with("TVL"));
        assert!(drive1.starts_with("TVL"));
        assert_ne!(drive0, drive1);
        assert_eq!(
            reloaded.drive_mfg_serial(0).as_deref(),
            Some(drive0.as_str())
        );
        assert_eq!(
            reloaded.drive_mfg_serial(1).as_deref(),
            Some(drive1.as_str())
        );
    }

    #[test]
    fn library_legacy_chassis_serial_falls_back() {
        // Hand-craft a library.json without chassis_serial / mfg_serial
        // (mimicking a pre-field on-disk library) and verify the
        // accessors return the legacy literals.
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes_root = temp_dir.path().join("tapes");
        fs::create_dir_all(&lib_root).unwrap();
        fs::create_dir_all(&tapes_root).unwrap();
        let topology_json = serde_json::json!({
            "version": 1,
            "num_storage_slots": 2,
            "num_mail_slots": 0,
            "num_drives": 1,
            "lto_generation": 8,
            "transport_base": 0,
            "storage_base": 1001,
            "import_export_base": 101,
            "data_transfer_base": 1,
        });
        let inventory_json = serde_json::json!({
            "version": 1,
            "storage_slots": [
                {"id": 0, "occupied": false, "barcode": null},
                {"id": 1, "occupied": false, "barcode": null},
            ],
            "mail_slots": [],
            "drives": [
                {"id": 0, "occupied": false, "barcode": null, "home_slot": null}
            ],
        });
        fs::write(
            lib_root.join("library.json"),
            serde_json::to_string_pretty(&topology_json).unwrap(),
        )
        .unwrap();
        fs::write(
            lib_root.join("inventory.json"),
            serde_json::to_string_pretty(&inventory_json).unwrap(),
        )
        .unwrap();
        let library = Library::open(&lib_root, &tapes_root).unwrap();
        assert_eq!(library.chassis_serial(), LEGACY_CHASSIS_SERIAL);
        // Drive serial falls back to the historical per-LUN literal
        // (LUN = drive_id + 1, so drive 0 → "THUR-MFG-001").
        assert_eq!(library.drive_mfg_serial(0).as_deref(), Some("THUR-MFG-001"));
    }
}
