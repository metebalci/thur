// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Partition lookup/replace verbs on [`Library`].
//!
//! Lifted out of `library/mod.rs` to keep that file scoped to library
//! bring-up + accessors. Verbs: `partitions`, `partition_for_*`,
//! `get_partition`, `partition_index_one_based`, `set_partitions`.
//!
//! Slot-renumbering state has moved out under the chassis-into-YAML
//! refactor — chassis topology now lives in `thurvtl.yaml`'s
//! `library:` block; the reconcile engine in `library/reconcile.rs`
//! diffs and applies grow / shrink edits on every daemon start.

use crate::errors::Result;

use super::{Library, LibraryPartition, validate_partitions};

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
}
