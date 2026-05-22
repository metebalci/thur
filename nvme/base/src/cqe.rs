// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! 16-byte Completion Queue Entry (NVMe Base §4.6).
//!
//! Field layout (little-endian on the wire):
//!
//! ```text
//! Offset   Field
//!   0..3   DW0     command-specific (e.g. Get Features value)
//!   4..7   DW1     command-specific
//!   8..9   SQHD    SQ Head pointer
//!  10..11  SQID    SQ Identifier
//!  12..13  CID     Command Identifier (echoed)
//!  14..15  STATUS  CRD(2) | M(1) | DNR(1) | SCT(3) | SC(8) | P(1)
//! ```
//!
//! NVMe/TCP wraps the CQE in a CapsuleResp PDU; the wire image is
//! identical to PCIe NVMe.

use crate::error::NvmeError;
use crate::status::StatusField;

#[derive(Debug, Clone, Copy)]
pub struct Cqe {
    /// Command-specific result. Identify returns 0; Get Features
    /// returns the feature value; NVM Compare with non-match returns
    /// the offset of the first mismatched byte; etc.
    pub dw0: u32,
    pub dw1: u32,
    pub sqhd: u16,
    pub sqid: u16,
    pub cid: u16,
    pub status: StatusField,
}

impl Cqe {
    /// Build a CQE for a successful completion of `cid` from `sqid`.
    /// `dw0` is command-specific (0 is fine for commands without a
    /// result payload). The Phase Tag bit is set by the transport
    /// just before write — this constructor leaves it at zero.
    pub fn success(cid: u16, sqid: u16, sqhd: u16, dw0: u32) -> Self {
        Self {
            dw0,
            dw1: 0,
            sqhd,
            sqid,
            cid,
            status: StatusField::SUCCESS,
        }
    }

    /// Build a CQE carrying a non-success [`StatusField`].
    pub fn failure(cid: u16, sqid: u16, sqhd: u16, status: StatusField) -> Self {
        Self {
            dw0: 0,
            dw1: 0,
            sqhd,
            sqid,
            cid,
            status,
        }
    }

    /// Encode into a 16-byte slice. Returns `NvmeError::CqeLength`
    /// if the slice is the wrong size.
    pub fn write_into(&self, buf: &mut [u8]) -> Result<(), NvmeError> {
        if buf.len() != crate::CQE_SIZE {
            return Err(NvmeError::CqeLength(buf.len()));
        }
        buf[0..4].copy_from_slice(&self.dw0.to_le_bytes());
        buf[4..8].copy_from_slice(&self.dw1.to_le_bytes());
        buf[8..10].copy_from_slice(&self.sqhd.to_le_bytes());
        buf[10..12].copy_from_slice(&self.sqid.to_le_bytes());
        buf[12..14].copy_from_slice(&self.cid.to_le_bytes());
        buf[14..16].copy_from_slice(&self.status.to_u16().to_le_bytes());
        Ok(())
    }

    /// Convenience helper — allocate a fresh 16-byte buffer.
    pub fn to_bytes(&self) -> [u8; crate::CQE_SIZE] {
        let mut out = [0u8; crate::CQE_SIZE];
        // write_into only fails on wrong length; our buffer is fixed.
        let _ = self.write_into(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_round_trip() {
        let cqe = Cqe::success(0x1234, 1, 7, 0);
        let bytes = cqe.to_bytes();
        // CID echoed
        assert_eq!(&bytes[12..14], &0x1234u16.to_le_bytes());
        // SQHD
        assert_eq!(&bytes[8..10], &7u16.to_le_bytes());
        // Status = 0 (Success, P=0)
        assert_eq!(&bytes[14..16], &0u16.to_le_bytes());
    }
}
