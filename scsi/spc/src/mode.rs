// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! MODE PARAMETER HEADER encoders + parsers for MODE SENSE /
//! MODE SELECT 6 (4-byte header) and 10 (8-byte header) —
//! SPC-4 §7.5.4.
//!
//! Both products emit identical headers; only the per-page bodies
//! and the block descriptor that follow are product-specific.
//! SSC-4 (thurvtl tape) carries an 8-byte SSC-specific block
//! descriptor; SBC-3 (thurvsa block) carries the 8-byte short or
//! 16-byte long LBA descriptor. Each emit helper writes only the
//! header — callers append the block descriptor + page bodies and
//! patch the MODE DATA LENGTH field via
//! [`patch_mode_data_length_6`] / [`patch_mode_data_length_10`]
//! once the response is complete.
//!
//! The parse helpers walk the symmetric direction (MODE SELECT
//! parameter list): byte-offset decode of the header without
//! interpreting the per-page bodies that follow.

/// Length of the MODE PARAMETER HEADER for MODE SENSE 6 / MODE SELECT 6.
pub const MODE_PARAM_HEADER_6_LEN: usize = 4;

/// Length of the MODE PARAMETER HEADER for MODE SENSE 10 / MODE SELECT 10.
pub const MODE_PARAM_HEADER_10_LEN: usize = 8;

/// MODE PARAMETER HEADER for MODE SENSE 6 / MODE SELECT 6 (4 bytes).
///
/// Layout (SPC-4 Table 432):
/// ```text
/// byte 0  MODE DATA LENGTH (excludes byte 0 itself)
/// byte 1  MEDIUM TYPE
/// byte 2  DEVICE-SPECIFIC PARAMETER
/// byte 3  BLOCK DESCRIPTOR LENGTH
/// ```
///
/// `medium_type` is 0x00 for direct-access; tape uses 0x00 too
/// (SSC-4 doesn't define a non-zero medium type for these surfaces).
/// `device_specific` carries WP / DPOFUA / future bits — callers
/// pass the byte they want.
/// `block_descriptor_length` is the length of the block descriptor
/// list that follows the header (8 bytes per descriptor).
pub fn write_mode_param_header_6(
    buf: &mut Vec<u8>,
    medium_type: u8,
    device_specific: u8,
    block_descriptor_length: u8,
) {
    let header_offset = buf.len();
    buf.push(0); // MODE DATA LENGTH (filled by patch_*)
    buf.push(medium_type);
    buf.push(device_specific);
    buf.push(block_descriptor_length);

    // Sanity: caller's Vec must have grown by exactly 4.
    debug_assert_eq!(buf.len() - header_offset, 4);
}

/// Patch the MODE DATA LENGTH byte in a 6-byte-header response.
/// `header_offset` is the index where the header began (typically
/// 0 if the header is at the start of the response).
pub fn patch_mode_data_length_6(buf: &mut [u8], header_offset: usize) {
    let total_len = buf.len() - header_offset;
    // MODE DATA LENGTH excludes byte 0 itself per SPC-4 §7.5.4.
    let value = (total_len - 1).min(255) as u8;
    buf[header_offset] = value;
}

/// MODE PARAMETER HEADER for MODE SENSE 10 / MODE SELECT 10 (8 bytes).
///
/// Layout (SPC-4 Table 433):
/// ```text
/// bytes 0..2  MODE DATA LENGTH (big-endian; excludes bytes 0..2)
/// byte 2      MEDIUM TYPE
/// byte 3      DEVICE-SPECIFIC PARAMETER
/// byte 4      LONGLBA (bit 0) | reserved
/// byte 5      reserved
/// bytes 6..8  BLOCK DESCRIPTOR LENGTH (big-endian)
/// ```
///
/// `longlba` selects the 16-byte block-descriptor format (bit 0).
/// Both products clear it today (8-byte short descriptors only).
pub fn write_mode_param_header_10(
    buf: &mut Vec<u8>,
    medium_type: u8,
    device_specific: u8,
    longlba: bool,
    block_descriptor_length: u16,
) {
    let header_offset = buf.len();
    buf.push(0); // MODE DATA LENGTH high (filled later)
    buf.push(0); // MODE DATA LENGTH low (filled later)
    buf.push(medium_type);
    buf.push(device_specific);
    buf.push(if longlba { 0x01 } else { 0x00 });
    buf.push(0); // reserved
    buf.extend_from_slice(&block_descriptor_length.to_be_bytes());

    debug_assert_eq!(buf.len() - header_offset, 8);
}

/// Patch the MODE DATA LENGTH (big-endian u16 at bytes 0..2) in a
/// 10-byte-header response.
pub fn patch_mode_data_length_10(buf: &mut [u8], header_offset: usize) {
    let total_len = buf.len() - header_offset;
    // MODE DATA LENGTH excludes bytes 0..2 themselves per SPC-4 §7.5.4.
    let value = ((total_len - 2) as u16).to_be_bytes();
    buf[header_offset..header_offset + 2].copy_from_slice(&value);
}

/// Decoded MODE PARAMETER HEADER for MODE SENSE 6 / MODE SELECT 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeParamHeader6 {
    pub mode_data_length: u8,
    pub medium_type: u8,
    pub device_specific: u8,
    pub block_descriptor_length: u8,
}

/// Decode a 4-byte MODE PARAMETER HEADER. Returns `None` if `data`
/// is shorter than the header.
pub fn parse_mode_param_header_6(data: &[u8]) -> Option<ModeParamHeader6> {
    if data.len() < MODE_PARAM_HEADER_6_LEN {
        return None;
    }
    Some(ModeParamHeader6 {
        mode_data_length: data[0],
        medium_type: data[1],
        device_specific: data[2],
        block_descriptor_length: data[3],
    })
}

/// Decoded MODE PARAMETER HEADER for MODE SENSE 10 / MODE SELECT 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeParamHeader10 {
    pub mode_data_length: u16,
    pub medium_type: u8,
    pub device_specific: u8,
    pub longlba: bool,
    pub block_descriptor_length: u16,
}

/// Decode an 8-byte MODE PARAMETER HEADER. Returns `None` if `data`
/// is shorter than the header.
pub fn parse_mode_param_header_10(data: &[u8]) -> Option<ModeParamHeader10> {
    if data.len() < MODE_PARAM_HEADER_10_LEN {
        return None;
    }
    Some(ModeParamHeader10 {
        mode_data_length: u16::from_be_bytes([data[0], data[1]]),
        medium_type: data[2],
        device_specific: data[3],
        longlba: (data[4] & 0x01) != 0,
        block_descriptor_length: u16::from_be_bytes([data[6], data[7]]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_6_header_layout_and_length_patch() {
        let mut buf = Vec::new();
        write_mode_param_header_6(&mut buf, 0x00, 0x10, 8);
        // 8-byte block descriptor + 20-byte caching page body.
        buf.extend_from_slice(&[0u8; 8]); // block descriptor
        buf.extend_from_slice(&[0u8; 20]); // page body
        patch_mode_data_length_6(&mut buf, 0);

        assert_eq!(buf[0], (4 + 8 + 20 - 1) as u8);
        assert_eq!(buf[1], 0x00); // medium type
        assert_eq!(buf[2], 0x10); // device-specific
        assert_eq!(buf[3], 8); // block descriptor length
    }

    #[test]
    fn mode_10_header_layout_and_length_patch() {
        let mut buf = Vec::new();
        write_mode_param_header_10(&mut buf, 0x00, 0x10, false, 0);
        buf.extend_from_slice(&[0u8; 12]); // page body
        patch_mode_data_length_10(&mut buf, 0);

        let total = u16::from_be_bytes([buf[0], buf[1]]);
        assert_eq!(total, (8 + 12 - 2) as u16);
        assert_eq!(buf[2], 0x00);
        assert_eq!(buf[3], 0x10);
        assert_eq!(buf[4], 0x00); // longlba clear
        assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 0);
    }

    #[test]
    fn mode_10_longlba_sets_bit_0() {
        let mut buf = Vec::new();
        write_mode_param_header_10(&mut buf, 0, 0, true, 0);
        assert_eq!(buf[4], 0x01);
    }

    #[test]
    fn parse_header_6_round_trips_emit() {
        let mut buf = Vec::new();
        write_mode_param_header_6(&mut buf, 0x00, 0x80, 8);
        buf.extend_from_slice(&[0u8; 8]);
        patch_mode_data_length_6(&mut buf, 0);
        let h = parse_mode_param_header_6(&buf).unwrap();
        assert_eq!(h.mode_data_length, (4 + 8 - 1) as u8);
        assert_eq!(h.medium_type, 0x00);
        assert_eq!(h.device_specific, 0x80);
        assert_eq!(h.block_descriptor_length, 8);
    }

    #[test]
    fn parse_header_6_short_returns_none() {
        assert!(parse_mode_param_header_6(&[0u8; 3]).is_none());
    }

    #[test]
    fn parse_header_10_round_trips_emit_with_longlba() {
        let mut buf = Vec::new();
        write_mode_param_header_10(&mut buf, 0x00, 0x10, true, 16);
        buf.extend_from_slice(&[0u8; 16]);
        patch_mode_data_length_10(&mut buf, 0);
        let h = parse_mode_param_header_10(&buf).unwrap();
        assert_eq!(h.mode_data_length, (8 + 16 - 2) as u16);
        assert_eq!(h.medium_type, 0x00);
        assert_eq!(h.device_specific, 0x10);
        assert!(h.longlba);
        assert_eq!(h.block_descriptor_length, 16);
    }

    #[test]
    fn parse_header_10_short_returns_none() {
        assert!(parse_mode_param_header_10(&[0u8; 7]).is_none());
    }
}
