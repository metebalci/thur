// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared types for the drive-LUN SCSI dispatcher.
//!
//! [`Pdu`] is the iSCSI PDU view (BHS + payload) that per-opcode
//! handlers read CDB bytes and write data segments out of. [`ScsiResp`]
//! / [`ScsiStatus`] are the canonical response shape. [`ScsiCtx`]
//! bundles the per-command state every drive-LUN handler needs —
//! topology lives behind a [`TapeDeviceFacade`] reference so
//! thurvtl's multi-drive `Library` plugs into the shared
//! dispatch surface without per-product duplication.
//!
//! SMC-side state (raw `Library` / `ElementAddressConfig`) intentionally
//! does NOT live here — it stays in `thurvtld`'s wrapper
//! context that `Deref`s to this `ScsiCtx`. The per-LUN
//! [`DiagnosticStore`] does live here: SEND/RECEIVE DIAGNOSTIC are
//! dispatched by the shared handlers, so both products thread their
//! own store through.

use core_mediachanger::{AuditActor, AuditChannel, AuditRateLimiter, TapeDeviceFacade, TapeEvent};
use shared_iscsi::unit_attention::UnitAttentionTracker;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use crate::diagnostics::DiagnosticStore;
use crate::drive_manager::DriveManager;
use crate::scsi;

/// iSCSI PDU view threaded through every handler. `bhs` carries the
/// 48-byte Basic Header Segment — handlers slice CDB bytes (`bhs[32..48]`)
/// and read transport fields (EDTL at `bhs[20..24]`, etc.). `data` is
/// the immediate-or-data-out payload accumulated by the transport
/// before dispatch; handlers may take ownership (`std::mem::take`)
/// when forwarding it to the cartridge layer.
#[derive(Debug)]
pub struct Pdu {
    pub opcode: u8,
    pub immediate: bool,
    pub final_bit: bool,
    pub total_ahs_len: u8,
    pub data_segment_len: u32,
    pub lun: [u8; 8],
    pub itt: u32,
    pub ttt: u32,
    pub cmdsn: u32,
    pub expstatsn: u32,
    pub bhs: [u8; 48],
    pub data: Vec<u8>,
}

impl Pdu {
    /// Build a synthetic `Pdu` from the raw fields a single SCSI
    /// command carries. Used by the transport adapter — which converts
    /// a `shared_iscsi::ScsiRequest` into the PDU view handlers read —
    /// and by dispatch unit tests. Only the fields per-opcode handlers
    /// reach into are populated:
    /// - `bhs[1]` / `lun[1]` — single-level LUN byte (LUNs < 256)
    /// - `bhs[20..24]` — Expected Data Transfer Length, synthesized
    ///   from `data_in_max` so [`pdu_expected_xfer_len`] keeps working
    /// - `bhs[32..48]` — 16-byte CDB, zero-padded if shorter
    /// - `data` — the Data-Out payload already drained by the transport
    pub fn synth(cdb: &[u8], lun: u64, data_in_max: usize, data_out: &[u8]) -> Pdu {
        let mut bhs = [0u8; 48];
        let lun_byte = (lun & 0xFF) as u8;
        bhs[1] = lun_byte;
        let edtl = u32::try_from(data_in_max).unwrap_or(u32::MAX);
        bhs[20..24].copy_from_slice(&edtl.to_be_bytes());
        let n = cdb.len().min(16);
        bhs[32..32 + n].copy_from_slice(&cdb[..n]);

        let mut lun_field = [0u8; 8];
        lun_field[1] = lun_byte;

        Pdu {
            opcode: 0x01, // SCSI Command
            immediate: false,
            final_bit: true,
            total_ahs_len: 0,
            data_segment_len: data_out.len() as u32,
            lun: lun_field,
            itt: 0,
            ttt: 0,
            cmdsn: 0,
            expstatsn: 0,
            bhs,
            data: data_out.to_vec(),
        }
    }
}

#[cfg(test)]
mod pdu_tests {
    use super::*;

    #[test]
    fn synth_populates_cdb_lun_and_edtl() {
        let cdb = [0x12u8, 0x00, 0x00, 0x00, 0x60, 0x00];
        let pdu = Pdu::synth(&cdb, 3, 0x60, &[]);
        assert_eq!(pdu.opcode, 0x01);
        assert_eq!(pdu.bhs[1], 3);
        assert_eq!(pdu.lun[1], 3);
        assert_eq!(&pdu.bhs[32..38], &cdb);
        assert_eq!(pdu_expected_xfer_len(&pdu), 0x60);
        assert!(pdu.data.is_empty());
        assert_eq!(pdu.data_segment_len, 0);
    }

    #[test]
    fn synth_truncates_overlong_cdb_and_carries_data_out() {
        let cdb = [0xAAu8; 20];
        let data = [1u8, 2, 3, 4];
        let pdu = Pdu::synth(&cdb, 0, 0, &data);
        // CDB clamped to the 16-byte BHS window.
        assert_eq!(&pdu.bhs[32..48], &[0xAA; 16]);
        assert_eq!(pdu.data, data);
        assert_eq!(pdu.data_segment_len, 4);
    }

    #[test]
    fn synth_clamps_oversized_data_in_max_to_u32_max() {
        let pdu = Pdu::synth(&[0], 0, usize::MAX, &[]);
        assert_eq!(pdu_expected_xfer_len(&pdu), u32::MAX);
    }
}

/// SCSI status code for `ScsiResp`. The dispatcher only ever emits
/// `Good` or `CheckCondition` — RESERVATION CONFLICT / BUSY / TASK SET
/// FULL etc. aren't modeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScsiStatus {
    Good,
    CheckCondition,
}

/// One SCSI command response. `data_out` holds the bytes sent back as
/// Data-In PDUs; `sense` is an explicit sense buffer for
/// CHECK CONDITION (when `None` and status is CheckCondition the
/// transport falls back to "ILLEGAL REQUEST / INVALID COMMAND
/// OPERATION CODE").
pub struct ScsiResp {
    pub status: ScsiStatus,
    pub data_out: Vec<u8>,
    /// Optional sense override for CHECK CONDITION responses. Set this
    /// to surface non-default sense like
    /// `DataProtect / WRITE PROTECTED` for the LicensedReadOnly path.
    pub sense: Option<Vec<u8>>,
}

impl ScsiResp {
    pub fn good() -> Self {
        Self {
            status: ScsiStatus::Good,
            data_out: Vec::new(),
            sense: None,
        }
    }

    pub fn check_condition() -> Self {
        Self {
            status: ScsiStatus::CheckCondition,
            data_out: Vec::new(),
            sense: None,
        }
    }

    /// CHECK CONDITION carrying a sense buffer derived from a
    /// `SmcError`. Lets handlers surface real causes (WORM
    /// violation, legal hold, EOD/filemark, decryption error,
    /// not-ready, etc.) instead of the default INVALID COMMAND
    /// OPERATION CODE that `check_condition()` produces.
    pub fn check_condition_for(err: &core_mediachanger::errors::SmcError) -> Self {
        Self {
            status: ScsiStatus::CheckCondition,
            data_out: Vec::new(),
            sense: Some(scsi::sense::error_to_sense(err)),
        }
    }

    /// CHECK CONDITION with an explicit sense buffer (built by the
    /// caller). Used by the dispatch entry's UA pop.
    pub fn check_condition_with_sense(sense: Vec<u8>) -> Self {
        Self {
            status: ScsiStatus::CheckCondition,
            data_out: Vec::new(),
            sense: Some(sense),
        }
    }
}

/// Per-command context built once at the top of `handle_scsi_command`
/// and threaded through `dispatch_scsi` plus every per-opcode handler
/// so arms don't drag captured locals each. Lifetimes tie back to the
/// caller's parameters — every borrow lives for the duration of one
/// SCSI command.
///
/// thurvtl wraps this in `scsi_smc::SmcScsiCtx` (which
/// `Deref`s here) to add the SMC-side `library` / `element_config`
/// borrows the changer / library-touching handlers need.
#[allow(clippy::too_many_arguments)]
pub struct ScsiCtx<'a> {
    pub pdu: &'a mut Pdu,
    pub cdb: [u8; 16],
    pub lun: u8,
    pub drive_id: usize,
    pub device_type: u8,
    pub device_name: String,
    pub tsih: u16,
    pub drive_manager: &'a Arc<DriveManager>,
    pub facade: &'a dyn TapeDeviceFacade,
    pub ua_tracker: &'a Arc<Mutex<UnitAttentionTracker>>,
    pub event_tx: &'a broadcast::Sender<TapeEvent>,
    pub data_dir: &'a std::path::Path,
    pub audit_log: &'a Option<AuditChannel>,
    pub audit_ratelimiter: &'a AuditRateLimiter,
    pub initiator_iqn: Option<&'a str>,
    pub peer: &'a str,
    /// Per-LUN ring buffer of self-test results. SEND DIAGNOSTIC reads
    /// `last(lun)`; RECEIVE DIAGNOSTIC RESULTS page 0x10 walks
    /// `snapshot(lun)`. Populated by thurvtl's `run_and_record`
    /// pre-hook.
    pub diagnostic_store: &'a Arc<DiagnosticStore>,
    /// Logical partition this session was bound to at login time
    /// (CHAP user → partition mapping). `None` = no fence (legacy
    /// unpartitioned access). When `Some`, drive-LUN dispatch is
    /// refused for drives outside the partition, MOVE MEDIUM is
    /// refused for source/dest outside the partition, and READ
    /// ELEMENT STATUS filters to in-partition elements only.
    pub session_partition: Option<&'a str>,
    /// Topology flag: `true` when LUN 0 is the SMC medium changer
    /// (thurvtl default). Drive-LUN handlers in
    /// `dispatch::handlers` consult this through
    /// [`ScsiCtx::is_changer_lun`] to decide whether to refuse a
    /// drive opcode on LUN 0 as a "wrong device type" condition or
    /// run it normally.
    pub has_changer: bool,
}

impl ScsiCtx<'_> {
    /// Build a fresh `iscsi`-kind audit actor. Called per audit-write
    /// site (so the `String` allocations stay scoped to actual logging).
    pub fn audit_actor(&self) -> AuditActor {
        AuditActor::iscsi(
            self.initiator_iqn.map(str::to_string),
            self.peer.to_string(),
        )
    }

    /// `true` iff this command targets the SMC medium changer at LUN 0.
    /// Drive-LUN handlers refuse drive opcodes when this returns true.
    pub fn is_changer_lun(&self) -> bool {
        self.has_changer && self.lun == 0
    }
}

// --- byte helpers ---
//
// `u24` / `put_u24` lifted to `shared_iscsi::transport` (BHS field
// pack/unpack). The remaining helpers below are read by the
// per-opcode handlers.

pub fn put_be_u32(dst: &mut [u8], v: u32) {
    dst.copy_from_slice(&v.to_be_bytes());
}

pub fn pdu_expected_xfer_len(pdu: &Pdu) -> u32 {
    // RFC 3720 section 10.3.1: Expected Data Transfer Length is at
    // bytes 20-23 of SCSI Command BHS.
    u32::from_be_bytes([pdu.bhs[20], pdu.bhs[21], pdu.bhs[22], pdu.bhs[23]])
}

pub fn limit_len(mut d: Vec<u8>, max: u32) -> Vec<u8> {
    if d.len() as u32 > max {
        d.truncate(max as usize);
    }
    d
}

/// iSCSI PDU opcode → human label, for tracing.
pub fn opcode_name(op: u8) -> &'static str {
    match op & 0x3F {
        // Initiator opcodes
        0x00 => "NOP-Out",
        0x01 => "SCSI Command",
        0x02 => "Task Mgmt Req",
        0x03 => "Login Req",
        0x04 => "Text Req",
        0x05 => "Data-Out",
        0x06 => "Logout Req",
        // Target opcodes
        0x20 => "NOP-In",
        0x21 => "SCSI Resp",
        0x22 => "Task Mgmt Resp",
        0x23 => "Login Resp",
        0x24 => "Text Resp",
        0x25 => "Data-In",
        0x26 => "Logout Resp",
        0x3F => "Reject",
        _ => "Unknown",
    }
}

/// Legacy fallback for VPD `0xB1` / LOG SENSE 0x14 param 0x0040 when
/// the on-disk inventory pre-dates the `mfg_serial` field. Mirrors the
/// historical literal so pre-field deployments don't observe a serial
/// change. New libraries draw from `Library::drive_mfg_serial` (random
/// per-drive, persisted in inventory.json).
pub fn drive_mfg_serial_fallback(lun: u8) -> String {
    format!("THUR-MFG-{:03}", lun)
}
