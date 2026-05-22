// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! REPORT LUNS (opcode 0xA0) response framing — SPC-4 §6.30.
//!
//! Layout: 8-byte header (4-byte LUN-list length + 4-byte
//! reserved) followed by N × 8-byte LUN field. Both products call
//! into this; the LUN list and SELECT REPORT byte are
//! product-specific (thurvsa's full-list + admin-only selectors,
//! thurvtl's library-LUN + drive-LUN selectors).

use crate::lun::encode_lun_field;

/// Build a REPORT LUNS response payload from a slice of LUNs.
/// Returns the un-truncated bytes; callers truncate to the
/// initiator's allocation length before wrapping into
/// `ScsiResponse::good`.
///
/// The LUNs are emitted in the order supplied — both products
/// today already maintain canonical ordering (sorted ascending)
/// before passing in. SELECT REPORT decoding (which subset to
/// include) stays at the call site since it touches CDB bytes
/// the framing layer doesn't see.
pub fn build_report_luns(luns: &[u64]) -> Vec<u8> {
    let lun_list_length = (luns.len() * 8) as u32;
    let mut buf = Vec::with_capacity(8 + luns.len() * 8);
    buf.extend_from_slice(&lun_list_length.to_be_bytes());
    buf.extend_from_slice(&[0u8; 4]); // reserved
    for &lun in luns {
        buf.extend_from_slice(&encode_lun_field(lun));
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_lun_list() {
        let buf = build_report_luns(&[]);
        assert_eq!(buf.len(), 8);
        assert_eq!(&buf[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn single_lun_layout() {
        let buf = build_report_luns(&[0]);
        assert_eq!(buf.len(), 16);
        assert_eq!(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]), 8);
        assert_eq!(&buf[8..16], &[0u8; 8]);
    }

    #[test]
    fn multiple_luns_in_order() {
        let buf = build_report_luns(&[0, 1, 2]);
        assert_eq!(buf.len(), 8 + 3 * 8);
        assert_eq!(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]), 24);
        assert_eq!(buf[8 + 1], 0); // LUN 0 in byte 1 of first slot
        assert_eq!(buf[16 + 1], 1);
        assert_eq!(buf[24 + 1], 2);
    }

    #[test]
    fn flat_space_lun_uses_top_bits() {
        let buf = build_report_luns(&[0x100]);
        // Single descriptor at offset 8.
        assert_eq!(buf[8], 0x40 | 0x01); // flat-space addressing
        assert_eq!(buf[9], 0x00);
    }
}
