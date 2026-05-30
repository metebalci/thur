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
//! reservation-notification source in `nvme-nvm` supplies the three
//! field values, and a future namespace-change source would reuse the
//! same helper with different constants.

/// Asynchronous Event Type — I/O Command Set specific status
/// (covers reservation-log-page-available). NVMe Base §5.2 Figure
/// "Asynchronous Event Type".
pub const AET_IO_COMMAND_SET: u8 = 0x6;

/// Asynchronous Event Information for an I/O Command Set specific
/// event: a reservation notification log page is available. NVMe NVM
/// Command Set "Reservation Log Page Available".
pub const AEI_RESERVATION_LOG_AVAILABLE: u8 = 0x00;

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
    fn async_event_dw0_packs_each_field() {
        assert_eq!(async_event_dw0(0x7, 0xFF, 0xAB), 0x00AB_FF07);
        // AET is masked to 3 bits.
        assert_eq!(async_event_dw0(0xF8, 0, 0) & 0x7, 0);
    }
}
