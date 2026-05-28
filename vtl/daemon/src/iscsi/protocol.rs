// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// iSCSI Protocol — SCSI dispatch layer (post-Step-3c-phase-2,
// post-5.B.6-follow-up-step-1).
//
// The connection lifecycle (PDU framing, login phase, R2T loop,
// dispatch-loop write-back) lives in `shared_iscsi::transport`.
// Drive-LUN per-opcode handlers with no library access (TUR /
// REQUEST SENSE / READ BLOCK LIMITS / REPORT DENSITY / REWIND /
// READ POSITION / LOAD UNLOAD / SPACE / FILEMARKS / LOCATE / ERASE
// / SET CAPACITY / READ-WRITE 6 / VERIFY / PREVENT-ALLOW /
// ALLOW OVERWRITE / FORMAT MEDIUM / READ-WRITE ATTRIBUTE /
// READ-WRITE BUFFER / RESERVE-RELEASE) live in
// `scsi_ssc::dispatch::handlers`, plus the dispatch types
// (`Pdu` / `ScsiCtx` / `ScsiResp` / `ScsiStatus`), audit helpers,
// and byte helpers.
//
// What stays here: the SCSI command dispatch tree —
// `handle_scsi_command` + `dispatch_scsi` + the library- /
// diagnostic-store-touching per-opcode handler arms (INQUIRY VPDs,
// REPORT LUNS, MODE SENSE/SELECT changer pages + drive pages,
// LOG SENSE, SEND/RECEIVE DIAGNOSTIC, INITIALIZE/READ ELEMENT
// STATUS, MOVE/EXCHANGE MEDIUM, MAINTENANCE IN/OUT, SECURITY
// PROTOCOL IN, B5 overloaded, PR IN/OUT, SEND VOLUME TAG) — plus
// the pre-/post-hooks the iSCSI READ / MOVE MEDIUM / SEND
// DIAGNOSTIC paths rely on (`ensure_chunk_local_for_next_read`,
// `read_legal_hold_at_load`). `IscsiLibraryHandler::dispatch` (in
// `super::handler`) is the entry point — it threads a synthetic
// `Pdu` and the same `Arc` handles `serve_connection` used to
// thread.

#![allow(dead_code)] // Some opcode-handler helpers aren't reached in dev builds.

use super::drive_manager;
use super::scsi;
use super::unit_attention;

use crate::diagnostics::DiagnosticStore;

use drive_manager::DriveManager;
use scsi_smc::SmcScsiCtx;
use scsi_smc::changer::ElementAddressConfig;
use unit_attention::UnitAttentionTracker;

use anyhow::{Result, anyhow};
use core_mediachanger::{
    AuditChannel, AuditRateLimiter, Library, LibraryFacade, NextReadChunk, ObjectStoreConfig,
    TapeEvent,
};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use super::server::ObjectStoreRegistry;

// Re-use shared dispatch types so the library wrapper, library-side
// handlers, and the moved drive-LUN handlers all speak the same
// `Pdu` / `ScsiCtx` / `ScsiResp` / `ScsiStatus` types. Audit helpers
// and byte helpers come along too — call sites keep their unqualified
// names (`audit_append(...)`, `limit_len(...)`, etc.).
use scsi_ssc::dispatch::handlers as shared_handlers;
pub(crate) use scsi_ssc::dispatch::{
    Pdu, ScsiCtx, ScsiResp, ScsiStatus, limit_len, pdu_expected_xfer_len,
};

use scsi_spc::inquiry::{
    Identity, InquiryFlags, PeripheralQualifier, PeripheralType, build_inquiry_std,
};
use scsi_spc::vpd::{
    Association, CodeSet, DesignatorType, build_device_identification, build_supported_vpd_pages,
    build_unit_serial_number, finalize_vpd, push_designator, vpd_header,
};

/// Wire-shape flags for thurvtl's changer-LUN standard INQUIRY:
/// SPC-3 (the version SMC-3 references), HISUP=1, CMDQUE=0
/// (per-drive serialization in the daemon — no queue-tag semantics
/// to advertise), TPGS=01b (implicit-only ALUA — REPORT TARGET
/// PORT GROUPS honored since #43).
const CHANGER_INQUIRY_FLAGS: InquiryFlags = InquiryFlags {
    spc_version: 0x05,
    hisup: true,
    tpgs: scsi_spc::vpd::TpgsField::Implicit,
    cmdque: false,
};

// Wire-level negotiation constants (`MAX_RECV_DATA_SEGMENT_LENGTH`,
// `MAX_BURST_LENGTH`, `FIRST_BURST_LENGTH`, `TPGT`, login `STAGE_*`)
// moved to `shared_iscsi::transport` in Step 3c phase 2. The target
// IQN is resolved from `iscsi.target_iqn` at boot and stored on
// `IscsiLibraryHandler`, returned through the `ScsiHandler` trait.

// VTL configuration.
// Slot/drive counts come from element_config (driven by library topology),
// LTO generation + firmware revision come from the loaded Library
// (`lib.lto_generation()` / `lib.drive_firmware()`), set at
// `library init --lto-generation N --firmware CODE`.

// `STAGE_SECURITY` / `STAGE_OPNEG` / `STAGE_FULL`, `SessionParams`,
// and `SessionGuard` lifted to `shared_iscsi::transport` in
// Step 3c phase 2. Per-session teardown (drive-lock release,
// PREVENT/ALLOW clear) now runs through
// `IscsiLibraryHandler::on_session_close` — the same Drop-time
// behavior, just plumbed via the trait.

// `handle_connection` / `serve_connection` lifted to
// `shared_iscsi::transport::run` + `serve_connection` in Step 3c
// phase 2. `IscsiServer::run` now constructs `IscsiLibraryHandler`
// and hands it to the shared transport — the per-PDU dispatch loop,
// login phase, R2T loop, and SessionGuard cleanup all live in
// shared-iscsi.

// `Pdu`, `ScsiStatus`, `ScsiResp` (and its `good()` /
// `check_condition()` / `check_condition_for()` /
// `check_condition_with_sense()` impls)
// lifted to `scsi_ssc::dispatch::types` in 5.B.6 follow-up step 1.
// Re-imported at the top of this module so existing handler bodies
// keep building unchanged.

/// Cloud-prefetch hook for the SCSI READ path. Peeks the loaded
/// cartridge for the chunk backing the next read LBA and, if the
/// local pool file is missing, pulls it from the cartridge's bound
/// cloud backend (lazy-initialized via the shared
/// [`ObjectStoreRegistry`]) and writes it into the pool. This
/// covers two failure modes:
///   1. Cold-start daemon facing a wiped chunks directory (e.g.
///      operator nuked `/chunks/`, or the host was reimaged) —
///      the in-memory `Cartridge` was just opened by drive_manager
///      with no cloud backend, so its sync read path has no
///      refetch surface.
///   2. Cache eviction during a live iSCSI session that pruned a
///      chunk this drive's loaded cartridge had marked Both —
///      the on-disk manifest is updated to S3Only but the
///      drive's in-memory cartridge still believes Both.
///
/// Best-effort: returns Ok if there's nothing to do (no chunk for
/// the LBA, file already present, or cartridge cloud-blind by
/// configuration). Any error is propagated to the caller, which
/// logs it at debug and lets the sync read path surface its own
/// I/O error.
pub(crate) async fn ensure_chunk_local_for_next_read(
    drive_manager: &Arc<DriveManager>,
    drive_id: usize,
    tsih: u16,
    backends: &ObjectStoreRegistry,
    storage_config: &Arc<ObjectStoreConfig>,
) -> Result<()> {
    // Snapshot the next-read chunk metadata under the drive lock,
    // then release the lock before any async I/O. Holding the
    // sync drive Mutex across an await is forbidden — this is the
    // whole reason the prefetch lives outside `with_drive`.
    let next: Option<NextReadChunk> = drive_manager
        .with_drive(drive_id, tsih, |cart| {
            let lba = cart.head_lba();
            Ok(cart.peek_chunk_for_lba(lba))
        })
        .ok()
        .flatten();
    let Some(next) = next else {
        return Ok(());
    };

    if next.store_path.is_file() {
        return Ok(());
    }

    // Lazy-init the bound backend if this is the first cache miss
    // for it. `clone()` here returns a fresh `Box<dyn ObjectStoreBackend>`
    // (via the trait's `clone_box`); cheap by design — the
    // expensive work was the auth/network round-trip during init.
    let backend = {
        let mut reg = backends.lock().await;
        if !reg.contains_key(&next.backend_name) {
            let b = storage_config
                .create_backend_named(&next.backend_name)
                .await
                .map_err(|e| {
                    anyhow!(
                        "init backend '{}' for read prefetch: {}",
                        next.backend_name,
                        e
                    )
                })?;
            reg.insert(next.backend_name.clone(), b);
        }
        reg.get(&next.backend_name).expect("just inserted").clone()
    };

    let object_key = next.object_key.clone();
    let bytes = backend.download_chunk(&object_key).await.map_err(|e| {
        anyhow!(
            "download chunk {} ({}..) from {}: {}",
            next.chunk_id,
            &next.hash[..8.min(next.hash.len())],
            next.backend_name,
            e
        )
    })?;

    if let Some(parent) = next.store_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&next.store_path, &bytes).await?;
    let downloaded = bytes.len() as u64;

    tracing::info!(
        "iSCSI prefetch: refetched chunk {} ({} bytes) from {} into pool for drive {}",
        next.chunk_id,
        downloaded,
        next.backend_name,
        drive_id
    );

    // Attribute the downloaded bytes to the loaded cartridge's
    // lifetime `backend_bytes_read` counter. The sync read path
    // (`read_block`) cannot bump it itself — the cloud fetch happens
    // here, outside `with_drive`, exactly so the drive lock is not
    // held across the await. Re-enter the lock for the cheap bump.
    // A miss (drive unloaded between the peek and here) just drops
    // the bump — it is telemetry, not correctness state.
    let _ = drive_manager.with_drive(drive_id, tsih, |cart| {
        cart.bump_backend_bytes_read(downloaded);
        Ok(())
    });

    Ok(())
}

/// Read the cloud sentinel
/// (`manifests/<barcode>/manifest-latest.json`) and return whether
/// legal-hold is on. Best-effort: any failure (backend doesn't support
/// legal hold, sentinel missing, network error) returns `false` — the
/// auto-hold-on-upload worker is the safety net for cloud-side
/// preservation, and the host gets the conservative "not held" surface
/// rather than blocking writes on transient cloud problems.
///
/// Lazy-inits the bound backend in the shared registry on first miss
/// (mirroring `ensure_chunk_local_for_next_read`) so the same handle
/// is reused for every subsequent load against the same backend.
pub(crate) async fn read_legal_hold_at_load(
    backends: &ObjectStoreRegistry,
    storage_config: &Arc<ObjectStoreConfig>,
    backend_name: &str,
    barcode: &str,
) -> bool {
    let backend = {
        let mut reg = backends.lock().await;
        if !reg.contains_key(backend_name) {
            match storage_config.create_backend_named(backend_name).await {
                Ok(b) => {
                    reg.insert(backend_name.to_string(), b);
                }
                Err(e) => {
                    tracing::debug!(
                        "iSCSI legal-hold post-hook: backend '{}' init failed ({}) - treating as not held",
                        backend_name,
                        e
                    );
                    return false;
                }
            }
        }
        reg.get(backend_name).expect("just inserted").clone()
    };
    if !backend.supports_legal_hold() {
        return false;
    }
    let backend_arc: Arc<dyn core_mediachanger::ObjectStoreBackend> = Arc::from(backend);
    match core_mediachanger::read_cartridge_held(backend_arc, barcode.to_string()).await {
        Ok(held) => held,
        Err(e) => {
            tracing::debug!(
                "iSCSI legal-hold post-hook: sentinel read for '{}' on '{}' returned {} - treating as not held",
                barcode,
                backend_name,
                e
            );
            false
        }
    }
}

// SMC-flavor dispatch context lives in `scsi_smc::SmcScsiCtx`: wraps
// `scsi_ssc::dispatch::ScsiCtx` and adds the library + element_config
// borrows the changer / library-touching handlers need. Imported at the
// top of this file.

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_scsi_command(
    pdu: &mut Pdu,
    tsih: u16,
    drive_manager: Arc<DriveManager>,
    library: Arc<Mutex<Library>>,
    ua_tracker: Arc<Mutex<UnitAttentionTracker>>,
    element_config: ElementAddressConfig,
    event_tx: broadcast::Sender<TapeEvent>,
    data_dir: &std::path::Path,
    audit_log: &Option<AuditChannel>,
    audit_ratelimiter: Arc<AuditRateLimiter>,
    initiator_iqn: Option<&str>,
    peer: &str,
    session_partition: Option<String>,
    diagnostic_store: Arc<DiagnosticStore>,
    alua: Option<Arc<shared_iscsi::alua::AluaTopology>>,
) -> Result<ScsiResp> {
    // CDB lives in BHS bytes [32..48]; copy it to a local so handlers
    // don't have to re-slice pdu.bhs every time.
    let mut cdb = [0u8; 16];
    cdb.copy_from_slice(&pdu.bhs[32..48]);

    // Extract LUN number (simple peripheral device addressing)
    let lun = pdu.lun[1];

    // LUN 0 = Medium Changer (0x08); LUN 1..N = Sequential Access (0x01).
    let device_type = if lun == 0 { 0x08 } else { 0x01 };
    let device_name = if lun == 0 {
        "Medium Changer".to_string()
    } else {
        format!("Tape Drive {} (LUN {})", lun - 1, lun)
    };
    // Map LUN to drive_id (LUN 1 = drive 0, LUN 2 = drive 1, etc.)
    let drive_id = if lun >= 1 { (lun - 1) as usize } else { 0 };

    let opcode = cdb[0];
    tracing::debug!(
        "SCSI CDB: LUN={} ({}) opcode=0x{:02x} bytes={:02x?}",
        lun,
        device_name,
        opcode,
        &cdb[..16]
    );

    // SPC: report a pending Unit Attention before dispatching the
    // command. INQUIRY (0x12) and REQUEST SENSE (0x03) are excepted
    // — they must complete normally even with UAs pending so the
    // initiator can read inquiry data / discover the UA via sense.
    // REPORT LUNS (0xA0) is similarly excepted per SPC. Any other
    // opcode preempts to CHECK CONDITION + sense key 0x06 and pops
    // one UA off the queue. Backup software relies on the
    // 0x06/0x28/0x00 (MEDIUM MAY HAVE CHANGED) signal after a
    // MOVE/EXCHANGE MEDIUM to re-read inquiry data and element
    // status — without this pop the UA we push during 0xA5/0xA6
    // never reaches the host.
    if !matches!(opcode, 0x12 | 0x03 | 0xA0) {
        let popped = ua_tracker
            .lock()
            .map_err(|_| anyhow!("UA tracker mutex poisoned"))?
            .check_and_pop_ua(tsih, lun);
        if let Some(code) = popped {
            tracing::info!(
                "Reporting Unit Attention to TSIH={} LUN={}: ASC=0x{:02x} ASCQ=0x{:02x} (preempts opcode 0x{:02x})",
                tsih,
                lun,
                code.asc,
                code.ascq,
                opcode,
            );
            let sense = scsi::sense::SenseDataBuilder::new(
                scsi::sense::SenseKey::UnitAttention,
                scsi::sense::AdditionalSenseCode {
                    asc: code.asc,
                    ascq: code.ascq,
                },
            )
            .build();
            return Ok(ScsiResp::check_condition_with_sense(sense));
        }
    }

    // Build the shared facade (clones the `Arc<Mutex<Library>>`; the
    // `LibraryFacade` locks per-call inside `TapeDeviceFacade`
    // methods, so the dispatcher never holds the lock across
    // long-running work). Stored in a local so the borrow lives for
    // the duration of the dispatch.
    let library_facade = LibraryFacade::new(library.clone());

    let inner = ScsiCtx {
        pdu,
        cdb,
        lun,
        drive_id,
        device_type,
        device_name,
        tsih,
        drive_manager: &drive_manager,
        facade: &library_facade,
        ua_tracker: &ua_tracker,
        event_tx: &event_tx,
        data_dir,
        audit_log,
        audit_ratelimiter: &audit_ratelimiter,
        initiator_iqn,
        peer,
        diagnostic_store: &diagnostic_store,
        session_partition: session_partition.as_deref(),
        has_changer: true,
        alua: alua.as_deref(),
    };
    let mut ctx = SmcScsiCtx {
        inner,
        library: &library,
        element_config: &element_config,
    };

    dispatch_scsi(&mut ctx)
}

/// Per-opcode dispatch over `ctx.cdb[0]`. Each arm calls either a
/// drive-LUN handler in [`scsi_ssc::dispatch::handlers`] (passed
/// `&mut ctx.inner`) or a library-touching handler defined below in
/// this module (passed `&mut ctx`). The single `match` stays here so
/// the opcode → handler mapping is easy to scan in one place.
fn dispatch_scsi(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    // Per-LUN partition fence. Lifted to `scsi_ssc::dispatch::handlers`
    // in 5.B.6 follow-up step 4 — runs against `ctx.facade`, so an
    // unpartitioned topology is a no-op.
    if let Some(refusal) = shared_handlers::check_partition_fence(&ctx.inner)? {
        return Ok(refusal);
    }

    // Wrapper-only arms first. These either need the SMC `Library`
    // lock or `element_config` (so they can't move to the shared
    // dispatcher), OR they have a LUN-0 (changer) override of an
    // opcode that the shared dispatcher would otherwise route to its
    // drive-LUN handler. Anything not matched here falls through to
    // `dispatch_drive_lun`.
    match ctx.cdb[0] {
        // INQUIRY: LUN-0 (changer) and drive-LUN VPD `0xB4` stay here.
        // Standard / other-VPD drive-LUN INQUIRYs go to the shared
        // dispatcher.
        0x12 => {
            if ctx.lun == 0 {
                return handle_inquiry(ctx);
            }
            let evpd = (ctx.cdb[1] & 0x01) != 0;
            if evpd && ctx.cdb[2] == 0xB4 {
                return handle_inquiry(ctx);
            }
        }
        // LOG SENSE: LUN-0 (changer-side log pages) stays here.
        0x4D => {
            if ctx.lun == 0 {
                return handle_log_sense(ctx);
            }
        }
        // 0xB5 is overloaded — REQUEST VOLUME ELEMENT ADDRESS on the
        // changer (LUN 0), SECURITY PROTOCOL OUT on a drive (LUN >= 1).
        // The drive-side body is in scsi-ssc; the LUN-0 stub stays
        // library-local since changer 0xB5 only makes sense here.
        0xB5 => {
            if ctx.lun == 0 {
                let alloc = u32::from_be_bytes([0, ctx.cdb[7], ctx.cdb[8], ctx.cdb[9]]);
                let mut d = vec![0u8; 8];
                return Ok(ScsiResp {
                    status: ScsiStatus::Good,
                    data_out: limit_len(d.split_off(0), alloc),
                    sense: None,
                });
            }
        }
        // MODE SENSE 6/10: LUN-0 (changer pages, uses `element_config`)
        // stays here. Drive LUNs go to the shared dispatcher
        // (`handle_mode_sense_{6,10}_drive`).
        0x1A => {
            if ctx.lun == 0 {
                return handle_mode_sense_6_changer(ctx);
            }
        }
        0x5A => {
            if ctx.lun == 0 {
                return handle_mode_sense_10_changer(ctx);
            }
        }
        // MODE SELECT 6/10: LUN-0 (changer no-op) stays here. Drive
        // LUNs go to the shared dispatcher.
        0x15 | 0x55 => {
            if ctx.lun == 0 {
                return Ok(ScsiResp::good());
            }
        }
        _ => {}
    }

    // SMC changer commands lifted to `scsi-smc`. Returns `Some(_)`
    // when the opcode (0x07 / 0x37 / 0xA5 / 0xA6 / 0xB6 / 0xB8) is in
    // the shared set; `None` falls through to the drive-LUN dispatcher
    // below.
    if let Some(r) = scsi_smc::dispatch::dispatch_changer_lun(ctx) {
        return r;
    }

    // Delegate to the shared drive-LUN dispatcher. Returns `None` for
    // opcodes not in the shared set — those become INVALID OPERATION
    // CODE here.
    match scsi_ssc::dispatch::dispatch_drive_lun(&mut ctx.inner) {
        Some(r) => r,
        None => {
            tracing::warn!(
                "Unsupported SCSI command: 0x{:02x}, full CDB: {:02x?}",
                ctx.cdb[0],
                &ctx.cdb[..16]
            );
            Ok(ScsiResp::check_condition())
        }
    }
}

// ==== Per-opcode SCSI handlers ====
//
// Each handler is dispatched from `dispatch_scsi` based on `ctx.cdb[0]`.
// They run on the tokio blocking thread pool — `serve_connection`
// wraps `handle_scsi_command` in `tokio::task::spawn_blocking` so the
// sync `Cartridge` operations (which can park on `PoolBudget` for up
// to `backpressure_max_wait_seconds`) never wedge an async worker. No
// `await` allowed inside any handler. Cloud-hitting work stays in
// `serve_connection` around the dispatch (legal-hold post-hook,
// chunk prefetch).
//
// Pattern: rebind ctx fields into local variables matching the names
// used by the original captured-locals body, then run the body. This
// keeps the body verbatim from the pre-extraction match — easier to
// review, easier to bisect.

/// INQUIRY (SPC-4) — only the LUN-0 (changer) and LUN ≥ 1 VPD `0xB4`
/// arms still live here. All other drive-LUN INQUIRY paths went to
/// `scsi_ssc::dispatch::inquiry::handle_inquiry` in 5.B.6 follow-up
/// step 4. `dispatch_scsi` routes drive-LUN INQUIRY straight to the
/// shared dispatcher except when VPD `0xB4` is requested — VPD `0xB4`
/// returns the chassis element address bound to the drive, and
/// element-address state is library-specific.
fn handle_inquiry(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let device_type = ctx.device_type;

    let evpd = (cdb[1] & 0x01) != 0;
    let page_code = cdb[2];
    let alloc = u16::from_be_bytes([cdb[3], cdb[4]]) as u32;
    tracing::debug!(
        "INQUIRY: LUN={} EVPD={} page_code=0x{:02x} allocation_length={}",
        lun,
        evpd,
        page_code,
        alloc
    );

    // VPD 0xB4 (Data Transfer Device Element Address) — drive-LUN-only,
    // library-specific (uses `element_config`). Routed here from
    // `dispatch_scsi`. One vendor-specific designator carrying the
    // 4-byte LSB-aligned element address.
    if evpd && page_code == 0xB4 {
        if lun < 1 {
            return Ok(ScsiResp::check_condition());
        }
        let element_addr = ctx.element_config.drive_id_to_address((lun - 1) as u32);
        let mut d = vpd_header(
            PeripheralQualifier::Connected,
            PeripheralType::SequentialAccess,
            0xB4,
            4 + 4, // one 4-byte-header + 4-byte-body designator
        );
        let element_value: [u8; 4] = [
            0x00,
            0x00,
            (element_addr >> 8) as u8,
            (element_addr & 0xFF) as u8,
        ];
        push_designator(
            &mut d,
            CodeSet::Binary,
            Association::TargetDevice,
            DesignatorType::VendorSpecific,
            &element_value,
        );
        finalize_vpd(&mut d);
        tracing::debug!(
            "INQUIRY VPD page 0xB4 response: drive={} element=0x{:04x}",
            lun - 1,
            element_addr,
        );
        return Ok(ScsiResp {
            status: ScsiStatus::Good,
            data_out: limit_len(d, alloc),
            sense: None,
        });
    }

    // From here on, only LUN 0 (changer) reaches us — drive LUNs (LUN ≥ 1)
    // never enter this function except for the VPD 0xB4 branch above.
    debug_assert_eq!(
        lun, 0,
        "drive LUN INQUIRY should route to shared dispatcher"
    );

    if evpd {
        match page_code {
            0x00 => {
                // Medium-changer VPD page list. SPC-4 mandatory (0x00,
                // 0x80, 0x83) + Management Network Address (0x85) +
                // Firmware Build Information (0xC0, vendor-specific
                // range — we use it for our own daemon version).
                let d = build_supported_vpd_pages(
                    PeripheralQualifier::Connected,
                    PeripheralType::MediumChanger,
                    &[0x80, 0x83, 0x85, 0xC0],
                );
                tracing::debug!("INQUIRY VPD page 0x00 response: {} bytes", d.len());
                Ok(ScsiResp {
                    status: ScsiStatus::Good,
                    data_out: limit_len(d, alloc),
                    sense: None,
                })
            }
            0x80 => {
                // Unit Serial Number — 14-byte chassis serial +
                // `_LLNN` partition suffix (always reported, even on
                // non-partitioned libraries — non-partitioned reports
                // as Partition 1). Per-partition serial means
                // initiators bound to different partitions see
                // distinct serials and back-up software treats them
                // as separate libraries (correct).
                let (chassis_ser, part_index) = {
                    let lib = ctx
                        .library
                        .lock()
                        .map_err(|_| anyhow!("library mutex poisoned"))?;
                    (
                        lib.chassis_serial().to_string(),
                        lib.partition_index_one_based(ctx.session_partition),
                    )
                };
                let serial = format!("{}_LL{:02}", chassis_ser, part_index);
                let d = build_unit_serial_number(
                    PeripheralQualifier::Connected,
                    PeripheralType::MediumChanger,
                    &serial,
                    serial.len(),
                );
                tracing::debug!(
                    "INQUIRY VPD page 0x80 response: {} bytes (serial={:?})",
                    d.len(),
                    serial
                );
                Ok(ScsiResp {
                    status: ScsiStatus::Good,
                    data_out: limit_len(d, alloc),
                    sense: None,
                })
            }
            0x83 => {
                // Device Identification — NAA + T10 + LUG (Logical Unit Group).
                let chassis_ser = {
                    let lib = ctx
                        .library
                        .lock()
                        .map_err(|_| anyhow!("library mutex poisoned"))?;
                    lib.chassis_serial().to_string()
                };
                let lug_id = scsi_spc::naa::logical_unit_group(&chassis_ser, ctx.session_partition);
                let naa =
                    scsi_spc::naa::naa3_locally_assigned(&chassis_ser, lun, ctx.session_partition);

                let mut vendor_id = [b' '; 8];
                let v = shared_naming::VENDOR_INQUIRY.as_bytes();
                vendor_id[..v.len()].copy_from_slice(v);
                let mut product_id = [b' '; 16];
                let p = shared_naming::TAPE_LIBRARY_PRODUCT.as_bytes();
                product_id[..p.len()].copy_from_slice(p);
                let mut t10_value = Vec::with_capacity(vendor_id.len() + product_id.len());
                t10_value.extend_from_slice(&vendor_id);
                t10_value.extend_from_slice(&product_id);

                let mut descriptors = Vec::new();
                push_designator(
                    &mut descriptors,
                    CodeSet::Binary,
                    Association::LogicalUnit,
                    DesignatorType::Naa,
                    &naa,
                );
                push_designator(
                    &mut descriptors,
                    CodeSet::Ascii,
                    Association::LogicalUnit,
                    DesignatorType::T10VendorId,
                    &t10_value,
                );
                push_designator(
                    &mut descriptors,
                    CodeSet::Binary,
                    Association::TargetDevice,
                    DesignatorType::LogicalUnitGroup,
                    &lug_id,
                );
                let d = build_device_identification(
                    PeripheralQualifier::Connected,
                    PeripheralType::MediumChanger,
                    &descriptors,
                );

                tracing::debug!(
                    "INQUIRY VPD page 0x83 response: {} bytes (NAA + T10 + LUG)",
                    d.len()
                );
                Ok(ScsiResp {
                    status: ScsiStatus::Good,
                    data_out: limit_len(d, alloc),
                    sense: None,
                })
            }
            0x85 => {
                // Management Network Address (SPC-4 §7.7.5). One
                // network-services descriptor: SERVICE TYPE 0x03
                // (storage management), 4-byte-padded URL body.
                let url = b"http://0.0.0.0:9090/";
                let pad = (4 - (url.len() % 4)) % 4;
                let net_len = url.len() + pad;
                let mut d = vpd_header(
                    PeripheralQualifier::Connected,
                    PeripheralType::MediumChanger,
                    0x85,
                    4 + net_len,
                );
                d.push(0x03); // SERVICE TYPE = storage management
                d.push(0x00); // reserved
                d.extend_from_slice(&(net_len as u16).to_be_bytes());
                d.extend_from_slice(url);
                d.resize(4 + 4 + net_len, 0); // zero pad URL to net_len
                finalize_vpd(&mut d);
                Ok(ScsiResp {
                    status: ScsiStatus::Good,
                    data_out: limit_len(d, alloc),
                    sense: None,
                })
            }
            0xC0 => {
                // Firmware Build Information (vendor-specific page
                // 0xC0). 64-byte ASCII payload, space-padded.
                let fw = {
                    let lib = ctx.library.lock().unwrap_or_else(|e| e.into_inner());
                    lib.drive_firmware().to_string()
                };
                let body = format!("thurvtl {}", fw);
                let mut payload = body.into_bytes();
                payload.resize(64, b' ');
                let mut d = vpd_header(
                    PeripheralQualifier::Connected,
                    PeripheralType::MediumChanger,
                    0xC0,
                    payload.len(),
                );
                d.extend_from_slice(&payload);
                finalize_vpd(&mut d);
                Ok(ScsiResp {
                    status: ScsiStatus::Good,
                    data_out: limit_len(d, alloc),
                    sense: None,
                })
            }
            _ => {
                tracing::debug!(
                    "INQUIRY VPD page 0x{:02x} not supported on changer LUN",
                    page_code
                );
                Ok(ScsiResp::check_condition())
            }
        }
    } else {
        // Standard INQUIRY for the changer LUN — generic SMC-3
        // medium-changer identity. Vendor + product strings come
        // from shared_naming; library firmware string is the
        // revision.
        debug_assert_eq!(
            device_type, 0x08,
            "changer-LUN standard INQUIRY runs only for LUN 0"
        );
        let revision = {
            let lib = ctx.library.lock().unwrap_or_else(|e| e.into_inner());
            lib.drive_firmware().to_string()
        };
        let d = build_inquiry_std(
            PeripheralQualifier::Connected,
            PeripheralType::MediumChanger,
            true, // RMB: medium-changer carries removable cartridges
            Identity {
                vendor: shared_naming::VENDOR_INQUIRY,
                product: shared_naming::TAPE_LIBRARY_PRODUCT,
                revision: &revision,
            },
            CHANGER_INQUIRY_FLAGS,
        );
        tracing::debug!(
            "INQUIRY standard response: {} bytes ({} {}, rev {})",
            d.len(),
            shared_naming::VENDOR_INQUIRY,
            shared_naming::TAPE_LIBRARY_PRODUCT,
            revision
        );
        Ok(ScsiResp {
            status: ScsiStatus::Good,
            data_out: limit_len(d, alloc),
            sense: None,
        })
    }
}

// REPORT LUNS (0xA0) lifted to `scsi_ssc::dispatch::handlers::handle_report_luns`
// in 5.B.6 follow-up step 4. Partition fencing happens via
// `LibraryFacade::drive_ids_in_partition`; the dispatcher prepends
// LUN 0 (changer) and remaps each in-partition drive_id to LUN
// drive_id+1.

/// Append the 20-byte Element Address Assignment page (0x1D). Source of
/// truth for element addressing the host should use.
fn append_changer_page_1d(out: &mut Vec<u8>, cfg: &ElementAddressConfig) {
    let start = out.len();
    out.extend_from_slice(&[
        0x1D, 0x12, // page code, page length (= 18)
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]);
    out[start + 2..start + 4].copy_from_slice(&cfg.transport_start.to_be_bytes());
    out[start + 4..start + 6].copy_from_slice(&1u16.to_be_bytes()); // 1 transport
    out[start + 6..start + 8].copy_from_slice(&cfg.storage_start.to_be_bytes());
    out[start + 8..start + 10].copy_from_slice(&cfg.storage_count.to_be_bytes());
    out[start + 10..start + 12].copy_from_slice(&cfg.import_export_start.to_be_bytes());
    out[start + 12..start + 14].copy_from_slice(&cfg.import_export_count.to_be_bytes());
    out[start + 14..start + 16].copy_from_slice(&cfg.data_transfer_start.to_be_bytes());
    out[start + 16..start + 18].copy_from_slice(&cfg.data_transfer_count.to_be_bytes());
}

/// Append the 4-byte Transport Geometry page (0x1E). Single transport
/// element, no media rotation.
fn append_changer_page_1e(out: &mut Vec<u8>) {
    out.extend_from_slice(&[
        0x1E, 0x02, // page code, page length (= 2)
        0x00, // Rotate=0
        0x00, // member number = 0
    ]);
}

/// Append the 20-byte Device Capabilities page (0x1F). Advertises which
/// MOVE MEDIUM / EXCHANGE MEDIUM combinations the changer supports —
/// backup software reads this to discover topology capabilities. We
/// support all combinations except medium-transport as source/dest
/// (the robot is a conduit, not a holder). READ/WRITE ATTRIBUTE on the
/// changer is unsupported.
fn append_changer_page_1f(out: &mut Vec<u8>) {
    let mut p = [0u8; 20];
    p[0] = 0x1F;
    p[1] = 0x12; // page length = 18
    // byte 2: reserved
    // byte 3: SMC capability flags. S2C=1 (we honour the SMC-2 fields
    // below), VTRP=1 (virtual barcode reader is always present), ACE=0
    // (no auto-cleaning).
    p[3] = 0b0000_0011; // S2C(0) | VTRP(1)
    // byte 4: which element types can store medium (Stor*: DT/I/E/ST/MT
    // in low nibble high-to-low). MT cannot store cartridges.
    p[4] = 0b0000_1110; // DT|I/E|ST = 1, MT = 0
    // bytes 5..8: MOVE MEDIUM matrix, source -> {DT,I/E,ST,MT} in low nibble
    p[5] = 0b0000_0000; // MT -> *  (no moves originate from transport)
    p[6] = 0b0000_1110; // ST -> {DT, I/E, ST}
    p[7] = 0b0000_1110; // I/E -> {DT, I/E, ST}
    p[8] = 0b0000_1110; // DT -> {DT, I/E, ST}
    // byte 9: reserved
    // bytes 10..13: EXCHANGE MEDIUM matrix
    p[10] = 0b0000_0000; // MT exchanges: none
    p[11] = 0b0000_1110; // ST exchanges
    p[12] = 0b0000_1110; // I/E exchanges
    p[13] = 0b0000_1110; // DT exchanges
    // bytes 14..17: READ ATTRIBUTE per source (unsupported)
    // bytes 18..19: WRITE ATTRIBUTE high nibble (unsupported)
    out.extend_from_slice(&p);
}

/// Append the 12-byte Tape Alert page (0x1C). MRIE=0 (poll via LOG SENSE
/// 0x2E), no informational exception reporting.
fn append_changer_page_1c(out: &mut Vec<u8>) {
    out.extend_from_slice(&[
        0x1C, 0x0A, // page code, page length (= 10)
        0x00, // Perf=0, EBF=0, EWasc=0, DExcpt=0, Test=0, LogErr=0
        0x00, // MRIE = 0 (no notification — host polls)
        0, 0, 0, 0, // interval timer = 0
        0, 0, 0, 0, // report count = 0
    ]);
}

/// Append the 32-byte Control Extension subpage 01h (0x0A/0x01). SCSIP=1
/// (SET TIMESTAMP precedence), TCMOS=0, IALUAE=0.
fn append_changer_page_0a(out: &mut Vec<u8>) {
    let mut p = [0u8; 32];
    p[0] = 0x40 | 0x0A; // SPF=1 (subpage form) | page code
    p[1] = 0x01; // subpage code
    p[2] = 0x00; // page length MSB
    p[3] = 0x1C; // page length LSB (= 28 = 32 - 4 header)
    p[4] = 0x02; // SCSIP=1
    // p[5] = initial priority = 0
    out.extend_from_slice(&p);
}

/// Build the changer mode-page payload for `(page_code, subpage_code)`.
/// Per SPC-3, `page_code = 0x3F` with `subpage_code = 0x00` returns every
/// page that has SPF=0 (i.e. excludes Control Extension subpage 01h);
/// `subpage_code = 0xFF` returns subpages too. Returns `None` if no
/// matching page exists.
fn build_changer_mode_pages(
    page_code: u8,
    subpage_code: u8,
    cfg: &ElementAddressConfig,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut emitted = false;
    let all_pages = page_code == 0x3F;
    let include_subpages = subpage_code == 0xFF;

    // Page 0x0A subpage 01h (SPF=1)
    if (page_code == 0x0A && subpage_code == 0x01) || (all_pages && include_subpages) {
        append_changer_page_0a(&mut out);
        emitted = true;
    }
    // Plain (SPF=0) pages — only emit when subpage_code == 0x00 (or 0xFF).
    if subpage_code == 0x00 || subpage_code == 0xFF {
        if page_code == 0x1C || all_pages {
            append_changer_page_1c(&mut out);
            emitted = true;
        }
        if page_code == 0x1D || all_pages {
            append_changer_page_1d(&mut out, cfg);
            emitted = true;
        }
        if page_code == 0x1E || all_pages {
            append_changer_page_1e(&mut out);
            emitted = true;
        }
        if page_code == 0x1F || all_pages {
            append_changer_page_1f(&mut out);
            emitted = true;
        }
    }
    if emitted { Some(out) } else { None }
}

/// Changer-LUN MODE SENSE(6) — emits the thurvtl-specific
/// element-address page set (`build_changer_mode_pages`). Drive-LUN
/// MODE SENSE was lifted to
/// `scsi_ssc::dispatch::handlers::handle_mode_sense_6_drive` in
/// 5.B.6 follow-up step 7; the wrapper only delegates here when the
/// command targets LUN 0.
fn handle_mode_sense_6_changer(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let element_config = ctx.element_config;

    let alloc = pdu_expected_xfer_len(ctx.pdu);
    let page_code = cdb[2] & 0x3F;
    let subpage_code = cdb[3];
    let pc = (cdb[2] >> 6) & 0x03;
    let dbd = (cdb[1] & 0x08) != 0;

    tracing::debug!(
        "MODE SENSE(6): LUN={}, page_code=0x{:02x}, subpage=0x{:02x}, PC={}, DBD={}, alloc={}",
        lun,
        page_code,
        subpage_code,
        pc,
        dbd,
        alloc
    );

    let pages = match build_changer_mode_pages(page_code, subpage_code, element_config) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "MODE SENSE(6): Unsupported page 0x{:02x}/sub 0x{:02x} on changer",
                page_code,
                subpage_code
            );
            return Ok(ScsiResp::check_condition());
        }
    };
    let total = 4 + pages.len();
    let mut d = vec![0u8; total];
    d[0] = (total - 1) as u8; // mode data length
    // d[1..4] = medium type | device-specific | block descriptor length (all 0)
    d[4..].copy_from_slice(&pages);
    tracing::debug!(
        "MODE SENSE(6) changer: page=0x{:02x}/sub 0x{:02x}, {} bytes",
        page_code,
        subpage_code,
        d.len()
    );
    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(d, alloc),
        sense: None,
    })
}

/// Changer-LUN MODE SENSE(10). Same content as
/// [`handle_mode_sense_6_changer`] with the 10-byte CDB form.
fn handle_mode_sense_10_changer(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let element_config = ctx.element_config;

    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
    let page_code = cdb[2] & 0x3F;
    let subpage_code = cdb[3];
    let pages = match build_changer_mode_pages(page_code, subpage_code, element_config) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "MODE SENSE(10): Unsupported page 0x{:02x}/sub 0x{:02x} on changer",
                page_code,
                subpage_code
            );
            return Ok(ScsiResp::check_condition());
        }
    };
    let total = 8 + pages.len();
    let mut d = vec![0u8; total];
    let mode_data_len = (total - 2) as u16;
    d[0..2].copy_from_slice(&mode_data_len.to_be_bytes());
    // d[2..8] = medium type | device-specific | LongLBA | reserved | block desc len (all 0)
    d[8..].copy_from_slice(&pages);
    tracing::debug!(
        "MODE SENSE(10) changer: page=0x{:02x}/sub 0x{:02x}, {} bytes",
        page_code,
        subpage_code,
        d.len()
    );
    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(d, alloc),
        sense: None,
    })
}

/// LOG SENSE (SPC-4) — only the LUN-0 (changer) arm stays here. The
/// drive-LUN path moved to `scsi_ssc::dispatch::handlers::handle_log_sense`
/// in 5.B.6 follow-up step 4 (mfg serial sourced from
/// [`core_mediachanger::TapeDeviceFacade::drive_mfg_serial`]).
fn handle_log_sense(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    debug_assert_eq!(
        ctx.lun, 0,
        "drive LUN LOG SENSE should route to shared dispatcher"
    );

    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
    let page_code = cdb[2] & 0x3F;
    let subpage_code = cdb[3];
    let pc = (cdb[2] >> 6) & 0x03;

    match scsi::log_pages::handle_changer_log_sense(page_code, subpage_code, pc) {
        Ok(data) => Ok(ScsiResp {
            status: ScsiStatus::Good,
            data_out: limit_len(data, alloc),
            sense: None,
        }),
        Err(e) => {
            tracing::warn!("LOG SENSE (changer) error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

// MAINTENANCE IN (0xA3) lifted to `scsi_ssc::dispatch::handlers` in
// 5.B.6 follow-up step 5 — REPORT SUPPORTED OPCODES, READ DYNAMIC
// RUNTIME ATTRIBUTE, READ LOGGED-IN HOST TABLE, REPORT SUPPORTED TASK
// MGMT FUNCTIONS, REPORT TARGET PORT GROUPS (ALUA), REPORT TIMESTAMP
// — all drive-LUN with no library access.

// --- tiny helpers ---
// `u24` / `put_u24` lifted to `shared_iscsi::transport` (BHS field
// pack/unpack). The remaining helpers below — `put_be_u32`,
// `pdu_expected_xfer_len`, `limit_len` — are read by the per-opcode
// handlers below and stay here.
// Text key/value parsers (`parse_text_kv`, `push_kv`) lifted to
// `shared_iscsi::transport` — login phase + SendTargets are the
// only consumers and they live there now.

// `preview` (hex byte preview) and `format_text_data` (debug dump
// of `key=value\0` sequences) lifted to `shared_iscsi::transport`
// — only the lifted PDU/login code consumed them.

// Metrics server moved to unified HTTP server in Phase 6

// Transport-layer invariant tests (R2T TTT collision avoidance,
// FIRST_BURST / MAX_BURST bounds) lifted to
// `shared_iscsi::transport::tests` in Step 3c phase 2 alongside the
// PDU/R2T code they cover.

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg() -> ElementAddressConfig {
        ElementAddressConfig::new(0, 1001, 40, 101, 5, 1, 3)
    }

    #[test]
    fn changer_page_1d_encodes_element_topology() {
        let cfg = sample_cfg();
        let mut out = Vec::new();
        append_changer_page_1d(&mut out, &cfg);
        // page code 0x1D, page length 0x12 (18), total 20 bytes.
        assert_eq!(out.len(), 20);
        assert_eq!(out[0], 0x1D);
        assert_eq!(out[1], 0x12);
        // storage_start at bytes 6..8 big-endian.
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 1001);
        assert_eq!(u16::from_be_bytes([out[8], out[9]]), 40);
        // data_transfer_count at bytes 16..18.
        assert_eq!(u16::from_be_bytes([out[16], out[17]]), 3);
    }

    #[test]
    fn changer_page_1e_is_fixed_four_bytes() {
        let mut out = Vec::new();
        append_changer_page_1e(&mut out);
        assert_eq!(out, vec![0x1E, 0x02, 0x00, 0x00]);
    }

    #[test]
    fn changer_page_1f_is_twenty_bytes() {
        let mut out = Vec::new();
        append_changer_page_1f(&mut out);
        assert_eq!(out.len(), 20);
        assert_eq!(out[0], 0x1F);
        assert_eq!(out[1], 0x12);
    }

    #[test]
    fn changer_page_1c_is_tape_alert_twelve_bytes() {
        let mut out = Vec::new();
        append_changer_page_1c(&mut out);
        assert_eq!(out.len(), 12);
        assert_eq!(out[0], 0x1C);
        assert_eq!(out[1], 0x0A);
    }

    #[test]
    fn changer_page_0a_sets_subpage_form_bit() {
        let mut out = Vec::new();
        append_changer_page_0a(&mut out);
        assert_eq!(out.len(), 32);
        // SPF bit (0x40) is set on byte 0.
        assert_eq!(out[0], 0x40 | 0x0A);
        assert_eq!(out[1], 0x01);
    }

    #[test]
    fn build_changer_mode_pages_single_page() {
        let cfg = sample_cfg();
        let page = build_changer_mode_pages(0x1D, 0x00, &cfg).expect("page 1D exists");
        assert_eq!(page[0], 0x1D);
        assert_eq!(page.len(), 20);
    }

    #[test]
    fn build_changer_mode_pages_all_pages_no_subpages() {
        let cfg = sample_cfg();
        let pages = build_changer_mode_pages(0x3F, 0x00, &cfg).expect("all pages");
        // 0x1C(12) + 0x1D(20) + 0x1E(4) + 0x1F(20) = 56; no 0x0A subpage.
        assert_eq!(pages.len(), 56);
    }

    #[test]
    fn build_changer_mode_pages_all_pages_with_subpages() {
        let cfg = sample_cfg();
        let pages = build_changer_mode_pages(0x3F, 0xFF, &cfg).expect("all pages + subpages");
        // Includes the 32-byte 0x0A subpage on top of the 56 above.
        assert_eq!(pages.len(), 88);
    }

    #[test]
    fn build_changer_mode_pages_subpage_only() {
        let cfg = sample_cfg();
        let page = build_changer_mode_pages(0x0A, 0x01, &cfg).expect("0x0A subpage");
        assert_eq!(page.len(), 32);
        assert_eq!(page[0], 0x40 | 0x0A);
    }

    #[test]
    fn build_changer_mode_pages_unknown_page_is_none() {
        let cfg = sample_cfg();
        assert!(build_changer_mode_pages(0x55, 0x00, &cfg).is_none());
    }

    #[test]
    fn build_changer_mode_pages_plain_page_with_subpage_query_is_none() {
        let cfg = sample_cfg();
        // A plain (SPF=0) page requested with a specific non-zero,
        // non-0xFF subpage code yields nothing.
        assert!(build_changer_mode_pages(0x1D, 0x07, &cfg).is_none());
    }
}
