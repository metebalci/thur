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
use scsi_smc::dispatch::{SmcScsiCtx, dispatch_changer_lun, handlers, pr_enforce};
use scsi_spc::reservations::{Nexus, PrOutOutcome};
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
    reservations: Arc<scsi_spc::reservations::ReservationManager>,
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
            reservations: Arc::new(scsi_spc::reservations::ReservationManager::new()),
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
            initiator_isid: [0u8; 6],
            peer: "test",
            diagnostic_store: &self.diag,
            session_partition: None,
            has_changer: true,
            alua: None,
            reservations: &self.reservations,
        };
        SmcScsiCtx {
            inner,
            library: &self.library,
            element_config: &self.element_config,
        }
    }

    /// Like [`Fixture::ctx`] but with a caller-chosen initiator port so
    /// a test can drive distinct I_T nexuses through the reservation
    /// gate. The reservation identity is now `(IQN, ISID)`, so the
    /// `tsih` argument seeds a distinct ISID (it no longer keys the
    /// registrant); [`Fixture::reserve_changer`] seeds its ISID the same
    /// way so the two line up.
    fn ctx_tsih<'a>(
        &'a self,
        pdu: &'a mut Pdu,
        cdb: [u8; 16],
        lun: u8,
        tsih: u16,
    ) -> SmcScsiCtx<'a> {
        let mut ctx = self.ctx(pdu, cdb, lun);
        ctx.inner.tsih = tsih;
        ctx.inner.initiator_isid = [tsih as u8; 6];
        ctx
    }

    /// Register key `sark` then RESERVE the given `type_byte` on the
    /// changer LUN (LUN 0) for the nexus identified by `tsih`. Used to
    /// set up the reservation a test then probes for fencing.
    fn reserve_changer(&self, tsih: u16, sark: u64, type_byte: u8) {
        let nexus = Nexus::iscsi(None, [tsih as u8; 6]);
        // REGISTER AND IGNORE EXISTING KEY (SA 0x06): RESERVATION KEY 0,
        // SERVICE ACTION RESERVATION KEY = sark.
        let reg = prout_params(0, sark);
        assert_eq!(
            self.reservations
                .prout(0, 0x06, 0, 0, &reg, 24, &nexus, true),
            PrOutOutcome::Good,
            "register",
        );
        // RESERVE (SA 0x01): RESERVATION KEY = sark, LU scope, type.
        let res = prout_params(sark, 0);
        assert_eq!(
            self.reservations
                .prout(0, 0x01, 0, type_byte, &res, 24, &nexus, true),
            PrOutOutcome::Good,
            "reserve",
        );
    }
}

fn blank_pdu() -> Pdu {
    Pdu::synth(&[], 0, 0, &[])
}

/// 24-byte PROUT parameter list: RESERVATION KEY + SERVICE ACTION
/// RESERVATION KEY, all flags clear.
fn prout_params(rk: u64, sark: u64) -> Vec<u8> {
    let mut p = vec![0u8; 24];
    p[0..8].copy_from_slice(&rk.to_be_bytes());
    p[8..16].copy_from_slice(&sark.to_be_bytes());
    p
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
fn read_element_status_dispatch_is_not_partition_filtered() {
    // Dispatch-level partition filtering is intentionally off — mtx
    // breaks on zero-descriptor per-type pages. Verify a session bound
    // to a non-existent partition still gets a non-empty data-transfer
    // response (chassis-wide view), instead of an empty 16-byte
    // header-only response.
    let fx = Fixture::default();
    let mut c = cdb(0xB8);
    c[1] = 0x04; // DataTransfer
    c[4..6].copy_from_slice(&2u16.to_be_bytes()); // num_elements
    c[7..10].copy_from_slice(&[0x00, 0x01, 0x00]); // alloc = 256
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx(&mut pdu, c, 0);
    ctx.inner.session_partition = Some("nonexistent-partition");
    let resp = handlers::handle_read_element_status(&mut ctx).unwrap();
    assert_eq!(resp.status, ScsiStatus::Good);
    // 8-byte ESD header + 8-byte page header = 16 bytes when filtered
    // empty. With filtering off we expect at least one descriptor (the
    // fixture creates 2 drives, base descriptor size = 12 bytes), so
    // a full response is > 16 bytes.
    assert!(
        resp.data_out.len() > 16,
        "expected unfiltered response (>16 bytes), got {}",
        resp.data_out.len()
    );
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

#[test]
fn move_medium_is_fenced_by_a_persistent_reservation() {
    // Issue #53: a reservation held on the changer LUN fences MOVE
    // MEDIUM against every other I_T nexus, mirroring the drive-LUN
    // data-path fence. Set up a loadable tape, reserve EXCLUSIVE
    // ACCESS (type 0x03) for nexus A (TSIH 1), then confirm a
    // non-holder (TSIH 2) is refused and the holder is not.
    let fx = Fixture::default();
    let slot_id = {
        let mut lib = fx.library.lock().unwrap();
        lib.add_or_create_tape("TAPE01", "primary").unwrap()
    };
    let slot_addr = STORAGE_BASE + slot_id as u16;
    fx.reserve_changer(1, 0xAAAA, 0x03);

    let mut load = cdb(0xA5);
    load[4..6].copy_from_slice(&slot_addr.to_be_bytes());
    load[6..8].copy_from_slice(&DRIVE_BASE.to_be_bytes());

    // Non-holder (TSIH 2): RESERVATION CONFLICT, returned by the gate
    // before the move runs.
    {
        let mut pdu = blank_pdu();
        let mut ctx = fx.ctx_tsih(&mut pdu, load, 0, 2);
        let resp = dispatch_changer_lun(&mut ctx).unwrap().unwrap();
        assert_eq!(resp.status, ScsiStatus::ReservationConflict);
    }

    // The move never happened — the tape is still in its slot.
    {
        let lib = fx.library.lock().unwrap();
        let slot = lib
            .storage_slots()
            .iter()
            .find(|s| s.id == slot_id)
            .expect("slot exists");
        assert!(slot.occupied, "fenced MOVE must leave the tape in its slot");
    }

    // The holder (TSIH 1) is not fenced — the load succeeds.
    {
        let mut pdu = blank_pdu();
        let mut ctx = fx.ctx_tsih(&mut pdu, load, 0, 1);
        let resp = dispatch_changer_lun(&mut ctx).unwrap().unwrap();
        assert_eq!(resp.status, ScsiStatus::Good, "holder moves freely");
    }
}

#[test]
fn read_element_status_is_fenced_under_exclusive_access() {
    // The read gate: under EXCLUSIVE ACCESS, READ ELEMENT STATUS from
    // a non-holder is a RESERVATION CONFLICT; the holder reads freely.
    let fx = Fixture::default();
    fx.reserve_changer(1, 0xBBBB, 0x03);

    let mut c = cdb(0xB8);
    c[1] = 0x00; // all element types
    c[4..6].copy_from_slice(&0xFFFFu16.to_be_bytes());
    c[7..10].copy_from_slice(&[0x01, 0x00, 0x00]); // alloc = 64 KiB

    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx_tsih(&mut pdu, c, 0, 2);
    assert_eq!(
        dispatch_changer_lun(&mut ctx).unwrap().unwrap().status,
        ScsiStatus::ReservationConflict,
        "non-holder READ ELEMENT STATUS fenced",
    );

    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx_tsih(&mut pdu, c, 0, 1);
    assert_eq!(
        dispatch_changer_lun(&mut ctx).unwrap().unwrap().status,
        ScsiStatus::Good,
        "holder READ ELEMENT STATUS allowed",
    );
}

#[test]
fn exchange_and_initialize_element_status_are_fenced() {
    // Issue #53: MOVE MEDIUM and READ ELEMENT STATUS have fence tests;
    // the remaining write-gated changer opcodes did not. Pin every
    // write-gated entry of `pr_gate` (EXCHANGE MEDIUM, INITIALIZE
    // ELEMENT STATUS [+WITH RANGE], SEND VOLUME TAG) so a dropped match
    // arm can't silently unfence a reserved changer.
    let fx = Fixture::default();
    fx.reserve_changer(1, 0xAAAA, 0x03); // EXCLUSIVE ACCESS by nexus A

    // A non-holder (TSIH 2) is refused by the gate before the handler.
    for op in [0xA6u8, 0x07, 0x37, 0xB6] {
        let mut pdu = blank_pdu();
        let mut ctx = fx.ctx_tsih(&mut pdu, cdb(op), 0, 2);
        assert_eq!(
            dispatch_changer_lun(&mut ctx).unwrap().unwrap().status,
            ScsiStatus::ReservationConflict,
            "non-holder opcode {op:#04x} must be fenced",
        );
    }

    // The holder (TSIH 1) is not fenced — the gate lets the command
    // through (INITIALIZE ELEMENT STATUS is a benign rescan).
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx_tsih(&mut pdu, cdb(0x07), 0, 1);
    assert_ne!(
        dispatch_changer_lun(&mut ctx).unwrap().unwrap().status,
        ScsiStatus::ReservationConflict,
        "holder must not be fenced",
    );
}

#[test]
fn write_exclusive_allows_nonholder_read_blocks_write() {
    // Issue #53: the changer read/write gate split is only ever tested
    // under EXCLUSIVE ACCESS (both denied). WRITE EXCLUSIVE (type 0x01)
    // is the asymmetric case — a non-holder may READ ELEMENT STATUS but
    // not MOVE MEDIUM — and pins the Read-vs-Write routing of
    // `pr_enforce` (swapping its arms would pass every EA test).
    let fx = Fixture::default();
    fx.reserve_changer(1, 0xAAAA, 0x01); // WRITE EXCLUSIVE by nexus A

    // Non-holder READ ELEMENT STATUS is allowed under Write Exclusive.
    let mut rdcdb = cdb(0xB8);
    rdcdb[1] = 0x00; // all element types
    rdcdb[4..6].copy_from_slice(&0xFFFFu16.to_be_bytes());
    rdcdb[7..10].copy_from_slice(&[0x01, 0x00, 0x00]); // alloc = 64 KiB
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx_tsih(&mut pdu, rdcdb, 0, 2);
    assert_eq!(
        dispatch_changer_lun(&mut ctx).unwrap().unwrap().status,
        ScsiStatus::Good,
        "WrEx: non-holder READ ELEMENT STATUS allowed",
    );

    // Non-holder MOVE MEDIUM is fenced.
    let mut pdu = blank_pdu();
    let mut ctx = fx.ctx_tsih(&mut pdu, cdb(0xA5), 0, 2);
    assert_eq!(
        dispatch_changer_lun(&mut ctx).unwrap().unwrap().status,
        ScsiStatus::ReservationConflict,
        "WrEx: non-holder MOVE MEDIUM fenced",
    );
}

#[test]
fn pr_enforce_classifies_request_volume_element_address_as_a_read() {
    // REQUEST VOLUME ELEMENT ADDRESS (0xB5) is dispatched by the
    // thurvtl wrapper, not `dispatch_changer_lun`, so the wrapper
    // calls `pr_enforce` directly. Verify the gate classifies it as a
    // read: fenced for a non-holder under EXCLUSIVE ACCESS, open to
    // the holder.
    let fx = Fixture::default();
    fx.reserve_changer(1, 0xCCCC, 0x03);

    let mut pdu = blank_pdu();
    let ctx = fx.ctx_tsih(&mut pdu, cdb(0xB5), 0, 2);
    assert!(pr_enforce(&ctx).is_some(), "non-holder 0xB5 must be fenced",);

    let mut pdu = blank_pdu();
    let ctx = fx.ctx_tsih(&mut pdu, cdb(0xB5), 0, 1);
    assert!(pr_enforce(&ctx).is_none(), "holder 0xB5 must pass");
}

#[test]
fn pr_enforce_leaves_identity_and_pr_opcodes_open() {
    // SAM-5 §5.9.1: identity / status / the PR commands themselves are
    // never fenced, even from a non-holder while a reservation is held.
    let fx = Fixture::default();
    fx.reserve_changer(1, 0xDDDD, 0x03);
    for op in [0x00u8, 0x12, 0x03, 0xA0, 0x5E, 0x5F] {
        let mut pdu = blank_pdu();
        let ctx = fx.ctx_tsih(&mut pdu, cdb(op), 0, 2);
        assert!(
            pr_enforce(&ctx).is_none(),
            "opcode {op:#04x} must never be fenced",
        );
    }
}
