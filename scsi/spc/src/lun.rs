// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SAM-5 Logical Unit Number encoding.
//!
//! The 8-byte LUN field on an iSCSI PDU is a SAM-5 "hierarchical
//! LUN" structure (SAM-5 §4.7.4) — 4 × 2-byte levels, each carrying
//! an addressing method in the top 2 bits and a 14-bit value below.
//! Two methods cover the entire surface today's products use:
//!
//! - **Single-level peripheral-device addressing** (top 2 bits =
//!   0b00, target address byte = 0x00). The LUN value lives in the
//!   low byte. Range 0..=255.
//! - **Flat-space addressing** (top 2 bits = 0b01). The 14-bit
//!   value spans both bytes. Range 0..=16383.
//!
//! Both products use single-level addressing exclusively today
//! (thurvtl's library + drives map to LUNs 0..N where N is small;
//! thurvsa's volumes map to per-volume LUNs assigned at register
//! time, also small). The flat-space encoding is here for
//! completeness — REPORT LUNS responses include it for any LUN
//! greater than 255, and a host that picks a flat-space LUN out
//! of the list must round-trip.
//!
//! Higher levels (hierarchical, dependent-LU) are out of scope —
//! the products neither emit nor accept them. Decoding such a LUN
//! truncates to zero, which is wrong but unreachable through the
//! configurations either daemon supports.

/// Encode a LUN into the 8-byte SAM-5 field format used by iSCSI
/// PDUs and REPORT LUNS responses.
///
/// `lun < 256` → single-level peripheral-device addressing.
/// `lun < 16384` → flat-space addressing.
/// `lun >= 16384` → saturated to the largest flat-space value
/// (0x3FFF). The skeleton ships nothing that exceeds the
/// flat-space range; callers should keep their LUN allocation
/// inside it.
pub fn encode_lun_field(lun: u64) -> [u8; 8] {
    let mut buf = [0u8; 8];
    if lun < 256 {
        buf[1] = lun as u8;
    } else if lun < (1 << 14) {
        buf[0] = 0x40 | ((lun >> 8) as u8 & 0x3F);
        buf[1] = lun as u8;
    } else {
        // Saturate at 0x3FFF — caller error, but emit something
        // that at least decodes back to a valid flat-space LUN.
        buf[0] = 0x40 | 0x3F;
        buf[1] = 0xFF;
    }
    buf
}

/// Decode an 8-byte SAM-5 LUN field back to a u64. Recognized
/// addressing methods:
/// - **0b00** single-level peripheral-device → low byte of level 0.
/// - **0b01** flat-space → 14-bit value.
///
/// Any other top-bits pattern (hierarchical / extended /
/// dependent-LU) returns `None`.
pub fn decode_lun_field(field: [u8; 8]) -> Option<u64> {
    let method = field[0] >> 6;
    match method {
        0b00 => {
            // Single-level peripheral device. Bus byte must be zero
            // for a valid encoding; we accept any value defensively
            // (real initiators send 0x00).
            Some(u64::from(field[1]))
        }
        0b01 => {
            let high = u64::from(field[0] & 0x3F);
            let low = u64::from(field[1]);
            Some((high << 8) | low)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_level_for_small_lun() {
        let b = encode_lun_field(0x42);
        assert_eq!(b[0], 0x00);
        assert_eq!(b[1], 0x42);
    }

    #[test]
    fn flat_space_for_large_lun() {
        let b = encode_lun_field(0x100);
        assert_eq!(b[0], 0x40 | 0x01);
        assert_eq!(b[1], 0x00);
    }

    #[test]
    fn flat_space_max() {
        let b = encode_lun_field(0x3FFF);
        assert_eq!(b[0], 0x40 | 0x3F);
        assert_eq!(b[1], 0xFF);
    }

    #[test]
    fn out_of_range_saturates() {
        let b = encode_lun_field(1 << 20);
        assert_eq!(b[0], 0x40 | 0x3F);
        assert_eq!(b[1], 0xFF);
    }

    #[test]
    fn round_trip_single_level() {
        for lun in [0u64, 1, 127, 255] {
            let b = encode_lun_field(lun);
            assert_eq!(decode_lun_field(b), Some(lun));
        }
    }

    #[test]
    fn round_trip_flat_space() {
        for lun in [256u64, 1024, 0x3FFE, 0x3FFF] {
            let b = encode_lun_field(lun);
            assert_eq!(decode_lun_field(b), Some(lun));
        }
    }

    #[test]
    fn unsupported_addressing_returns_none() {
        // 0b10 = logical-unit addressing, 0b11 = extended.
        assert_eq!(decode_lun_field([0x80, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(decode_lun_field([0xC0, 0, 0, 0, 0, 0, 0, 0]), None);
    }
}
