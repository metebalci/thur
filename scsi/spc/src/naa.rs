// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NAA-3 (Locally Assigned) + Logical Unit Group designator builders
//! for VPD page `0x83` (SPC-4 §7.7.3.6).
//!
//! Both are content-derived from `chassis_serial` (plus LUN and
//! partition) so the identifiers are stable across daemon restarts and
//! globally distinct across (chassis, partition, LUN) triples. Backup
//! software keys on these to auto-correlate "drives in this group
//! belong to one logical library."

/// Build the 8-byte NAA-3 (Locally Assigned) identifier for VPD `0x83`.
/// First byte's top nibble is `0x3` (NAA type 3); the remaining 60
/// bits are derived from BLAKE3 of `chassis_serial || lun ||
/// partition_name` so the identifier is stable across daemon restarts
/// and globally distinct across (chassis, partition, LUN) triples.
pub fn naa3_locally_assigned(
    chassis_serial: &str,
    lun: u8,
    partition_name: Option<&str>,
) -> [u8; 8] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(chassis_serial.as_bytes());
    hasher.update(b"|");
    hasher.update(&[lun]);
    hasher.update(b"|");
    hasher.update(partition_name.unwrap_or("").as_bytes());
    let h = hasher.finalize();
    let bytes = h.as_bytes();
    let mut naa = [0u8; 8];
    naa[0] = 0x30 | (bytes[0] & 0x0F);
    naa[1..8].copy_from_slice(&bytes[1..8]);
    naa
}

/// Derive the 4-byte Logical Unit Group designator value for VPD `0x83`.
/// Drives in the same partition share the same group; backup software
/// uses this to auto-correlate "drives in this group belong to one
/// logical library." Group ID is the first 4 bytes of BLAKE3 of
/// `chassis_serial || partition_name` (or `chassis_serial` alone when
/// unpartitioned).
pub fn logical_unit_group(chassis_serial: &str, partition_name: Option<&str>) -> [u8; 4] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(chassis_serial.as_bytes());
    hasher.update(b"|");
    hasher.update(partition_name.unwrap_or("").as_bytes());
    let h = hasher.finalize();
    let bytes = h.as_bytes();
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}
