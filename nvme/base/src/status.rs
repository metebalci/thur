// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! CQE Status Field (NVMe Base §4.6.1).
//!
//! Bit layout (little-endian 16-bit word):
//!
//! ```text
//! bit  15  14  13      11 10        8 7              0
//!     +--+---+----------+-----------+----------------+
//!     |DNR| M | CRD(2)   | SCT (3)   | SC (8)         | + P(1) at bit 0
//!     +--+---+----------+-----------+----------------+
//! ```
//!
//! - **P**: Phase Tag, toggled each round of the CQ ring. Set by
//!   the transport, not this layer.
//! - **SC**: Status Code (8 bits).
//! - **SCT**: Status Code Type (3 bits) — Generic / Command-Specific
//!   / Media / Path / Vendor.
//! - **CRD**: Command Retry Delay (2 bits, indexes a host-side delay
//!   table). 0 for everything we emit today.
//! - **M**: More info available (an associated Async Event Request
//!   payload). 0 for everything we emit today.
//! - **DNR**: Do Not Retry — set when the failure is permanent and
//!   re-issuing the same command would just fail the same way
//!   (Invalid Field, Invalid Namespace, etc.).

/// SCT — Status Code Type (NVMe Base §4.6.1.4).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCodeType {
    Generic = 0,
    CommandSpecific = 1,
    MediaAndDataIntegrity = 2,
    PathRelated = 3,
    Vendor = 7,
}

/// Decoded CQE status field. `Default` is success (SCT=0, SC=0,
/// DNR=0, M=0, CRD=0) — Phase Tag is the transport's responsibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusField {
    pub sct: StatusCodeType,
    pub sc: u8,
    pub dnr: bool,
    pub more: bool,
    pub crd: u8,
}

impl StatusField {
    pub const SUCCESS: Self = Self {
        sct: StatusCodeType::Generic,
        sc: 0x00,
        dnr: false,
        more: false,
        crd: 0,
    };

    /// Pack into the 16-bit STATUS word (P=0; the transport ORs in
    /// the phase tag on writeback).
    pub fn to_u16(self) -> u16 {
        let mut w: u16 = 0;
        // bit 0 = P (left zero)
        // bits 8..0 = SC (shifted by 1 because P occupies bit 0)
        w |= (u16::from(self.sc)) << 1;
        w |= (u16::from(self.sct as u8) & 0b111) << 9;
        w |= (u16::from(self.crd) & 0b11) << 12;
        if self.more {
            w |= 1 << 14;
        }
        if self.dnr {
            w |= 1 << 15;
        }
        w
    }

    // Generic-status helpers (NVMe Base §4.6.1.5 Figure 96).

    pub fn invalid_opcode() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x01,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    pub fn invalid_field() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x02,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    pub fn data_transfer_error() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x04,
            dnr: false,
            ..Self::SUCCESS
        }
    }

    pub fn internal_error() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x06,
            dnr: false,
            ..Self::SUCCESS
        }
    }

    /// Generic: Namespace Is Write Protected (NVMe Base §4.6.1.5,
    /// Generic Command Status 0x20). Returned for a write-class command
    /// (Write / Write Zeroes / Dataset Management Deallocate / fused
    /// Compare+Write) against a WORM namespace — the NVM-Command-Set
    /// analog of SCSI WRITE PROTECTED / DATA PROTECT (0x07 / 0x27). DNR:
    /// the volume's WORM flag is sticky, so re-issuing fails the same.
    pub fn namespace_write_protected() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x20,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    pub fn lba_out_of_range() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x80,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    pub fn capacity_exceeded() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x81,
            dnr: false,
            ..Self::SUCCESS
        }
    }

    pub fn namespace_not_ready() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x82,
            dnr: false,
            ..Self::SUCCESS
        }
    }

    /// NVM Command Set §3.2.5 — Compare half of a fused
    /// Compare+Write mismatched.
    pub fn compare_failure() -> Self {
        Self {
            sct: StatusCodeType::MediaAndDataIntegrity,
            sc: 0x85,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    /// Command-specific: Invalid Namespace or Format (e.g. NSID
    /// the target doesn't have attached).
    pub fn invalid_namespace() -> Self {
        Self {
            sct: StatusCodeType::CommandSpecific,
            sc: 0x0B,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    /// Generic: Aborted due to failed fused command. Returned on
    /// the second half of a fused operation when the first half
    /// failed (NVMe Base §4.2.6).
    pub fn aborted_due_to_failed_fused() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x0A,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    /// Generic: Aborted due to missing fused command. Returned when
    /// the second half of a fused operation arrives without a
    /// preceding first half, or vice versa.
    pub fn aborted_due_to_missing_fused() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x0B,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    // NVMe-oF Connect failures (§6.3.1.5). Returned in CapsuleResp
    // CQE.status; the host treats all of these as DNR.

    /// Connect Invalid Parameters — used for SUBNQN mismatch (host
    /// asked for a subsystem we don't serve), unsupported queue ID,
    /// or a malformed Connect Data structure.
    pub fn connect_invalid_parameters() -> Self {
        Self {
            sct: StatusCodeType::CommandSpecific,
            sc: 0x82,
            dnr: true,
            ..Self::SUCCESS
        }
    }

    /// Generic: Reservation Conflict (NVM Command Set reservations,
    /// NVMe Base §4.6.1.2.1 — Generic Command Status 0x83). Returned
    /// when an I/O command is blocked by a reservation held by another
    /// host, or when a reservation command's key check fails — the
    /// protocol-native analog of SCSI RESERVATION CONFLICT (0x18). The
    /// SCT must be Generic (0): the Linux `nvme` driver only maps
    /// SC=0x83 to `BLK_STS_NEXUS` when SCT=0, and nvme-cli decodes the
    /// Command-Specific 0x83 slot as an unrelated string ("Command Size
    /// Limit Exceeded").
    pub fn reservation_conflict() -> Self {
        Self {
            sct: StatusCodeType::Generic,
            sc: 0x83,
            dnr: true,
            ..Self::SUCCESS
        }
    }
}

impl Default for StatusField {
    fn default() -> Self {
        Self::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_zero() {
        assert_eq!(StatusField::SUCCESS.to_u16(), 0);
    }

    #[test]
    fn invalid_opcode_packs_correctly() {
        let s = StatusField::invalid_opcode();
        let w = s.to_u16();
        // SC 0x01 << 1 = 0x02; SCT 0 (Generic); DNR bit 15.
        assert_eq!(w, 0x8002);
    }

    #[test]
    fn lba_oor_carries_generic_0x80() {
        let s = StatusField::lba_out_of_range();
        let w = s.to_u16();
        // SC 0x80 << 1 = 0x100; SCT 0; DNR bit 15.
        assert_eq!(w, 0x8100);
    }

    #[test]
    fn namespace_write_protected_packs_generic_0x20() {
        let s = StatusField::namespace_write_protected();
        // SC 0x20 << 1 = 0x40; SCT 0 (Generic); DNR bit 15 = 0x8000.
        assert_eq!(s.to_u16(), 0x8040);
        assert_eq!(s.sct, StatusCodeType::Generic);
        assert_eq!(s.sc, 0x20);
        assert!(s.dnr);
    }

    #[test]
    fn reservation_conflict_packs_generic_0x83() {
        let s = StatusField::reservation_conflict();
        // SC 0x83 << 1 = 0x106; SCT 0 (Generic); DNR bit 15 = 0x8000.
        // Must be Generic, not Command-Specific: the Linux nvme driver
        // and nvme-cli only recognize SC=0x83 as Reservation Conflict
        // when SCT=0 (NVMe Base §4.6.1.2.1).
        assert_eq!(s.to_u16(), 0x8106);
        assert_eq!(s.sct, StatusCodeType::Generic);
        assert_eq!(s.sc, 0x83);
    }
}
