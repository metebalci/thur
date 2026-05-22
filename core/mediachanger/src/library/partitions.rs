// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Partition-membership accessors + topology resize on [`Library`].
//!
//! Lifted out of `library/mod.rs` (parent was ~2300 lines after the
//! inventory split). Holds two coherent seams:
//! - partition lookup/replace verbs (`partitions`, `partition_for_*`,
//!   `get_partition`, `partition_index_one_based`, `set_partitions`);
//! - the `resize` slot-renumbering state machine that has to honor
//!   in-flight partitions and persist topology + inventory together.
//!
//! No behaviour change from the move. The three fan-out helpers used
//! only by `resize` (`take_evicted_payloads`, `count_empty_storage`,
//! `pour_into_storage`) move with it.

use crate::errors::{Result, SmcError};

use super::{
    DriveInfo, Library, LibraryPartition, MailSlotInfo, SlotInfo, generate_drive_mfg_serial,
    validate_firmware, validate_partitions,
};

/// Drain every occupied entry from the truncated tail of an inventory
/// slice, mapping it through `extract` to a relocation payload. Used
/// by `Library::resize` shrink branches to capture the cartridges/drives
/// that need to move *before* the inventory tail is truncated. `extract`
/// returns `Some(payload)` for occupied entries and `None` for empty
/// ones.
fn take_evicted_payloads<T, P>(tail: &[T], extract: impl Fn(&T) -> Option<P>) -> Vec<P> {
    tail.iter().filter_map(extract).collect()
}

/// Count empty (unoccupied) storage slots in the given slice.
fn count_empty_storage(slots: &[SlotInfo]) -> usize {
    slots.iter().filter(|s| !s.occupied).count()
}

/// Pour barcodes into unoccupied storage slots until either the iterator
/// or the empty positions run out. Caller is responsible for sizing —
/// `resize` does the fail-fast capacity check up front so leftover items
/// here would indicate a bug.
fn pour_into_storage(barcodes: &mut impl Iterator<Item = String>, slots: &mut [SlotInfo]) {
    for slot in slots.iter_mut() {
        if !slot.occupied {
            match barcodes.next() {
                Some(barcode) => {
                    slot.barcode = Some(barcode);
                    slot.occupied = true;
                }
                None => return,
            }
        }
    }
}

impl Library {
    /// Currently-defined logical partitions. Empty slice = legacy
    /// single-partition library.
    pub fn partitions(&self) -> &[LibraryPartition] {
        &self.topology.partitions
    }

    /// Find the partition that owns a given drive id, by name. Returns
    /// `None` when the library is unpartitioned or the drive belongs
    /// to none (which only happens before validate_partitions has run).
    pub fn partition_for_drive(&self, drive_id: u32) -> Option<&str> {
        self.topology
            .partitions
            .iter()
            .find(|p| p.drives.contains(&drive_id))
            .map(|p| p.name.as_str())
    }

    pub fn partition_for_storage_slot(&self, slot_id: u32) -> Option<&str> {
        self.topology
            .partitions
            .iter()
            .find(|p| p.storage_slots.contains(slot_id))
            .map(|p| p.name.as_str())
    }

    pub fn partition_for_mail_slot(&self, slot_id: u32) -> Option<&str> {
        self.topology
            .partitions
            .iter()
            .find(|p| p.mail_slots.contains(slot_id))
            .map(|p| p.name.as_str())
    }

    /// Look up a partition by name.
    pub fn get_partition(&self, name: &str) -> Option<&LibraryPartition> {
        self.topology.partitions.iter().find(|p| p.name == name)
    }

    /// One-based partition index used for the SCSI VPD `0x80`
    /// `_LLNN` Unit Serial Number suffix (first partition is
    /// `_LL01`). Returns `1` for unpartitioned libraries
    /// (non-partitioned chassis report as Partition 1).
    pub fn partition_index_one_based(&self, partition_name: Option<&str>) -> u8 {
        match partition_name {
            None => 1,
            Some(name) => self
                .topology
                .partitions
                .iter()
                .position(|p| p.name == name)
                .map(|i| (i + 1).min(99) as u8)
                .unwrap_or(1),
        }
    }

    /// Replace the entire partition layout. Validates non-overlap and
    /// full coverage against the current chassis topology before
    /// persisting. An empty `partitions` slice reverts the library to
    /// legacy single-partition mode.
    pub fn set_partitions(&mut self, partitions: Vec<LibraryPartition>) -> Result<()> {
        validate_partitions(
            &partitions,
            self.topology.num_storage_slots,
            self.topology.num_mail_slots,
            self.topology.num_drives,
        )?;
        self.topology.partitions = partitions;

        let lib_path = self.root.join("library.json");
        Self::write_locked(&lib_path, &serde_json::to_string_pretty(&self.topology)?)?;
        Ok(())
    }

    /// Resize library topology (add/remove slots and drives, change LTO generation,
    /// override firmware revision). Returns the new counts (slots, mail_slots,
    /// drives, lto_generation). Note: This modifies both topology AND inventory.
    pub fn resize(
        &mut self,
        new_storage_slots: Option<u32>,
        new_mail_slots: Option<u32>,
        new_drives: Option<u32>,
        new_lto_generation: Option<u8>,
        new_firmware: Option<Option<String>>,
    ) -> Result<(u32, u32, u32, u8)> {
        let current_slots = self.topology.num_storage_slots;
        let current_mail = self.topology.num_mail_slots;
        let current_drives = self.topology.num_drives;

        // Storage slots: relocate within own bucket only (no fallback).
        if let Some(new_count) = new_storage_slots {
            if !(1..=65535).contains(&new_count) {
                return Err(SmcError::InvalidOp(
                    "Cartridge slots must be between 1 and 65535",
                ));
            }
            match new_count.cmp(&current_slots) {
                std::cmp::Ordering::Less => {
                    let evictees: Vec<String> = take_evicted_payloads(
                        &self.inventory.storage_slots[new_count as usize..],
                        |s| if s.occupied { s.barcode.clone() } else { None },
                    );
                    if !evictees.is_empty() {
                        let kept = &mut self.inventory.storage_slots[..new_count as usize];
                        let primary_empty = count_empty_storage(kept);
                        if primary_empty < evictees.len() {
                            return Err(SmcError::LibraryConfig(format!(
                                "Cannot shrink to {} slots: {} cartridge(s) in slots {}-{} and only {} empty slot(s) available",
                                new_count,
                                evictees.len(),
                                new_count,
                                current_slots - 1,
                                primary_empty,
                            )));
                        }
                        let mut iter = evictees.into_iter();
                        pour_into_storage(&mut iter, kept);
                    }
                    self.inventory.storage_slots.truncate(new_count as usize);
                    self.topology.num_storage_slots = new_count;
                }
                std::cmp::Ordering::Greater => {
                    for i in current_slots..new_count {
                        self.inventory.storage_slots.push(SlotInfo {
                            id: i,
                            barcode: None,
                            occupied: false,
                        });
                    }
                    self.topology.num_storage_slots = new_count;
                }
                std::cmp::Ordering::Equal => {}
            }
        }

        // Mail slots: prefer kept mail slots, fall back to storage slots.
        if let Some(new_count) = new_mail_slots {
            if new_count > 65535 {
                return Err(SmcError::InvalidOp(
                    "Mail slots must be between 0 and 65535",
                ));
            }
            match new_count.cmp(&current_mail) {
                std::cmp::Ordering::Less => {
                    let evictees: Vec<String> = take_evicted_payloads(
                        &self.inventory.mail_slots[new_count as usize..],
                        |s| if s.occupied { s.barcode.clone() } else { None },
                    );
                    if !evictees.is_empty() {
                        let primary_empty = self.inventory.mail_slots[..new_count as usize]
                            .iter()
                            .filter(|s| !s.occupied)
                            .count();
                        let secondary_empty = count_empty_storage(&self.inventory.storage_slots);
                        let total_empty = primary_empty + secondary_empty;
                        if total_empty < evictees.len() {
                            return Err(SmcError::LibraryConfig(format!(
                                "Cannot shrink to {} mail slot(s): Mail slot(s) {}-{} have cartridge(s) and only {} empty slot(s) available",
                                new_count,
                                new_count,
                                current_mail - 1,
                                total_empty,
                            )));
                        }
                        let mut iter = evictees.into_iter();
                        // Prefer mail slots in the kept range, then spill to storage.
                        for slot in self.inventory.mail_slots[..new_count as usize].iter_mut() {
                            if !slot.occupied
                                && let Some(barcode) = iter.next()
                            {
                                slot.barcode = Some(barcode);
                                slot.occupied = true;
                            }
                        }
                        pour_into_storage(&mut iter, &mut self.inventory.storage_slots);
                    }
                    self.inventory.mail_slots.truncate(new_count as usize);
                    self.topology.num_mail_slots = new_count;
                }
                std::cmp::Ordering::Greater => {
                    for i in current_mail..new_count {
                        self.inventory.mail_slots.push(MailSlotInfo {
                            id: i,
                            barcode: None,
                            occupied: false,
                            accessible: true,
                        });
                    }
                    self.topology.num_mail_slots = new_count;
                }
                std::cmp::Ordering::Equal => {}
            }
        }

        // Drives: prefer kept drives, fall back to storage slots. Drive
        // payload carries (barcode, home_slot) — home_slot is preserved
        // when the destination is another drive, dropped when we spill
        // to storage (a slot has no home_slot field).
        if let Some(new_count) = new_drives {
            if !(1..=255).contains(&new_count) {
                return Err(SmcError::InvalidOp("Drives must be between 1 and 255"));
            }
            match new_count.cmp(&current_drives) {
                std::cmp::Ordering::Less => {
                    let evictees: Vec<(String, Option<u16>)> =
                        take_evicted_payloads(&self.inventory.drives[new_count as usize..], |d| {
                            if d.occupied {
                                d.barcode.clone().map(|b| (b, d.home_slot))
                            } else {
                                None
                            }
                        });
                    if !evictees.is_empty() {
                        let primary_empty = self.inventory.drives[..new_count as usize]
                            .iter()
                            .filter(|d| !d.occupied)
                            .count();
                        let secondary_empty = count_empty_storage(&self.inventory.storage_slots);
                        if primary_empty + secondary_empty < evictees.len() {
                            return Err(SmcError::LibraryConfig(format!(
                                "Cannot shrink to {} drive(s): Drive(s) {}-{} have loaded cartridge(s) and insufficient empty drives ({}) or slots ({}) available",
                                new_count,
                                new_count,
                                current_drives - 1,
                                primary_empty,
                                secondary_empty,
                            )));
                        }
                        let mut iter = evictees.into_iter();
                        for drive in self.inventory.drives[..new_count as usize].iter_mut() {
                            if !drive.occupied
                                && let Some((barcode, home_slot)) = iter.next()
                            {
                                drive.barcode = Some(barcode);
                                drive.occupied = true;
                                drive.home_slot = home_slot;
                            }
                        }
                        // Spill remainder into storage; home_slot is dropped on
                        // the slot (slots don't carry that field).
                        let mut barcode_only = iter.map(|(b, _)| b);
                        pour_into_storage(&mut barcode_only, &mut self.inventory.storage_slots);
                    }
                    self.inventory.drives.truncate(new_count as usize);
                    self.topology.num_drives = new_count;
                }
                std::cmp::Ordering::Greater => {
                    for i in current_drives..new_count {
                        self.inventory.drives.push(DriveInfo {
                            id: i,
                            barcode: None,
                            occupied: false,
                            home_slot: None,
                            mfg_serial: Some(generate_drive_mfg_serial()),
                        });
                    }
                    self.topology.num_drives = new_count;
                }
                std::cmp::Ordering::Equal => {}
            }
        }

        // Validate and change LTO generation
        if let Some(new_lto) = new_lto_generation {
            if !(7..=8).contains(&new_lto) {
                return Err(SmcError::InvalidOp("LTO generation must be 7 or 8"));
            }
            self.topology.lto_generation = new_lto;
        }

        // Apply firmware override. `Some(None)` clears any prior override
        // (revert to per-LTO default); `Some(Some(s))` validates + sets;
        // `None` leaves whatever's there alone.
        if let Some(new_fw) = new_firmware {
            if let Some(ref s) = new_fw {
                validate_firmware(s)?;
            }
            self.topology.firmware = new_fw;
        }

        // If partitions are defined, the new chassis size must still
        // fully cover them — refuse a shrink that would orphan a
        // partition's claimed slots/drives. The operator can drop the
        // partition first via `library partition delete` (or modify it
        // via `library partition modify`) and retry.
        if !self.topology.partitions.is_empty() {
            validate_partitions(
                &self.topology.partitions,
                self.topology.num_storage_slots,
                self.topology.num_mail_slots,
                self.topology.num_drives,
            )?;
        }

        // Persist both topology and inventory with file locking
        let lib_path = self.root.join("library.json");
        let inv_path = self.root.join("inventory.json");
        Self::write_locked(&lib_path, &serde_json::to_string_pretty(&self.topology)?)?;
        Self::write_locked(&inv_path, &serde_json::to_string_pretty(&self.inventory)?)?;

        Ok((
            self.topology.num_storage_slots,
            self.topology.num_mail_slots,
            self.topology.num_drives,
            self.topology.lto_generation,
        ))
    }
}
