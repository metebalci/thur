// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! INQUIRY standard data layout (SPC-4 §6.4.2).
//!
//! Both products emit the same byte-for-byte standard-data shape;
//! only the peripheral type and the vendor / product / revision
//! ASCII triple change. This module owns the layout — callers
//! supply the variable bits and get a 96-byte response back.
//!
//! Convention: vendor / product / revision are space-padded ASCII
//! to their fixed widths (8 / 16 / 4 bytes). Strings longer than
//! the field are silently truncated; strings shorter are right-
//! padded with spaces.

/// SCSI peripheral device type (SPC-4 Table 60). The two products
/// today use:
/// - `MediumChanger` (0x08) — thurvtl LUN 0 (SMC-3).
/// - `SequentialAccess` (0x01) — thurvtl drive LUNs 1..N (SSC-4).
/// - `DirectAccess` (0x00) — thurvsa volume LUNs (SBC-3).
///
/// Other types are listed for completeness; a future zoned-block
/// target would pick `ZonedBlock` (0x14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PeripheralType {
    DirectAccess = 0x00,
    SequentialAccess = 0x01,
    Printer = 0x02,
    Processor = 0x03,
    WriteOnce = 0x04,
    CdDvd = 0x05,
    OpticalCard = 0x07,
    MediumChanger = 0x08,
    StorageArrayController = 0x0C,
    EnclosureServices = 0x0D,
    ObjectStorage = 0x11,
    ZonedBlock = 0x14,
    /// Sentinel "no logical unit at this address" (SAM-5 §4.6.6 +
    /// SPC-4 §4.4.6). Combined with peripheral qualifier 0b011 it
    /// gives the byte pattern 0x7F that initiators use to
    /// distinguish "LUN unmapped" from "LUN unknown".
    NoLun = 0x1F,
}

/// SCSI peripheral qualifier (SPC-4 §6.4.2 byte 0 high 3 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PeripheralQualifier {
    /// 0b000 — peripheral device is connected and ready (or not
    /// ready but capable). Default for every mapped LUN.
    Connected = 0x00,
    /// 0b001 — physically connected but no peripheral device
    /// available. Not used by either product today.
    NotConnected = 0x01,
    /// 0b011 — no peripheral device at this LU address. Paired
    /// with [`PeripheralType::NoLun`] to form the SAM-5 "no LUN"
    /// sentinel.
    NoDevice = 0x03,
}

/// Identity strings carried in INQUIRY standard data bytes 8..36.
/// Convention: ASCII, space-padded to width.
#[derive(Debug, Clone, Copy)]
pub struct Identity<'a> {
    /// 8-byte vendor identification (e.g. `"MB"`).
    pub vendor: &'a str,
    /// 16-byte product identification (e.g. `"ULTRIUM-TD8     "`).
    pub product: &'a str,
    /// 4-byte product revision level (e.g. `"V01H"`).
    pub revision: &'a str,
}

/// Per-product capability flags carried in INQUIRY standard data
/// bytes 2 / 3 / 7. The three products diverge here:
/// - thurvtl: SPC-3, HISUP=1, CMDQUE=0
/// - thurvsa: SPC-4, HISUP=1, CMDQUE=1
/// - real LTO drives: SPC-3 or SPC-4, HISUP=1, CMDQUE=1
///
/// Each caller passes its own struct rather than us picking one
/// "right" combination; this lets the spec-version-byte (which
/// some backup software gates on) stay stable per product.
#[derive(Debug, Clone, Copy)]
pub struct InquiryFlags {
    /// Byte 2 — SPC version. `0x05` = SPC-3, `0x06` = SPC-4. Real LTO
    /// drives advertise SPC-3 (the version the SSC-4 / SMC-3 specs
    /// reference); modern block targets advertise SPC-4.
    pub spc_version: u8,
    /// Byte 3 bit 4 — HISUP. Set to 1 if the target supports the
    /// LUN structure defined in SAM-5 (both products do).
    pub hisup: bool,
    /// Byte 5 bits 5:4 — TPGS (Target Port Group Support). 00b = no
    /// ALUA, 01b = implicit only, 10b = explicit only, 11b = both.
    /// ALUA-aware initiators check this to decide whether to issue
    /// REPORT TARGET PORT GROUPS / SET TARGET PORT GROUPS; the same
    /// field is mirrored in VPD 0x86 byte 5.
    pub tpgs: crate::vpd::TpgsField,
    /// Byte 7 bit 1 — CMDQUE. Set to 1 if the target supports the
    /// full task management model (queueing, ordering tags). Thurvsa
    /// asserts this; thurvtl historically does not because the tape
    /// LUNs serialize per-drive in the daemon.
    pub cmdque: bool,
}

/// Build an SPC-4 INQUIRY standard data response. Emits exactly 36
/// bytes — the SPC-4 minimum that covers the header + vendor /
/// product / revision triple. Bytes 36..96 of the full spec layout
/// (version descriptors, vendor specific) stay unimplemented; both
/// products advertise `additional length = 31` (= `36 - 5`) so
/// initiators don't expect anything past byte 35.
///
/// `removable` corresponds to the RMB bit (byte 1 high bit) — set
/// for tape (cartridges go in and out) and SMC libraries, clear for
/// thurvsa's fixed volumes.
pub fn build_inquiry_std(
    qualifier: PeripheralQualifier,
    peripheral_type: PeripheralType,
    removable: bool,
    identity: Identity<'_>,
    flags: InquiryFlags,
) -> Vec<u8> {
    let mut buf = vec![0u8; 36];

    // Byte 0: peripheral qualifier (3 bits) + peripheral type (5 bits).
    buf[0] = ((qualifier as u8) << 5) | ((peripheral_type as u8) & 0x1F);

    // Byte 1: RMB bit + reserved.
    if removable {
        buf[1] = 0x80;
    }

    // Byte 2: SPC version (per `flags`).
    buf[2] = flags.spc_version;

    // Byte 3: HISUP (bit 4) | RESPONSE DATA FORMAT (low 4 bits = 2,
    // SPC-4 standard).
    buf[3] = if flags.hisup { 0x10 } else { 0x00 } | 0x02;

    // Byte 4: additional length (n - 4). 36-byte response → 31.
    buf[4] = 31;

    // Byte 5: SCCS(7) | ACC(6) | TPGS(5:4) | 3PC(3) | reserved(2:1) |
    // PROTECT(0). Only TPGS is variable per product today —
    // implicit-only ALUA for both daemons since #43 landed.
    buf[5] = ((flags.tpgs as u8) & 0x03) << 4;
    // Byte 6: ENCSERV / MULTIP / VS — both products keep them zero
    // today.
    // Byte 7: BQUE / ENCSERV / VS / MULTIP / MCHNGR / ADDR16 / CMDQUE
    // / VS. Only CMDQUE (bit 1) is variable per product.
    buf[7] = if flags.cmdque { 0x02 } else { 0x00 };

    // Bytes 8..16: vendor (space-padded ASCII).
    write_padded_ascii(&mut buf[8..16], identity.vendor);
    // Bytes 16..32: product.
    write_padded_ascii(&mut buf[16..32], identity.product);
    // Bytes 32..36: revision.
    write_padded_ascii(&mut buf[32..36], identity.revision);

    buf
}

/// Build the SPC-4 "no logical unit" sentinel response — peripheral
/// qualifier 0b011 + peripheral type 0x1F (byte 0 = 0x7F). Initiators
/// rely on this to distinguish unmapped LUNs from unknown ones during
/// REPORT LUNS-less discovery walks; thurvsa emits it for INQUIRY
/// against any LUN not in its registry.
///
/// `flags` controls the version / HISUP / CMDQUE bytes the same way
/// `build_inquiry_std` does — the unmapped sentinel still wants to
/// look like a response from the right kind of target.
pub fn build_inquiry_no_lun(identity: Identity<'_>, flags: InquiryFlags) -> Vec<u8> {
    build_inquiry_std(
        PeripheralQualifier::NoDevice,
        PeripheralType::NoLun,
        false,
        identity,
        flags,
    )
}

/// Copy `s` into `dst` as ASCII, right-padding the remainder with
/// spaces. Truncates if `s` is longer than `dst`.
pub fn write_padded_ascii(dst: &mut [u8], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    for byte in &mut dst[n..] {
        *byte = b' ';
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity<'static> {
        Identity {
            vendor: "MB",
            product: "TEST",
            revision: "V01",
        }
    }

    fn flags_spc4_cmdque() -> InquiryFlags {
        InquiryFlags {
            spc_version: 0x06,
            hisup: true,
            tpgs: crate::vpd::TpgsField::None,
            cmdque: true,
        }
    }

    fn flags_spc3_no_cmdque() -> InquiryFlags {
        InquiryFlags {
            spc_version: 0x05,
            hisup: true,
            tpgs: crate::vpd::TpgsField::None,
            cmdque: false,
        }
    }

    #[test]
    fn inquiry_std_layout_spc4() {
        let buf = build_inquiry_std(
            PeripheralQualifier::Connected,
            PeripheralType::DirectAccess,
            false,
            id(),
            flags_spc4_cmdque(),
        );
        assert_eq!(buf.len(), 36);
        assert_eq!(buf[0], 0x00); // direct access, qualifier 0
        assert_eq!(buf[1], 0x00); // not removable
        assert_eq!(buf[2], 0x06); // SPC-4
        assert_eq!(buf[3], 0x12); // HISUP=1 | format=2
        assert_eq!(buf[4], 31);
        assert_eq!(buf[7], 0x02); // CMDQUE=1
        assert_eq!(&buf[8..16], b"MB      ");
        assert_eq!(&buf[16..32], b"TEST            ");
        assert_eq!(&buf[32..36], b"V01 ");
    }

    #[test]
    fn inquiry_std_layout_spc3_no_cmdque() {
        let buf = build_inquiry_std(
            PeripheralQualifier::Connected,
            PeripheralType::SequentialAccess,
            true,
            id(),
            flags_spc3_no_cmdque(),
        );
        assert_eq!(buf.len(), 36);
        assert_eq!(buf[0], 0x01); // SSC
        assert_eq!(buf[1], 0x80); // RMB=1
        assert_eq!(buf[2], 0x05); // SPC-3
        assert_eq!(buf[3], 0x12); // HISUP=1 | format=2
        assert_eq!(buf[7], 0x00); // CMDQUE=0
    }

    #[test]
    fn inquiry_std_hisup_cleared() {
        let buf = build_inquiry_std(
            PeripheralQualifier::Connected,
            PeripheralType::DirectAccess,
            false,
            id(),
            InquiryFlags {
                spc_version: 0x06,
                hisup: false,
                tpgs: crate::vpd::TpgsField::None,
                cmdque: false,
            },
        );
        assert_eq!(buf[3], 0x02); // HISUP=0, format=2
    }

    #[test]
    fn inquiry_std_byte_5_carries_tpgs_field() {
        let buf = build_inquiry_std(
            PeripheralQualifier::Connected,
            PeripheralType::DirectAccess,
            false,
            id(),
            InquiryFlags {
                spc_version: 0x06,
                hisup: true,
                tpgs: crate::vpd::TpgsField::Implicit,
                cmdque: true,
            },
        );
        // Implicit ALUA (01b) in bits 5:4 of byte 5 = 0x10.
        assert_eq!(buf[5] & 0x30, 0x10);
        // Other fields in byte 5 stay zero.
        assert_eq!(buf[5] & !0x30, 0);
    }

    #[test]
    fn no_lun_sentinel_byte_pattern() {
        let buf = build_inquiry_no_lun(id(), flags_spc4_cmdque());
        assert_eq!(buf[0], 0x7F); // qualifier 0b011 + type 0x1F
        assert_eq!(buf.len(), 36);
    }

    #[test]
    fn padded_ascii_truncates_then_pads() {
        let mut buf = [0u8; 8];
        write_padded_ascii(&mut buf, "ABC");
        assert_eq!(&buf, b"ABC     ");

        let mut buf = [0u8; 4];
        write_padded_ascii(&mut buf, "TOOLONG");
        assert_eq!(&buf, b"TOOL");
    }
}
