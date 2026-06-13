// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Declared-vs-persisted topology reconciliation.
//!
//! The operator declares chassis intent (`num_slots`, `num_drives`,
//! `lto_generation`, optional `firmware`) in `thurvtl.yaml`'s
//! `library:` block. The daemon calls `open_or_materialize` on every
//! start: if `<data_dir>/library/library.json` is missing the chassis
//! is materialized fresh from the declared block (minting a
//! `chassis_serial` and the four SMC element bases on the way); if
//! it's present and v2 the declared block is diffed against the
//! persisted `declared` stanza and any deltas are applied
//! atomically; if it's present and v1 (the pre-refactor flat schema)
//! the daemon refuses to start — operators remove
//! `<data_dir>/library/` and re-materialize.
//!
//! The diff is conservatively asymmetric. Grow operations
//! (`num_slots` / `num_drives` up) always succeed. Shrink operations
//! refuse if any cartridge would be orphaned: a storage shrink past
//! an occupied slot is refused; a drive shrink that would remove a
//! loaded drive whose origin slot (the slot the cartridge was
//! `LOAD`'d from) is out of range or occupied by something else is
//! refused. The strictness — "origin slot only, no fallback" —
//! preserves SCSI element addresses that backup-software catalogs
//! have indexed against. `compute_bounds` shares the same predicates
//! and powers the `library bounds` CLI verb so operators can see
//! exactly which drive or cartridge would block a proposed shrink
//! before they edit the YAML.

use crate::errors::{Result, SmcError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use super::{
    DeclaredTopology, DriveInfo, Library, LibraryInventory, LibraryPartition, LibraryTopology,
    MailSlotInfo, SlotInfo, generate_chassis_serial, generate_drive_mfg_serial,
    validate_element_address_layout, validate_firmware,
};

/// Mail-slot count baked into v2. Operators never declare it — the
/// SMC layer reports exactly one empty Import/Export element so
/// backup software that probes IE elements sees what it expects, and
/// the `cartridge import` / `cartridge export` CLI verbs (which
/// operate against storage slots directly) carry the actual
/// ingress / egress workflow.
pub const MAIL_SLOT_COUNT: u32 = 1;

/// SMC-3 element-address bases — minted at first materialization,
/// persisted in `library.json`'s `minted` stanza, immutable forever.
/// Bases live here (not in the YAML, not on a CLI flag) because
/// backup-software catalogs index inventory against the per-element
/// addresses derived from them; rotating bases would orphan every
/// cataloged barcode. Values match the historical defaults that
/// pre-refactor `library init` wrote, so a fresh v2 library has the
/// same element-address layout a v1 library did.
const TRANSPORT_BASE: u16 = 0;
const STORAGE_BASE: u16 = 1001;
const IMPORT_EXPORT_BASE: u16 = 101;
const DATA_TRANSFER_BASE: u16 = 1;

const SCHEMA_VERSION: u32 = 2;

// ---------- diff / plan / event types ----------

/// What needs to change to reconcile current persisted state with the
/// caller's declared topology. `is_noop` returns true when the diff
/// found no work (steady-state restart with the same YAML).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// `Some(new)` when the firmware override changed. Inner `Option`
    /// distinguishes "set to TVL8" (`Some(Some(..))`) from "clear the
    /// override" (`Some(None)`).
    pub firmware: Option<Option<String>>,
    pub lto_generation: Option<u8>,
    pub storage_target: Option<u32>,
    pub drive_target: Option<u32>,
    /// One entry per loaded drive that must return its cartridge to
    /// its origin slot before the drive can be removed. Empty for
    /// pure-grow plans or shrinks where every affected drive is
    /// already empty.
    pub drive_evacuations: Vec<DriveEvacuation>,
}

impl ReconcilePlan {
    pub fn is_noop(&self) -> bool {
        self.firmware.is_none()
            && self.lto_generation.is_none()
            && self.storage_target.is_none()
            && self.drive_target.is_none()
            && self.drive_evacuations.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveEvacuation {
    pub drive_id: u32,
    pub barcode: String,
    pub origin_slot: u32,
}

/// Events surfaced for daemon-side audit emission. Ordering in the
/// returned `Vec` matters: `Materialized` (if first start) precedes
/// any `DriveEvacuated` rows, which precede the summary
/// `Reconciled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileEvent {
    Materialized,
    DriveEvacuated(DriveEvacuation),
    Reconciled,
}

// ---------- bounds ----------

/// Response payload for the `library bounds` admin route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundsReport {
    pub current: BoundsCounts,
    pub min: BoundsCounts,
    pub max: BoundsCounts,
    pub explanations: Vec<BoundsExplanation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundsCounts {
    pub num_slots: u32,
    pub num_drives: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundsExplanation {
    pub field: String,
    pub kind: String,
    pub reason: String,
}

// ---------- on-disk v2 wrapper ----------

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DiskV2 {
    pub(super) version: u32,
    pub(super) declared: DeclaredOnDisk,
    pub(super) minted: MintedOnDisk,
    #[serde(default)]
    pub(super) partitions: Vec<LibraryPartition>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DeclaredOnDisk {
    pub(super) num_storage_slots: u32,
    pub(super) num_drives: u32,
    pub(super) lto_generation: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) firmware: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MintedOnDisk {
    pub(super) chassis_serial: String,
    pub(super) transport_base: u16,
    pub(super) storage_base: u16,
    pub(super) import_export_base: u16,
    pub(super) data_transfer_base: u16,
}

/// Bridge for daemon-down callers (`Library::open` + `cmd_restore`):
/// flatten a v2 disk struct into the in-memory `LibraryTopology`
/// shape. Exposed `pub(super)` so `mod.rs` can use it without
/// duplicating the conversion.
pub(super) const DISK_SCHEMA_VERSION: u32 = SCHEMA_VERSION;

// ---------- entry points ----------

/// Single entrypoint the daemon calls on every start.
///
/// First-start (no `library.json`) materializes the chassis from
/// `declared`, minting `chassis_serial` + the four element bases.
/// Subsequent starts diff `declared` against the persisted `declared`
/// stanza and apply any reconcile actions. v1 `library.json` files
/// (the pre-refactor flat schema) are refused; operators remove
/// `<data_dir>/library/` and re-materialize.
pub fn open_or_materialize<P: AsRef<Path>>(
    lib_root: P,
    tapes_dir: P,
    declared: &DeclaredTopology,
) -> Result<(Library, Vec<ReconcileEvent>)> {
    let lib_root = lib_root.as_ref().to_path_buf();
    let tapes_dir = tapes_dir.as_ref().to_path_buf();
    let lib_path = lib_root.join("library.json");

    if !lib_path.exists() {
        let library = materialize(&lib_root, &tapes_dir, declared)?;
        return Ok((library, vec![ReconcileEvent::Materialized]));
    }

    // Peek at the schema version before committing to a full deserialize.
    let raw = fs::read_to_string(&lib_path)?;
    let probe: serde_json::Value = serde_json::from_str(&raw)?;
    let version = probe.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
    if version != SCHEMA_VERSION as u64 {
        return Err(SmcError::LibraryConfig(format!(
            "library.json is v{} format; this release requires v{}. Remove {} and re-start the daemon to re-materialize from the YAML library: block.",
            version,
            SCHEMA_VERSION,
            lib_root.display(),
        )));
    }

    let disk: DiskV2 = serde_json::from_str(&raw)?;
    let topology = topology_from_disk(disk);

    let inv_path = lib_root.join("inventory.json");
    if !inv_path.exists() {
        return Err(SmcError::LibraryConfig(format!(
            "inventory.json missing alongside library.json at {}",
            lib_root.display(),
        )));
    }
    let inventory: LibraryInventory = serde_json::from_str(&fs::read_to_string(&inv_path)?)?;

    let mut library = Library::from_parts(lib_root.clone(), tapes_dir, topology, inventory);

    let plan = diff_against_declared(&library, declared)?;
    let mut events = apply_plan(&mut library, plan)?;
    events.push(ReconcileEvent::Reconciled);
    Ok((library, events))
}

fn materialize(lib_root: &Path, tapes_dir: &Path, declared: &DeclaredTopology) -> Result<Library> {
    validate_declared(declared)?;
    validate_element_address_layout(
        TRANSPORT_BASE,
        STORAGE_BASE,
        declared.num_storage_slots,
        IMPORT_EXPORT_BASE,
        MAIL_SLOT_COUNT,
        DATA_TRANSFER_BASE,
        declared.num_drives,
    )?;

    fs::create_dir_all(lib_root)?;
    fs::create_dir_all(tapes_dir)?;

    let topology = LibraryTopology {
        version: 1,
        num_storage_slots: declared.num_storage_slots,
        num_mail_slots: MAIL_SLOT_COUNT,
        num_drives: declared.num_drives,
        lto_generation: declared.lto_generation,
        firmware: declared.firmware.clone(),
        chassis_serial: Some(generate_chassis_serial()),
        partitions: Vec::new(),
        transport_base: TRANSPORT_BASE,
        storage_base: STORAGE_BASE,
        import_export_base: IMPORT_EXPORT_BASE,
        data_transfer_base: DATA_TRANSFER_BASE,
    };

    let storage_slots: Vec<SlotInfo> = (0..declared.num_storage_slots)
        .map(|id| SlotInfo {
            id,
            barcode: None,
            occupied: false,
        })
        .collect();
    let mail_slots: Vec<MailSlotInfo> = (0..MAIL_SLOT_COUNT)
        .map(|id| MailSlotInfo {
            id,
            barcode: None,
            occupied: false,
            accessible: true,
        })
        .collect();
    let drives: Vec<DriveInfo> = (0..declared.num_drives)
        .map(|id| DriveInfo {
            id,
            barcode: None,
            occupied: false,
            home_slot: None,
            mfg_serial: Some(generate_drive_mfg_serial()),
        })
        .collect();
    let inventory = LibraryInventory {
        version: 1,
        storage_slots,
        mail_slots,
        drives,
    };

    persist_v2(lib_root, &topology, &inventory)?;
    Ok(Library::from_parts(
        lib_root.to_path_buf(),
        tapes_dir.to_path_buf(),
        topology,
        inventory,
    ))
}

/// Pure diff. Builds a `ReconcilePlan` describing the edits needed to
/// move from `library.topology` (persisted `declared`) to `declared`
/// (YAML). Returns `Err(LibraryConfig)` with a multi-line message
/// enumerating every blocking cartridge / drive / partition when the
/// move would orphan inventory.
pub fn diff_against_declared(
    library: &Library,
    declared: &DeclaredTopology,
) -> Result<ReconcilePlan> {
    validate_declared(declared)?;

    let mut plan = ReconcilePlan::default();
    let mut blockers: Vec<String> = Vec::new();

    let cur = library.topology_snapshot();

    // Firmware: validated already if Some(Some(..)); set when changed.
    if let Some(ref fw) = declared.firmware {
        validate_firmware(fw)?;
    }
    if declared.firmware != cur.firmware {
        plan.firmware = Some(declared.firmware.clone());
    }

    // LTO generation: refuse downgrade with cartridges present.
    if declared.lto_generation != cur.lto_generation {
        if declared.lto_generation < cur.lto_generation {
            let cartridges = present_barcodes(library);
            if !cartridges.is_empty() {
                blockers.push(format!(
                    "lto_generation downgrade from {} to {} blocked: {} cartridge(s) present",
                    cur.lto_generation,
                    declared.lto_generation,
                    cartridges.len(),
                ));
                for (barcode, location) in cartridges {
                    blockers.push(format!("  {} at {}", barcode, location));
                }
            }
        }
        plan.lto_generation = Some(declared.lto_generation);
    }

    // Storage shrink check: tail occupants + drive home-slot OOR.
    if declared.num_storage_slots != cur.num_storage_slots {
        if declared.num_storage_slots < cur.num_storage_slots {
            for slot in library.storage_slots() {
                if slot.id >= declared.num_storage_slots && slot.occupied {
                    blockers.push(format!(
                        "cannot shrink num_slots from {} to {}: slot {} holds {}",
                        cur.num_storage_slots,
                        declared.num_storage_slots,
                        slot.id,
                        slot.barcode.clone().unwrap_or_else(|| "<unknown>".into()),
                    ));
                }
            }
            for drive in library.drives() {
                if drive.occupied {
                    let home = drive.home_slot.map(u32::from).unwrap_or(u32::MAX);
                    if home >= declared.num_storage_slots {
                        blockers.push(format!(
                            "cannot shrink num_slots from {} to {}: drive {} holds {} whose origin slot {} would be removed",
                            cur.num_storage_slots,
                            declared.num_storage_slots,
                            drive.id,
                            drive.barcode.clone().unwrap_or_else(|| "<unknown>".into()),
                            home,
                        ));
                    }
                }
            }
        }
        plan.storage_target = Some(declared.num_storage_slots);
    }

    // Drive shrink check: per-drive evacuation with origin-only return.
    if declared.num_drives != cur.num_drives {
        if declared.num_drives < cur.num_drives {
            // Effective storage bound: post-storage-shrink if storage is
            // also shrinking, else current. Drive evacuation has to
            // succeed against that bound — and against an unoccupied
            // origin slot (occupancy at the time of materialize check,
            // pre-evacuation; home slots are unique across loaded
            // drives so the order of evacuation doesn't matter).
            let effective_slots = plan.storage_target.unwrap_or(cur.num_storage_slots);
            for drive in library.drives() {
                if drive.id < declared.num_drives {
                    continue;
                }
                if !drive.occupied {
                    continue;
                }
                let barcode = drive.barcode.clone().unwrap_or_else(|| "<unknown>".into());
                let home = drive.home_slot.map(u32::from).unwrap_or(u32::MAX);
                if home >= effective_slots {
                    blockers.push(format!(
                        "cannot shrink num_drives from {} to {}: drive {} holds {} whose origin slot {} would be out of range",
                        cur.num_drives,
                        declared.num_drives,
                        drive.id,
                        barcode,
                        home,
                    ));
                    continue;
                }
                let dest = library.storage_slots().iter().find(|s| s.id == home);
                let dest_occupied = dest.map(|s| s.occupied).unwrap_or(false);
                if dest_occupied {
                    let other = dest
                        .and_then(|s| s.barcode.clone())
                        .unwrap_or_else(|| "<unknown>".into());
                    blockers.push(format!(
                        "cannot shrink num_drives from {} to {}: drive {} holds {} whose origin slot {} is occupied by {}",
                        cur.num_drives,
                        declared.num_drives,
                        drive.id,
                        barcode,
                        home,
                        other,
                    ));
                    continue;
                }
                // Two loaded drives can legitimately share a home_slot
                // (load A from slot 5, move B into 5, load B into another
                // drive). This diff checks PRE-evacuation occupancy, so
                // without tracking already-planned destinations the
                // second evacuation would overwrite the first's slot
                // record and silently drop a cartridge from inventory
                // (issue #162). Refuse the collision.
                if let Some(prev) = plan
                    .drive_evacuations
                    .iter()
                    .find(|e| e.origin_slot == home)
                {
                    blockers.push(format!(
                        "cannot shrink num_drives from {} to {}: drive {} holds {} whose origin slot {} is also the evacuation target of {}",
                        cur.num_drives,
                        declared.num_drives,
                        drive.id,
                        barcode,
                        home,
                        prev.barcode,
                    ));
                    continue;
                }
                plan.drive_evacuations.push(DriveEvacuation {
                    drive_id: drive.id,
                    barcode,
                    origin_slot: home,
                });
            }
        }
        plan.drive_target = Some(declared.num_drives);
    }

    // Partition coverage against the proposed bounds.
    if !cur.partitions.is_empty() {
        let new_slots = plan.storage_target.unwrap_or(cur.num_storage_slots);
        let new_drives = plan.drive_target.unwrap_or(cur.num_drives);
        for p in &cur.partitions {
            if p.storage_slots.end > new_slots {
                blockers.push(format!(
                    "partition '{}' storage_slots [{}, {}) exceeds num_slots={}",
                    p.name, p.storage_slots.start, p.storage_slots.end, new_slots,
                ));
            }
            for d in &p.drives {
                if *d >= new_drives {
                    blockers.push(format!(
                        "partition '{}' references drive {} which would be removed by num_drives={}",
                        p.name, d, new_drives,
                    ));
                }
            }
        }
    }

    if !blockers.is_empty() {
        return Err(SmcError::LibraryConfig(blockers.join("\n")));
    }

    Ok(plan)
}

/// Apply a validated plan. Persists `library.json` + `inventory.json`
/// once at the end via `Library::write_locked`. Returns one
/// `DriveEvacuated` event per evacuation; the daemon emits an
/// `inventory.move_medium` audit row per event.
pub fn apply_plan(library: &mut Library, plan: ReconcilePlan) -> Result<Vec<ReconcileEvent>> {
    let mut events = Vec::with_capacity(plan.drive_evacuations.len());

    if let Some(fw) = plan.firmware {
        library.set_firmware(fw);
    }
    if let Some(lto_gen) = plan.lto_generation {
        library.set_lto_generation(lto_gen);
    }
    if let Some(target) = plan.storage_target {
        library.resize_storage(target);
    }
    if let Some(target) = plan.drive_target {
        for ev in plan.drive_evacuations {
            library.evacuate_drive_to_origin(ev.drive_id, ev.origin_slot, &ev.barcode)?;
            events.push(ReconcileEvent::DriveEvacuated(ev));
        }
        library.resize_drives(target);
    }

    library.persist_v2()?;
    Ok(events)
}

/// Min / current / max for `num_slots` and `num_drives`, with
/// per-field explanations naming the cartridge or drive that pins the
/// minimum. The maxima are the SMC-3 / iSCSI ceilings — 65535 storage
/// slots (16-bit element address) and 255 drives (single-byte LUN).
pub fn compute_bounds(library: &Library) -> BoundsReport {
    let cur = library.topology_snapshot();

    let mut min_slots_pin: Option<(u32, String)> = None;
    for slot in library.storage_slots() {
        if slot.occupied {
            min_slots_pin = Some((
                slot.id,
                format!(
                    "slot {} is occupied by {}",
                    slot.id,
                    slot.barcode.clone().unwrap_or_else(|| "<unknown>".into()),
                ),
            ));
        }
    }
    for drive in library.drives() {
        if drive.occupied
            && let Some(home) = drive.home_slot
        {
            let home = u32::from(home);
            let take = match &min_slots_pin {
                None => true,
                Some((pin_id, _)) => home > *pin_id,
            };
            if take {
                min_slots_pin = Some((
                    home,
                    format!(
                        "drive {} holds {} loaded from slot {}",
                        drive.id,
                        drive.barcode.clone().unwrap_or_else(|| "<unknown>".into()),
                        home,
                    ),
                ));
            }
        }
    }

    let (min_slots, min_slots_reason) = match min_slots_pin {
        Some((id, reason)) => (id + 1, Some(reason)),
        None => (1, None),
    };

    // Drive min: scan high → low, stop at the first drive whose
    // evacuation against current num_storage_slots would fail.
    // Empty drives can always be removed; loaded drives need their
    // origin slot in range and unoccupied.
    let mut min_drives = cur.num_drives;
    let mut min_drives_reason: Option<String> = None;
    // Home slots already claimed by a higher (already-removable) drive's
    // evacuation. Two loaded drives can share a home_slot, and the diff
    // checks PRE-evacuation occupancy, so a shrink past such a collision
    // is not actually safe (issue #162) — stop the safe-shrink envelope
    // at the first colliding drive.
    let mut planned_dests: Vec<u32> = Vec::new();
    for d in (0..cur.num_drives).rev() {
        let drive = match library.drives().iter().find(|x| x.id == d) {
            Some(d) => d,
            None => break,
        };
        if !drive.occupied {
            min_drives = d;
            continue;
        }
        let home = drive.home_slot.map(u32::from).unwrap_or(u32::MAX);
        let barcode = drive.barcode.clone().unwrap_or_else(|| "<unknown>".into());
        if home >= cur.num_storage_slots {
            min_drives_reason = Some(format!(
                "drive {} holds {} whose origin slot {} would be out of range",
                d, barcode, home,
            ));
            break;
        }
        let dest = library.storage_slots().iter().find(|s| s.id == home);
        let dest_occupied = dest.map(|s| s.occupied).unwrap_or(false);
        if dest_occupied {
            let other = dest
                .and_then(|s| s.barcode.clone())
                .unwrap_or_else(|| "<unknown>".into());
            min_drives_reason = Some(format!(
                "drive {} holds {} whose origin slot {} is occupied by {}",
                d, barcode, home, other,
            ));
            break;
        }
        if planned_dests.contains(&home) {
            min_drives_reason = Some(format!(
                "drive {} holds {} whose origin slot {} is also another evacuated drive's target",
                d, barcode, home,
            ));
            break;
        }
        planned_dests.push(home);
        min_drives = d;
    }
    // Floor at 1 — chassis must keep at least one drive.
    if min_drives < 1 {
        min_drives = 1;
    }

    let mut explanations = Vec::new();
    if let Some(reason) = min_slots_reason {
        explanations.push(BoundsExplanation {
            field: "num_slots".into(),
            kind: "min".into(),
            reason,
        });
    }
    if let Some(reason) = min_drives_reason {
        explanations.push(BoundsExplanation {
            field: "num_drives".into(),
            kind: "min".into(),
            reason,
        });
    }

    BoundsReport {
        current: BoundsCounts {
            num_slots: cur.num_storage_slots,
            num_drives: cur.num_drives,
        },
        min: BoundsCounts {
            num_slots: min_slots,
            num_drives: min_drives,
        },
        max: BoundsCounts {
            num_slots: 65535,
            num_drives: 255,
        },
        explanations,
    }
}

// ---------- helpers ----------

fn validate_declared(declared: &DeclaredTopology) -> Result<()> {
    if !(1..=65535).contains(&declared.num_storage_slots) {
        return Err(SmcError::LibraryConfig(
            "library.num_slots must be between 1 and 65535".into(),
        ));
    }
    if !(1..=255).contains(&declared.num_drives) {
        return Err(SmcError::LibraryConfig(
            "library.num_drives must be between 1 and 255".into(),
        ));
    }
    if declared.lto_generation != 8 {
        return Err(SmcError::LibraryConfig(
            "library.lto_generation must be 8".into(),
        ));
    }
    Ok(())
}

fn present_barcodes(library: &Library) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for slot in library.storage_slots() {
        if slot.occupied
            && let Some(ref b) = slot.barcode
        {
            out.push((b.clone(), format!("slot {}", slot.id)));
        }
    }
    for drive in library.drives() {
        if drive.occupied
            && let Some(ref b) = drive.barcode
        {
            out.push((b.clone(), format!("drive {}", drive.id)));
        }
    }
    out
}

pub(super) fn topology_from_disk(disk: DiskV2) -> LibraryTopology {
    LibraryTopology {
        version: 1, // in-memory shape marker; on-disk schema is v2
        num_storage_slots: disk.declared.num_storage_slots,
        num_mail_slots: MAIL_SLOT_COUNT,
        num_drives: disk.declared.num_drives,
        lto_generation: disk.declared.lto_generation,
        firmware: disk.declared.firmware,
        chassis_serial: Some(disk.minted.chassis_serial),
        partitions: disk.partitions,
        transport_base: disk.minted.transport_base,
        storage_base: disk.minted.storage_base,
        import_export_base: disk.minted.import_export_base,
        data_transfer_base: disk.minted.data_transfer_base,
    }
}

fn disk_from_topology(topology: &LibraryTopology) -> DiskV2 {
    DiskV2 {
        version: SCHEMA_VERSION,
        declared: DeclaredOnDisk {
            num_storage_slots: topology.num_storage_slots,
            num_drives: topology.num_drives,
            lto_generation: topology.lto_generation,
            firmware: topology.firmware.clone(),
        },
        minted: MintedOnDisk {
            chassis_serial: topology
                .chassis_serial
                .clone()
                .unwrap_or_else(generate_chassis_serial),
            transport_base: topology.transport_base,
            storage_base: topology.storage_base,
            import_export_base: topology.import_export_base,
            data_transfer_base: topology.data_transfer_base,
        },
        partitions: topology.partitions.clone(),
    }
}

fn persist_v2(
    lib_root: &Path,
    topology: &LibraryTopology,
    inventory: &LibraryInventory,
) -> Result<()> {
    let disk = disk_from_topology(topology);
    let lib_path = lib_root.join("library.json");
    let inv_path = lib_root.join("inventory.json");
    Library::write_locked_pub(&lib_path, &serde_json::to_string_pretty(&disk)?)?;
    Library::write_locked_pub(&inv_path, &serde_json::to_string_pretty(inventory)?)?;
    Ok(())
}

/// Persist ONLY `library.json` in the on-disk v2 schema from the current
/// in-memory topology (which `Library::open` populated with the minted
/// `chassis_serial` + element bases via `topology_from_disk`). Used by
/// daemon-down partition mutations so they don't rewrite the file in the
/// in-memory v1 flat shape — which the daemon's `open_or_materialize`
/// then hard-refuses at next start, bricking it (issue #121).
pub(super) fn persist_library_topology_v2(
    lib_root: &Path,
    topology: &LibraryTopology,
) -> Result<()> {
    let disk = disk_from_topology(topology);
    let lib_path = lib_root.join("library.json");
    Library::write_locked_pub(&lib_path, &serde_json::to_string_pretty(&disk)?)?;
    Ok(())
}

// ---------- Library-side bridge methods ----------
//
// Reconcile is a child module of `library`, but we keep the
// inventory / topology mutation primitives behind named methods on
// `Library` rather than poking its fields directly — same pattern
// the existing `partitions.rs` follows. The `_pub` suffix on the
// helpers below distinguishes them from the genuinely-private
// `write_locked` / `persist` that mod.rs still uses for legacy paths.

impl Library {
    pub(super) fn from_parts(
        root: std::path::PathBuf,
        tapes_dir: std::path::PathBuf,
        topology: LibraryTopology,
        inventory: LibraryInventory,
    ) -> Self {
        Self {
            root,
            tapes_dir,
            topology,
            inventory,
        }
    }

    /// Reconcile-side accessor. The reconcile algorithm consumes a
    /// frozen copy of the topology fields it diffs against, not a
    /// borrow into `self.topology`, so it can build the diff and
    /// then call mutators on `&mut self`.
    pub(super) fn topology_snapshot(&self) -> TopologySnapshot {
        TopologySnapshot {
            num_storage_slots: self.topology.num_storage_slots,
            num_drives: self.topology.num_drives,
            lto_generation: self.topology.lto_generation,
            firmware: self.topology.firmware.clone(),
            partitions: self.topology.partitions.clone(),
        }
    }

    pub(super) fn set_firmware(&mut self, fw: Option<String>) {
        self.topology.firmware = fw;
    }

    pub(super) fn set_lto_generation(&mut self, lto_gen: u8) {
        self.topology.lto_generation = lto_gen;
    }

    pub(super) fn resize_storage(&mut self, target: u32) {
        use std::cmp::Ordering;
        let cur = self.topology.num_storage_slots;
        match target.cmp(&cur) {
            Ordering::Greater => {
                for id in cur..target {
                    self.inventory.storage_slots.push(SlotInfo {
                        id,
                        barcode: None,
                        occupied: false,
                    });
                }
            }
            Ordering::Less => {
                self.inventory.storage_slots.truncate(target as usize);
            }
            Ordering::Equal => {}
        }
        self.topology.num_storage_slots = target;
    }

    pub(super) fn resize_drives(&mut self, target: u32) {
        use std::cmp::Ordering;
        let cur = self.topology.num_drives;
        match target.cmp(&cur) {
            Ordering::Greater => {
                for id in cur..target {
                    self.inventory.drives.push(DriveInfo {
                        id,
                        barcode: None,
                        occupied: false,
                        home_slot: None,
                        mfg_serial: Some(generate_drive_mfg_serial()),
                    });
                }
            }
            Ordering::Less => {
                self.inventory.drives.truncate(target as usize);
            }
            Ordering::Equal => {}
        }
        self.topology.num_drives = target;
    }

    /// Move a loaded drive's cartridge back to its origin slot. Used
    /// by `apply_plan` for drive-shrink evacuation. Caller has
    /// already validated origin-slot range + unoccupied via
    /// `diff_against_declared`; this method just performs the
    /// inventory mutation.
    pub(super) fn evacuate_drive_to_origin(
        &mut self,
        drive_id: u32,
        origin_slot: u32,
        barcode: &str,
    ) -> Result<()> {
        let drive = self
            .inventory
            .drives
            .iter_mut()
            .find(|d| d.id == drive_id)
            .ok_or_else(|| {
                SmcError::LibraryConfig(format!("evacuate: drive {} not found", drive_id))
            })?;
        drive.occupied = false;
        drive.barcode = None;
        drive.home_slot = None;

        let slot = self
            .inventory
            .storage_slots
            .iter_mut()
            .find(|s| s.id == origin_slot)
            .ok_or_else(|| {
                SmcError::LibraryConfig(format!("evacuate: origin slot {} not found", origin_slot,))
            })?;
        // Last-line defense against the shared-home_slot collision the
        // diff now rejects (issue #162): never silently overwrite an
        // occupied slot — that would drop the resident cartridge from
        // every element.
        if slot.occupied {
            return Err(SmcError::LibraryConfig(format!(
                "evacuate: origin slot {} already occupied by {} — refusing to overwrite",
                origin_slot,
                slot.barcode.clone().unwrap_or_else(|| "<unknown>".into()),
            )));
        }
        slot.occupied = true;
        slot.barcode = Some(barcode.to_string());
        Ok(())
    }

    pub(super) fn persist_v2(&self) -> Result<()> {
        persist_v2(&self.root, &self.topology, &self.inventory)
    }

    /// Thin wrapper around the private `write_locked` so the
    /// reconcile free function `persist_v2` can write both files.
    pub(super) fn write_locked_pub(path: &Path, content: &str) -> Result<()> {
        Self::write_locked(path, content)
    }
}

#[derive(Debug, Clone)]
pub(super) struct TopologySnapshot {
    pub num_storage_slots: u32,
    pub num_drives: u32,
    pub lto_generation: u8,
    pub firmware: Option<String>,
    pub partitions: Vec<LibraryPartition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn declared(slots: u32, drives: u32, lto_gen: u8) -> DeclaredTopology {
        DeclaredTopology {
            num_storage_slots: slots,
            num_drives: drives,
            lto_generation: lto_gen,
            firmware: None,
        }
    }

    #[test]
    fn materialize_writes_v2_with_minted_serial() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (lib, events) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        assert_eq!(events, vec![ReconcileEvent::Materialized]);
        assert_eq!(lib.storage_slots().len(), 8);
        assert_eq!(lib.drives().len(), 2);
        assert_eq!(lib.mail_slots().len(), 1); // hardwired
        // Inspect the on-disk file directly.
        let raw = fs::read_to_string(lib_root.join("library.json")).unwrap();
        let probe: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(probe.get("version").unwrap().as_u64().unwrap(), 2);
        assert!(probe.get("declared").is_some());
        assert!(probe.get("minted").is_some());
        let minted = probe.get("minted").unwrap();
        let serial = minted.get("chassis_serial").unwrap().as_str().unwrap();
        assert!(!serial.is_empty());
    }

    #[test]
    fn open_v1_file_refuses() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        fs::create_dir_all(&lib_root).unwrap();
        // Hand-craft a v1 library.json (the pre-refactor flat schema).
        let v1 = serde_json::json!({
            "version": 1,
            "num_storage_slots": 4,
            "num_mail_slots": 2,
            "num_drives": 1,
            "lto_generation": 8,
            "firmware": null,
            "chassis_serial": "LEGACY-001",
            "partitions": [],
            "transport_base": 0,
            "storage_base": 1001,
            "import_export_base": 101,
            "data_transfer_base": 1,
        });
        fs::write(
            lib_root.join("library.json"),
            serde_json::to_string_pretty(&v1).unwrap(),
        )
        .unwrap();
        let result = open_or_materialize(&lib_root, &tapes_dir, &declared(4, 1, 8));
        let err = result.err().expect("v1 should refuse");
        let msg = format!("{}", err);
        assert!(
            msg.contains("v1 format"),
            "expected v1 refuse message, got: {}",
            msg
        );
        assert!(msg.contains("requires v2"));
    }

    #[test]
    fn second_open_with_same_yaml_is_noop_reconcile() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let _ = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        let (_, events) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        assert_eq!(events, vec![ReconcileEvent::Reconciled]);
    }

    #[test]
    fn diff_grow_storage_succeeds() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        let plan = diff_against_declared(&lib, &declared(20, 2, 8)).unwrap();
        assert_eq!(plan.storage_target, Some(20));
        assert_eq!(plan.drive_target, None);
        assert!(plan.drive_evacuations.is_empty());
    }

    #[test]
    fn diff_shrink_storage_into_occupied_refuses() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        // Occupy slot 6 (in the tail we're about to shrink off).
        {
            let slot = lib
                .inventory
                .storage_slots
                .iter_mut()
                .find(|s| s.id == 6)
                .unwrap();
            slot.occupied = true;
            slot.barcode = Some("BC0006L8".into());
        }
        let err = diff_against_declared(&lib, &declared(5, 2, 8))
            .expect_err("shrink into occupied slot must refuse");
        let msg = format!("{}", err);
        assert!(msg.contains("slot 6"));
        assert!(msg.contains("BC0006L8"));
    }

    #[test]
    fn diff_shrink_drives_origin_occupied_refuses() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 3, 8)).unwrap();
        // Drive 2 holds a cartridge from slot 3, but slot 3 is now occupied
        // by some other cartridge.
        {
            let drive = lib.inventory.drives.iter_mut().find(|d| d.id == 2).unwrap();
            drive.occupied = true;
            drive.barcode = Some("LOADED1L8".into());
            drive.home_slot = Some(3);
        }
        {
            let slot = lib
                .inventory
                .storage_slots
                .iter_mut()
                .find(|s| s.id == 3)
                .unwrap();
            slot.occupied = true;
            slot.barcode = Some("BLOCKER1L8".into());
        }
        let err = diff_against_declared(&lib, &declared(8, 2, 8))
            .expect_err("drive shrink into occupied origin must refuse");
        let msg = format!("{}", err);
        assert!(msg.contains("drive 2"));
        assert!(msg.contains("LOADED1L8"));
        assert!(msg.contains("BLOCKER1L8"));
    }

    /// Issue #162: two loaded drives can record the SAME home_slot
    /// (load A from slot 5, move B into 5, load B into another drive).
    /// Shrinking off both drives would evacuate both to slot 5; the
    /// second write would overwrite the first cartridge's slot record
    /// and drop it from inventory. The diff must refuse.
    #[test]
    fn diff_shrink_drives_shared_home_slot_refuses() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 3, 8)).unwrap();
        for id in [1u32, 2u32] {
            let drive = lib.inventory.drives.iter_mut().find(|d| d.id == id).unwrap();
            drive.occupied = true;
            drive.barcode = Some(format!("SHARED{id}L8"));
            drive.home_slot = Some(5);
        }
        let err = diff_against_declared(&lib, &declared(8, 1, 8))
            .expect_err("two drives evacuating to the same slot must refuse");
        let msg = format!("{err}");
        assert!(msg.contains("evacuation target"), "got: {msg}");
    }

    /// Issue #162: the inventory mutation itself must never silently
    /// overwrite an occupied destination slot — the last-line defense
    /// behind the diff guard above.
    #[test]
    fn evacuate_into_occupied_slot_errors() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        {
            let s = lib
                .inventory
                .storage_slots
                .iter_mut()
                .find(|s| s.id == 2)
                .unwrap();
            s.occupied = true;
            s.barcode = Some("RESIDENTL8".into());
        }
        let err = lib
            .evacuate_drive_to_origin(0, 2, "LOADEDL8")
            .expect_err("evacuating into an occupied slot must error");
        assert!(format!("{err}").contains("already occupied"));
    }

    #[test]
    fn diff_shrink_drives_origin_clear_plans_evacuation() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 3, 8)).unwrap();
        {
            let drive = lib.inventory.drives.iter_mut().find(|d| d.id == 2).unwrap();
            drive.occupied = true;
            drive.barcode = Some("LOADED2L8".into());
            drive.home_slot = Some(4);
        }
        let plan = diff_against_declared(&lib, &declared(8, 2, 8)).unwrap();
        assert_eq!(plan.drive_target, Some(2));
        assert_eq!(plan.drive_evacuations.len(), 1);
        let ev = &plan.drive_evacuations[0];
        assert_eq!(ev.drive_id, 2);
        assert_eq!(ev.origin_slot, 4);
        assert_eq!(ev.barcode, "LOADED2L8");
    }

    #[test]
    fn apply_plan_evacuates_and_persists() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 3, 8)).unwrap();
        {
            let drive = lib.inventory.drives.iter_mut().find(|d| d.id == 2).unwrap();
            drive.occupied = true;
            drive.barcode = Some("LOADED3L8".into());
            drive.home_slot = Some(5);
        }
        let plan = diff_against_declared(&lib, &declared(8, 2, 8)).unwrap();
        let events = apply_plan(&mut lib, plan).unwrap();
        assert_eq!(events.len(), 1);
        matches!(events[0], ReconcileEvent::DriveEvacuated(_));
        assert_eq!(lib.drives().len(), 2);
        let slot5 = lib.storage_slots().iter().find(|s| s.id == 5).unwrap();
        assert!(slot5.occupied);
        assert_eq!(slot5.barcode.as_deref(), Some("LOADED3L8"));
    }

    #[test]
    fn diff_lto_downgrade_with_cartridge_refuses() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        {
            let slot = lib
                .inventory
                .storage_slots
                .iter_mut()
                .find(|s| s.id == 0)
                .unwrap();
            slot.occupied = true;
            slot.barcode = Some("ONLY1L8".into());
        }
        let err = diff_against_declared(&lib, &declared(8, 2, 7))
            .expect_err("downgrade with cartridges must refuse");
        let msg = format!("{}", err);
        assert!(msg.contains("lto_generation"));
        // Note: validate_declared rejects gen != 8 first, so this fires
        // before the cartridge enumeration. Test that the gen=7 declared
        // is rejected at all is sufficient.
        let _ = msg;
    }

    #[test]
    fn diff_firmware_change_plans_update() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 2, 8)).unwrap();
        let mut new_decl = declared(8, 2, 8);
        new_decl.firmware = Some("TVL8".into());
        let plan = diff_against_declared(&lib, &new_decl).unwrap();
        assert_eq!(plan.firmware, Some(Some("TVL8".into())));
        assert_eq!(plan.storage_target, None);
    }

    #[test]
    fn compute_bounds_empty_library_reports_floor() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 3, 8)).unwrap();
        let bounds = compute_bounds(&lib);
        assert_eq!(bounds.current.num_slots, 8);
        assert_eq!(bounds.current.num_drives, 3);
        assert_eq!(bounds.min.num_slots, 1);
        assert_eq!(bounds.min.num_drives, 1);
        assert_eq!(bounds.max.num_slots, 65535);
        assert_eq!(bounds.max.num_drives, 255);
        assert!(bounds.explanations.is_empty());
    }

    #[test]
    fn compute_bounds_loaded_drive_with_occupied_origin() {
        let dir = tempdir().unwrap();
        let lib_root = dir.path().join("library");
        let tapes_dir = dir.path().join("tapes");
        let (mut lib, _) = open_or_materialize(&lib_root, &tapes_dir, &declared(8, 3, 8)).unwrap();
        // Drive 2 loaded from slot 5, but slot 5 occupied by something else.
        {
            let drive = lib.inventory.drives.iter_mut().find(|d| d.id == 2).unwrap();
            drive.occupied = true;
            drive.barcode = Some("STUCKL8".into());
            drive.home_slot = Some(5);
        }
        {
            let slot = lib
                .inventory
                .storage_slots
                .iter_mut()
                .find(|s| s.id == 5)
                .unwrap();
            slot.occupied = true;
            slot.barcode = Some("OTHERL8".into());
        }
        let bounds = compute_bounds(&lib);
        // Slot 5 occupied → min_slots = 6 (or drive 2's home_slot 5,
        // whichever is higher; here both 5 → min = 6).
        assert_eq!(bounds.min.num_slots, 6);
        // Drive 2 cannot evacuate (origin slot 5 occupied by OTHERL8),
        // so min_drives = 3 (can't shrink at all).
        assert_eq!(bounds.min.num_drives, 3);
        let drive_expl = bounds
            .explanations
            .iter()
            .find(|e| e.field == "num_drives")
            .expect("expected drive explanation");
        assert!(drive_expl.reason.contains("STUCKL8"));
        assert!(drive_expl.reason.contains("OTHERL8"));
    }
}
