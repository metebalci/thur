// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Vital Product Data page header + descriptor framing helpers
//! (SPC-4 §7.7).
//!
//! VPD pages share a 4-byte header: byte 0 carries the peripheral
//! qualifier + type (same encoding as INQUIRY std data), byte 1 is
//! the page code, bytes 2..4 are the big-endian length of the
//! payload that follows. Both products emit pages 0x00 (Supported
//! VPD Pages), 0x80 (Unit Serial Number), 0x83 (Device
//! Identification); thurvsa also emits 0xB0 (Block Limits) and 0xB2
//! (Logical Block Provisioning) — those are SBC-3-specific so they
//! stay product-side.
//!
//! Every helper here writes the header + body into a freshly-
//! allocated `Vec<u8>`. Callers can wrap the result in
//! `ScsiResponse::good(...)` directly or further trim to the
//! initiator's allocation length.

use crate::inquiry::{PeripheralQualifier, PeripheralType, write_padded_ascii};

/// Write the standard 4-byte VPD page header + return a `Vec<u8>`
/// pre-sized to fit the body. Caller fills bytes 4.. with the page
/// payload, then calls [`finalize_vpd`] (or hand-patches bytes 2..4)
/// to set the length.
pub fn vpd_header(
    qualifier: PeripheralQualifier,
    peripheral_type: PeripheralType,
    page_code: u8,
    body_capacity: usize,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + body_capacity);
    buf.push(((qualifier as u8) << 5) | ((peripheral_type as u8) & 0x1F));
    buf.push(page_code);
    buf.push(0); // length high byte (filled later)
    buf.push(0); // length low byte (filled later)
    buf
}

/// Patch the header's PAGE LENGTH (bytes 2..4) with the actual
/// body length (`buf.len() - 4`). Call after appending the body.
pub fn finalize_vpd(buf: &mut [u8]) {
    let body_len = (buf.len() - 4) as u16;
    buf[2..4].copy_from_slice(&body_len.to_be_bytes());
}

/// Build VPD page 0x00 — Supported VPD Pages. `pages` lists every
/// other VPD page code the device implements; the helper sorts /
/// dedupes so the wire output is canonical regardless of caller
/// input order.
pub fn build_supported_vpd_pages(
    qualifier: PeripheralQualifier,
    peripheral_type: PeripheralType,
    pages: &[u8],
) -> Vec<u8> {
    let mut sorted: Vec<u8> = pages.to_vec();
    sorted.push(0x00); // page 0x00 always lists itself
    sorted.sort_unstable();
    sorted.dedup();

    let mut buf = vpd_header(qualifier, peripheral_type, 0x00, sorted.len());
    buf.extend_from_slice(&sorted);
    finalize_vpd(&mut buf);
    buf
}

/// Build VPD page 0x80 — Unit Serial Number. `serial` is ASCII,
/// space-padded to `width` bytes (real SAS / SCSI gear emits 20 or
/// 24 bytes depending on family).
pub fn build_unit_serial_number(
    qualifier: PeripheralQualifier,
    peripheral_type: PeripheralType,
    serial: &str,
    width: usize,
) -> Vec<u8> {
    let mut buf = vpd_header(qualifier, peripheral_type, 0x80, width);
    let pad_start = buf.len();
    buf.resize(pad_start + width, b' ');
    write_padded_ascii(&mut buf[pad_start..pad_start + width], serial);
    finalize_vpd(&mut buf);
    buf
}

/// Code-set field of a designation descriptor (SPC-4 §7.7.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CodeSet {
    Binary = 0x01,
    Ascii = 0x02,
    Utf8 = 0x03,
}

/// Designator type (SPC-4 §7.7.6.1 Table 459).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DesignatorType {
    /// 0x00 — Vendor-specific (T10 vendor ID prefix). Common on
    /// real SAS gear; both thurvtl and thurvsa use this for the
    /// vendor-prefixed unique ID.
    VendorSpecific = 0x00,
    /// 0x01 — T10 vendor ID. Eight-byte ASCII vendor field +
    /// vendor-defined remainder.
    T10VendorId = 0x01,
    /// 0x03 — NAA (IEEE Network Address Authority).
    Naa = 0x03,
    /// 0x06 — Logical Unit Group. 4-byte group ID identifying a
    /// set of LUs that comprise one logical device (thurvtl emits
    /// this on drive + changer LUNs so backup software auto-
    /// correlates LUNs to the same library).
    LogicalUnitGroup = 0x06,
    /// 0x08 — SCSI name string. ASCII / UTF-8 string, often the
    /// IQN on iSCSI targets.
    ScsiName = 0x08,
}

/// Association field of a designation descriptor (SPC-4
/// §7.7.6.1 Table 458).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Association {
    LogicalUnit = 0x00,
    TargetPort = 0x01,
    TargetDevice = 0x02,
}

/// Append one designation descriptor to a VPD page 0x83 buffer.
/// Layout per SPC-4 Table 457:
///
/// ```text
/// byte 0  PROTOCOL_ID (4 bits) | CODE_SET (4 bits)
/// byte 1  PIV (1 bit) | ASSOCIATION (2 bits) | DESIGNATOR_TYPE (4 bits)
/// byte 2  reserved
/// byte 3  designator length
/// byte 4..  designator value
/// ```
///
/// `protocol_id` defaults to 0 unless PIV is set; this helper
/// keeps PIV clear (matches what every thurvtl / thurvsa VPD 0x83
/// site does today).
pub fn push_designator(
    buf: &mut Vec<u8>,
    code_set: CodeSet,
    association: Association,
    designator_type: DesignatorType,
    value: &[u8],
) {
    buf.push(code_set as u8); // PROTOCOL_ID = 0, CODE_SET in low nibble.
    buf.push(((association as u8) << 4) | (designator_type as u8));
    buf.push(0); // reserved
    buf.push(value.len() as u8);
    buf.extend_from_slice(value);
}

/// Build VPD page 0x83 — Device Identification. Wraps
/// [`vpd_header`] + caller-supplied descriptors (built via
/// [`push_designator`]) into the full page response.
pub fn build_device_identification(
    qualifier: PeripheralQualifier,
    peripheral_type: PeripheralType,
    descriptors: &[u8],
) -> Vec<u8> {
    let mut buf = vpd_header(qualifier, peripheral_type, 0x83, descriptors.len());
    buf.extend_from_slice(descriptors);
    finalize_vpd(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_pages_sorts_and_includes_self() {
        let buf = build_supported_vpd_pages(
            PeripheralQualifier::Connected,
            PeripheralType::DirectAccess,
            &[0x83, 0x80, 0xB0],
        );
        // Header: byte 0 type, byte 1 page code, bytes 2..4 length.
        assert_eq!(buf[0], 0x00);
        assert_eq!(buf[1], 0x00);
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        assert_eq!(len, buf.len() - 4);
        // Sorted + deduped, includes 0x00 itself.
        assert_eq!(&buf[4..], &[0x00, 0x80, 0x83, 0xB0]);
    }

    #[test]
    fn unit_serial_pads_to_width() {
        let buf = build_unit_serial_number(
            PeripheralQualifier::Connected,
            PeripheralType::DirectAccess,
            "ABC123",
            16,
        );
        assert_eq!(buf[1], 0x80);
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        assert_eq!(len, 16);
        assert_eq!(&buf[4..20], b"ABC123          ");
    }

    #[test]
    fn device_identification_descriptor_layout() {
        let mut descriptors = Vec::new();
        push_designator(
            &mut descriptors,
            CodeSet::Ascii,
            Association::LogicalUnit,
            DesignatorType::T10VendorId,
            b"MB      MBD_DEADBEEF",
        );
        let buf = build_device_identification(
            PeripheralQualifier::Connected,
            PeripheralType::DirectAccess,
            &descriptors,
        );
        assert_eq!(buf[1], 0x83);
        // Descriptor: byte 0 codeset=2, byte 1 type=1 assoc=0,
        // byte 3 length=20 (b"MB      MBD_DEADBEEF".len()).
        assert_eq!(buf[4], 0x02);
        assert_eq!(buf[5], 0x01);
        assert_eq!(buf[7], 20);
        assert_eq!(&buf[8..8 + 20], b"MB      MBD_DEADBEEF");
    }
}
