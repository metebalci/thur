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
