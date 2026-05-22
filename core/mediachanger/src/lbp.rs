// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Logical Block Protection (LTO-7+).
//!
//! End-to-end host-to-medium integrity per SPC-4 / SSC-5: when enabled
//! via Mode Page 0x0A subpage 0xF0 (Control Data Protection), every
//! WRITE / READ block carries a 4-byte CRC32C (Castagnoli polynomial
//! 0x1EDC6F41) trailer. The drive validates host-supplied CRCs on
//! WRITE and emits computed CRCs on READ, giving the host independent
//! verification of the path between HBA and drive.
//!
//! Thur VTL's "drive" is a daemon process whose chunk pool is already
//! BLAKE3-content-addressed and whose encrypted blocks carry an
//! AES-GCM auth tag, so storage corruption is caught at finer
//! granularity. LBP exists to surface the same wire-level integrity
//! guarantee real LTO-7+ drives offer to backup software that knows
//! how to consume it. We compute the CRC fresh on every READ — there
//! is nothing to pre-store, since BLAKE3 / GCM already prove the
//! plaintext block is the one originally written.

/// Width in bytes of the Logical Block Protection trailer (CRC32C).
pub const LBP_TRAILER_LEN: usize = 4;

/// LBP information format value emitted in VPD 0xB0 / Mode Page
/// 0x0A/0xF0: 0x01 = CRC32C only.
pub const LBP_INFO_CRC32C: u8 = 0x01;

/// Compute the 4-byte CRC32C trailer for a block. Big-endian network
/// byte order matches every Linux LBP-aware backup tool (st driver,
/// `mt-st`, NetBackup, Veeam) and the SSC-5 spec.
pub fn compute_lbp_trailer(data: &[u8]) -> [u8; LBP_TRAILER_LEN] {
    let crc = crc32c::crc32c(data);
    crc.to_be_bytes()
}

/// Validate a host-supplied LBP trailer against the data it covers.
/// Returns `true` if the trailer matches the freshly-computed CRC.
pub fn validate_lbp_trailer(data: &[u8], trailer: &[u8; LBP_TRAILER_LEN]) -> bool {
    let computed = compute_lbp_trailer(data);
    constant_time_eq(&computed, trailer)
}

/// Strip the LBP trailer from `block` (which must include the
/// trailer) and validate. On success, returns the data slice without
/// the trailer; on mismatch, returns `Err(LbpError::CrcMismatch)`.
/// On a too-short block, returns `Err(LbpError::ShortBlock)`.
pub fn strip_and_validate_lbp(block: &[u8]) -> Result<&[u8], LbpError> {
    if block.len() < LBP_TRAILER_LEN {
        return Err(LbpError::ShortBlock);
    }
    let split = block.len() - LBP_TRAILER_LEN;
    let (data, trailer_slice) = block.split_at(split);
    let mut trailer = [0u8; LBP_TRAILER_LEN];
    trailer.copy_from_slice(trailer_slice);
    if validate_lbp_trailer(data, &trailer) {
        Ok(data)
    } else {
        Err(LbpError::CrcMismatch)
    }
}

/// Errors raised when validating a host-supplied LBP trailer on
/// WRITE. Both map at the iSCSI layer to CHECK CONDITION + ABORTED
/// COMMAND + ASC/ASCQ 0x10/0x05 ("LOGICAL BLOCK PROTECTION METHOD
/// ERROR"). Distinguishing them helps trace logs but the host sense
/// surface is identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbpError {
    /// Block payload was shorter than the 4-byte trailer the host
    /// promised by setting WRPROTECT > 0.
    ShortBlock,
    /// CRC32C in the trailing 4 bytes did not match the data.
    CrcMismatch,
}

impl std::fmt::Display for LbpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LbpError::ShortBlock => {
                f.write_str("logical block protection: block shorter than declared trailer length")
            }
            LbpError::CrcMismatch => {
                f.write_str("logical block protection: CRC32C trailer mismatch")
            }
        }
    }
}

impl std::error::Error for LbpError {}

/// Constant-time byte comparison so a CRC mismatch doesn't leak the
/// position of the first differing byte through timing. CRC isn't a
/// MAC, but the cheap habit costs nothing.
fn constant_time_eq(a: &[u8; LBP_TRAILER_LEN], b: &[u8; LBP_TRAILER_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..LBP_TRAILER_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_validates() {
        let data = b"hello, lto-7 logical block protection";
        let trailer = compute_lbp_trailer(data);
        assert!(validate_lbp_trailer(data, &trailer));
    }

    #[test]
    fn known_test_vector() {
        // crc32c crate test vector for "123456789" — the standard CRC
        // smoke value.
        let trailer = compute_lbp_trailer(b"123456789");
        let crc = u32::from_be_bytes(trailer);
        assert_eq!(crc, 0xE3069283);
    }

    #[test]
    fn flipped_bit_fails_validation() {
        let data = vec![0xAAu8; 1024];
        let mut trailer = compute_lbp_trailer(&data);
        trailer[0] ^= 0x01;
        assert!(!validate_lbp_trailer(&data, &trailer));
    }

    #[test]
    fn strip_and_validate_round_trip() {
        let data = vec![0x42u8; 4096];
        let mut block = data.clone();
        block.extend_from_slice(&compute_lbp_trailer(&data));
        let stripped = strip_and_validate_lbp(&block).expect("validate");
        assert_eq!(stripped, &data[..]);
    }

    #[test]
    fn strip_short_block_errors() {
        let block = [0u8; 3]; // shorter than trailer
        let err = strip_and_validate_lbp(&block).unwrap_err();
        assert_eq!(err, LbpError::ShortBlock);
    }

    #[test]
    fn strip_corrupt_block_errors() {
        let mut block = vec![0u8; 100];
        block.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // wrong CRC
        let err = strip_and_validate_lbp(&block).unwrap_err();
        assert_eq!(err, LbpError::CrcMismatch);
    }
}
