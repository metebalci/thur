// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Drive-LUN SCSI dispatch — types, audit helpers, per-opcode handlers,
//! and the dispatch shell consumed by `thurvtld`.
//!
//! The dispatch shell ([`dispatch_drive_lun`]) routes every opcode that
//! the shared per-opcode handlers in [`handlers`] cover. thurvtl
//! (LUN 0 = changer, LUN ≥ 1 = drive, plus an SMC `Library` lock)
//! wraps it: the wrapper handles its library-touching arms (INQUIRY
//! VPD `0xB4`, changer INQUIRY / LOG SENSE / `0xB5`, MODE
//! SENSE/SELECT changer pages on LUN 0, INITIALIZE/READ ELEMENT
//! STATUS, MOVE/EXCHANGE MEDIUM, SEND VOLUME TAG) inline and falls
//! through to [`dispatch_drive_lun`] for everything else.

pub mod audit;
pub mod handlers;
pub mod inquiry;
pub mod types;

use anyhow::Result;

pub use audit::{audit_append, ratelimit_key_for};
pub use types::{
    Pdu, ScsiCtx, ScsiResp, ScsiStatus, drive_mfg_serial_fallback, limit_len, opcode_name,
    pdu_expected_xfer_len, put_be_u32,
};

/// Per-opcode dispatch over `ctx.cdb[0]` for every drive-LUN opcode the
/// shared handlers cover. Returns:
///
/// - `Some(Ok(resp))` — opcode was handled, here is the response.
/// - `Some(Err(e))` — opcode was handled but a handler bubbled an error.
/// - `None` — opcode is not in the shared drive-LUN set; thurvtl
///   decides whether to handle it locally (e.g. SMC changer ops,
///   wrapper-only drive-LUN arms) or surface INVALID OPERATION CODE.
///
/// The function dispatches purely on `ctx.cdb[0]` — no LUN-based
/// routing happens here. Per-handler topology decisions consult
/// [`ScsiCtx::is_changer_lun`](crate::dispatch::types::ScsiCtx::is_changer_lun)
/// (true when `lun == 0` AND the caller declared `has_changer: true`)
/// to refuse drive opcodes against the SMC medium changer.
/// thurvtl sets `has_changer: true` and intercepts the LUN-0 /
/// VPD-`0xB4` / wrapper-only arms before delegating.
pub fn dispatch_drive_lun(ctx: &mut ScsiCtx<'_>) -> Option<Result<ScsiResp>> {
    Some(match ctx.cdb[0] {
        // Identity surface — facade-driven (lifted in 5.B.6 follow-up
        // step 4).
        0x12 => inquiry::handle_inquiry(ctx),
        0xa0 => handlers::handle_report_luns(ctx),
        0x4D => handlers::handle_log_sense(ctx),

        // Lifecycle / housekeeping.
        0x00 => handlers::handle_test_unit_ready(ctx),
        0x03 => handlers::handle_request_sense(ctx),
        0x1E => handlers::handle_prevent_allow_medium_removal(ctx),

        // Verify / read-write attribute.
        0x13 => handlers::handle_verify_6(ctx),
        0x8F => handlers::handle_verify_16(ctx),
        0x8C => handlers::handle_read_attribute(ctx),
        0x8D => handlers::handle_write_attribute(ctx),

        // Drive parameters / density / position.
        0x05 => handlers::handle_read_block_limits(ctx),
        0x44 => handlers::handle_report_density_support(ctx),
        0x01 => handlers::handle_rewind(ctx),
        0x1B => handlers::handle_load_unload(ctx),
        0x34 => handlers::handle_read_position(ctx),
        0x11 => handlers::handle_space_6(ctx),
        0x91 => handlers::handle_space_16(ctx),
        0x10 => handlers::handle_write_filemarks_6(ctx),
        0x19 => handlers::handle_erase_6(ctx),
        0x0B => handlers::handle_set_capacity(ctx),
        0x80 => handlers::handle_write_filemarks_16(ctx),
        0x2B => handlers::handle_locate_10(ctx),
        0x92 => handlers::handle_locate_16(ctx),

        // Data path.
        0x08 => handlers::handle_read_6(ctx),
        0x0A => handlers::handle_write_6(ctx),
        0x04 => handlers::handle_format_medium(ctx),
        0x82 => handlers::handle_allow_overwrite(ctx),
        0x3B => handlers::handle_write_buffer(ctx),
        0x3C => handlers::handle_read_buffer(ctx),

        // RESERVE / RELEASE (SCSI-2 style).
        0x16 => handlers::handle_reserve_6(ctx),
        0x17 => handlers::handle_release_6(ctx),
        0x56 => handlers::handle_reserve_10(ctx),
        0x57 => handlers::handle_release_10(ctx),

        // Persistent reservations + maintenance + security protocol.
        0xA2 => handlers::handle_security_protocol_in(ctx),
        0xB5 => handlers::handle_security_protocol_out(ctx),
        0xA3 => handlers::handle_maintenance_in(ctx),
        0xA4 => handlers::handle_maintenance_out(ctx),
        0x5E => handlers::handle_persistent_reserve_in(ctx),
        0x5F => handlers::handle_persistent_reserve_out(ctx),

        // Drive-LUN MODE SENSE / MODE SELECT (5.B.6 follow-up step 7).
        // thurvtl's wrapper intercepts the LUN-0 (changer) path
        // before delegating.
        0x1A => handlers::handle_mode_sense_6_drive(ctx),
        0x5A => handlers::handle_mode_sense_10_drive(ctx),
        0x15 => handlers::handle_mode_select_6_drive(ctx),
        0x55 => handlers::handle_mode_select_10_drive(ctx),

        // LOG SELECT — no-op for both products.
        0x4C => handlers::handle_log_select(ctx),

        // SEND / RECEIVE DIAGNOSTIC — read/write the per-LUN
        // `DiagnosticStore` carried on the context.
        0x1C => handlers::handle_receive_diagnostic_results(ctx),
        0x1D => handlers::handle_send_diagnostic(ctx),

        _ => return None,
    })
}
