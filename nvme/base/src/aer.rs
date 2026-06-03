// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Asynchronous Event Request completion encoding (NVMe Base §5.2).
//!
//! When the controller has an event to report it completes an
//! outstanding AER (Admin opcode 0x0C) with a CQE whose DW0 carries
//! three packed fields telling the host what happened and which log
//! page to read for the detail:
//!
//! ```text
//! DW0 bits  Field
//!   2:0     Asynchronous Event Type
//!   7:3     reserved
//!  15:8     Asynchronous Event Information
//!  23:16    Associated Log Page Identifier
//!  31:24    reserved
//! ```
//!
//! The DW0 builder is transport- and command-set-agnostic; the
//! reservation-notification source and the namespace-change source in
//! `nvme-nvm` each supply the three field values, reusing the same
//! helper with different constants.

/// Asynchronous Event Type — I/O Command Set specific status
/// (covers reservation-log-page-available). NVMe Base §5.2 Figure
/// "Asynchronous Event Type".
pub const AET_IO_COMMAND_SET: u8 = 0x6;

/// Asynchronous Event Type — Notice (covers namespace-attribute /
/// firmware-activation / ANA-change notices). NVMe Base §5.2.
pub const AET_NOTICE: u8 = 0x2;

/// Asynchronous Event Information for an I/O Command Set specific
/// event: a reservation notification log page is available. NVMe NVM
/// Command Set "Reservation Log Page Available".
pub const AEI_RESERVATION_LOG_AVAILABLE: u8 = 0x00;

/// Asynchronous Event Information for a Notice event: the set of
/// namespace attributes changed and the Changed Namespace List log
/// (LID 0x04) is available. NVMe Base §5.2 "Namespace Attribute
/// Changed".
pub const AEI_NAMESPACE_ATTRIBUTE_CHANGED: u8 = 0x00;

/// Set Features FID 0x0B (Async Event Configuration) CDW11 bit 8 —
/// Namespace Attribute Notices. When the host sets this bit the
/// controller emits a Namespace Attribute Changed AER on volume
/// lifecycle changes; default clear (no notice). The same bit position
/// the controller advertises support for in Identify Controller OAES
/// (see [`crate::identify::oaes`]).
pub const AEN_CFG_NAMESPACE_ATTRIBUTE: u32 = 1 << 8;

/// Pack an AER completion DW0 from its three sub-fields. `aet` is
/// masked to 3 bits; `aei` and `lid` occupy bits 15:8 and 23:16
/// respectively.
pub const fn async_event_dw0(aet: u8, aei: u8, lid: u8) -> u32 {
    (aet as u32 & 0x7) | ((aei as u32) << 8) | ((lid as u32) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_page::lid;

    #[test]
    fn reservation_notice_dw0_matches_spec() {
        // AET=0x6, AEI=0x00, LID=0x80 → 0x0080_0006 (LID at bits 23:16).
        let dw0 = async_event_dw0(
            AET_IO_COMMAND_SET,
            AEI_RESERVATION_LOG_AVAILABLE,
            lid::RESERVATION_NOTIFICATION,
        );
        assert_eq!(dw0, 0x0080_0006);
    }

    #[test]
    fn namespace_notice_dw0_matches_spec() {
        // AET=0x2 (Notice), AEI=0x00 (Namespace Attribute Changed),
        // LID=0x04 → 0x0004_0002 (LID at bits 23:16).
        let dw0 = async_event_dw0(
            AET_NOTICE,
            AEI_NAMESPACE_ATTRIBUTE_CHANGED,
            lid::CHANGED_NAMESPACE_LIST,
        );
        assert_eq!(dw0, 0x0004_0002);
    }

    #[test]
    fn async_event_dw0_packs_each_field() {
        assert_eq!(async_event_dw0(0x7, 0xFF, 0xAB), 0x00AB_FF07);
        // AET is masked to 3 bits.
        assert_eq!(async_event_dw0(0xF8, 0, 0) & 0x7, 0);
    }
}
