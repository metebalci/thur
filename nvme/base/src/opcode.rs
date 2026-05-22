// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe Admin command set opcodes + SQE CDW0 sub-fields.
//!
//! NVM Command Set opcodes (Read / Write / Flush / ...) live in
//! [`nvme_nvm`](../../nvme_nvm/index.html) so this crate stays
//! command-set-agnostic — Admin is the only opcode set the Base
//! Spec defines.

/// NVMe Admin command set opcodes (NVMe Base §5). Names match the
/// canonical wire names; values are the 8-bit OPC field of SQE
/// CDW0[7:0]. Only the opcodes the target actually implements are
/// enumerated here; unknown opcodes flow through as `u8` and the
/// dispatcher returns Invalid Command Opcode.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOpcode {
    DeleteIoSubmissionQueue = 0x00,
    CreateIoSubmissionQueue = 0x01,
    GetLogPage = 0x02,
    DeleteIoCompletionQueue = 0x04,
    CreateIoCompletionQueue = 0x05,
    Identify = 0x06,
    Abort = 0x08,
    SetFeatures = 0x09,
    GetFeatures = 0x0A,
    AsyncEventRequest = 0x0C,
    KeepAlive = 0x18,
    /// Fabrics command set (NVMe-oF §6.1). Sub-type discriminated
    /// inside CDW10 (Connect / Property Get / Property Set /
    /// Authentication Send/Receive / Disconnect).
    Fabrics = 0x7F,
}

impl AdminOpcode {
    /// Parse an opcode byte. Returns `None` for opcodes this target
    /// doesn't implement — the dispatcher turns that into a CQE with
    /// status `Invalid Command Opcode` (SCT=Generic, SC=0x01).
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::DeleteIoSubmissionQueue,
            0x01 => Self::CreateIoSubmissionQueue,
            0x02 => Self::GetLogPage,
            0x04 => Self::DeleteIoCompletionQueue,
            0x05 => Self::CreateIoCompletionQueue,
            0x06 => Self::Identify,
            0x08 => Self::Abort,
            0x09 => Self::SetFeatures,
            0x0A => Self::GetFeatures,
            0x0C => Self::AsyncEventRequest,
            0x18 => Self::KeepAlive,
            0x7F => Self::Fabrics,
            _ => return None,
        })
    }
}

/// FUSE field — SQE CDW0[9:8] (NVMe Base §4.5).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fuse {
    /// Normal (non-fused) operation.
    Normal = 0b00,
    /// First command of a fused operation. Currently only fused
    /// Compare + Write (NVM Command Set §3.2.5) is defined.
    First = 0b01,
    /// Second command of a fused operation.
    Second = 0b10,
}

impl Fuse {
    pub fn from_bits(b: u8) -> Self {
        match b & 0b11 {
            0b01 => Self::First,
            0b10 => Self::Second,
            _ => Self::Normal,
        }
    }
}

/// PSDT field — SQE CDW0[15:14] (NVMe Base §4.5). Indicates the
/// data-pointer layout: PRP (legacy PCIe) or SGL (modern + every
/// NVMe-oF transport).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Psdt {
    /// PRP list / PRP entries. Legacy PCIe transport only.
    Prp = 0b00,
    /// SGL, descriptor inline in SQE DPTR. Used by fabrics
    /// transports including NVMe/TCP.
    SglInline = 0b01,
    /// SGL, descriptor at the address in SQE DPTR points to.
    SglPointer = 0b10,
}

impl Psdt {
    pub fn from_bits(b: u8) -> Self {
        match (b >> 6) & 0b11 {
            0b01 => Self::SglInline,
            0b10 => Self::SglPointer,
            _ => Self::Prp,
        }
    }
}
