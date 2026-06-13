// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVM Command Set reservation wire shapes.
//!
//! The protocol-native analog of SCSI PERSISTENT RESERVE. The
//! reservation *state machine* is shared with the SCSI side
//! (`scsi_spc::reservations::ReservationManager`); this module only
//! holds the NVMe-specific wire encoding — the CDW10/CDW11 field
//! layouts for Reservation Register / Acquire / Release, the
//! in-capsule key payloads, the RTYPE numbering (which differs from
//! the SCSI type byte), and the Reservation Status Data Structure the
//! Reservation Report returns.
//!
//! Endianness: little-endian on the wire (keys and the status
//! structure both).
//!
//! The RTYPE map is kept as raw `u8 ↔ u8` so this crate doesn't take
//! a dependency on `scsi-spc`; the `nvme-nvm` adapter (which depends
//! on both) converts the SCSI type byte to `ReservationType`.

/// Reservation Register action (RREGA), Reservation Register CDW10[2:0].
pub const RREGA_REGISTER: u8 = 0;
pub const RREGA_UNREGISTER: u8 = 1;
pub const RREGA_REPLACE: u8 = 2;

/// Reservation Acquire action (RACQA), Reservation Acquire CDW10[2:0].
pub const RACQA_ACQUIRE: u8 = 0;
pub const RACQA_PREEMPT: u8 = 1;
pub const RACQA_PREEMPT_ABORT: u8 = 2;

/// Reservation Release action (RRELA), Reservation Release CDW10[2:0].
pub const RRELA_RELEASE: u8 = 0;
pub const RRELA_CLEAR: u8 = 1;

/// CPTPL (Change Persist Through Power Loss State), Register
/// CDW10[31:30]. `0b00` = no change, `0b10` = clear PTPL, `0b11` =
/// set PTPL. The adapter maps no-change / clear / set onto the shared
/// manager's per-LU APTPL bit (issue #57); a set request is honored
/// when persistence is wired and rejected otherwise (mirror of the
/// SCSI APTPL=1 reject).
pub const CPTPL_NO_CHANGE: u8 = 0b00;
pub const CPTPL_CLEAR: u8 = 0b10;
pub const CPTPL_PERSIST: u8 = 0b11;

/// Reservation Status Data Structure header length, short form (EDS=0):
/// bytes 0..23, registered-controller entries start at byte 24.
pub const STATUS_HEADER_LEN: usize = 24;
/// Reservation Status Data Structure header length, extended form
/// (EDS=1): NVMe Base 1.4 §6.13 reserves bytes 24..63, so extended
/// entries start at byte 64 (issue #129).
pub const STATUS_HEADER_EXT_LEN: usize = 64;
/// Registered Controller Data Structure length (EDS = 0).
pub const REG_CTLR_LEN: usize = 24;
/// Registered Controller Extended Data Structure length (EDS = 1).
pub const REG_CTLR_EXT_LEN: usize = 64;

// ---- CDW10 / CDW11 field extractors ----

/// RREGA / RACQA / RRELA — the action code in CDW10[2:0].
pub fn action(cdw10: u32) -> u8 {
    (cdw10 & 0x7) as u8
}

/// IEKEY (Ignore Existing Key) — CDW10[3].
pub fn iekey(cdw10: u32) -> bool {
    (cdw10 >> 3) & 1 != 0
}

/// CPTPL — Register CDW10[31:30].
pub fn cptpl(cdw10: u32) -> u8 {
    ((cdw10 >> 30) & 0x3) as u8
}

/// RTYPE — Acquire / Release CDW10[15:8].
pub fn rtype(cdw10: u32) -> u8 {
    ((cdw10 >> 8) & 0xFF) as u8
}

/// EDS (Extended Data Structure) — Reservation Report CDW11[0]. When
/// set, the host wants 64-byte controller entries carrying the full
/// 128-bit Host Identifier.
pub fn report_eds(cdw11: u32) -> bool {
    cdw11 & 1 != 0
}

// ---- in-capsule key payloads ----

/// Reservation Register data: CRKEY (current) + NRKEY (new), 16 bytes.
pub fn parse_register_keys(data: &[u8]) -> Option<(u64, u64)> {
    if data.len() < 16 {
        return None;
    }
    let crkey = u64::from_le_bytes(data[0..8].try_into().ok()?);
    let nrkey = u64::from_le_bytes(data[8..16].try_into().ok()?);
    Some((crkey, nrkey))
}

/// Reservation Acquire data: CRKEY (current) + PRKEY (preempt), 16
/// bytes. PRKEY is meaningful only for Preempt / Preempt-and-Abort.
pub fn parse_acquire_keys(data: &[u8]) -> Option<(u64, u64)> {
    if data.len() < 16 {
        return None;
    }
    let crkey = u64::from_le_bytes(data[0..8].try_into().ok()?);
    let prkey = u64::from_le_bytes(data[8..16].try_into().ok()?);
    Some((crkey, prkey))
}

/// Reservation Release data: CRKEY (current), 8 bytes.
pub fn parse_release_key(data: &[u8]) -> Option<u64> {
    if data.len() < 8 {
        return None;
    }
    Some(u64::from_le_bytes(data[0..8].try_into().ok()?))
}

// ---- RTYPE numbering ----
//
// NVMe numbers the six reservation types 1..6 contiguously; the SCSI
// type byte uses 0x01 / 0x03 / 0x05 / 0x06 / 0x07 / 0x08. Same order,
// different values — the single most error-prone spot, so it lives in
// one place with a round-trip test.

/// NVMe RTYPE (1..6) → SCSI PERSISTENT RESERVE type byte. `None` for
/// 0 / 7+ (a host requested an unsupported / reserved type).
pub fn nvme_rtype_to_scsi_byte(rtype: u8) -> Option<u8> {
    Some(match rtype {
        1 => 0x01, // Write Exclusive
        2 => 0x03, // Exclusive Access
        3 => 0x05, // Write Exclusive – Registrants Only
        4 => 0x06, // Exclusive Access – Registrants Only
        5 => 0x07, // Write Exclusive – All Registrants
        6 => 0x08, // Exclusive Access – All Registrants
        _ => return None,
    })
}

/// SCSI PERSISTENT RESERVE type byte → NVMe RTYPE (1..6). `None` for
/// any byte that isn't one of the six.
pub fn scsi_byte_to_nvme_rtype(scsi: u8) -> Option<u8> {
    Some(match scsi {
        0x01 => 1,
        0x03 => 2,
        0x05 => 3,
        0x06 => 4,
        0x07 => 5,
        0x08 => 6,
        _ => return None,
    })
}

// ---- Reservation Status Data Structure ----

/// One registered-controller entry for the Reservation Report. The
/// caller fills `cntlid` with a real per-controller CNTLID (the
/// registrant host's representative live controller, or 0 if it has
/// none). The registration is host-keyed, so the report stays one
/// entry per HOSTID; the identity that matters for fencing is `hostid`.
pub struct ReportEntry {
    pub cntlid: u16,
    pub holds_reservation: bool,
    pub hostid: [u8; 16],
    pub rkey: u64,
}

/// Build the Reservation Status Data Structure (NVM Command Set
/// reservations). `rtype_nvme` is the current reservation type (0 if
/// none). `eds` selects the 64-byte extended controller entry (full
/// 128-bit HOSTID) vs the 24-byte form (low 64 bits of HOSTID).
pub fn reservation_status(
    generation: u32,
    rtype_nvme: u8,
    entries: &[ReportEntry],
    eds: bool,
    ptpls: bool,
) -> Vec<u8> {
    let entry_len = if eds { REG_CTLR_EXT_LEN } else { REG_CTLR_LEN };
    let header_len = if eds {
        STATUS_HEADER_EXT_LEN
    } else {
        STATUS_HEADER_LEN
    };
    let mut out = vec![0u8; header_len + entries.len() * entry_len];
    out[0..4].copy_from_slice(&generation.to_le_bytes());
    out[4] = rtype_nvme;
    out[5..7].copy_from_slice(&(entries.len() as u16).to_le_bytes()); // REGCTL
    // bytes 7..9 reserved; byte 9 PTPLS = current Persist Through Power
    // Loss State for the namespace (issue #57); the rest of the header
    // (bytes 10..24 short, 10..64 extended) is reserved.
    out[9] = u8::from(ptpls);
    for (i, e) in entries.iter().enumerate() {
        let base = header_len + i * entry_len;
        let entry = &mut out[base..base + entry_len];
        // Registered Controller (Extended) Data Structure, NVMe Base 1.4
        // §6.13: CNTLID 0..2, RCSTS at 2, bytes 3..8 reserved. The
        // earlier layout shifted HOSTID/RKEY 3 bytes left into the
        // reserved region, so the Linux PR API decoded garbled keys and
        // host ids (issue #129).
        entry[0..2].copy_from_slice(&e.cntlid.to_le_bytes());
        entry[2] = u8::from(e.holds_reservation); // RCSTS bit 0
        if eds {
            // Extended: RKEY at 8..16, full 128-bit HOSTID at 16..32.
            entry[8..16].copy_from_slice(&e.rkey.to_le_bytes());
            entry[16..32].copy_from_slice(&e.hostid);
        } else {
            // Short: low 64 bits of HOSTID at 8..16, RKEY at 16..24.
            entry[8..16].copy_from_slice(&e.hostid[0..8]);
            entry[16..24].copy_from_slice(&e.rkey.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdw10_extractors() {
        // action=2, IEKEY=1, RTYPE=0x03, CPTPL=0b11.
        let cdw10 = 0b10u32 | (1 << 3) | (0x03 << 8) | (0b11 << 30);
        assert_eq!(action(cdw10), 2);
        assert!(iekey(cdw10));
        assert_eq!(rtype(cdw10), 0x03);
        assert_eq!(cptpl(cdw10), 0b11);
        assert!(!iekey(0));
        assert!(report_eds(1));
        assert!(!report_eds(0));
    }

    #[test]
    fn key_payload_parsers() {
        let mut data = [0u8; 16];
        data[0..8].copy_from_slice(&0xAAAA_BBBBu64.to_le_bytes());
        data[8..16].copy_from_slice(&0xCCCC_DDDDu64.to_le_bytes());
        assert_eq!(parse_register_keys(&data), Some((0xAAAA_BBBB, 0xCCCC_DDDD)));
        assert_eq!(parse_acquire_keys(&data), Some((0xAAAA_BBBB, 0xCCCC_DDDD)));
        assert_eq!(parse_release_key(&data), Some(0xAAAA_BBBB));
        assert_eq!(parse_register_keys(&[0u8; 8]), None);
        assert_eq!(parse_release_key(&[0u8; 4]), None);
    }

    #[test]
    fn rtype_map_round_trips() {
        for (nvme, scsi) in [
            (1, 0x01),
            (2, 0x03),
            (3, 0x05),
            (4, 0x06),
            (5, 0x07),
            (6, 0x08),
        ] {
            assert_eq!(nvme_rtype_to_scsi_byte(nvme), Some(scsi));
            assert_eq!(scsi_byte_to_nvme_rtype(scsi), Some(nvme));
        }
        assert_eq!(nvme_rtype_to_scsi_byte(0), None);
        assert_eq!(nvme_rtype_to_scsi_byte(7), None);
        assert_eq!(scsi_byte_to_nvme_rtype(0x02), None);
    }

    #[test]
    fn status_short_form_byte_exact() {
        let hostid = [0x11; 16];
        let entries = [ReportEntry {
            cntlid: 1,
            holds_reservation: true,
            hostid,
            rkey: 0xDEAD_BEEF,
        }];
        let buf = reservation_status(7, 1, &entries, false, false);
        assert_eq!(buf.len(), STATUS_HEADER_LEN + REG_CTLR_LEN);
        // Header
        assert_eq!(&buf[0..4], &7u32.to_le_bytes()); // GEN
        assert_eq!(buf[4], 1); // RTYPE
        assert_eq!(&buf[5..7], &1u16.to_le_bytes()); // REGCTL
        assert_eq!(buf[9], 0); // PTPLS = 0 (ptpls arg false)
        // Entry — spec offsets (NVMe Base 1.4 §6.13): RCSTS at 2, bytes
        // 3..8 reserved, HOSTID at 8..16, RKEY at 16..24 (issue #129).
        let e = &buf[STATUS_HEADER_LEN..];
        assert_eq!(&e[0..2], &1u16.to_le_bytes()); // CNTLID
        assert_eq!(e[2], 1); // RCSTS holds-reservation
        assert_eq!(&e[3..8], &[0u8; 5]); // reserved
        assert_eq!(&e[8..16], &hostid[0..8]); // HOSTID low 64
        assert_eq!(&e[16..24], &0xDEAD_BEEFu64.to_le_bytes()); // RKEY
    }

    #[test]
    fn status_extended_form_byte_exact() {
        let hostid = [0x22; 16];
        let entries = [ReportEntry {
            cntlid: 1,
            holds_reservation: false,
            hostid,
            rkey: 0x1234_5678,
        }];
        // ptpls=true here: PTPLS (byte 9) must reflect it. Extended form
        // uses the 64-byte header (issue #129).
        let buf = reservation_status(3, 2, &entries, true, true);
        assert_eq!(buf.len(), STATUS_HEADER_EXT_LEN + REG_CTLR_EXT_LEN);
        assert_eq!(buf[9], 1); // PTPLS = 1
        // Extended entries start at byte 64, not 24.
        let e = &buf[STATUS_HEADER_EXT_LEN..];
        assert_eq!(&e[0..2], &1u16.to_le_bytes()); // CNTLID
        assert_eq!(e[2], 0); // not holder
        assert_eq!(&e[3..8], &[0u8; 5]); // reserved
        assert_eq!(&e[8..16], &0x1234_5678u64.to_le_bytes()); // RKEY at 8..16
        assert_eq!(&e[16..32], &hostid); // full 128-bit HOSTID at 16..32
    }

    #[test]
    fn status_empty_is_header_only() {
        let buf = reservation_status(0, 0, &[], false, false);
        assert_eq!(buf.len(), STATUS_HEADER_LEN);
        assert_eq!(&buf[5..7], &0u16.to_le_bytes());
    }

    #[test]
    fn status_extended_empty_uses_64_byte_header() {
        let buf = reservation_status(0, 0, &[], true, false);
        assert_eq!(buf.len(), STATUS_HEADER_EXT_LEN);
    }
}
