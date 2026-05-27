// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the changer-LUN SCSI dispatch surface.
//!
//! Builds a real `core_mediachanger::Library` plus the shared
//! `ScsiCtx` state every SMC handler threads through, wraps both in
//! an `SmcScsiCtx`, and drives the six SMC opcode handlers + the
//! `dispatch_changer_lun` router directly.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_mediachanger::{AuditChannel, Library, LibraryFacade, TapeEvent};
use scsi_smc::changer::ElementAddressConfig;
use scsi_smc::dispatch::{SmcScsiCtx, dispatch_changer_lun, handlers};
use scsi_ssc::diagnostics::DiagnosticStore;
use scsi_ssc::dispatch::{Pdu, ScsiCtx, ScsiResp, ScsiStatus};
use scsi_ssc::drive_manager::DriveManager;
use shared_audit::AuditRateLimiter;
use shared_iscsi::unit_attention::{UnitAttentionCode, UnitAttentionTracker};
use tempfile::TempDir;
use tokio::sync::broadcast;

// Element-address bases — same convention core-mediachanger's own
// test helpers use (transport 0, storage 1001, mail 101, drive 1).
const STORAGE_BASE: u16 = 1001;
const DRIVE_BASE: u16 = 1;

/// Signature shared by every SMC opcode handler.
type SmcHandler = fn(&mut SmcScsiCtx<'_>) -> anyhow::Result<ScsiResp>;

/// Owns every long-lived value an `SmcScsiCtx` borrows; `ctx()` hands
/// out a context bound to one synthetic PDU per call.
struct Fixture {
    _tmp: TempDir,
    library: Arc<Mutex<Library>>,
    facade: LibraryFacade,
    drive_manager: Arc<DriveManager>,
    ua: Arc<Mutex<UnitAttentionTracker>>,
    event_tx: broadcast::Sender<TapeEvent>,
    diag: Arc<DiagnosticStore>,
    ratelimiter: AuditRateLimiter,
    element_config: ElementAddressConfig,
    data_dir: PathBuf,
    audit_log: Option<AuditChannel>,
}

impl Fixture {
    fn new(slots: u32, mail: u32, drives: u32) -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let lib_root = tmp.path().join("library");
        let tapes_dir = tmp.path().join("tapes");
        let library = Library::initialize(
            &lib_root,
            &tapes_dir,
            slots,
            mail,
            drives,
            8,
            None,
            0, // transport_base
            STORAGE_BASE,
            101, // import_export_base
            DRIVE_BASE,
        )
        .expect("library initializes");
        let library = Arc::new(Mutex::new(library));
        let element_config = ElementAddressConfig::new(
            0,
            STORAGE_BASE,
            slots as u16,
            101,
            mail as u16,
            DRIVE_BASE,
            drives as u16,
        );
        let (event_tx, _rx) = broadcast::channel(64);
        Self {
            facade: LibraryFacade::new(Arc::clone(&library)),
            drive_manager: Arc::new(DriveManager::new(drives as usize, tapes_dir)),
            ua: Arc::new(Mutex::new(UnitAttentionTracker::new())),
            event_tx,
            diag: Arc::new(DiagnosticStore::new()),
            ratelimiter: AuditRateLimiter::new(Duration::from_secs(60)),
            element_config,
            data_dir: tmp.path().to_path_buf(),
            audit_log: None,
            library,
            _tmp: tmp,
        }
    }

    fn default() -> Self {
        Self::new(8, 2, 2)
    }

    fn ctx<'a>(&'a self, pdu: &'a mut Pdu, cdb: [u8; 16], lun: u8) -> SmcScsiCtx<'a> {
        let inner = ScsiCtx {
            pdu,
            cdb,
            lun,
            drive_id: 0,
            device_type: 0x08,
            device_name: "changer".to_string(),
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
            has_changer: true,
        };
        SmcScsiCtx {
            inner,
            library: &self.library,
            element_config: &self.element_config,
        }
    }
}

fn blank_pdu() -> Pdu {
    Pdu::synth(&[], 0, 0, &[])
}

/// 16-byte CDB with just the opcode set.
fn cdb(op: u8) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = op;
    c
}

#[test]
fn every_handler_refuses_a_non_zero_lun() {
    let fx = Fixture::default();
    // (opcode, handler) pairs — all six must CHECK CONDITION on LUN 1.
    let cases: [(u8, SmcHandler); 6] = [
        (0x07, handlers::handle_initialize_element_status),
        (0x37, handlers::handle_initialize_element_status_with_range),
        (0xA5, handlers::handle_move_medium),
        (0xA6, handlers::handle_exchange_medium),
        (0xB6, handlers::handle_send_volume_tag),
        (0xB8, handlers::handle_read_element_status),
    ];
    for (op, handler) in cases {
        let mut pdu = blank_pdu();
        let mut ctx = fx.ctx(&mut pdu, cdb(op), 1);
        let resp = handler(&mut ctx).expect("handler returns Ok");
        assert_eq!(
            resp.status,
            ScsiStatus::CheckCondition,
            "opcode {op:#04x} should refuse LUN 1",
        );
    }
}

#[test]
fn initialize_element_status_on_lun_zero_succeeds() {
    let fx = Fixture::default();
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, cdb(0x07), 0);
    let resp = handlers::handle_initialize_element_status(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn initialize_element_status_with_range_is_acknowledged() {
    let fx = Fixture::default();
    let mut c = cdb(0x37);
    c[2..4].copy_from_slice(&STORAGE_BASE.to_be_bytes()); // start
    c[6..8].copy_from_slice(&4u16.to_be_bytes()); // count
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    let resp = handlers::handle_initialize_element_status_with_range(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn send_volume_tag_is_accepted() {
    let fx = Fixture::default();
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, cdb(0xB6), 0);
    let resp = handlers::handle_send_volume_tag(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn read_element_status_returns_descriptors_per_element_type() {
    let fx = Fixture::default();
    // 0x00 all, 0x01 transport, 0x02 storage, 0x03 mail, 0x04 drive.
    for elem_type in [0x00u8, 0x01, 0x02, 0x03, 0x04] {
        let mut c = cdb(0xB8);
        c[1] = elem_type;
        c[4..6].copy_from_slice(&0xFFFFu16.to_be_bytes()); // num_elements
        c[7..10].copy_from_slice(&[0x01, 0x00, 0x00]); // alloc = 64 KiB
        let mut pdu = blank_pdu();
        let mut ctx = fx.ctx(&mut pdu, c, 0);
        let resp = handlers::handle_read_element_status(&mut ctx).unwrap();
        assert_eq!(
            resp.status,
            ScsiStatus::Good,
            "element type {elem_type:#04x} should succeed",
        );
    }
}

#[test]
fn read_element_status_rejects_an_invalid_element_type() {
    let fx = Fixture::default();
    let mut c = cdb(0xB8);
    c[1] = 0x0F; // not a valid SMC element type
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    let resp = handlers::handle_read_element_status(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

#[test]
fn move_medium_from_an_empty_slot_is_refused() {
    let fx = Fixture::default();
    let mut c = cdb(0xA5);
    c[4..6].copy_from_slice(&STORAGE_BASE.to_be_bytes()); // source: empty slot 0
    c[6..8].copy_from_slice(&DRIVE_BASE.to_be_bytes()); // dest: drive 0
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    let resp = handlers::handle_move_medium(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

#[test]
fn move_medium_loads_a_cartridge_from_a_slot_into_a_drive() {
    let fx = Fixture::default();
    let slot_id = {
        let mut lib = fx.library.lock().unwrap();
        lib.add_or_create_tape("TAPE01", "primary")
            .expect("tape lands in a slot")
    };
    let mut c = cdb(0xA5);
    c[4..6].copy_from_slice(&(STORAGE_BASE + slot_id as u16).to_be_bytes());
    c[6..8].copy_from_slice(&DRIVE_BASE.to_be_bytes());
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    let resp = handlers::handle_move_medium(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn move_medium_unloads_a_cartridge_from_a_drive_back_to_a_slot() {
    let fx = Fixture::default();
    let slot_id = {
        let mut lib = fx.library.lock().unwrap();
        lib.add_or_create_tape("TAPE01", "primary").unwrap()
    };
    let slot_addr = STORAGE_BASE + slot_id as u16;

    // Load: slot -> drive.
    let mut load = cdb(0xA5);
    load[4..6].copy_from_slice(&slot_addr.to_be_bytes());
    load[6..8].copy_from_slice(&DRIVE_BASE.to_be_bytes());
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, load, 0);
    assert_eq!(
        handlers::handle_move_medium(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    // Unload: drive -> slot — exercises the drive-source branch.
    let mut unload = cdb(0xA5);
    unload[4..6].copy_from_slice(&DRIVE_BASE.to_be_bytes());
    unload[6..8].copy_from_slice(&slot_addr.to_be_bytes());
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, unload, 0);
    assert_eq!(
        handlers::handle_move_medium(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );
}

#[test]
fn exchange_medium_with_empty_slots_is_refused() {
    let fx = Fixture::default();
    let mut c = cdb(0xA6);
    c[4..6].copy_from_slice(&STORAGE_BASE.to_be_bytes()); // source
    c[6..8].copy_from_slice(&(STORAGE_BASE + 1).to_be_bytes()); // first dest
    c[8..10].copy_from_slice(&(STORAGE_BASE + 2).to_be_bytes()); // second dest
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    let resp = handlers::handle_exchange_medium(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::CheckCondition);
}

#[test]
fn move_medium_raises_ua_only_on_affected_drives() {
    // Regression for issue #37: MEDIUM MAY HAVE CHANGED must be
    // queued only on the drive LUN(s) whose cartridge actually
    // changed. Broadcasting across every drive LUN (the prior
    // "conservative" behavior) preempted the host's next command
    // on unrelated drives — when the host's positioning sequence
    // ignored the resulting CHECK CONDITION, the daemon-side
    // head_lba never reset and follow-up writes landed at the
    // wrong LBA.
    let fx = Fixture::new(8, 2, 2);
    let slot_id = {
        let mut lib = fx.library.lock().unwrap();
        lib.add_or_create_tape("TAPE01", "primary").unwrap()
    };
    let mut c = cdb(0xA5);
    // Load slot -> drive 0 (DRIVE_BASE).
    c[4..6].copy_from_slice(&(STORAGE_BASE + slot_id as u16).to_be_bytes());
    c[6..8].copy_from_slice(&DRIVE_BASE.to_be_bytes());
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    assert_eq!(
        handlers::handle_move_medium(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    let ua = fx.ua.lock().unwrap();
    // Drive 0 (LUN 1) is the destination — it gained a cartridge.
    let drive_0_ua = ua.check_and_pop_ua(1, 1);
    assert_eq!(drive_0_ua, Some(UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED));
    // Drive 1 (LUN 2) is uninvolved — it must NOT receive the UA.
    // The bug was that every drive LUN got the UA regardless.
    assert!(
        ua.check_and_pop_ua(1, 2).is_none(),
        "uninvolved drive 1 (LUN 2) must not receive MEDIUM MAY HAVE CHANGED",
    );
    // The changer LUN (0) is not a drive — UA tracking only applies
    // to drive LUNs, but check anyway that we didn't accidentally
    // queue one.
    assert!(ua.check_and_pop_ua(1, 0).is_none());
}

#[test]
fn move_medium_slot_to_slot_raises_no_drive_ua() {
    // Slot-to-slot moves don't change any drive's cartridge, so
    // no drive LUN should receive MEDIUM MAY HAVE CHANGED.
    let fx = Fixture::new(8, 2, 2);
    let (a, b) = {
        let mut lib = fx.library.lock().unwrap();
        let a = lib.add_or_create_tape("TAPE_A", "primary").unwrap();
        // Find an empty target slot — add_or_create_tape lands the
        // first cart at the first free slot; pick a different free
        // slot for the destination.
        let b = lib
            .storage_slots()
            .iter()
            .find(|s| !s.occupied)
            .map(|s| s.id)
            .expect("at least one free slot");
        (a, b)
    };
    let mut c = cdb(0xA5);
    c[4..6].copy_from_slice(&(STORAGE_BASE + a as u16).to_be_bytes());
    c[6..8].copy_from_slice(&(STORAGE_BASE + b as u16).to_be_bytes());
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    assert_eq!(
        handlers::handle_move_medium(&mut ctx).unwrap().status,
        ScsiStatus::Good,
    );

    let ua = fx.ua.lock().unwrap();
    for lun in 0..=2 {
        assert!(
            ua.check_and_pop_ua(1, lun).is_none(),
            "no drive's cartridge changed, LUN {lun} must not receive UA",
        );
    }
}

#[test]
fn exchange_medium_swaps_two_cartridges() {
    let fx = Fixture::default();
    let (src, dst1) = {
        let mut lib = fx.library.lock().unwrap();
        let a = lib.add_or_create_tape("TAPE_A", "primary").unwrap();
        let b = lib.add_or_create_tape("TAPE_B", "primary").unwrap();
        (a, b)
    };
    // EXCHANGE moves first_dest -> second_dest, then source -> first_dest.
    // second_dest must be a free slot; pick one past the two tapes.
    let mut c = cdb(0xA6);
    c[4..6].copy_from_slice(&(STORAGE_BASE + src as u16).to_be_bytes());
    c[6..8].copy_from_slice(&(STORAGE_BASE + dst1 as u16).to_be_bytes());
    c[8..10].copy_from_slice(&(STORAGE_BASE + 5).to_be_bytes());
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    let resp = handlers::handle_exchange_medium(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
}

#[test]
fn dispatch_router_handles_known_opcodes_and_passes_through_unknown() {
    let fx = Fixture::default();
    for op in [0x07u8, 0x37, 0xA5, 0xA6, 0xB6, 0xB8] {
        let mut pdu = blank_pdu();
        let mut ctx = fx.ctx(&mut pdu, cdb(op), 0);
        assert!(
            dispatch_changer_lun(&mut ctx).is_some(),
            "opcode {op:#04x} should be routed",
        );
    }
    // 0x00 (TEST UNIT READY) is not an SMC changer opcode.
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, cdb(0x00), 0);
    assert!(dispatch_changer_lun(&mut ctx).is_none());
}
