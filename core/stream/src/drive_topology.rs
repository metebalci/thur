// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Topology-agnostic view of "what drives does this device expose?".
//!
//! Lives in `core-stream` (not `core-mediachanger`) so any consumer of the
//! drive-LUN dispatch surface can describe its drive set without
//! pulling in the medium-changer + library-inventory code that
//! `core-mediachanger` carries.

/// Abstraction over "what drives does this device expose?".
///
/// `thurvtl` implements this for `core_mediachanger::Library` (N drives indexed
/// by `drive_id`, with optional logical-partition fence). The shared
/// SCSI / iSCSI surface in `scsi-ssc` and any code in `core-stream` that
/// wants to walk drives goes through this trait so the dispatcher can
/// stay topology-agnostic.
///
/// Trait surface is intentionally minimal — element-address mapping
/// (`READ ELEMENT STATUS`, `MOVE MEDIUM`) is SMC-specific and stays on
/// `Library`'s inherent impl.
pub trait DriveTopology {
    /// Number of drives exposed.
    fn drive_count(&self) -> usize;

    /// Logical drive ids in some stable order — the same numbering
    /// used by `drive_state` / inventory entries.
    fn drive_ids(&self) -> Vec<u32>;

    /// Logical-partition name for a drive, if this topology is
    /// partitioned. Returns `None` when the topology is unpartitioned.
    /// Owned `String` rather than `&str` so impls backed by an internal
    /// lock can clone the name out under the lock and drop it before
    /// returning.
    fn partition_for_drive(&self, drive_id: u32) -> Option<String>;
}
