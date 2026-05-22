// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! MAINTENANCE IN (opcode 0xA3) — capability-discovery service
//! actions.
//!
//! Two service actions today:
//!   - 0x0C REPORT SUPPORTED OPERATION CODES (SPC-4 §6.27.2):
//!     VAAI / Hyper-V probe this to discover offload primitives
//!     (CAW, WRITE SAME, UNMAP, COMPARE AND WRITE) without trying
//!     them and parsing CHECK CONDITION on failure.
//!   - 0x0D REPORT SUPPORTED TASK MANAGEMENT FUNCTIONS (SPC-4
//!     §6.27.3): 4-byte response advertising the SAM-/iSCSI-
//!     standard set the session layer accepts.
//!
//! Other service actions (REPORT TARGET PORT GROUPS 0x0A, REPORT
//! IDENTIFYING INFORMATION 0x05, etc.) aren't wired — initiators
//! that probe them get INVALID FIELD IN CDB. We don't model ALUA
//! (single-port virtual target) or identifying-info storage.

use super::types::{ScsiRequest, ScsiResponse, SenseData};

/// Every opcode the thurvsa dispatcher routes today, in ascending
/// order. Used to populate REPORT SUPPORTED OPERATION CODES (SA
/// 0x0C). Keep in sync with `handler.rs::dispatch`.
const SUPPORTED_OPCODES: &[u8] = &[
    0x00, // TEST UNIT READY
    0x03, // REQUEST SENSE
    0x12, // INQUIRY
    0x15, // MODE SELECT 6
    0x1A, // MODE SENSE 6
    0x1B, // START STOP UNIT
    0x1E, // PREVENT/ALLOW MEDIUM REMOVAL
    0x25, // READ CAPACITY 10
    0x28, // READ 10
    0x2A, // WRITE 10
    0x2F, // VERIFY 10
    0x35, // SYNCHRONIZE CACHE 10
    0x41, // WRITE SAME 10
    0x42, // UNMAP
    0x4D, // LOG SENSE
    0x55, // MODE SELECT 10
    0x5A, // MODE SENSE 10
    0x5E, // PERSISTENT RESERVE IN
    0x5F, // PERSISTENT RESERVE OUT
    0x88, // READ 16
    0x89, // COMPARE AND WRITE
    0x8A, // WRITE 16
    0x8F, // VERIFY 16
    0x91, // SYNCHRONIZE CACHE 16
    0x93, // WRITE SAME 16
    0x9E, // SERVICE ACTION IN 16
    0xA0, // REPORT LUNS
    0xA3, // MAINTENANCE IN
];

/// Dispatch the MAINTENANCE IN service action. Service action is
/// in CDB byte 1 bits 4-0; allocation length is in bytes 6-9 (BE).
pub(super) fn maintenance_in(req: &ScsiRequest<'_>) -> ScsiResponse {
    if req.cdb.len() < 12 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let service_action = req.cdb[1] & 0x1F;
    let alloc_len = u32::from_be_bytes([req.cdb[6], req.cdb[7], req.cdb[8], req.cdb[9]]) as usize;

    let body = match service_action {
        0x0C => report_supported_opcodes(),
        0x0D => report_supported_tmf(),
        _ => return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
    };

    let truncated: Vec<u8> = body.into_iter().take(alloc_len).collect();
    ScsiResponse::good(truncated)
}

/// REPORT SUPPORTED OPERATION CODES (SA 0x0C) — "all opcodes"
/// reporting option (REPORTING_OPTIONS=0). Response shape (SPC-4
/// §6.27.2):
///
///   bytes 0..3   COMMAND DATA LENGTH (BE32, excludes these 4)
///   per-entry (8 bytes):
///     byte 0       OPERATION CODE
///     byte 1       reserved
///     bytes 2..3   SERVICE ACTION (BE16, 0 when SA not used)
///     byte 4       reserved
///     byte 5       CTDP=0 | SERVACTV=0 | reserved
///     bytes 6..7   CDB LENGTH (BE16; 0xFFFF = "ask via specific
///                  SA reporting", which is what we report — we
///                  don't track per-opcode CDB lengths today).
///
/// REPORTING_OPTIONS=1/2/3 (single-opcode forms with timeout
/// descriptors) aren't implemented; initiators that need them
/// fall back to issuing the opcode and parsing the response.
fn report_supported_opcodes() -> Vec<u8> {
    let entry_count = SUPPORTED_OPCODES.len();
    let body_len = (entry_count * 8) as u32;
    let mut data = Vec::with_capacity(4 + entry_count * 8);
    data.extend_from_slice(&body_len.to_be_bytes());
    for &op in SUPPORTED_OPCODES {
        let mut entry = [0u8; 8];
        entry[0] = op;
        // SERVICE ACTION = 0 (none of our routed opcodes carry a
        // dispatched SA except 0x9E and 0xA3 themselves; for those
        // we report the CDB shape only — VAAI probes by opcode,
        // not by SA, and the present SA is implicit in the opcode
        // routing here).
        entry[6] = 0xFF;
        entry[7] = 0xFF; // CDB LENGTH = "use SA-specific report"
        data.extend_from_slice(&entry);
    }
    data
}

/// REPORT SUPPORTED TASK MANAGEMENT FUNCTIONS (SA 0x0D). 4-byte
/// response advertising which TMFs the iSCSI session layer accepts.
///
///   byte 0 bit 7 ATS    ABORT TASK
///   byte 0 bit 6 ATSS   ABORT TASK SET
///   byte 0 bit 4 CTSS   CLEAR TASK SET
///   byte 0 bit 3 LURS   LOGICAL UNIT RESET
///   byte 1 bit 7 ITNRS  I_T NEXUS RESET
///
/// CACAS / QTS / QAES / QTSS aren't advertised — thurvsa doesn't
/// model ACA, query semantics, or async-event endpoints. WAKES
/// (byte 0 bit 0) is obsolete.
fn report_supported_tmf() -> Vec<u8> {
    let mut data = vec![0u8; 4];
    data[0] = 0x80 | 0x40 | 0x10 | 0x08; // ATS | ATSS | CTSS | LURS
    data[1] = 0x80; // ITNRS
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(cdb: &'a [u8]) -> ScsiRequest<'a> {
        ScsiRequest {
            lun: 0,
            cdb,
            data_out: &[],
            data_in_max: 4096,
            tsih: 0,
            initiator_iqn: None,
            cid: 0,
            peer: "",
            session_partition: None,
        }
    }

    fn build_cdb(service_action: u8, alloc: u32) -> Vec<u8> {
        let mut cdb = vec![0u8; 12];
        cdb[0] = 0xA3;
        cdb[1] = service_action & 0x1F;
        cdb[6..10].copy_from_slice(&alloc.to_be_bytes());
        cdb
    }

    #[test]
    fn report_supported_opcodes_returns_every_routed_opcode() {
        let cdb = build_cdb(0x0C, 4 + 8 * 64);
        let r = maintenance_in(&req(&cdb));
        assert!(r.sense.is_none());
        let body_len =
            u32::from_be_bytes([r.data_in[0], r.data_in[1], r.data_in[2], r.data_in[3]]) as usize;
        assert_eq!(body_len, SUPPORTED_OPCODES.len() * 8);
        // Every advertised opcode must appear in the entry stream.
        let entries = &r.data_in[4..];
        for &op in SUPPORTED_OPCODES {
            let found = entries.chunks(8).any(|e| e[0] == op);
            assert!(found, "opcode {:#04x} missing", op);
        }
    }

    #[test]
    fn report_supported_opcodes_includes_caw_unmap_and_writesame() {
        // VAAI / Linux / blkdiscard probe these specifically.
        let cdb = build_cdb(0x0C, 4 + 8 * 64);
        let r = maintenance_in(&req(&cdb));
        let entries = &r.data_in[4..];
        let opcodes: Vec<u8> = entries.chunks(8).map(|e| e[0]).collect();
        assert!(opcodes.contains(&0x89), "COMPARE AND WRITE absent");
        assert!(opcodes.contains(&0x42), "UNMAP absent");
        assert!(opcodes.contains(&0x41), "WRITE SAME 10 absent");
        assert!(opcodes.contains(&0x93), "WRITE SAME 16 absent");
    }

    #[test]
    fn report_supported_tmf_advertises_standard_set() {
        let cdb = build_cdb(0x0D, 4);
        let r = maintenance_in(&req(&cdb));
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 4);
        // ATS | ATSS | CTSS | LURS in byte 0
        assert_eq!(r.data_in[0], 0xD8);
        // ITNRS in byte 1
        assert_eq!(r.data_in[1], 0x80);
    }

    #[test]
    fn unknown_service_action_rejected() {
        let cdb = build_cdb(0x05, 256); // REPORT IDENTIFYING INFO — not wired
        let r = maintenance_in(&req(&cdb));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[test]
    fn alloc_length_truncates_response() {
        let cdb = build_cdb(0x0D, 2);
        let r = maintenance_in(&req(&cdb));
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 2);
    }
}
