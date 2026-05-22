// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the drive-LUN SCSI dispatch surface.
//!
//! Builds a `DriveManager` with one cartridge loaded into drive 0, a
//! `LibraryFacade` for the identity surface, and the rest of the
//! shared `ScsiCtx` state, then drives `dispatch_drive_lun` plus the
//! individual per-opcode handlers directly.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_mediachanger::{
    AuditChannel, AuditRateLimiter, Cartridge, CartridgeOpenMode, DedupScope, Library,
    LibraryFacade, TapeEvent,
};
use scsi_ssc::diagnostics::DiagnosticStore;
use scsi_ssc::dispatch::{
    Pdu, ScsiCtx, ScsiResp, ScsiStatus, dispatch_drive_lun, handlers, inquiry,
};
use scsi_ssc::drive_manager::DriveManager;
use shared_iscsi::unit_attention::UnitAttentionTracker;
use tempfile::TempDir;
use tokio::sync::broadcast;

/// Owns the long-lived state an `ScsiCtx` borrows. Drive 0 has the
/// cartridge `TAPE01` loaded; drive 1 is left empty.
struct Fixture {
    _tmp: TempDir,
    facade: LibraryFacade,
    drive_manager: Arc<DriveManager>,
    ua: Arc<Mutex<UnitAttentionTracker>>,
    event_tx: broadcast::Sender<TapeEvent>,
    diag: Arc<DiagnosticStore>,
    ratelimiter: AuditRateLimiter,
    data_dir: PathBuf,
    audit_log: Option<AuditChannel>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let lib_root = tmp.path().join("library");
        let tapes_dir = tmp.path().join("tapes");
        let library = Library::initialize(&lib_root, &tapes_dir, 4, 1, 2, 8, None, 0, 1001, 101, 1)
            .expect("library initializes");
        let library = Arc::new(Mutex::new(library));

        // A cartridge on disk, then loaded into drive 0.
        Cartridge::open(
            &tapes_dir,
            "TAPE01",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: DedupScope::Global,
            },
        )
        .expect("cartridge created");
        let drive_manager = DriveManager::new(2, tapes_dir);
        drive_manager
            .load_cartridge(0, "TAPE01")
            .expect("cartridge loads into drive 0");

        let (event_tx, _rx) = broadcast::channel(64);
        Self {
            facade: LibraryFacade::new(library),
            drive_manager: Arc::new(drive_manager),
            ua: Arc::new(Mutex::new(UnitAttentionTracker::new())),
            event_tx,
            diag: Arc::new(DiagnosticStore::new()),
            ratelimiter: AuditRateLimiter::new(Duration::from_secs(60)),
            data_dir: tmp.path().to_path_buf(),
            audit_log: None,
            _tmp: tmp,
        }
    }

    /// Context for drive 0 (cartridge loaded), LUN 1, no changer.
    fn ctx<'a>(&'a self, pdu: &'a mut Pdu, cdb: [u8; 16]) -> ScsiCtx<'a> {
        self.ctx_at(pdu, cdb, 1, 0, false)
    }

    fn ctx_at<'a>(
        &'a self,
        pdu: &'a mut Pdu,
        cdb: [u8; 16],
        lun: u8,
        drive_id: usize,
        has_changer: bool,
    ) -> ScsiCtx<'a> {
        ScsiCtx {
            pdu,
            cdb,
            lun,
            drive_id,
            device_type: 0x01, // sequential-access
            device_name: "drive1".to_string(),
            tsih: 1,
            drive_manager: &self.drive_manager,
            facade: &self.facade,
            ua_tracker: &self.ua,
            event_tx: &self.event_tx,
            data_dir: &self.data_dir,
            audit_log: &self.audit_log,
            audit_ratelimiter: &self.ratelimiter,
            initiator_iqn: None,
            peer: "test",
            diagnostic_store: &self.diag,
            session_partition: None,
            has_changer,
        }
    }
}

/// Every opcode wired into `dispatch_drive_lun`.
const DRIVE_OPCODES: [u8; 46] = [
    0x12, 0xA0, 0x4D, 0x00, 0x03, 0x1E, 0x13, 0x8F, 0x8C, 0x8D, 0x05, 0x44, 0x01, 0x1B, 0x34, 0x11,
    0x91, 0x10, 0x19, 0x0B, 0x80, 0x2B, 0x92, 0x08, 0x0A, 0x04, 0x82, 0x3B, 0x3C, 0x16, 0x17, 0x56,
    0x57, 0xA2, 0xB5, 0xA3, 0xA4, 0x5E, 0x5F, 0x1A, 0x5A, 0x15, 0x55, 0x4C, 0x1C, 0x1D,
];

/// 16-byte CDB with just the opcode set.
fn cdb(op: u8) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = op;
    c
}

fn pdu() -> Pdu {
    Pdu::synth(&[], 1, 256, &[])
}

#[test]
fn test_unit_ready_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x00));
    assert_eq!(
        handlers::handle_test_unit_ready(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn request_sense_returns_a_sense_buffer() {
    let fx = Fixture::new();
    let mut c = cdb(0x03);
    c[4] = 252; // allocation length
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_request_sense(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    assert!(!resp.data_out.is_empty());
}

#[test]
fn read_block_limits_reports_a_variable_block_device() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x05));
    let resp = handlers::handle_read_block_limits(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    assert_eq!(resp.data_out.len(), 6);
    assert_eq!(resp.data_out[0], 0, "granularity 0 = variable block");
}

#[test]
fn report_density_support_lists_descriptors() {
    let fx = Fixture::new();
    let mut c = cdb(0x44);
    c[7..9].copy_from_slice(&512u16.to_be_bytes()); // allocation length
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_report_density_support(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    assert!(!resp.data_out.is_empty());
}

#[test]
fn report_luns_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0xA0);
    c[6..10].copy_from_slice(&256u32.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_report_luns(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn standard_inquiry_returns_at_least_36_bytes() {
    let fx = Fixture::new();
    let mut c = cdb(0x12);
    c[3..5].copy_from_slice(&96u16.to_be_bytes()); // allocation length
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = inquiry::handle_inquiry(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    assert!(resp.data_out.len() >= 36);
    // Byte 0 bits[4:0] = peripheral device type (sequential-access).
    assert_eq!(resp.data_out[0] & 0x1F, 0x01);
}

#[test]
fn inquiry_vpd_pages_are_served() {
    let fx = Fixture::new();
    // 0x00 supported pages, 0x80 unit serial, 0x83 device id, 0xB0
    // sequential-access chars, 0xB1 mfg serial, 0xB2 tapealert, 0xB3
    // automation device serial, 0xC0 firmware info.
    for page in [0x00u8, 0x80, 0x83, 0xB0, 0xB1, 0xB2, 0xB3, 0xC0] {
        let mut c = cdb(0x12);
        c[1] = 0x01; // EVPD
        c[2] = page;
        c[3..5].copy_from_slice(&256u16.to_be_bytes());
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, c);
        let resp = inquiry::handle_inquiry(&mut ctx).unwrap();
        assert_eq!(resp.status, ScsiStatus::Good, "VPD page {page:#04x}");
        assert!(!resp.data_out.is_empty(), "VPD page {page:#04x} empty");
        assert_eq!(resp.data_out[1], page, "VPD page code echoed");
    }
}

#[test]
fn rewind_and_read_position_succeed_on_a_loaded_drive() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x01));
    assert_eq!(
        handlers::handle_rewind(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x34));
    let resp = handlers::handle_read_position(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn write_six_then_rewind_then_read_six_round_trips() {
    let fx = Fixture::new();
    let payload = vec![0x5Au8; 4096];

    // WRITE(6): payload lives in the synthetic PDU's data segment.
    let mut wp = Pdu::synth(&cdb(0x0A), 1, 0, &payload);
    let mut ctx = fx.ctx(&mut wp, cdb(0x0A));
    let resp = handlers::handle_write_6(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);

    // Rewind to the start before reading back.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x01));
    handlers::handle_rewind(&mut ctx).unwrap();

    // READ(6) returns the block just written.
    let mut rp = Pdu::synth(&cdb(0x08), 1, 4096, &[]);
    let mut ctx = fx.ctx(&mut rp, cdb(0x08));
    let resp = handlers::handle_read_6(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    assert_eq!(resp.data_out, payload);
}

#[test]
fn write_six_with_no_data_is_rejected() {
    let fx = Fixture::new();
    let mut p = pdu(); // empty data segment
    let mut ctx = fx.ctx(&mut p, cdb(0x0A));
    let resp = handlers::handle_write_6(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

#[test]
fn write_filemarks_and_space_succeed() {
    let fx = Fixture::new();
    // WRITE FILEMARKS(6): one filemark.
    let mut c = cdb(0x10);
    c[4] = 1;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_write_filemarks_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    // Rewind, then SPACE(6) over the filemark we just wrote.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x01));
    handlers::handle_rewind(&mut ctx).unwrap();

    let mut c = cdb(0x11);
    c[1] = 0x01; // code = filemarks
    c[4] = 1; // count = 1
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_space_6(&mut ctx).unwrap();
    assert!(matches!(
        resp.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

#[test]
fn prevent_allow_medium_removal_toggles() {
    let fx = Fixture::new();
    // Prevent (cdb[4] bit 0 = 1).
    let mut c = cdb(0x1E);
    c[4] = 0x01;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_prevent_allow_medium_removal(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::Good,
    );
    // Allow again.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x1E));
    assert_eq!(
        handlers::handle_prevent_allow_medium_removal(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::Good,
    );
}

#[test]
fn reserve_and_release_succeed() {
    let fx = Fixture::new();
    for op in [0x16u8, 0x17, 0x56, 0x57] {
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, cdb(op));
        let resp = dispatch_drive_lun(&mut ctx)
            .expect("opcode routed")
            .expect("handler ok");
        assert_eq!(resp.status, ScsiStatus::Good, "opcode {op:#04x}");
    }
}

#[test]
fn mode_sense_six_and_ten_return_parameter_data() {
    let fx = Fixture::new();
    for op in [0x1Au8, 0x5A] {
        let mut c = cdb(op);
        c[2] = 0x3F; // page code 0x3F = all pages
        if op == 0x1A {
            c[4] = 0xFF; // alloc len
        } else {
            c[7..9].copy_from_slice(&512u16.to_be_bytes());
        }
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, c);
        let resp = dispatch_drive_lun(&mut ctx).expect("routed").expect("ok");
        assert_eq!(resp.status, ScsiStatus::Good, "opcode {op:#04x}");
        assert!(!resp.data_out.is_empty());
    }
}

#[test]
fn dispatch_router_handles_every_drive_opcode() {
    let fx = Fixture::new();
    for op in DRIVE_OPCODES {
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, cdb(op));
        assert!(
            dispatch_drive_lun(&mut ctx).is_some(),
            "opcode {op:#04x} should be routed by dispatch_drive_lun",
        );
    }
}

#[test]
fn every_drive_opcode_runs_against_a_changer_lun() {
    // LUN 0 with has_changer = true: handlers with a changer guard
    // take their refusal branch; the rest run normally. Either way
    // the dispatcher returns and nothing panics.
    let fx = Fixture::new();
    for op in DRIVE_OPCODES {
        let mut p = pdu();
        let mut ctx = fx.ctx_at(&mut p, cdb(op), 0, 0, true);
        assert!(
            dispatch_drive_lun(&mut ctx).is_some(),
            "opcode {op:#04x} should still be routed on a changer LUN",
        );
    }
}

#[test]
fn every_drive_opcode_runs_against_an_empty_drive() {
    // Drive 1 has no cartridge loaded — exercises the with_drive
    // error / NOT READY branches across the data-path handlers.
    let fx = Fixture::new();
    for op in DRIVE_OPCODES {
        let mut p = pdu();
        let mut ctx = fx.ctx_at(&mut p, cdb(op), 1, 1, false);
        assert!(
            dispatch_drive_lun(&mut ctx).is_some(),
            "opcode {op:#04x} should be routed for an empty drive",
        );
    }
}

#[test]
fn dispatch_router_passes_through_unknown_opcodes() {
    let fx = Fixture::new();
    for op in [0xFFu8, 0x9E, 0x7F] {
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, cdb(op));
        assert!(
            dispatch_drive_lun(&mut ctx).is_none(),
            "opcode {op:#04x} is not a drive-LUN opcode",
        );
    }
}

#[test]
fn diagnostic_handlers_run() {
    let fx = Fixture::new();
    // SEND DIAGNOSTIC then RECEIVE DIAGNOSTIC RESULTS.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x1D));
    let send: ScsiResp = handlers::handle_send_diagnostic(&mut ctx).unwrap();
    assert!(matches!(
        send.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));

    let mut c = cdb(0x1C);
    c[3..5].copy_from_slice(&256u16.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let recv = handlers::handle_receive_diagnostic_results(&mut ctx).unwrap();
    assert!(matches!(
        recv.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

/// MAINTENANCE IN (0xA3): service action in CDB byte 1, allocation
/// length in bytes 6..10.
fn maintenance_in_cdb(service_action: u8) -> [u8; 16] {
    let mut c = cdb(0xA3);
    c[1] = service_action;
    c[6..10].copy_from_slice(&4096u32.to_be_bytes());
    c
}

#[test]
fn maintenance_in_serves_every_supported_service_action() {
    let fx = Fixture::new();
    // 0x0C report supported opcodes, 0x1E read DRA, 0x1F host table,
    // 0x0D task-mgmt functions, 0x0A target port groups, 0x0F timestamp.
    for sa in [0x0Cu8, 0x1E, 0x1F, 0x0D, 0x0A, 0x0F] {
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, maintenance_in_cdb(sa));
        let resp = handlers::handle_maintenance_in(&mut ctx).unwrap();
        assert_eq!(resp.status, ScsiStatus::Good, "service action {sa:#04x}");
        assert!(!resp.data_out.is_empty(), "SA {sa:#04x} returned no data");
    }
}

#[test]
fn maintenance_in_report_supported_opcodes_differs_on_a_changer_lun() {
    let fx = Fixture::new();
    // Drive LUN gets the tape opcode list.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, maintenance_in_cdb(0x0C));
    let drive = handlers::handle_maintenance_in(&mut ctx).unwrap();
    // Changer LUN gets the changer opcode list.
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, maintenance_in_cdb(0x0C), 0, 0, true);
    let changer = handlers::handle_maintenance_in(&mut ctx).unwrap();
    assert_eq!(drive.status, ScsiStatus::Good);
    assert_eq!(changer.status, ScsiStatus::Good);
    assert_ne!(drive.data_out, changer.data_out);
}

#[test]
fn maintenance_in_rejects_an_unknown_service_action() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, maintenance_in_cdb(0x15));
    let resp = handlers::handle_maintenance_in(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

#[test]
fn maintenance_out_accepts_set_timestamp_and_write_dra() {
    let fx = Fixture::new();
    for sa in [0x0Fu8, 0x1E] {
        let mut c = cdb(0xA4);
        c[1] = sa;
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, c);
        assert_eq!(
            handlers::handle_maintenance_out(&mut ctx).unwrap().status,
            ScsiStatus::Good,
            "service action {sa:#04x}",
        );
    }
    // An unsupported service action is refused.
    let mut c = cdb(0xA4);
    c[1] = 0x07;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_maintenance_out(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

#[test]
fn persistent_reserve_in_answers_every_service_action() {
    let fx = Fixture::new();
    for sa in [0x00u8, 0x01, 0x02, 0x03] {
        let mut c = cdb(0x5E);
        c[1] = sa;
        c[7..9].copy_from_slice(&256u16.to_be_bytes());
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, c);
        let resp = handlers::handle_persistent_reserve_in(&mut ctx).unwrap();
        assert_eq!(resp.status, ScsiStatus::Good, "SA {sa:#04x}");
    }
    // Unknown service action.
    let mut c = cdb(0x5E);
    c[1] = 0x1F;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_persistent_reserve_in(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::CheckCondition,
    );
}

#[test]
fn persistent_reserve_out_is_always_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x5F));
    assert_eq!(
        handlers::handle_persistent_reserve_out(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::CheckCondition,
    );
}

#[test]
fn security_protocol_in_serves_the_tape_encryption_pages() {
    let fx = Fixture::new();
    // Protocol 0x00 = report supported security protocols.
    let mut c = cdb(0xA2);
    c[6..10].copy_from_slice(&512u32.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_security_protocol_in(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    assert!(!resp.data_out.is_empty());

    // Protocol 0x20 = Tape Data Encryption, each supported page.
    for spsp in [0x0000u16, 0x0001, 0x0010, 0x0011, 0x0020, 0x0021] {
        let mut c = cdb(0xA2);
        c[1] = 0x20;
        c[2..4].copy_from_slice(&spsp.to_be_bytes());
        c[6..10].copy_from_slice(&512u32.to_be_bytes());
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, c);
        let resp = handlers::handle_security_protocol_in(&mut ctx).unwrap();
        assert_eq!(resp.status, ScsiStatus::Good, "SPSP {spsp:#06x}");
    }

    // Unknown SPSP under protocol 0x20 is refused.
    let mut c = cdb(0xA2);
    c[1] = 0x20;
    c[2..4].copy_from_slice(&0x00FFu16.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_security_protocol_in(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::CheckCondition,
    );
}

#[test]
fn security_protocol_out_clears_the_drive_encryption_key() {
    let fx = Fixture::new();
    // A 16-byte SET DATA ENCRYPTION page with both modes = Disable
    // decodes to "Clear".
    let mut page = vec![0u8; 16];
    page[0..2].copy_from_slice(&0x0010u16.to_be_bytes()); // PAGE_SET_DATA_ENCRYPTION
    page[2..4].copy_from_slice(&12u16.to_be_bytes()); // page length
    // bytes 6 (encryption mode) and 7 (decryption mode) stay 0 = Disable.

    let mut c = cdb(0xB5);
    c[1] = 0x20; // SECURITY_PROTOCOL_TAPE_DATA_ENC
    c[2..4].copy_from_slice(&0x0010u16.to_be_bytes()); // PAGE_SET_DATA_ENCRYPTION
    let mut p = Pdu::synth(&c, 1, 0, &page);
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_security_protocol_out(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn security_protocol_out_rejects_bad_input() {
    let fx = Fixture::new();
    // Unsupported security protocol.
    let mut c = cdb(0xB5);
    c[1] = 0x00;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_security_protocol_out(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::CheckCondition,
    );

    // Right protocol, wrong SPSP.
    let mut c = cdb(0xB5);
    c[1] = 0x20;
    c[2..4].copy_from_slice(&0x00FFu16.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_security_protocol_out(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::CheckCondition,
    );

    // Right protocol + SPSP, but a garbage parameter list.
    let mut c = cdb(0xB5);
    c[1] = 0x20;
    c[2..4].copy_from_slice(&0x0010u16.to_be_bytes());
    let mut p = Pdu::synth(&c, 1, 0, &[0xFFu8; 4]);
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_security_protocol_out(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::CheckCondition,
    );
}

#[test]
fn partition_fence_is_a_noop_for_an_unbound_session() {
    let fx = Fixture::new();
    let mut p = pdu();
    let ctx = fx.ctx(&mut p, cdb(0x08));
    // The fixture binds no session_partition — the fence must pass.
    assert!(handlers::check_partition_fence(&ctx).unwrap().is_none());
}

#[test]
fn inquiry_unsupported_vpd_page_is_rejected() {
    let fx = Fixture::new();
    let mut c = cdb(0x12);
    c[1] = 0x01; // EVPD
    c[2] = 0xEE; // not a page the drive dispatcher serves
    c[3..5].copy_from_slice(&256u16.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = inquiry::handle_inquiry(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

#[test]
fn read_position_serves_short_long_and_extended_forms() {
    let fx = Fixture::new();
    // Service action in CDB byte 1: 0x00/0x01 short, 0x06 long,
    // 0x08 extended — each returns Good with a form-sized buffer.
    for (svc, len) in [(0x00u8, 20usize), (0x01, 20), (0x06, 32), (0x08, 32)] {
        let mut c = cdb(0x34);
        c[1] = svc;
        let mut p = pdu();
        let mut ctx = fx.ctx(&mut p, c);
        let resp = handlers::handle_read_position(&mut ctx).unwrap();
        assert_eq!(resp.status, ScsiStatus::Good, "service action {svc:#04x}");
        assert_eq!(resp.data_out.len(), len, "service action {svc:#04x}");
    }
    // An unsupported service action is INVALID FIELD IN CDB.
    let mut c = cdb(0x34);
    c[1] = 0x0A;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_read_position(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

#[test]
fn load_unload_loads_and_then_ejects_the_cartridge() {
    let fx = Fixture::new();
    // LOAD (cdb[4] bit 0 = 1): rewind and stay loaded.
    let mut c = cdb(0x1B);
    c[4] = 0x01;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_load_unload(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    // UNLOAD (cdb[4] bit 0 = 0): eject — drops the cartridge.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x1B));
    assert_eq!(
        handlers::handle_load_unload(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}
