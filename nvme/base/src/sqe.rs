// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! 64-byte Submission Queue Entry (NVMe Base §4.5).
//!
//! Field layout (little-endian on the wire):
//!
//! ```text
//! Offset   Field
//! ------   -----
//!   0..1   CDW0[7:0]   OPC      command opcode
//!          CDW0[9:8]   FUSE     fused-operation marker
//!          CDW0[13:10] reserved
//!          CDW0[15:14] PSDT     PRP vs SGL data-pointer scheme
//!          CDW0[31:16] CID      command identifier
//!   4..7   NSID
//!   8..15  reserved
//!  16..23  MPTR        metadata pointer
//!  24..39  DPTR        data pointer (PRP/SGL — 16 bytes)
//!  40..43  CDW10
//!  44..47  CDW11
//!  48..51  CDW12
//!  52..55  CDW13
//!  56..59  CDW14
//!  60..63  CDW15
//! ```
//!
//! Capsule transports (NVMe-oF) place the SQE in the first 64 bytes
//! of a CapsuleCmd PDU; admin / I/O dispatch only ever reads from
//! the parsed [`Sqe`] view, never from the raw bytes.

use crate::error::NvmeError;
use crate::opcode::{Fuse, Psdt};

/// Decoded SQE. Holds owned 16-byte DPTR + the eleven scalar fields
/// extracted from the 64-byte wire image. The CDW10..CDW15 raw u32s
/// are kept as-is so per-command decoders (NVM Read / Write, Identify
/// CNS, etc.) can pull out their command-specific bit lanes without
/// re-parsing the wire image.
#[derive(Debug, Clone)]
pub struct Sqe {
    pub opcode: u8,
    pub fuse: Fuse,
    pub psdt: Psdt,
    pub cid: u16,
    pub nsid: u32,
    pub mptr: u64,
    pub dptr: [u8; 16],
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

impl Sqe {
    /// Decode from a 64-byte slice. Returns `NvmeError::SqeLength`
    /// if the slice is the wrong size (transport bug — every call
    /// site has a fixed-width buffer).
    pub fn parse(buf: &[u8]) -> Result<Self, NvmeError> {
        if buf.len() != crate::SQE_SIZE {
            return Err(NvmeError::SqeLength(buf.len()));
        }
        let cdw0 = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let opcode = (cdw0 & 0xFF) as u8;
        let fuse = Fuse::from_bits(((cdw0 >> 8) & 0b11) as u8);
        let psdt = Psdt::from_bits((((cdw0 >> 14) & 0b11) as u8) << 6);
        let cid = ((cdw0 >> 16) & 0xFFFF) as u16;
        let nsid = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        // bytes 8..15 reserved
        let mptr = u64::from_le_bytes([
            buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
        ]);
        let mut dptr = [0u8; 16];
        dptr.copy_from_slice(&buf[24..40]);
        let cdw10 = u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]);
        let cdw11 = u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]]);
        let cdw12 = u32::from_le_bytes([buf[48], buf[49], buf[50], buf[51]]);
        let cdw13 = u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]);
        let cdw14 = u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]);
        let cdw15 = u32::from_le_bytes([buf[60], buf[61], buf[62], buf[63]]);
        Ok(Self {
            opcode,
            fuse,
            psdt,
            cid,
            nsid,
            mptr,
            dptr,
            cdw10,
            cdw11,
            cdw12,
            cdw13,
            cdw14,
            cdw15,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_sqe_bytes() -> Vec<u8> {
        let mut b = vec![0u8; crate::SQE_SIZE];
        // CDW0: OPC=0x06 (Identify), CID=0x1234
        b[0] = 0x06;
        b[2] = 0x34;
        b[3] = 0x12;
        // NSID = 1
        b[4] = 0x01;
        // CDW10 = 0x0000_0001 (Identify CNS=Namespace)
        b[40] = 0x01;
        b
    }

    #[test]
    fn parses_basic_sqe_fields() {
        let bytes = build_sqe_bytes();
        let sqe = Sqe::parse(&bytes).expect("sqe parse");
        assert_eq!(sqe.opcode, 0x06);
        assert_eq!(sqe.cid, 0x1234);
        assert_eq!(sqe.nsid, 1);
        assert_eq!(sqe.cdw10, 1);
    }

    #[test]
    fn wrong_length_errors() {
        let res = Sqe::parse(&[0u8; 60]);
        assert!(matches!(res, Err(NvmeError::SqeLength(60))));
    }

    #[test]
    fn fuse_bits_decode() {
        let mut b = vec![0u8; crate::SQE_SIZE];
        b[1] = 0b01; // FUSE = 01 (first)
        let sqe = Sqe::parse(&b).expect("parse");
        assert_eq!(sqe.fuse, Fuse::First);
    }
}
