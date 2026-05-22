// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVM Command Set opcodes (NVM Command Set Specification §3).
//!
//! Distinct enum from `nvme_base::AdminOpcode` because the opcode
//! number space is per-command-set — the same byte 0x02 is "Get Log
//! Page" in the admin set and "Read" in the NVM set.

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvmOpcode {
    Flush = 0x00,
    Write = 0x01,
    Read = 0x02,
    WriteUncorrectable = 0x04,
    Compare = 0x05,
    WriteZeroes = 0x08,
    DatasetManagement = 0x09,
    Verify = 0x0C,
    ReservationRegister = 0x0D,
    ReservationReport = 0x0E,
    ReservationAcquire = 0x11,
    ReservationRelease = 0x15,
}

impl NvmOpcode {
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::Flush,
            0x01 => Self::Write,
            0x02 => Self::Read,
            0x04 => Self::WriteUncorrectable,
            0x05 => Self::Compare,
            0x08 => Self::WriteZeroes,
            0x09 => Self::DatasetManagement,
            0x0C => Self::Verify,
            0x0D => Self::ReservationRegister,
            0x0E => Self::ReservationReport,
            0x11 => Self::ReservationAcquire,
            0x15 => Self::ReservationRelease,
            _ => return None,
        })
    }

    /// Whether this opcode mutates user data. Mirrors the
    /// host-visible-write-opcode set the SBC dispatcher recognizes.
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Self::Write
                | Self::WriteUncorrectable
                | Self::WriteZeroes
                | Self::DatasetManagement
                | Self::ReservationRegister
                | Self::ReservationAcquire
                | Self::ReservationRelease
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_u8_round_trips_known_opcodes() {
        for (byte, op) in [
            (0x00u8, NvmOpcode::Flush),
            (0x01, NvmOpcode::Write),
            (0x02, NvmOpcode::Read),
            (0x05, NvmOpcode::Compare),
            (0x08, NvmOpcode::WriteZeroes),
            (0x0C, NvmOpcode::Verify),
            (0x15, NvmOpcode::ReservationRelease),
        ] {
            assert_eq!(NvmOpcode::from_u8(byte), Some(op));
            assert_eq!(op as u8, byte);
        }
    }

    #[test]
    fn from_u8_rejects_unknown_opcodes() {
        // 0x03 sits between Read and WriteUncorrectable — unassigned.
        assert_eq!(NvmOpcode::from_u8(0x03), None);
        assert_eq!(NvmOpcode::from_u8(0xFF), None);
    }

    #[test]
    fn is_write_flags_only_data_mutating_opcodes() {
        for op in [
            NvmOpcode::Write,
            NvmOpcode::WriteUncorrectable,
            NvmOpcode::WriteZeroes,
            NvmOpcode::DatasetManagement,
            NvmOpcode::ReservationRegister,
            NvmOpcode::ReservationAcquire,
            NvmOpcode::ReservationRelease,
        ] {
            assert!(op.is_write(), "{op:?} should classify as a write");
        }
        for op in [
            NvmOpcode::Flush,
            NvmOpcode::Read,
            NvmOpcode::Compare,
            NvmOpcode::Verify,
            NvmOpcode::ReservationReport,
        ] {
            assert!(!op.is_write(), "{op:?} should not classify as a write");
        }
    }
}
