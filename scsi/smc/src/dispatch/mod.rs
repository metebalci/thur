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

use scsi_spc::reservations::Nexus;

pub use scsi_ssc::dispatch::ScsiResp;
pub use types::SmcScsiCtx;

/// Per-opcode dispatch for the six SMC changer commands. All six
/// require LUN 0 by SCSI surface; handlers internally refuse with
/// CHECK CONDITION on `lun != 0`.
///
/// PERSISTENT RESERVE enforcement runs first: a reservation held on
/// the changer LUN fences the movement / element-status opcodes
/// against non-permitted I_T nexuses (issue #53). See [`pr_enforce`].
pub fn dispatch_changer_lun(ctx: &mut SmcScsiCtx<'_>) -> Option<Result<ScsiResp>> {
    if let Some(refusal) = pr_enforce(ctx) {
        return Some(Ok(refusal));
    }
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

/// PERSISTENT RESERVE enforcement class for a changer-LUN opcode.
enum PrGate {
    /// Element-reading — gated by `allow_read`.
    Read,
    /// Inventory-mutating — gated by `allow_write`.
    Write,
    /// Always permitted regardless of reservation (SAM-5 §5.9.1):
    /// identity, status, mode pages, PRIN / PROUT themselves.
    None,
}

/// Classify a changer-LUN opcode for PERSISTENT RESERVE enforcement.
/// Only the medium-movement + element-status surface is fenced;
/// identity / status / PRIN / PROUT stay open — mirroring how a real
/// SMC-3 changer gates on a reservation.
fn pr_gate(opcode: u8) -> PrGate {
    match opcode {
        0xA5 // MOVE MEDIUM
        | 0xA6 // EXCHANGE MEDIUM
        | 0x07 // INITIALIZE ELEMENT STATUS
        | 0x37 // INITIALIZE ELEMENT STATUS WITH RANGE
        | 0xB6 // SEND VOLUME TAG
        => PrGate::Write,
        0xB8 // READ ELEMENT STATUS
        | 0xB5 // REQUEST VOLUME ELEMENT ADDRESS
        => PrGate::Read,
        _ => PrGate::None,
    }
}

/// Consult the reservation manager for the current changer command.
/// Returns `Some(RESERVATION CONFLICT)` when a reservation held on the
/// changer LUN fences this I_T nexus out of the opcode, `None` when
/// the command may proceed.
///
/// `pub` so the thurvtl wrapper can run the same gate for REQUEST
/// VOLUME ELEMENT ADDRESS (0xB5), which it dispatches itself rather
/// than routing through [`dispatch_changer_lun`]. A no-op on any LUN
/// that isn't the SMC medium changer — the drive LUNs run their own
/// gate in `scsi_ssc::dispatch::dispatch_drive_lun`, and reservation
/// state is keyed per-LUN so the two never collide.
pub fn pr_enforce(ctx: &SmcScsiCtx<'_>) -> Option<ScsiResp> {
    if !ctx.is_changer_lun() {
        return None;
    }
    let gate = pr_gate(ctx.cdb[0]);
    if matches!(gate, PrGate::None) {
        return None;
    }
    let nexus = Nexus::iscsi(ctx.initiator_iqn.map(str::to_string), ctx.initiator_isid);
    let lun = ctx.lun as u64;
    let allowed = match gate {
        PrGate::Write => ctx.reservations.allow_write(lun, &nexus),
        PrGate::Read => ctx.reservations.allow_read(lun, &nexus),
        PrGate::None => true,
    };
    if allowed {
        None
    } else {
        tracing::debug!(
            "RESERVATION CONFLICT: changer opcode 0x{:02x} on LUN {} refused for TSIH {}",
            ctx.cdb[0],
            ctx.lun,
            ctx.tsih
        );
        Some(ScsiResp::reservation_conflict())
    }
}
