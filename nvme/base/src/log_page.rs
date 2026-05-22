// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Log Page builders (NVMe Base §5.16).
//!
//! Hosts use Get Log Page (Admin opcode 0x02) to read controller and
//! namespace health / telemetry / firmware data. The structures
//! returned are fixed-layout binary blobs; this module emits the
//! ones a fabrics target actually has to populate to keep Linux
//! happy:
//!
//! - LID 0x01 — Error Information (64 bytes per entry × N entries)
//! - LID 0x02 — SMART / Health Information (512 bytes)
//! - LID 0x03 — Firmware Slot Information (512 bytes)
//!
//! Everything else (Changed Namespace List, Commands Supported and
//! Effects, ANA, Sanitize Status, Endurance Group, ...) is optional
//! per spec and currently returned as Invalid Field by the
//! dispatcher.

/// SMART / Health Information log (NVMe Base §5.16.1.3). 512 bytes.
pub const SMART_HEALTH_LEN: usize = 512;
/// Error Information log entry (NVMe Base §5.16.1.2). 64 bytes each.
pub const ERROR_INFO_ENTRY_LEN: usize = 64;
/// Firmware Slot Information log (NVMe Base §5.16.1.4). 512 bytes.
pub const FIRMWARE_SLOT_INFO_LEN: usize = 512;

/// Log Page IDs hosts query against an NVMe-oF controller.
pub mod lid {
    pub const ERROR_INFO: u8 = 0x01;
    pub const SMART_HEALTH: u8 = 0x02;
    pub const FIRMWARE_SLOT: u8 = 0x03;
}

/// Build a SMART / Health Information page.
///
/// A software target has no real telemetry; the only field hosts
/// reliably inspect is Composite Temperature. We return a constant
/// 300 K (27 °C) so monitoring dashboards don't show "unknown" or
/// trigger thermal alarms. Critical Warning and Available Spare
/// stay zero — there's nothing wrong and no spare blocks model
/// applies to a software backend.
pub fn smart_health() -> [u8; SMART_HEALTH_LEN] {
    let mut buf = [0u8; SMART_HEALTH_LEN];
    // Composite Temperature in Kelvin at bytes 1..3.
    let temp_k: u16 = 300;
    buf[1..3].copy_from_slice(&temp_k.to_le_bytes());
    // Available Spare = 100 (percent), threshold = 10. Lets hosts'
    // monitoring scripts compute a safe-headroom value.
    buf[3] = 100;
    buf[4] = 10;
    buf
}

/// Build a single Error Information log entry — all zeros (no error
/// to report). ELPE in Identify Controller is 0 by default, so the
/// host only ever asks for one entry.
pub fn error_info_zero_entry() -> [u8; ERROR_INFO_ENTRY_LEN] {
    [0u8; ERROR_INFO_ENTRY_LEN]
}

/// Build a Firmware Slot Information log.
///
/// - AFI (byte 0): bits 2:0 = active slot, bits 6:4 = next active
///   slot. We populate slot 1 only, with current = next = 1.
/// - FRS1 (bytes 8..16): 8 ASCII bytes, space-padded, of the active
///   firmware revision. Caller supplies the revision string;
///   truncated to 8 bytes if longer.
pub fn firmware_slot_info(active_revision: &str) -> [u8; FIRMWARE_SLOT_INFO_LEN] {
    let mut buf = [0u8; FIRMWARE_SLOT_INFO_LEN];
    // AFI: active=1 (bits 2:0), next active=1 (bits 6:4).
    buf[0] = 0b0001_0001;
    // FRS1 at bytes 8..16, ASCII, space-padded.
    let revision = active_revision.as_bytes();
    let n = revision.len().min(8);
    buf[8..8 + n].copy_from_slice(&revision[..n]);
    for b in buf[8 + n..16].iter_mut() {
        *b = b' ';
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_health_carries_temperature() {
        let log = smart_health();
        assert_eq!(log.len(), SMART_HEALTH_LEN);
        let temp = u16::from_le_bytes([log[1], log[2]]);
        assert_eq!(temp, 300);
        assert_eq!(log[3], 100);
        assert_eq!(log[4], 10);
    }

    #[test]
    fn firmware_slot_info_pads_revision_to_8_bytes() {
        let log = firmware_slot_info("0.1.0");
        assert_eq!(log.len(), FIRMWARE_SLOT_INFO_LEN);
        assert_eq!(log[0], 0b0001_0001);
        assert_eq!(&log[8..16], b"0.1.0   ");
    }

    #[test]
    fn firmware_slot_info_truncates_long_revision() {
        let log = firmware_slot_info("0.1.0-alpha.1+x");
        // Truncated to 8 bytes.
        assert_eq!(&log[8..16], b"0.1.0-al");
    }
}
