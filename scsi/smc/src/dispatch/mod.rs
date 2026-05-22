// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Changer-LUN SCSI dispatch — per-command context + six SMC opcode
//! handlers.
//!
//! [`dispatch_changer_lun`] is called by `thurvtld`'s
//! `dispatch_scsi` for opcodes 0x07 / 0x37 / 0xA5 / 0xA6 / 0xB6 / 0xB8.
//! Returns `Some(_)` when the opcode was handled (Ok or Err) and `None`
//! for unrecognized opcodes — the caller then handles its own
//! changer-LUN INQUIRY / LOG SENSE / MODE SENSE / MODE SELECT
//! variants or falls through to the drive-LUN dispatcher in
//! `scsi-ssc`.

pub mod handlers;
pub mod types;

use anyhow::Result;

pub use scsi_ssc::dispatch::ScsiResp;
pub use types::SmcScsiCtx;

/// Per-opcode dispatch for the six SMC changer commands. All six
/// require LUN 0 by SCSI surface; handlers internally refuse with
/// CHECK CONDITION on `lun != 0`.
pub fn dispatch_changer_lun(ctx: &mut SmcScsiCtx<'_>) -> Option<Result<ScsiResp>> {
    Some(match ctx.cdb[0] {
        0x07 => handlers::handle_initialize_element_status(ctx),
        0x37 => handlers::handle_initialize_element_status_with_range(ctx),
        0xA5 => handlers::handle_move_medium(ctx),
        0xA6 => handlers::handle_exchange_medium(ctx),
        0xB6 => handlers::handle_send_volume_tag(ctx),
        0xB8 => handlers::handle_read_element_status(ctx),
        _ => return None,
    })
}
