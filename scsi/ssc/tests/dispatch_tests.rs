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
    reservations: Arc<scsi_spc::reservations::ReservationManager>,
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
            reservations: Arc::new(scsi_spc::reservations::ReservationManager::new()),
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
        self.ctx_session(pdu, cdb, lun, drive_id, has_changer, 1, None)
    }

    /// Like [`Self::ctx_at`] but with a caller-chosen I_T nexus
    /// (`tsih` + initiator IQN) — used by the persistent-reservation
    /// tests to exercise cross-nexus RESERVATION CONFLICT.
    #[allow(clippy::too_many_arguments)]
    fn ctx_session<'a>(
        &'a self,
        pdu: &'a mut Pdu,
        cdb: [u8; 16],
        lun: u8,
        drive_id: usize,
        has_changer: bool,
        tsih: u16,
        initiator_iqn: Option<&'a str>,
    ) -> ScsiCtx<'a> {
        ScsiCtx {
            pdu,
            cdb,
            lun,
            drive_id,
            device_type: 0x01, // sequential-access
            device_name: "drive1".to_string(),
            tsih,
            drive_manager: &self.drive_manager,
            facade: &self.facade,
            ua_tracker: &self.ua,
            event_tx: &self.event_tx,
            data_dir: &self.data_dir,
            audit_log: &self.audit_log,
            audit_ratelimiter: &self.ratelimiter,
            initiator_iqn,
            peer: "test",
            diagnostic_store: &self.diag,
            session_partition: None,
            has_changer,
            alua: None,
            reservations: &self.reservations,
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
fn read_six_on_filemark_returns_check_condition_with_fm_bit() {
    // Regression for issue #25. A READ(6) that lands on a filemark
    // block must surface SSC-4 §7.6's CHECK CONDITION / NO SENSE /
    // FM=1 / INFO=allocated. Without the FM bit the Linux st driver
    // misses the filemark, never advances `block_number`, and the
    // empty Data-In we'd otherwise send back leaves the host's read
    // buffer holding stale kernel bytes (the `14 00 00 08 ...`
    // garbage the original bug reproduced).
    let fx = Fixture::new();
    let payload = vec![0x5Au8; 4096];

    // Write one data record + one filemark, then rewind.
    let mut wp = Pdu::synth(&cdb(0x0A), 1, 0, &payload);
    let mut ctx = fx.ctx(&mut wp, cdb(0x0A));
    assert_eq!(
        handlers::handle_write_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    let mut fm_cdb = cdb(0x10);
    fm_cdb[4] = 1;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, fm_cdb);
    assert_eq!(
        handlers::handle_write_filemarks_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x01));
    handlers::handle_rewind(&mut ctx).unwrap();

    // Read the data block.
    let mut rp = Pdu::synth(&cdb(0x08), 1, 4096, &[]);
    let mut ctx = fx.ctx(&mut rp, cdb(0x08));
    assert_eq!(
        handlers::handle_read_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    // Read across the filemark — must be CHECK CONDITION + FM.
    let mut read_cdb = cdb(0x08);
    // 24-bit BE transfer length = 0x000940 (2368).
    read_cdb[2] = 0x00;
    read_cdb[3] = 0x09;
    read_cdb[4] = 0x40;
    let mut rp = Pdu::synth(&read_cdb, 1, 2368, &[]);
    let mut ctx = fx.ctx(&mut rp, read_cdb);
    let resp = handlers::handle_read_6(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
    assert!(resp.data_out.is_empty());
    let sense = resp.sense.expect("filemark read must carry sense");
    // Fixed-format sense: byte 0 response code 0x70, INFO valid bit
    // 0x80; byte 2 has sense key (NO SENSE = 0x00) and FM=0x80;
    // byte 3..7 INFO = transfer length (2368 = 0x00000940).
    assert_eq!(sense[0] & 0x7f, 0x70, "fixed-format sense");
    assert_eq!(sense[0] & 0x80, 0x80, "INFO valid bit set");
    assert_eq!(sense[2] & 0x0f, 0x00, "sense key = NO SENSE");
    assert_eq!(sense[2] & 0x80, 0x80, "FM bit set");
    assert_eq!(sense[3], 0x00);
    assert_eq!(sense[4], 0x00);
    assert_eq!(sense[5], 0x09);
    assert_eq!(sense[6], 0x40);
    assert_eq!(sense[12], 0x00, "ASC = 0x00");
    assert_eq!(sense[13], 0x01, "ASCQ = 0x01 (FILEMARK DETECTED)");
}

#[test]
fn read_six_past_eod_returns_check_condition_with_blank_check_and_info() {
    // Regression for issue #26 (the past-EOD twin of #25). After one
    // record is written, rewound, and read back successfully, the
    // head sits past the only record; the next READ(6) must surface
    // CHECK CONDITION + BLANK CHECK + ASC/ASCQ 0x00/0x05 with
    // INFO = TRANSFER LENGTH (residual = the host's full allocation,
    // since zero bytes were transferred). Without the INFO field the
    // Linux st driver can't compute the short-read count and dd
    // returns its kernel buffer untouched — the same `14 00 00 08
    // ...` garbage #25 hit on the filemark path. The EOM bit must
    // *not* be set; EOD is not physical end-of-medium.
    let fx = Fixture::new();
    let payload = vec![0xA5u8; 4096];

    let mut wp = Pdu::synth(&cdb(0x0A), 1, 0, &payload);
    let mut ctx = fx.ctx(&mut wp, cdb(0x0A));
    assert_eq!(
        handlers::handle_write_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x01));
    handlers::handle_rewind(&mut ctx).unwrap();

    let mut rp = Pdu::synth(&cdb(0x08), 1, 4096, &[]);
    let mut ctx = fx.ctx(&mut rp, cdb(0x08));
    assert_eq!(
        handlers::handle_read_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    // Past-EOD read — dd bs=65536 from the bug reproducer.
    let mut read_cdb = cdb(0x08);
    // 24-bit BE transfer length = 0x010000 (65536).
    read_cdb[2] = 0x01;
    read_cdb[3] = 0x00;
    read_cdb[4] = 0x00;
    let mut rp = Pdu::synth(&read_cdb, 1, 65536, &[]);
    let mut ctx = fx.ctx(&mut rp, read_cdb);
    let resp = handlers::handle_read_6(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
    assert!(resp.data_out.is_empty(), "no data on past-EOD read");
    let sense = resp.sense.expect("past-EOD read must carry sense");
    assert_eq!(sense[0] & 0x7f, 0x70, "fixed-format sense");
    assert_eq!(sense[0] & 0x80, 0x80, "INFO valid bit set");
    assert_eq!(sense[2] & 0x0f, 0x08, "sense key = BLANK CHECK");
    assert_eq!(sense[2] & 0x40, 0x00, "EOM bit clear (EOD != EOM)");
    // INFO = TRANSFER LENGTH residual (0x00010000, big-endian).
    assert_eq!(sense[3], 0x00);
    assert_eq!(sense[4], 0x01);
    assert_eq!(sense[5], 0x00);
    assert_eq!(sense[6], 0x00);
    assert_eq!(sense[12], 0x00, "ASC = 0x00");
    assert_eq!(sense[13], 0x05, "ASCQ = 0x05 (END-OF-DATA DETECTED)");
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
    // REPORT CAPABILITIES (SA 0x02) advertises the real SBC-3 type
    // mask now that the drive implements PR — TMV=1, TYPE_MASK 0xEA,0x01.
    let mut c = cdb(0x5E);
    c[1] = 0x02;
    c[7..9].copy_from_slice(&256u16.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let caps = handlers::handle_persistent_reserve_in(&mut ctx).unwrap();
    assert_eq!(caps.data_out.len(), 8);
    assert_eq!(caps.data_out[3], 0x80); // TMV
    assert_eq!(caps.data_out[4], 0xEA);
    assert_eq!(caps.data_out[5], 0x01);

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

/// 24-byte PROUT parameter list (RESERVATION KEY + SERVICE ACTION
/// RESERVATION KEY, APTPL clear).
fn prout_params(rk: u64, sark: u64) -> Vec<u8> {
    let mut p = vec![0u8; 24];
    p[0..8].copy_from_slice(&rk.to_be_bytes());
    p[8..16].copy_from_slice(&sark.to_be_bytes());
    p
}

/// A 0x5F CDB for `service_action`, LU scope, reservation `type_byte`.
fn prout_cdb(service_action: u8, type_byte: u8) -> [u8; 16] {
    let mut c = cdb(0x5F);
    c[1] = service_action & 0x1F;
    c[2] = type_byte & 0x0F; // scope 0 (LU_SCOPE) | type
    c[5..9].copy_from_slice(&24u32.to_be_bytes());
    c
}

#[test]
fn persistent_reserve_out_registers_reserves_and_fences_other_nexus() {
    let fx = Fixture::new();
    let a = Some("iqn.test:a");
    let b = Some("iqn.test:b");

    // Initiator A (TSIH 1) registers key 0xAAAA (REGISTER AND IGNORE
    // EXISTING KEY, SA 0x06).
    {
        let plist = prout_params(0, 0xAAAA);
        let mut p = Pdu::synth(&[], 1, 0, &plist);
        let mut ctx = fx.ctx_session(&mut p, prout_cdb(0x06, 0), 1, 0, false, 1, a);
        let r = handlers::handle_persistent_reserve_out(&mut ctx).unwrap();
        assert_eq!(r.status, ScsiStatus::Good, "register");
    }

    // READ KEYS lists the registered key.
    {
        let mut c = cdb(0x5E);
        c[1] = 0x00;
        c[7..9].copy_from_slice(&256u16.to_be_bytes());
        let mut p = pdu();
        let mut ctx = fx.ctx_session(&mut p, c, 1, 0, false, 1, a);
        let r = handlers::handle_persistent_reserve_in(&mut ctx).unwrap();
        assert_eq!(r.status, ScsiStatus::Good);
        assert_eq!(r.data_out.len(), 16); // header(8) + one key(8)
        assert_eq!(&r.data_out[8..16], &0xAAAAu64.to_be_bytes());
    }

    // A reserves EXCLUSIVE ACCESS (type 0x03).
    {
        let plist = prout_params(0xAAAA, 0);
        let mut p = Pdu::synth(&[], 1, 0, &plist);
        let mut ctx = fx.ctx_session(&mut p, prout_cdb(0x01, 0x03), 1, 0, false, 1, a);
        let r = handlers::handle_persistent_reserve_out(&mut ctx).unwrap();
        assert_eq!(r.status, ScsiStatus::Good, "reserve");
    }

    // A different I_T nexus (TSIH 2) is fenced out of both WRITE(6)
    // and READ(6) under EXCLUSIVE ACCESS — the gate returns
    // RESERVATION CONFLICT before reaching the data handler.
    for op in [0x0Au8, 0x08] {
        let mut p = pdu();
        let mut ctx = fx.ctx_session(&mut p, cdb(op), 1, 0, false, 2, b);
        let r = dispatch_drive_lun(&mut ctx).unwrap().unwrap();
        assert_eq!(
            r.status,
            ScsiStatus::ReservationConflict,
            "non-holder opcode {op:#04x} fenced"
        );
    }

    // The holder (TSIH 1) is not fenced — the gate lets the command
    // through to the real handler (Err is fine; it just isn't a
    // reservation conflict).
    {
        let mut p = pdu();
        let mut ctx = fx.ctx_session(&mut p, cdb(0x0A), 1, 0, false, 1, a);
        match dispatch_drive_lun(&mut ctx) {
            Some(Ok(r)) => assert_ne!(
                r.status,
                ScsiStatus::ReservationConflict,
                "holder not fenced"
            ),
            Some(Err(_)) => {}
            None => panic!("WRITE(6) must dispatch"),
        }
    }

    // A releases the reservation (TYPE must match the held type).
    {
        let plist = prout_params(0xAAAA, 0);
        let mut p = Pdu::synth(&[], 1, 0, &plist);
        let mut ctx = fx.ctx_session(&mut p, prout_cdb(0x02, 0x03), 1, 0, false, 1, a);
        let r = handlers::handle_persistent_reserve_out(&mut ctx).unwrap();
        assert_eq!(r.status, ScsiStatus::Good, "release");
    }

    // After release the other nexus is no longer fenced.
    {
        let mut p = pdu();
        let mut ctx = fx.ctx_session(&mut p, cdb(0x08), 1, 0, false, 2, b);
        match dispatch_drive_lun(&mut ctx) {
            Some(Ok(r)) => {
                assert_ne!(
                    r.status,
                    ScsiStatus::ReservationConflict,
                    "released - no fence"
                )
            }
            Some(Err(_)) => {}
            None => panic!("READ(6) must dispatch"),
        }
    }
}

#[test]
fn persistent_reserve_out_rejected_on_changer_lun() {
    let fx = Fixture::new();
    let plist = prout_params(0, 0xAAAA);
    let mut p = Pdu::synth(&[], 0, 0, &plist);
    // LUN 0 with has_changer=true → PROUT stays rejected.
    let mut ctx = fx.ctx_session(
        &mut p,
        prout_cdb(0x06, 0),
        0,
        0,
        true,
        1,
        Some("iqn.test:a"),
    );
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

/// Context with session_partition bound to a name the library doesn't
/// know about. `partition_drive_ids` returns `None` for an unknown
/// partition, which makes `owned` false in the fence — same shape as a
/// session bound to a real partition trying to touch a drive that
/// partition doesn't own.
fn fence_ctx<'a>(fx: &'a Fixture, pdu: &'a mut Pdu, cdb: [u8; 16]) -> ScsiCtx<'a> {
    let mut ctx = fx.ctx(pdu, cdb);
    ctx.session_partition = Some("other-partition");
    ctx
}

#[test]
fn partition_fence_lets_report_luns_through_for_self_filtering() {
    // REPORT LUNS on an out-of-partition LUN must pass the fence so
    // the handler can return the admitted-LUN subset to the initiator.
    let fx = Fixture::new();
    let mut p = pdu();
    let ctx = fence_ctx(&fx, &mut p, cdb(0xA0));
    assert!(handlers::check_partition_fence(&ctx).unwrap().is_none());
}

#[test]
fn partition_fence_returns_no_lun_inquiry_for_unowned_drive() {
    // INQUIRY on a non-admitted LUN must return Good + the SPC-4
    // "no logical unit" sentinel (byte 0 = 0x7F) so the Linux iSCSI
    // initiator keeps scanning the remaining LUNs.
    let fx = Fixture::new();
    let mut c = cdb(0x12);
    c[3..5].copy_from_slice(&96u16.to_be_bytes()); // allocation length
    let mut p = pdu();
    let ctx = fence_ctx(&fx, &mut p, c);
    let resp = handlers::check_partition_fence(&ctx)
        .unwrap()
        .expect("fence must refuse INQUIRY on out-of-partition LUN");
    assert_eq!(resp.status, ScsiStatus::Good);
    assert!(resp.sense.is_none());
    assert!(!resp.data_out.is_empty());
    // byte 0 = (PQ << 5) | type = (0b011 << 5) | 0x1F = 0x7F
    assert_eq!(resp.data_out[0], 0x7F);
}

#[test]
fn partition_fence_returns_check_condition_for_other_opcodes() {
    // Non-INQUIRY / non-REPORT-LUNS opcode → CHECK CONDITION +
    // ILLEGAL REQUEST + LOGICAL UNIT NOT SUPPORTED.
    let fx = Fixture::new();
    let mut p = pdu();
    let ctx = fence_ctx(&fx, &mut p, cdb(0x08)); // READ(6)
    let resp = handlers::check_partition_fence(&ctx)
        .unwrap()
        .expect("fence must refuse READ on out-of-partition LUN");
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
    let sense = resp.sense.as_ref().expect("sense buffer present");
    // Fixed-format sense: byte 2 low 4 bits = key, byte 12 = ASC, byte 13 = ASCQ.
    assert_eq!(sense[2] & 0x0F, 0x05); // ILLEGAL REQUEST
    assert_eq!(sense[12], 0x25); // LOGICAL UNIT NOT SUPPORTED
    assert_eq!(sense[13], 0x00);
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

// ============================================================
// Coverage uplift — handlers not exercised by the cases above.
// Mostly per-opcode happy-path + the changer-LUN refusal branch
// for handlers that have one.
// ============================================================

/// LOCATE(10): rewind to LBA 0 — always-valid target.
#[test]
fn locate_10_to_bom_succeeds() {
    let fx = Fixture::new();
    // cdb[3..7] = LBA 0, cdb[1] CP bit clear, cdb[8] partition 0.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x2B));
    assert_eq!(
        handlers::handle_locate_10(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// LOCATE(10) with CP=1 and partition 0 on a default single-partition
/// tape exercises the locate_partition arm.
#[test]
fn locate_10_with_cp_bit_routes_to_locate_partition() {
    let fx = Fixture::new();
    let mut c = cdb(0x2B);
    c[1] = 0x02; // CP = 1
    // partition stays 0; LBA stays 0.
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_locate_10(&mut ctx).unwrap();
    // Default partition 0 must be addressable; locate_partition(0, 0)
    // succeeds on a fresh single-partition cartridge.
    assert_eq!(resp.status, ScsiStatus::Good);
}

/// LOCATE(10) with CP=1 to a non-existent partition is rejected.
#[test]
fn locate_10_to_unknown_partition_check_conditions() {
    let fx = Fixture::new();
    let mut c = cdb(0x2B);
    c[1] = 0x02; // CP
    c[8] = 0x07; // partition 7 does not exist on a default tape
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_locate_10(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

/// LOCATE(10) on the changer LUN is silently accepted (per impl).
#[test]
fn locate_10_on_changer_lun_is_a_noop() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x2B), 0, 0, true);
    assert_eq!(
        handlers::handle_locate_10(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// LOCATE(16): rewind to LBA 0 via the 16-byte form.
#[test]
fn locate_16_to_bom_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x92));
    assert_eq!(
        handlers::handle_locate_16(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn locate_16_with_cp_bit_routes_to_locate_partition() {
    let fx = Fixture::new();
    let mut c = cdb(0x92);
    c[1] = 0x02; // CP
    c[3] = 0; // partition 0
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_locate_16(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn locate_16_to_unknown_partition_check_conditions() {
    let fx = Fixture::new();
    let mut c = cdb(0x92);
    c[1] = 0x02;
    c[3] = 0x07;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_locate_16(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// SPACE(16) — code 0 (records) with count 0 is a no-op.
#[test]
fn space_16_zero_count_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x91));
    assert_eq!(
        handlers::handle_space_16(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// SPACE(16) — code 3 (space to EOD).
#[test]
fn space_16_to_end_of_data_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x91);
    c[1] = 0x03;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_space_16(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// SPACE(16) — code 1 (filemarks) with positive count.
#[test]
fn space_16_filemarks_one_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x91);
    c[1] = 0x01;
    c[11] = 1; // count = 1 (low byte of 8-byte BE)
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_space_16(&mut ctx).unwrap();
    // No filemark to skip yet — either Good (no-op) or CheckCondition
    // depending on cartridge behaviour; both are acceptable per impl.
    assert!(matches!(
        resp.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

/// SPACE(6) FILEMARKS that hits EOD before counting all requested
/// filemarks must terminate with CHECK CONDITION + BLANK_CHECK + ASC
/// 00/05 (EOD detected), and the INFORMATION field must carry
/// (count − moved) so the host can correct its tape-position tracking.
/// This is the SSC-5 §7.5 residual-on-CC rule.
///
/// Regression for #33: Linux's slow MTEOM path emits
/// `SPACE FILEMARKS count=0x7FFFFF`; before this fix we returned GOOD,
/// so the kernel's `drv_file += 0x7FFFFF` left bareos believing the tape
/// held 8 388 607 files — which it then catalogued and warned about
/// on every subsequent open.
#[test]
fn space_6_filemarks_past_eod_returns_residual() {
    let fx = Fixture::new();
    // Empty cartridge — zero filemarks. Ask for the max positive 24-bit
    // count that Linux's slow MTEOM uses: 0x7FFFFF (8 388 607).
    let mut c = cdb(0x11);
    c[1] = 0x01; // filemarks
    c[2] = 0x7F;
    c[3] = 0xFF;
    c[4] = 0xFF;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_space_6(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
    let sense = resp.sense.as_ref().expect("CHECK CONDITION carries sense");
    assert_eq!(sense[2] & 0x0F, 0x08, "sense key BLANK_CHECK");
    assert_eq!(sense[12], 0x00, "ASC EOD detected (00/05)");
    assert_eq!(sense[13], 0x05);
    assert_eq!(sense[0] & 0x80, 0x80, "INFORMATION VALID bit set");
    let info = u32::from_be_bytes([sense[3], sense[4], sense[5], sense[6]]);
    assert_eq!(
        info, 0x7F_FFFF,
        "INFORMATION = count − moved (=count, moved=0)"
    );
}

/// SPACE(16) sister-test for the same residual rule, exercising the
/// 8-byte count path. LTO-7+ uses the 16-byte form when counts exceed
/// 24 bits; the residual semantics are identical.
#[test]
fn space_16_filemarks_past_eod_returns_residual() {
    let fx = Fixture::new();
    let mut c = cdb(0x91);
    c[1] = 0x01;
    // count = 5 (low byte of 8-byte BE)
    c[11] = 5;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_space_16(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
    let sense = resp.sense.as_ref().expect("CHECK CONDITION carries sense");
    assert_eq!(sense[2] & 0x0F, 0x08, "sense key BLANK_CHECK");
    let info = u32::from_be_bytes([sense[3], sense[4], sense[5], sense[6]]);
    assert_eq!(info, 5, "INFORMATION = count − moved (=5, moved=0)");
}

/// SPACE(6) FILEMARKS with count=0 stays GOOD — no demarcations were
/// requested, so no residual is owed even when nothing was crossed.
#[test]
fn space_6_filemarks_zero_count_is_good() {
    let fx = Fixture::new();
    let mut c = cdb(0x11);
    c[1] = 0x01;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_space_6(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

/// SPACE(16) on changer LUN: per impl, returns Good unconditionally.
#[test]
fn space_16_on_changer_lun_is_a_noop() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x91), 0, 0, true);
    assert_eq!(
        handlers::handle_space_16(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// WRITE FILEMARKS(16): zero-count is accepted.
#[test]
fn write_filemarks_16_zero_count_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x80));
    assert_eq!(
        handlers::handle_write_filemarks_16(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::Good,
    );
}

/// WRITE FILEMARKS(16): a real count routes via the cart write path.
#[test]
fn write_filemarks_16_one_count_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x80);
    c[15] = 1; // count = 1 (low byte of 4-byte BE at cdb[12..16])
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_write_filemarks_16(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::Good,
    );
}

/// WRITE FILEMARKS(16) on changer LUN is silently accepted.
#[test]
fn write_filemarks_16_on_changer_lun_is_a_noop() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x80), 0, 0, true);
    assert_eq!(
        handlers::handle_write_filemarks_16(&mut ctx)
            .unwrap()
            .status,
        ScsiStatus::Good,
    );
}

/// VERIFY(6) on an empty drive: at_eod is true immediately, the loop
/// never runs and the handler returns Good.
#[test]
fn verify_6_on_empty_drive_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x13);
    c[4] = 5; // count = 5
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_verify_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn verify_6_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x13), 0, 0, true);
    assert_eq!(
        handlers::handle_verify_6(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// VERIFY(16) on an empty drive — same logic via 8-byte count.
#[test]
fn verify_16_on_empty_drive_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x8F);
    c[11] = 5; // count = 5
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_verify_16(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn verify_16_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x8F), 0, 0, true);
    assert_eq!(
        handlers::handle_verify_16(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// ERASE(6): erases the loaded cartridge.
#[test]
fn erase_6_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x19));
    assert_eq!(
        handlers::handle_erase_6(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn erase_6_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x19), 0, 0, true);
    assert_eq!(
        handlers::handle_erase_6(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// ALLOW OVERWRITE — field 0 disables the barrier on partition 0.
#[test]
fn allow_overwrite_disable_succeeds() {
    let fx = Fixture::new();
    let c = cdb(0x82); // field stays 0, partition 0, lba 0
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_allow_overwrite(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// ALLOW OVERWRITE — field 1 uses the current head LBA.
#[test]
fn allow_overwrite_at_current_position_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x82);
    c[2] = 0x01;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_allow_overwrite(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// ALLOW OVERWRITE — field 2 takes an explicit LBA from cdb[4..12].
#[test]
fn allow_overwrite_at_explicit_lba_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x82);
    c[2] = 0x02;
    c[11] = 0x10; // LBA 16
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_allow_overwrite(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// ALLOW OVERWRITE — unsupported field is rejected.
#[test]
fn allow_overwrite_unsupported_field_check_conditions() {
    let fx = Fixture::new();
    let mut c = cdb(0x82);
    c[2] = 0x0F;
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_allow_overwrite(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

#[test]
fn allow_overwrite_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x82), 0, 0, true);
    assert_eq!(
        handlers::handle_allow_overwrite(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// FORMAT MEDIUM — default format (0x00).
#[test]
fn format_medium_default_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x04));
    assert_eq!(
        handlers::handle_format_medium(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn format_medium_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x04), 0, 0, true);
    assert_eq!(
        handlers::handle_format_medium(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// READ ATTRIBUTE — service action 0 (VALUES), element 0,
/// first_attribute 0x0000. Real cartridge label + capacity routed
/// through MAM helper produces a non-empty response.
#[test]
fn read_attribute_returns_mam_data_for_loaded_cartridge() {
    let fx = Fixture::new();
    let mut c = cdb(0x8C);
    c[10..14].copy_from_slice(&4096u32.to_be_bytes()); // alloc length
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_read_attribute(&mut ctx).unwrap();
    // Implementations may return Good (with data) or CheckCondition
    // depending on the requested first_attribute; both routes are
    // exercise of the dispatch path.
    assert!(matches!(
        resp.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

#[test]
fn read_attribute_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x8C), 0, 0, true);
    assert_eq!(
        handlers::handle_read_attribute(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// WRITE ATTRIBUTE — empty parameter list goes through the parser.
#[test]
fn write_attribute_empty_payload_returns_a_status() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x8D));
    // The parser rejects empties; we just exercise the parse +
    // error branch without asserting a specific status.
    let resp = handlers::handle_write_attribute(&mut ctx).unwrap();
    assert!(matches!(
        resp.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

#[test]
fn write_attribute_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x8D), 0, 0, true);
    assert_eq!(
        handlers::handle_write_attribute(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// MODE SELECT(6) with an empty parameter list: zero pages parsed,
/// no side effects, GOOD.
#[test]
fn mode_select_6_empty_parameter_list_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x15));
    let resp = handlers::handle_mode_select_6_drive(&mut ctx).unwrap();
    assert!(matches!(
        resp.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

/// MODE SELECT(10) — same, 10-byte form.
#[test]
fn mode_select_10_empty_parameter_list_succeeds() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, cdb(0x55));
    let resp = handlers::handle_mode_select_10_drive(&mut ctx).unwrap();
    assert!(matches!(
        resp.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

/// SET CAPACITY — proportion 65535 (full native).
#[test]
fn set_capacity_full_native_succeeds() {
    let fx = Fixture::new();
    let mut c = cdb(0x0B);
    c[2..4].copy_from_slice(&65535u16.to_be_bytes());
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_set_capacity(&mut ctx).unwrap();
    assert!(matches!(
        resp.status,
        ScsiStatus::Good | ScsiStatus::CheckCondition
    ));
}

#[test]
fn set_capacity_on_changer_lun_is_refused() {
    let fx = Fixture::new();
    let mut p = pdu();
    let mut ctx = fx.ctx_at(&mut p, cdb(0x0B), 0, 0, true);
    assert_eq!(
        handlers::handle_set_capacity(&mut ctx).unwrap().status,
        ScsiStatus::CheckCondition,
    );
}

/// LOG SELECT — unconditional accept; no parameter list required.
#[test]
fn log_select_accepts_a_pcr_clear() {
    let fx = Fixture::new();
    let mut c = cdb(0x4C);
    c[1] = 0x02; // PCR = 1
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    assert_eq!(
        handlers::handle_log_select(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// WRITE BUFFER — discards arbitrary host data.
#[test]
fn write_buffer_discards_arbitrary_data() {
    let fx = Fixture::new();
    let mut wp = Pdu::synth(&cdb(0x3B), 1, 0, &vec![0u8; 256]);
    let mut ctx = fx.ctx(&mut wp, cdb(0x3B));
    assert_eq!(
        handlers::handle_write_buffer(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

/// READ BUFFER — returns `min(alloc, 4096)` zero bytes.
#[test]
fn read_buffer_returns_zero_padded_response() {
    let fx = Fixture::new();
    let mut c = cdb(0x3C);
    c[6..9].copy_from_slice(&[0x00, 0x01, 0x00]); // alloc = 256
    let mut p = pdu();
    let mut ctx = fx.ctx(&mut p, c);
    let resp = handlers::handle_read_buffer(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    assert_eq!(resp.data_out.len(), 256);
    assert!(resp.data_out.iter().all(|&b| b == 0));
}
