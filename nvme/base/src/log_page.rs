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
/// Reservation Notification log page (NVMe NVM Command Set). One
/// 64-byte entry per Get Log Page.
pub const RESERVATION_NOTIFICATION_LEN: usize = 64;

/// Log Page IDs hosts query against an NVMe-oF controller.
pub mod lid {
    pub const ERROR_INFO: u8 = 0x01;
    pub const SMART_HEALTH: u8 = 0x02;
    pub const FIRMWARE_SLOT: u8 = 0x03;
    /// Reservation Notification (NVMe NVM Command Set). Carries the
    /// most-recent reservation event for the host to consume.
    pub const RESERVATION_NOTIFICATION: u8 = 0x80;
}

/// Reservation Notification Log Page Type (byte 8 of the LID 0x80
/// entry). 0 = no notification available; the other three name the
/// reservation event class.
pub mod resv_notif_type {
    pub const EMPTY: u8 = 0;
    pub const REGISTRATION_PREEMPTED: u8 = 1;
    pub const RESERVATION_RELEASED: u8 = 2;
    pub const RESERVATION_PREEMPTED: u8 = 3;
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

/// Build a Reservation Notification log page (LID 0x80, 64 bytes).
///
/// Get Log Page LID 0x80 returns the single oldest unconsumed
/// notification for the host:
///
/// - bytes 0..8  — Log Page Count (u64 LE). A controller-global,
///   monotonically increasing identifier; 0 means "no notification"
///   (the host treats type 0 / count 0 as an empty page).
/// - byte  8     — Reservation Notification Log Page Type (see
///   [`resv_notif_type`]).
/// - byte  9     — Number of Available Log Pages: how many *more*
///   notifications remain queued for the host *after* this one.
/// - bytes 12..16 — Namespace ID (u32 LE) the event applies to.
///
/// All other bytes are reserved / zero. An empty page (no event
/// queued) is the all-zero buffer: build it with
/// `reservation_notification(0, resv_notif_type::EMPTY, 0, 0)`.
pub fn reservation_notification(
    log_page_count: u64,
    notification_type: u8,
    num_available: u8,
    nsid: u32,
) -> [u8; RESERVATION_NOTIFICATION_LEN] {
    let mut buf = [0u8; RESERVATION_NOTIFICATION_LEN];
    buf[0..8].copy_from_slice(&log_page_count.to_le_bytes());
    buf[8] = notification_type;
    buf[9] = num_available;
    buf[12..16].copy_from_slice(&nsid.to_le_bytes());
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

    #[test]
    fn reservation_notification_layout() {
        let log = reservation_notification(
            0x0102_0304_0506_0708,
            resv_notif_type::RESERVATION_PREEMPTED,
            2,
            0x0000_002A,
        );
        assert_eq!(log.len(), RESERVATION_NOTIFICATION_LEN);
        // Log Page Count, u64 LE at 0..8.
        assert_eq!(
            u64::from_le_bytes(log[0..8].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        // Type at byte 8, available count at byte 9.
        assert_eq!(log[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(log[9], 2);
        // bytes 10..12 reserved / zero.
        assert_eq!(&log[10..12], &[0, 0]);
        // NSID, u32 LE at 12..16.
        assert_eq!(u32::from_le_bytes(log[12..16].try_into().unwrap()), 0x2A);
        // Tail reserved / zero.
        assert!(log[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn reservation_notification_empty_page_is_all_zero() {
        let log = reservation_notification(0, resv_notif_type::EMPTY, 0, 0);
        assert!(log.iter().all(|&b| b == 0));
    }
}
