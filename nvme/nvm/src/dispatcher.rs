// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVM command-set dispatcher.
//!
//! Holds an immutable reference to the namespace registry (via
//! `Arc<dyn NamespaceLookup>`); concurrent dispatches are safe
//! (`PageCache` methods take `&self`).
//!
//! Mirrors `scsi_sbc::SbcScsiDispatcher` opcode-table style: a
//! single `match` on the parsed opcode, each arm routing to a
//! per-opcode handler that translates SQE fields into `PageCache`
//! byte-grain calls.
//!
//! Admin commands handled: Identify (CNS = 0x00 Namespace /
//! 0x01 Controller / 0x02 Active NS List / 0x03 NS ID Descriptor
//! List / 0x06 I/O Command Set Identify Controller), Get Features /
//! Set Features (FID 0x07 Number of Queues + FID 0x0B Async Event
//! Configuration + FID 0x0F Keep Alive Timer + FID 0x82 Reservation
//! Notification Mask), Get Log Page (Error / SMART / FW Slot /
//! 0x04 Changed Namespace List / 0x80 Reservation Notification),
//! Keep Alive, Abort. AER returns Invalid Opcode (NVMe Base §5.2 lets
//! that terminate the host's AER loop cleanly).
//!
//! NVM I/O commands: Read, Write, Flush, Compare, Write Zeroes,
//! DSM Deallocate, Verify, fused Compare+Write (the transport
//! pairs the two SQEs and invokes
//! `handle_fused_compare_write`).

use std::sync::Arc;

use async_trait::async_trait;

use core_block::uploader::{FaultClass, UploaderError};
use core_block::{CawLocks, PageCache, RangeError};
use nvme_base::identify::CNS;
use nvme_base::{AdminOpcode, Cqe, IdentifyController, IdentifyNamespace, StatusField};
use scsi_spc::reservations::{RegistrantId, ReservationManager};

use crate::NamespaceLookup;
use crate::aer::ControllerRegistry;
use crate::handler::{AdminCommand, IoCommand, NvmeCommandHandler, NvmeResponse};
use crate::opcode::NvmOpcode;

/// Default I/O queue cap advertised via Get/Set Features
/// (FID 0x07 Number of Queues). Each I/O queue corresponds to one
/// NVMe/TCP connection; 64 matches typical kernel host expectations
/// without being so high that an aggressive host floods us with
/// connections.
const DEFAULT_NUM_IO_QUEUES: u16 = 64;

/// Per-hostnqn admission check. `None` admission slice = no fence
/// (legacy / no PSK admission entry — see-everything). Otherwise the
/// NSID's volume name must be in the slice. NSID 0 is reserved and
/// never admitted; the dispatcher catches that before this call.
fn nsid_admitted(registry: &dyn NamespaceLookup, nsid: u32, allow: Option<&[String]>) -> bool {
    match allow {
        // No fence (legacy / no PSK admission entry — see-everything).
        None => true,
        // Non-allocating in-place compare under one registry lock —
        // avoids the per-I/O volume-name String clone (issue #244).
        Some(names) => registry.is_admitted(nsid, names),
    }
}

/// Feature Identifier for Number of Queues (NVMe Base §5.21.1.7).
const FID_NUMBER_OF_QUEUES: u8 = 0x07;

/// Feature Identifier for Keep Alive Timer (NVMe Base §5.21.1.15).
/// Linux nvme-tcp's `nvme_set_keep_alive` issues Set Features 0x0F
/// with the negotiated KATO (in ms) on every controller bring-up;
/// without this handler, Identify completes but the very next admin
/// command fails and the host aborts the session.
const FID_KEEP_ALIVE_TIMER: u8 = 0x0F;

/// Feature Identifier for Reservation Notification Mask (NVMe NVM
/// Command Set). Per-namespace; CDW11 bits 1/2/3 mask Registration
/// Preempted / Reservation Released / Reservation Preempted
/// notifications respectively (0 = all enabled). The host's
/// enable/disable knob for reservation async events.
const FID_RESERVATION_NOTIF_MASK: u8 = 0x82;

/// Feature Identifier for Async Event Configuration (NVMe Base
/// §5.21.1.11). Controller-wide; CDW11 bit 8
/// (`AEN_CFG_NAMESPACE_ATTRIBUTE`) enables Namespace Attribute Changed
/// notices. The host's enable/disable knob for namespace-change async
/// events — distinct from FID 0x82, which gates reservation notices.
const FID_ASYNC_EVENT_CONFIG: u8 = 0x0B;

pub struct NvmeNvmDispatcher {
    registry: Arc<dyn NamespaceLookup>,
    subnqn: String,
    /// Static Identify Controller fields. Built once at boot from
    /// `shared_naming::DISK` + the daemon version. `nn` is filled
    /// in at Identify time from the live registry so dynamic
    /// namespace add / remove doesn't require rebuilding this.
    controller_sn: String,
    controller_mn: String,
    controller_fr: String,
    /// Shared reservation state machine, keyed by LUN. Injected by
    /// the daemon (`load_from(<data_dir>/reservations.json, ...)`) so
    /// CPTPL-set reservations survive a daemon restart (issue #57); the
    /// test fixtures pass an in-memory `new()` manager (PTPL not
    /// capable). The daemon hands the *same* `Arc` to the SBC
    /// dispatcher, so under a dual-transport export (issue #66) a
    /// reservation taken over iSCSI fences NVMe writes and vice-versa.
    reservations: Arc<ReservationManager>,
    /// Per-subsystem controller registry + AER hub. Shared (one `Arc`)
    /// with the NVMe/TCP transport: the transport allocates CNTLIDs at
    /// Connect, and an event raised here on a reservation op completes
    /// an AER parked on a controller's admin connection. Built once at
    /// boot alongside the dispatcher.
    aer: Arc<ControllerRegistry>,
    /// Per-LUN COMPARE AND WRITE / fused Compare+Write serialization.
    /// Defaults to a per-dispatcher registry; the daemon injects the
    /// *same* `Arc` it gives the SBC dispatcher so a fused CAW over NVMe
    /// serializes against a COMPARE AND WRITE over iSCSI on the same
    /// volume (issue #128).
    caw_locks: Arc<CawLocks>,
}

impl NvmeNvmDispatcher {
    /// Construct with a namespace registry and Identify Controller
    /// identity strings. The daemon supplies SN (volume-set
    /// fingerprint), MN (product name from shared-naming), FR
    /// (daemon version).
    pub fn new(
        registry: Arc<dyn NamespaceLookup>,
        subnqn: String,
        controller_sn: String,
        controller_mn: String,
        controller_fr: String,
        aer: Arc<ControllerRegistry>,
        reservations: Arc<ReservationManager>,
    ) -> Self {
        Self {
            registry,
            subnqn,
            controller_sn,
            controller_mn,
            controller_fr,
            reservations,
            aer,
            caw_locks: Arc::new(CawLocks::new()),
        }
    }

    /// Inject a shared per-LUN CAW lock registry so fused Compare+Write
    /// over NVMe serializes against COMPARE AND WRITE over iSCSI on the
    /// same volume under a dual-transport export (issue #128). The daemon
    /// hands the *same* `Arc<CawLocks>` to the SBC dispatcher.
    pub fn with_caw_locks(mut self, caw_locks: Arc<CawLocks>) -> Self {
        self.caw_locks = caw_locks;
        self
    }

    async fn dispatch_io(&self, cmd: IoCommand<'_>) -> NvmeResponse {
        let sqe = &cmd.sqe;
        let cid = sqe.cid;
        let Some(opcode) = NvmOpcode::from_u8(sqe.opcode) else {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode()));
        };
        // Per-hostnqn namespace admission: a non-admitted NSID is
        // surfaced to this connection as "no namespace", same shape
        // as an unknown NSID.
        if !nsid_admitted(self.registry.as_ref(), sqe.nsid, cmd.session_volumes) {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_namespace()));
        }
        let Some(cache) = self.registry.get(sqe.nsid) else {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_namespace()));
        };

        // Reservation commands are never conflict-gated (their own key
        // check is the only conflict path); they dispatch directly.
        // Reservation Register / Acquire / Release may fence another
        // host; the resulting LID 0x80 + AER fan-out is driven by the
        // shared `ReservationManager` observer hook (issue #67), so it
        // fires regardless of which transport originated the change —
        // the adapter here just returns the completion.
        let host_id = cmd.host_id.unwrap_or([0u8; 16]);
        match opcode {
            NvmOpcode::ReservationRegister => {
                return crate::reservations::reservation_register(
                    &self.reservations,
                    sqe.nsid,
                    host_id,
                    sqe,
                    cmd.data_out,
                );
            }
            NvmOpcode::ReservationReport => {
                return crate::reservations::reservation_report(
                    &self.reservations,
                    sqe.nsid,
                    sqe,
                    cmd.data_in_max,
                    |host_id| self.aer.representative_cntlid(host_id),
                );
            }
            NvmOpcode::ReservationAcquire => {
                return crate::reservations::reservation_acquire(
                    &self.reservations,
                    sqe.nsid,
                    host_id,
                    sqe,
                    cmd.data_out,
                );
            }
            NvmOpcode::ReservationRelease => {
                return crate::reservations::reservation_release(
                    &self.reservations,
                    sqe.nsid,
                    host_id,
                    sqe,
                    cmd.data_out,
                );
            }
            _ => {}
        }

        // Enforcement gate: a non-holder's data-path command is
        // rejected with Reservation Conflict. Read-side opcodes
        // (Read / Compare / Verify) consult `allow_read`; write-side
        // (Write / Write Zeroes / DSM-deallocate) consult
        // `allow_write`. Flush is deliberately NOT gated — the NVM
        // Command Set does not restrict it (it commits already-
        // accepted writes), which differs from SCSI SYNCHRONIZE CACHE.
        let registrant = RegistrantId::nvme(host_id);
        let lun = crate::reservations::nsid_to_lun(sqe.nsid);
        let denied = match opcode {
            NvmOpcode::Read | NvmOpcode::Compare | NvmOpcode::Verify => {
                !self.reservations.allow_read(lun, &registrant)
            }
            NvmOpcode::Write | NvmOpcode::WriteZeroes | NvmOpcode::DatasetManagement => {
                !self.reservations.allow_write(lun, &registrant)
            }
            _ => false,
        };
        if denied {
            return NvmeResponse::just(Cqe::failure(
                cid,
                0,
                0,
                StatusField::reservation_conflict(),
            ));
        }

        // WORM enforcement: a write-class command against a write-once
        // namespace is refused with Namespace Is Write Protected — the
        // NVMe analog of the SBC WRITE PROTECTED gate
        // (`scsi_sbc::data_path`), so the WORM guarantee holds on both
        // transports (issue #79). `Write` / `WriteZeroes` always mutate
        // and are gated here; DSM is gated inside `cmd_dsm` on its
        // deallocate (AD) branch only, so advisory non-deallocate hints
        // still no-op through. Reads / Verify / Flush are unaffected.
        if matches!(opcode, NvmOpcode::Write | NvmOpcode::WriteZeroes) && cache.manifest().worm {
            return NvmeResponse::just(Cqe::failure(
                cid,
                0,
                0,
                StatusField::namespace_write_protected(),
            ));
        }

        match opcode {
            NvmOpcode::Flush => self.cmd_flush(cid, &cache).await,
            NvmOpcode::Write => self.cmd_write(cid, sqe, cmd.data_out, &cache).await,
            NvmOpcode::Read => self.cmd_read(cid, sqe, cmd.data_in_max, &cache).await,
            NvmOpcode::Compare => self.cmd_compare(cid, sqe, cmd.data_out, &cache).await,
            NvmOpcode::WriteZeroes => self.cmd_write_zeroes(cid, sqe, &cache).await,
            NvmOpcode::DatasetManagement => self.cmd_dsm(cid, sqe, cmd.data_out, &cache).await,
            NvmOpcode::Verify => self.cmd_verify(cid, sqe, &cache).await,
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode())),
        }
    }

    async fn dispatch_admin(&self, cmd: AdminCommand<'_>) -> NvmeResponse {
        let sqe = &cmd.sqe;
        let cid = sqe.cid;
        let Some(opcode) = AdminOpcode::from_u8(sqe.opcode) else {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode()));
        };
        match opcode {
            AdminOpcode::Identify => {
                self.cmd_identify(cid, sqe, cmd.session_volumes, cmd.cntlid)
                    .await
            }
            AdminOpcode::GetFeatures => self.cmd_get_features(cid, sqe, cmd.cntlid),
            AdminOpcode::SetFeatures => self.cmd_set_features(cid, sqe, cmd.cntlid),
            AdminOpcode::GetLogPage => self.cmd_get_log_page(cid, sqe, cmd.cntlid),
            AdminOpcode::KeepAlive => {
                // No-op — host pings us on the admin queue to confirm
                // the controller is alive. We have nothing to update
                // on the controller side today; future Discovery /
                // AER work would touch a per-connection timer here.
                NvmeResponse::just(Cqe::success(cid, 0, 0, 0))
            }
            AdminOpcode::AsyncEventRequest => {
                // AER never completes synchronously — it parks until an
                // event fires. The NVMe/TCP transport intercepts AER
                // before this dispatch path (it owns the connection's
                // writer and the per-subsystem `ControllerRegistry`), so a real
                // host never reaches here. This arm only covers a
                // non-transport caller (e.g. a unit test); there is no
                // event to report on this synchronous path, so reflect
                // "no immediate completion" as Invalid Opcode.
                NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode()))
            }
            AdminOpcode::Abort => {
                // Best-effort: we don't queue commands at the
                // dispatcher level, so by the time the abort lands
                // the targeted command has either already completed
                // or never existed. CQE.DW0 bit 0 = 1 means "not
                // aborted" (host treats this as "command completed
                // normally").
                NvmeResponse::just(Cqe::success(cid, 0, 0, 1))
            }
            // Create / Delete IO SQ / CQ are PCIe-only — in fabrics,
            // queue creation happens via Connect on a new TCP
            // connection. Returning Invalid Opcode is the correct
            // response (host shouldn't be sending them on fabrics).
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode())),
        }
    }

    fn cmd_get_features(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        cntlid: Option<u16>,
    ) -> NvmeResponse {
        // CDW10[7:0] FID, [10:8] SEL (0=current, 1=default, 2=saved,
        // 3=supported). We treat all SELs as "current" for now —
        // host most commonly uses SEL=0 and ours have no separate
        // default / saved state to distinguish.
        let fid = (sqe.cdw10 & 0xFF) as u8;
        match fid {
            FID_NUMBER_OF_QUEUES => {
                // Per-controller (issue #245): report this controller's
                // negotiated grant, or the cap if it hasn't negotiated.
                let cntlid = cntlid.unwrap_or(0);
                let cap = DEFAULT_NUM_IO_QUEUES - 1;
                let (nsq, ncq) = self.aer.get_num_io_queues(cntlid).unwrap_or((cap, cap));
                let dw0 = u32::from(ncq) | (u32::from(nsq) << 16);
                NvmeResponse::just(Cqe::success(cid, 0, 0, dw0))
            }
            FID_KEEP_ALIVE_TIMER => {
                let cntlid = cntlid.unwrap_or(0);
                let kato = self.aer.get_kato_ms(cntlid);
                NvmeResponse::just(Cqe::success(cid, 0, 0, kato))
            }
            FID_RESERVATION_NOTIF_MASK => {
                // Per-controller, per-namespace mask (CDW1 NSID).
                // Outside a real connection (no CNTLID) report
                // all-enabled.
                let cntlid = cntlid.unwrap_or(0);
                let mask = self.aer.get_mask(cntlid, sqe.nsid);
                NvmeResponse::just(Cqe::success(cid, 0, 0, mask))
            }
            FID_ASYNC_EVENT_CONFIG => {
                // Controller-wide. Outside a real connection (no CNTLID)
                // report "nothing enabled."
                let cntlid = cntlid.unwrap_or(0);
                let config = self.aer.get_aen_config(cntlid);
                NvmeResponse::just(Cqe::success(cid, 0, 0, config))
            }
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field())),
        }
    }

    fn cmd_set_features(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        cntlid: Option<u16>,
    ) -> NvmeResponse {
        let fid = (sqe.cdw10 & 0xFF) as u8;
        tracing::debug!(
            fid = format!("0x{:02X}", fid),
            cdw11 = format!("0x{:08X}", sqe.cdw11),
            "nvme: Set Features",
        );
        match fid {
            FID_RESERVATION_NOTIF_MASK => {
                // Per-controller, per-namespace (CDW1 NSID). CDW11 bits
                // 1/2/3 suppress the three reservation notification
                // classes. Store keyed by (CNTLID, NSID) so one
                // controller's masking can't silence another's
                // notifications. Echo the stored value in CDW0.
                let cntlid = cntlid.unwrap_or(0);
                self.aer.set_mask(cntlid, sqe.nsid, sqe.cdw11);
                NvmeResponse::just(Cqe::success(cid, 0, 0, sqe.cdw11))
            }
            FID_ASYNC_EVENT_CONFIG => {
                // Controller-wide (CDW11). Bit 8 enables Namespace
                // Attribute Changed notices; store keyed by CNTLID so
                // one controller's config can't change another's. Echo
                // the stored value in CDW0.
                let cntlid = cntlid.unwrap_or(0);
                self.aer.set_aen_config(cntlid, sqe.cdw11);
                NvmeResponse::just(Cqe::success(cid, 0, 0, sqe.cdw11))
            }
            FID_NUMBER_OF_QUEUES => {
                // CDW11: requested NCQR | (NSQR << 16) — both
                // zero-based. We grant the minimum of what the host
                // requested and our cap, store, and echo back in
                // CDW0.
                let req_ncq = (sqe.cdw11 & 0xFFFF) as u16;
                let req_nsq = ((sqe.cdw11 >> 16) & 0xFFFF) as u16;
                let cap = DEFAULT_NUM_IO_QUEUES - 1;
                let granted_ncq = req_ncq.min(cap);
                let granted_nsq = req_nsq.min(cap);
                // Per-controller (issue #245): one host's grant must not
                // clobber another's.
                let cntlid = cntlid.unwrap_or(0);
                self.aer.set_num_io_queues(cntlid, granted_nsq, granted_ncq);
                let dw0 = u32::from(granted_ncq) | (u32::from(granted_nsq) << 16);
                NvmeResponse::just(Cqe::success(cid, 0, 0, dw0))
            }
            FID_KEEP_ALIVE_TIMER => {
                // CDW11 carries the timer in ms (Linux nvme-tcp's
                // nvme_set_keep_alive passes `kato * 1000`). We don't
                // run a watchdog — Keep Alive admin commands are
                // acknowledged unconditionally — so the value is stored
                // per-controller (issue #245) only for Get Features
                // symmetry.
                let cntlid = cntlid.unwrap_or(0);
                self.aer.set_kato_ms(cntlid, sqe.cdw11);
                NvmeResponse::just(Cqe::success(cid, 0, 0, sqe.cdw11))
            }
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field())),
        }
    }

    fn cmd_get_log_page(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        cntlid: Option<u16>,
    ) -> NvmeResponse {
        // CDW10[7:0] LID, [15:8] LSP, [31:16] NUMDL (zero-based).
        // CDW11[15:0] NUMDU (zero-based). Total dwords = (NUMDL |
        // (NUMDU << 16)) + 1.
        let lid = (sqe.cdw10 & 0xFF) as u8;
        let numdl = (sqe.cdw10 >> 16) & 0xFFFF;
        let numdu = sqe.cdw11 & 0xFFFF;
        let total_dwords = numdl | (numdu << 16);
        let total_bytes = total_dwords.saturating_add(1).saturating_mul(4) as usize;
        // CDW10 bit 15 = RAE (Retain Asynchronous Event): when set, the
        // host reads the log to inspect it without clearing, so the
        // event-bearing pages must not drain their per-controller state
        // (issue #247).
        let rae = (sqe.cdw10 >> 15) & 1 == 1;
        let payload: Vec<u8> = match lid {
            nvme_base::log_page::lid::ERROR_INFO => {
                let entry = nvme_base::log_page::error_info_zero_entry();
                entry[..total_bytes.min(entry.len())].to_vec()
            }
            nvme_base::log_page::lid::SMART_HEALTH => {
                let log = nvme_base::log_page::smart_health();
                log[..total_bytes.min(log.len())].to_vec()
            }
            nvme_base::log_page::lid::FIRMWARE_SLOT => {
                let log = nvme_base::log_page::firmware_slot_info(&self.controller_fr);
                log[..total_bytes.min(log.len())].to_vec()
            }
            nvme_base::log_page::lid::RESERVATION_NOTIFICATION => {
                // Read this controller's oldest reservation notification
                // (or an all-zero empty page when the queue is drained).
                // Only consume it when the host did NOT set RAE and gave a
                // buffer large enough for the full fixed-size page —
                // otherwise the notification would be silently lost
                // (issue #247). Outside a real connection (no CNTLID)
                // there is no per-controller queue — the empty page.
                let cntlid = cntlid.unwrap_or(0);
                let consume = !rae
                    && total_bytes >= nvme_base::log_page::RESERVATION_NOTIFICATION_LEN;
                let log = self.aer.take_log_entry(cntlid, consume);
                log[..total_bytes.min(log.len())].to_vec()
            }
            nvme_base::log_page::lid::CHANGED_NAMESPACE_LIST => {
                // Read this controller's Changed Namespace List. Only
                // clear it when the host did NOT set RAE and gave a buffer
                // large enough for the full fixed-size page, so a short /
                // RAE read doesn't drop the change list (issue #247).
                // Outside a real connection (no CNTLID) there is no
                // per-controller list — the empty (all-zero) page.
                let cntlid = cntlid.unwrap_or(0);
                let consume = !rae
                    && total_bytes >= nvme_base::log_page::CHANGED_NAMESPACE_LIST_LEN;
                let nsids = self.aer.take_changed_namespaces(cntlid, consume);
                let log = nvme_base::log_page::changed_namespace_list(&nsids);
                log[..total_bytes.min(log.len())].to_vec()
            }
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload)
    }

    async fn cmd_identify(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        session_volumes: Option<&[String]>,
        cntlid: Option<u16>,
    ) -> NvmeResponse {
        let raw_cns = (sqe.cdw10 & 0xFF) as u8;
        let Some(cns) = CNS::from_u8(raw_cns) else {
            tracing::warn!(
                cns = format!("0x{:02X}", raw_cns),
                nsid = sqe.nsid,
                "nvme: Identify with unsupported CNS - returning invalid_field",
            );
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
        };
        tracing::debug!(cns = ?cns, nsid = sqe.nsid, "nvme: Identify");
        match cns {
            CNS::Controller => {
                let nn = self.registry.active_namespaces().len() as u32;
                // FieldTooLong on any of SN/MN/FR/SUBNQN is a daemon
                // boot-time bug, not a host condition — but it surfaces
                // here as SC=0x06 which Linux nvme-cli renders as the
                // very opaque "Identify Controller failed (6)". Log
                // before returning so the next overflow is debuggable
                // from the daemon log alone.
                // Report the CNTLID assigned to this controller at
                // Connect. Outside a real connection (tests) fall back
                // to 1.
                let ic = match IdentifyController::new(
                    self.controller_sn.clone(),
                    self.controller_mn.clone(),
                    self.controller_fr.clone(),
                    cntlid.unwrap_or(1),
                    nn,
                    self.subnqn.clone(),
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "nvme: Identify Controller build failed");
                        return NvmeResponse::just(Cqe::failure(
                            cid,
                            0,
                            0,
                            StatusField::internal_error(),
                        ));
                    }
                };
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), ic.to_bytes().to_vec())
            }
            CNS::Namespace => {
                if !nsid_admitted(self.registry.as_ref(), sqe.nsid, session_volumes) {
                    return NvmeResponse::just(Cqe::failure(
                        cid,
                        0,
                        0,
                        StatusField::invalid_namespace(),
                    ));
                }
                let Some(cache) = self.registry.get(sqe.nsid) else {
                    return NvmeResponse::just(Cqe::failure(
                        cid,
                        0,
                        0,
                        StatusField::invalid_namespace(),
                    ));
                };
                let size_bytes = cache.size_bytes();
                let lba_bytes = cache.sector_size() as u32;
                let nguid = cache.manifest().uuid;
                let mut id = match IdentifyNamespace::from_volume(size_bytes, lba_bytes, nguid) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            nsid = sqe.nsid,
                            size_bytes,
                            lba_bytes,
                            "nvme: Identify Namespace build failed",
                        );
                        return NvmeResponse::just(Cqe::failure(
                            cid,
                            0,
                            0,
                            StatusField::internal_error(),
                        ));
                    }
                };
                // RESCAP bit 0 (PTPL) follows whether reservation state
                // is actually persisted across power loss (issue #57) —
                // set only when the manager is persistence-capable so the
                // advertised capability can't diverge from behavior.
                if self.reservations.ptpl_capable() {
                    id.rescap |= 0x01;
                }
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), id.to_bytes().to_vec())
            }
            CNS::ActiveNamespaceList => {
                let mut nsids = self.registry.active_namespaces_filtered(session_volumes);
                nsids.sort_unstable();
                let payload = nvme_base::identify::active_namespace_list(&nsids, sqe.nsid);
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload.to_vec())
            }
            CNS::NamespaceIdDescList => {
                // Validate NSID exists; per spec the Descriptor list
                // is per-namespace and an invalid NSID should error.
                // A non-admitted NSID is reported as invalid — same
                // shape as an unknown one.
                if !nsid_admitted(self.registry.as_ref(), sqe.nsid, session_volumes) {
                    return NvmeResponse::just(Cqe::failure(
                        cid,
                        0,
                        0,
                        StatusField::invalid_namespace(),
                    ));
                }
                let Some(cache) = self.registry.get(sqe.nsid) else {
                    return NvmeResponse::just(Cqe::failure(
                        cid,
                        0,
                        0,
                        StatusField::invalid_namespace(),
                    ));
                };
                let nguid = cache.manifest().uuid;
                let payload = nvme_base::identify::namespace_id_descriptor_list(nguid);
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload.to_vec())
            }
            CNS::IoCommandSetIdentifyController => {
                // 4 KiB of zeros = "no specific I/O Command Set
                // limits" — the right reply for a controller that
                // doesn't advertise any extra NVM-Command-Set caps.
                let payload = vec![0u8; nvme_base::IDENTIFY_DATA_SIZE];
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload)
            }
        }
    }

    async fn cmd_flush(&self, cid: u16, cache: &PageCache) -> NvmeResponse {
        // NVMe Flush over the whole namespace: synchronize the full
        // address range. core-block's synchronize_bytes(0, size) is
        // the right fence.
        let size = cache.size_bytes();
        match cache.synchronize_bytes(0, size).await {
            Ok(()) => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "flush failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, write_failure_status(&e)))
            }
        }
    }

    async fn cmd_read(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        data_in_max: u32,
        cache: &PageCache,
    ) -> NvmeResponse {
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let (byte_off, len_bytes) = match resolve_range(cid, cache, slba, u64::from(nlb)) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        if len_bytes > u64::from(data_in_max) {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
        }
        match cache.read_bytes(byte_off, len_bytes as usize).await {
            Ok(buf) => NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), buf),
            Err(e) => {
                tracing::warn!(error = %e, "read failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, read_failure_status(&e)))
            }
        }
    }

    async fn cmd_write(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        data_out: Option<&[u8]>,
        cache: &PageCache,
    ) -> NvmeResponse {
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let (byte_off, len_bytes) = match resolve_range(cid, cache, slba, u64::from(nlb)) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let payload = match data_out {
            Some(p) if p.len() as u64 == len_bytes => p,
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        match cache.write_bytes(byte_off, payload).await {
            Ok(()) => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "write failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, write_failure_status(&e)))
            }
        }
    }

    async fn cmd_compare(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        data_out: Option<&[u8]>,
        cache: &PageCache,
    ) -> NvmeResponse {
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let (byte_off, len_bytes) = match resolve_range(cid, cache, slba, u64::from(nlb)) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let expected = match data_out {
            Some(p) if p.len() as u64 == len_bytes => p,
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        match cache.read_bytes(byte_off, len_bytes as usize).await {
            Ok(actual) => {
                if actual == expected {
                    NvmeResponse::just(Cqe::success(cid, 0, 0, 0))
                } else {
                    NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::compare_failure()))
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "compare read failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, read_failure_status(&e)))
            }
        }
    }

    async fn cmd_write_zeroes(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        cache: &PageCache,
    ) -> NvmeResponse {
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let (byte_off, len_bytes) = match resolve_range(cid, cache, slba, u64::from(nlb)) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        // DEAC (CDW12 bit 25): the host asked to *deallocate* rather than
        // physically zero-fill. core-block's `unmap_bytes` preserves the
        // read-back contract (unallocated pages read as zero), so honor
        // it via the fast index-only path — ~N index updates instead of
        // pushing up to 256 MiB of zeros through the RMW/hash/dedup/flush
        // pipeline (issue #179; the SBC twin does this for WRITE
        // SAME+UNMAP). Linux sets DEAC for fallocate(ZERO_RANGE) /
        // blkdiscard -z / filesystem zeroing. Skip on WORM (unmap mutates
        // the medium) so WORM behavior matches the zero-write path below.
        let deac = (sqe.cdw12 & (1 << 25)) != 0;
        if deac && !cache.manifest().worm {
            return match cache.unmap_bytes(byte_off, len_bytes).await {
                Ok(()) => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
                Err(e) => {
                    tracing::warn!(error = %e, "write-zeroes deallocate failed");
                    NvmeResponse::just(Cqe::failure(cid, 0, 0, write_failure_status(&e)))
                }
            };
        }

        // Explicit zero-fill. Cap the in-memory zero buffer at ~16 MiB
        // and reuse it across iterations (hoisted out of the loop) so a
        // multi-GB Write Zeroes neither allocates one massive block nor a
        // fresh 16 MiB buffer per page-cohort — same chunking as SBC's
        // WRITE SAME.
        const TARGET_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
        let zeros = vec![0u8; len_bytes.min(TARGET_CHUNK_BYTES) as usize];
        let mut remaining = len_bytes;
        let mut cursor = byte_off;
        while remaining > 0 {
            let this = remaining.min(TARGET_CHUNK_BYTES) as usize;
            if let Err(e) = cache.write_bytes(cursor, &zeros[..this]).await {
                tracing::warn!(error = %e, "write-zeroes failed");
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, write_failure_status(&e)));
            }
            cursor += this as u64;
            remaining -= this as u64;
        }
        NvmeResponse::just(Cqe::success(cid, 0, 0, 0))
    }

    async fn cmd_dsm(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        data_out: Option<&[u8]>,
        cache: &PageCache,
    ) -> NvmeResponse {
        // DSM is a multi-range command; only the AD (Attribute -
        // Deallocate) bit in CDW11 is meaningful for our backend.
        // Range descriptors live in the data-out payload as 16-byte
        // entries: { ctx_attrs(4), nlb(4), slba(8) }, NR = CDW10[7:0]
        // (zero-based).
        let ad = (sqe.cdw11 & 0x0000_0004) != 0;
        if !ad {
            // We don't implement IDR (Integral Dataset for Read) /
            // IDW (Integral Dataset for Write) hints — return success
            // for those so the host's hint pass is a no-op.
            return NvmeResponse::just(Cqe::success(cid, 0, 0, 0));
        }
        // A DSM Deallocate mutates the medium (the SBC UNMAP analog), so
        // refuse it on a WORM namespace — Namespace Is Write Protected,
        // mirroring the iSCSI/SBC WORM gate (issue #79).
        if cache.manifest().worm {
            return NvmeResponse::just(Cqe::failure(
                cid,
                0,
                0,
                StatusField::namespace_write_protected(),
            ));
        }
        let nr_zero_based = (sqe.cdw10 & 0xFF) as usize;
        let count = nr_zero_based + 1;
        let expected_bytes = count * 16;
        let payload = match data_out {
            Some(p) if p.len() == expected_bytes => p,
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        for chunk in payload.chunks_exact(16) {
            let nlb = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let slba = u64::from_le_bytes([
                chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14],
                chunk[15],
            ]);
            let (byte_off, len) = match resolve_range(cid, cache, slba, u64::from(nlb)) {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            if let Err(e) = cache.unmap_bytes(byte_off, len).await {
                tracing::warn!(error = %e, "dsm deallocate failed");
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, write_failure_status(&e)));
            }
        }
        NvmeResponse::just(Cqe::success(cid, 0, 0, 0))
    }

    async fn cmd_verify(&self, cid: u16, sqe: &nvme_base::Sqe, cache: &PageCache) -> NvmeResponse {
        // Verify reads the range and discards the payload — the
        // controller's job is to surface medium errors, not to
        // hand data to the host. core-block has no opaque-medium
        // concept so this collapses to "can we successfully read
        // these LBAs."
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let (byte_off, len_bytes) = match resolve_range(cid, cache, slba, u64::from(nlb)) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        match cache.read_bytes(byte_off, len_bytes as usize).await {
            Ok(_) => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "verify failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, read_failure_status(&e)))
            }
        }
    }
}

/// SLBA — Starting LBA (NVM I/O CDW10..CDW11 little-endian u64).
fn read_slba(sqe: &nvme_base::Sqe) -> u64 {
    u64::from(sqe.cdw10) | (u64::from(sqe.cdw11) << 32)
}

/// NLB — Number of Logical Blocks (zero-based), low 16 bits of
/// CDW12 (NVM Command Set §3.2.2 etc).
fn read_nlb(sqe: &nvme_base::Sqe) -> u32 {
    u32::from((sqe.cdw12 & 0xFFFF) as u16) + 1
}

/// Map an `UploaderError` from a read-class command (Read, Compare,
/// Verify) into a CQE status via the shared `FaultClass` classifier —
/// the same one the SCSI dispatcher keys off, so the two transports
/// agree on the same fault (issue #109): integrity faults get the
/// media-class status, infrastructure faults Internal Error,
/// backpressure the retryable Namespace Not Ready.
fn read_failure_status(e: &UploaderError) -> StatusField {
    match e.fault_class() {
        FaultClass::Validation => StatusField::invalid_field(),
        FaultClass::Transient => StatusField::namespace_not_ready(),
        FaultClass::Integrity => StatusField::unrecovered_read_error(),
        FaultClass::Infrastructure => StatusField::internal_error(),
    }
}

/// Write-class counterpart of [`read_failure_status`] (Write, Write
/// Zeroes, DSM Deallocate, Flush, fused Compare+Write) — only the
/// media-class status differs (Write Fault vs Unrecovered Read
/// Error), mirroring the SBC mappers' WRITE ERROR / UNRECOVERED READ
/// ERROR split.
fn write_failure_status(e: &UploaderError) -> StatusField {
    match e.fault_class() {
        FaultClass::Validation => StatusField::invalid_field(),
        FaultClass::Transient => StatusField::namespace_not_ready(),
        FaultClass::Integrity => StatusField::write_fault(),
        FaultClass::Infrastructure => StatusField::internal_error(),
    }
}

/// Resolve an LBA span to `(byte_offset, byte_len)` against the
/// volume, overflow-safe and bounds-checked. Wraps the shared
/// [`PageCache::resolve_range`] (also used by the SBC data path) and
/// translates [`RangeError`] into a populated `NvmeResponse` so callers
/// `return` straight through on error. `blocks` is the real
/// (one-based) block count.
fn resolve_range(
    cid: u16,
    cache: &PageCache,
    slba: u64,
    blocks: u64,
) -> Result<(u64, u64), NvmeResponse> {
    match cache.resolve_range(slba, blocks) {
        Ok(v) => Ok(v),
        Err(RangeError::BadSectorSize) => Err(NvmeResponse::just(Cqe::failure(
            cid,
            0,
            0,
            StatusField::internal_error(),
        ))),
        Err(RangeError::OutOfRange) => Err(NvmeResponse::just(Cqe::failure(
            cid,
            0,
            0,
            StatusField::lba_out_of_range(),
        ))),
    }
}

#[async_trait]
impl NvmeCommandHandler for NvmeNvmDispatcher {
    fn subnqn(&self) -> &str {
        &self.subnqn
    }

    async fn handle_admin(&self, cmd: AdminCommand<'_>) -> NvmeResponse {
        self.dispatch_admin(cmd).await
    }

    async fn handle_io(&self, cmd: IoCommand<'_>) -> NvmeResponse {
        self.dispatch_io(cmd).await
    }

    async fn handle_fused_compare_write(
        &self,
        compare: IoCommand<'_>,
        write: IoCommand<'_>,
    ) -> (Cqe, Cqe) {
        let compare_cid = compare.sqe.cid;
        let write_cid = write.sqe.cid;

        // Both halves must be the right opcodes, same namespace,
        // same LBA range, with matching data payloads.
        if NvmOpcode::from_u8(compare.sqe.opcode) != Some(NvmOpcode::Compare) {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::invalid_opcode()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        }
        if NvmOpcode::from_u8(write.sqe.opcode) != Some(NvmOpcode::Write) {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::invalid_opcode()),
                Cqe::failure(write_cid, 0, 0, StatusField::invalid_opcode()),
            );
        }
        if compare.sqe.nsid != write.sqe.nsid
            || read_slba(&compare.sqe) != read_slba(&write.sqe)
            || read_nlb(&compare.sqe) != read_nlb(&write.sqe)
        {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::invalid_field()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        }
        // Per-hostnqn admission: both halves must share the same
        // session_volumes (they came in on the same I/O queue), so
        // gate once.
        if !nsid_admitted(
            self.registry.as_ref(),
            compare.sqe.nsid,
            compare.session_volumes,
        ) {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::invalid_namespace()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        }
        let Some(cache) = self.registry.get(compare.sqe.nsid) else {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::invalid_namespace()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        };
        // Reservation gate: a fused Compare+Write mutates the medium,
        // so the write-side gate applies. A host that can't write
        // can't CAW.
        let registrant = RegistrantId::nvme(compare.host_id.unwrap_or([0u8; 16]));
        if !self.reservations.allow_write(
            crate::reservations::nsid_to_lun(compare.sqe.nsid),
            &registrant,
        ) {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::reservation_conflict()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        }
        // WORM: a fused Compare+Write mutates the medium, so refuse it on
        // a write-once namespace — the reason rides the compare (first)
        // half and the write half aborts, the same shape as the
        // reservation-conflict refusal above (issue #79).
        if cache.manifest().worm {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::namespace_write_protected()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        }
        let slba = read_slba(&compare.sqe);
        let nlb = read_nlb(&compare.sqe);
        let (byte_off, len_bytes) = match cache.resolve_range(slba, u64::from(nlb)) {
            Ok(v) => v,
            Err(_) => {
                return (
                    Cqe::failure(compare_cid, 0, 0, StatusField::lba_out_of_range()),
                    Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
                );
            }
        };
        let expected = match compare.data_out {
            Some(d) if d.len() as u64 == len_bytes => d,
            _ => {
                return (
                    Cqe::failure(compare_cid, 0, 0, StatusField::invalid_field()),
                    Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
                );
            }
        };
        let new = match write.data_out {
            Some(d) if d.len() as u64 == len_bytes => d,
            _ => {
                return (
                    Cqe::failure(compare_cid, 0, 0, StatusField::invalid_field()),
                    Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
                );
            }
        };
        // Serialize the read-compare-write window per LUN. The primitive
        // is NOT internally atomic, so two concurrent fused CAWs (two I/O
        // queues, or pipelined) could both pass the compare and both
        // commit — split-brain on the cluster-coordination LBA (issue
        // #128). The shared registry also serializes against COMPARE AND
        // WRITE arriving over iSCSI on the same volume.
        let lun = crate::reservations::nsid_to_lun(compare.sqe.nsid);
        let caw_lock = self.caw_locks.lock_for(lun);
        let _caw_guard = caw_lock.lock().await;
        match cache.compare_and_write_bytes(byte_off, expected, new).await {
            Ok(true) => (
                Cqe::success(compare_cid, 0, 0, 0),
                Cqe::success(write_cid, 0, 0, 0),
            ),
            Ok(false) => (
                Cqe::failure(compare_cid, 0, 0, StatusField::compare_failure()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            ),
            Err(e) => {
                tracing::warn!(error = %e, "fused compare-and-write failed");
                // Classify like SBC's COMPARE AND WRITE Err arm (the
                // failing op is the read-modify half); the write CQE
                // stays aborted-due-to-failed-fused either way.
                (
                    Cqe::failure(compare_cid, 0, 0, read_failure_status(&e)),
                    Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use std::collections::BTreeMap;
    use std::sync::RwLock;
    use tempfile::TempDir;

    /// Test-only NamespaceLookup. The real daemon's VolumeRegistry
    /// lives in thurvsad; nvme-nvm carries only the trait, so
    /// test fixtures need their own implementation. Mirrors the
    /// TestRegistry in scsi-sbc verbatim.
    #[derive(Default)]
    struct TestRegistry {
        by_nsid: RwLock<BTreeMap<u32, Arc<PageCache>>>,
    }

    impl TestRegistry {
        fn register(&self, nsid: u32, cache: Arc<PageCache>) {
            self.by_nsid.write().unwrap().insert(nsid, cache);
        }
    }

    impl NamespaceLookup for TestRegistry {
        fn get(&self, nsid: u32) -> Option<Arc<PageCache>> {
            self.by_nsid.read().unwrap().get(&nsid).map(Arc::clone)
        }
        fn active_namespaces(&self) -> Vec<u32> {
            self.by_nsid.read().unwrap().keys().copied().collect()
        }
        fn name_for_nsid(&self, nsid: u32) -> Option<String> {
            self.by_nsid
                .read()
                .unwrap()
                .get(&nsid)
                .map(|c| c.manifest().name.clone())
        }
        fn active_namespaces_filtered(&self, allow: Option<&[String]>) -> Vec<u32> {
            let m = self.by_nsid.read().unwrap();
            match allow {
                None => m.keys().copied().collect(),
                Some(names) => m
                    .iter()
                    .filter(|(_, c)| names.iter().any(|n| n == &c.manifest().name))
                    .map(|(nsid, _)| *nsid)
                    .collect(),
            }
        }
    }

    async fn fixture_dispatcher() -> (TempDir, NvmeNvmDispatcher) {
        fixture_dispatcher_with_worm(false).await
    }

    /// WORM variant of [`fixture_dispatcher`] — the namespace is
    /// write-once, used by the issue #79 WORM-enforcement tests.
    async fn fixture_dispatcher_worm() -> (TempDir, NvmeNvmDispatcher) {
        fixture_dispatcher_with_worm(true).await
    }

    async fn fixture_dispatcher_with_worm(worm: bool) -> (TempDir, NvmeNvmDispatcher) {
        let tmp = TempDir::new().unwrap();
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = LocalBackend::new(&storage_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

        VolumeManifest::new(
            "ns1".into(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            worm,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "ns1", backend).unwrap());
        let cache = PageCache::new(writer);

        let reg = TestRegistry::default();
        reg.register(1, cache);
        // Wire the AER sink onto the manager exactly as the daemon does
        // (issue #67): the LID 0x80 + AER fan-out is now observer-driven,
        // so reservation-notification tests only fire through this path.
        let aer = Arc::new(ControllerRegistry::new());
        let reservations = Arc::new(ReservationManager::new());
        reservations.register_observer(Arc::new(crate::aer::AerReservationSink::new(Arc::clone(
            &aer,
        ))));
        let disp = NvmeNvmDispatcher::new(
            Arc::new(reg),
            "nqn.2025-10.com.metebalci:thurvsa".into(),
            "TESTSN".into(),
            "ThurVSA Volume".into(),
            "0.1.0".into(),
            aer,
            reservations,
        );
        (tmp, disp)
    }

    fn sqe_write(slba: u64, nlb_zero_based: u16) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::Write as u8;
        b[2] = 0x01; // CID = 1
        b[4] = 0x01; // NSID = 1
        b[40..44].copy_from_slice(&((slba & 0xFFFF_FFFF) as u32).to_le_bytes());
        b[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        b[48..52].copy_from_slice(&u32::from(nlb_zero_based).to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    fn sqe_read(slba: u64, nlb_zero_based: u16) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::Read as u8;
        b[2] = 0x02;
        b[4] = 0x01;
        b[40..44].copy_from_slice(&((slba & 0xFFFF_FFFF) as u32).to_le_bytes());
        b[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        b[48..52].copy_from_slice(&u32::from(nlb_zero_based).to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    fn sqe_write_zeroes(slba: u64, nlb_zero_based: u16, deac: bool) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::WriteZeroes as u8;
        b[2] = 0x03;
        b[4] = 0x01;
        b[40..44].copy_from_slice(&((slba & 0xFFFF_FFFF) as u32).to_le_bytes());
        b[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        let mut cdw12 = u32::from(nlb_zero_based);
        if deac {
            cdw12 |= 1 << 25; // DEAC
        }
        b[48..52].copy_from_slice(&cdw12.to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    /// Issue #179: Write Zeroes with the DEAC bit deallocates (fast
    /// index-only path) and the range still reads back zero.
    #[tokio::test]
    async fn write_zeroes_deac_deallocates_and_reads_back_zero() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let payload: Vec<u8> = vec![0xAB; 64 * 1024];
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_write(0, 15),
                data_out: Some(&payload),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS, "seed write failed");

        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_write_zeroes(0, 15, true),
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS, "write-zeroes DEAC failed");

        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_read(0, 15),
                data_out: None,
                data_in_max: 64 * 1024,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert!(
            resp.data_in.iter().all(|&b| b == 0),
            "deallocated range must read back zero"
        );
    }

    /// Issue #179: Write Zeroes without DEAC zero-fills (read-back zero).
    #[tokio::test]
    async fn write_zeroes_without_deac_zero_fills() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_write_zeroes(0, 15, false),
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_read(0, 15),
                data_out: None,
                data_in_max: 64 * 1024,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert!(resp.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let (_tmp, disp) = fixture_dispatcher().await;
        // 64 KiB page / 4 KiB sector → 16 sectors per page.
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i & 0xFF) as u8).collect();
        let wsqe = sqe_write(0, 15); // nlb zero-based = 15, so 16 sectors
        let resp = disp
            .handle_io(IoCommand {
                sqe: wsqe,
                data_out: Some(&payload),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS, "write failed");

        let rsqe = sqe_read(0, 15);
        let resp = disp
            .handle_io(IoCommand {
                sqe: rsqe,
                data_out: None,
                data_in_max: 64 * 1024,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert_eq!(resp.data_in, payload);
    }

    #[tokio::test]
    async fn read_out_of_range_returns_lba_oor() {
        let (_tmp, disp) = fixture_dispatcher().await;
        // 4 MiB / 4096 = 1024 sectors total. Read 1 sector at slba=2000.
        let rsqe = sqe_read(2000, 0);
        let resp = disp
            .handle_io(IoCommand {
                sqe: rsqe,
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::lba_out_of_range());
    }

    #[tokio::test]
    async fn identify_controller_returns_4096_bytes() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = AdminOpcode::Identify as u8;
        b[2] = 0x03; // CID = 3
        b[40] = CNS::Controller as u8;
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_admin(AdminCommand {
                sqe,
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert_eq!(resp.data_in.len(), 4096);
    }

    /// Identify Controller reports the CNTLID threaded in from the
    /// transport (Connect-assigned), not a static 1.
    #[tokio::test]
    async fn identify_controller_reports_assigned_cntlid() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = AdminOpcode::Identify as u8;
        b[40] = CNS::Controller as u8;
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = admin_io(&disp, sqe, Some(7)).await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        // CNTLID at Identify Controller bytes 78..80.
        assert_eq!(&resp.data_in[78..80], &7u16.to_le_bytes());
    }

    #[tokio::test]
    async fn get_features_number_of_queues_returns_default() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = AdminOpcode::GetFeatures as u8;
        b[2] = 0x21; // CID
        // CDW10[7:0] = FID 0x07 Number of Queues
        b[40] = FID_NUMBER_OF_QUEUES;
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_admin(AdminCommand {
                sqe,
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        let expected =
            u32::from(DEFAULT_NUM_IO_QUEUES - 1) | (u32::from(DEFAULT_NUM_IO_QUEUES - 1) << 16);
        assert_eq!(resp.cqe.dw0, expected);
    }

    #[tokio::test]
    async fn set_features_number_of_queues_clamps_to_cap() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = AdminOpcode::SetFeatures as u8;
        b[2] = 0x22;
        b[40] = FID_NUMBER_OF_QUEUES;
        // Host asks for 200 queues (zero-based = 199), much larger
        // than our cap.
        let req = 199u32 | (199u32 << 16);
        b[44..48].copy_from_slice(&req.to_le_bytes());
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_admin(AdminCommand {
                sqe,
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        let cap_zero = u32::from(DEFAULT_NUM_IO_QUEUES - 1);
        assert_eq!(resp.cqe.dw0, cap_zero | (cap_zero << 16));
    }

    #[tokio::test]
    async fn get_log_page_smart_returns_512_bytes() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = AdminOpcode::GetLogPage as u8;
        b[2] = 0x33;
        // LID 0x02 (SMART), NUMDL = 127 (= 128 dwords = 512 bytes,
        // zero-based)
        let cdw10 = u32::from(nvme_base::log_page::lid::SMART_HEALTH) | (127u32 << 16);
        b[40..44].copy_from_slice(&cdw10.to_le_bytes());
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_admin(AdminCommand {
                sqe,
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert_eq!(resp.data_in.len(), 512);
        // Temperature field
        let temp = u16::from_le_bytes([resp.data_in[1], resp.data_in[2]]);
        assert_eq!(temp, 300);
    }

    #[tokio::test]
    async fn get_log_page_unknown_lid_returns_invalid_field() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = AdminOpcode::GetLogPage as u8;
        b[2] = 0x34;
        let cdw10 = 0x7Fu32 | (127u32 << 16); // LID 0x7F (unknown)
        b[40..44].copy_from_slice(&cdw10.to_le_bytes());
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_admin(AdminCommand {
                sqe,
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_field());
    }

    #[tokio::test]
    async fn fused_compare_write_match_writes_new_buffer() {
        // First write a known pattern at LBA 0 via regular Write.
        let (_tmp, disp) = fixture_dispatcher().await;
        let original: Vec<u8> = (0..(64 * 1024)).map(|i| (i & 0xFF) as u8).collect();
        let mut wsqe = vec![0u8; nvme_base::SQE_SIZE];
        wsqe[0] = NvmOpcode::Write as u8;
        wsqe[2] = 0x40;
        wsqe[4] = 0x01;
        wsqe[48..52].copy_from_slice(&15u32.to_le_bytes()); // NLB=15 (16 sectors)
        let wsqe = nvme_base::Sqe::parse(&wsqe).unwrap();
        disp.handle_io(IoCommand {
            sqe: wsqe,
            data_out: Some(&original),
            data_in_max: 0,
            session_volumes: None,
            host_id: None,
        })
        .await;

        // Fused Compare (expecting `original`) + Write (`new`).
        let new: Vec<u8> = (0..(64 * 1024))
            .map(|i| ((i ^ 0x55) & 0xFF) as u8)
            .collect();
        let mut csqe = vec![0u8; nvme_base::SQE_SIZE];
        csqe[0] = NvmOpcode::Compare as u8;
        csqe[1] = 0b0000_0001; // FUSE = First
        csqe[2] = 0x41;
        csqe[4] = 0x01;
        csqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        let csqe = nvme_base::Sqe::parse(&csqe).unwrap();

        let mut wsqe2 = vec![0u8; nvme_base::SQE_SIZE];
        wsqe2[0] = NvmOpcode::Write as u8;
        wsqe2[1] = 0b0000_0010; // FUSE = Second
        wsqe2[2] = 0x42;
        wsqe2[4] = 0x01;
        wsqe2[48..52].copy_from_slice(&15u32.to_le_bytes());
        let wsqe2 = nvme_base::Sqe::parse(&wsqe2).unwrap();

        let (cqe_c, cqe_w) = disp
            .handle_fused_compare_write(
                IoCommand {
                    sqe: csqe,
                    data_out: Some(&original),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: None,
                },
                IoCommand {
                    sqe: wsqe2,
                    data_out: Some(&new),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: None,
                },
            )
            .await;
        assert_eq!(cqe_c.status, StatusField::SUCCESS);
        assert_eq!(cqe_w.status, StatusField::SUCCESS);
        assert_eq!(cqe_c.cid, 0x41);
        assert_eq!(cqe_w.cid, 0x42);

        // Verify the new buffer landed (read it back).
        let mut rsqe = vec![0u8; nvme_base::SQE_SIZE];
        rsqe[0] = NvmOpcode::Read as u8;
        rsqe[2] = 0x43;
        rsqe[4] = 0x01;
        rsqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        let rsqe = nvme_base::Sqe::parse(&rsqe).unwrap();
        let read_resp = disp
            .handle_io(IoCommand {
                sqe: rsqe,
                data_out: None,
                data_in_max: 64 * 1024,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(read_resp.data_in, new);
    }

    #[tokio::test]
    async fn fused_compare_mismatch_aborts_write() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let original: Vec<u8> = (0..(64 * 1024)).map(|i| (i & 0xFF) as u8).collect();
        // Write the original pattern.
        let mut wsqe = vec![0u8; nvme_base::SQE_SIZE];
        wsqe[0] = NvmOpcode::Write as u8;
        wsqe[2] = 0x50;
        wsqe[4] = 0x01;
        wsqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        disp.handle_io(IoCommand {
            sqe: nvme_base::Sqe::parse(&wsqe).unwrap(),
            data_out: Some(&original),
            data_in_max: 0,
            session_volumes: None,
            host_id: None,
        })
        .await;

        // Fused with a DIFFERENT compare expectation — must fail.
        let bad_compare: Vec<u8> = vec![0xFFu8; 64 * 1024];
        let new: Vec<u8> = vec![0xCCu8; 64 * 1024];
        let mut csqe = vec![0u8; nvme_base::SQE_SIZE];
        csqe[0] = NvmOpcode::Compare as u8;
        csqe[1] = 0b0000_0001;
        csqe[2] = 0x51;
        csqe[4] = 0x01;
        csqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        let mut wsqe2 = vec![0u8; nvme_base::SQE_SIZE];
        wsqe2[0] = NvmOpcode::Write as u8;
        wsqe2[1] = 0b0000_0010;
        wsqe2[2] = 0x52;
        wsqe2[4] = 0x01;
        wsqe2[48..52].copy_from_slice(&15u32.to_le_bytes());

        let (cqe_c, cqe_w) = disp
            .handle_fused_compare_write(
                IoCommand {
                    sqe: nvme_base::Sqe::parse(&csqe).unwrap(),
                    data_out: Some(&bad_compare),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: None,
                },
                IoCommand {
                    sqe: nvme_base::Sqe::parse(&wsqe2).unwrap(),
                    data_out: Some(&new),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: None,
                },
            )
            .await;
        assert_eq!(cqe_c.status, StatusField::compare_failure());
        assert_eq!(cqe_w.status, StatusField::aborted_due_to_failed_fused());

        // Read back — original should be untouched.
        let mut rsqe = vec![0u8; nvme_base::SQE_SIZE];
        rsqe[0] = NvmOpcode::Read as u8;
        rsqe[2] = 0x53;
        rsqe[4] = 0x01;
        rsqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        let read_resp = disp
            .handle_io(IoCommand {
                sqe: nvme_base::Sqe::parse(&rsqe).unwrap(),
                data_out: None,
                data_in_max: 64 * 1024,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(read_resp.data_in, original);
    }

    #[tokio::test]
    async fn unknown_nsid_returns_invalid_namespace() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::Read as u8;
        b[2] = 0x09;
        b[4] = 99; // NSID = 99 (not attached)
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_io(IoCommand {
                sqe,
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_namespace());
    }

    /// Issue #128: two concurrent fused Compare+Write commands against
    /// the same namespace, both expecting the current value, must
    /// serialize so exactly ONE wins (the loser sees the value the winner
    /// wrote and its compare fails). Without the per-LUN CAW lock both
    /// could pass the compare and both commit — split-brain.
    #[tokio::test]
    async fn concurrent_fused_caw_yields_exactly_one_winner() {
        let (_tmp, disp) = fixture_dispatcher().await;
        // Seed LBA 0..16 with a known pattern.
        let original: Vec<u8> = (0..(64 * 1024)).map(|i| (i & 0xFF) as u8).collect();
        let mut wsqe = vec![0u8; nvme_base::SQE_SIZE];
        wsqe[0] = NvmOpcode::Write as u8;
        wsqe[2] = 0x40;
        wsqe[4] = 0x01;
        wsqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        let wsqe = nvme_base::Sqe::parse(&wsqe).unwrap();
        disp.handle_io(IoCommand {
            sqe: wsqe,
            data_out: Some(&original),
            data_in_max: 0,
            session_volumes: None,
            host_id: None,
        })
        .await;

        let new_a = vec![0xAAu8; 64 * 1024];
        let new_b = vec![0xBBu8; 64 * 1024];
        let mk = |ccid: u8, wcid: u8| -> (nvme_base::Sqe, nvme_base::Sqe) {
            let mut c = vec![0u8; nvme_base::SQE_SIZE];
            c[0] = NvmOpcode::Compare as u8;
            c[1] = 0b0000_0001; // FUSE = First
            c[2] = ccid;
            c[4] = 0x01;
            c[48..52].copy_from_slice(&15u32.to_le_bytes());
            let mut w = vec![0u8; nvme_base::SQE_SIZE];
            w[0] = NvmOpcode::Write as u8;
            w[1] = 0b0000_0010; // FUSE = Second
            w[2] = wcid;
            w[4] = 0x01;
            w[48..52].copy_from_slice(&15u32.to_le_bytes());
            (
                nvme_base::Sqe::parse(&c).unwrap(),
                nvme_base::Sqe::parse(&w).unwrap(),
            )
        };
        let (c1, w1) = mk(0x11, 0x12);
        let (c2, w2) = mk(0x21, 0x22);
        let fut1 = disp.handle_fused_compare_write(
            IoCommand {
                sqe: c1,
                data_out: Some(&original),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            },
            IoCommand {
                sqe: w1,
                data_out: Some(&new_a),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            },
        );
        let fut2 = disp.handle_fused_compare_write(
            IoCommand {
                sqe: c2,
                data_out: Some(&original),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            },
            IoCommand {
                sqe: w2,
                data_out: Some(&new_b),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            },
        );
        let ((cc1, _), (cc2, _)) = tokio::join!(fut1, fut2);
        let wins = [cc1.status, cc2.status]
            .iter()
            .filter(|s| **s == StatusField::SUCCESS)
            .count();
        assert_eq!(wins, 1, "exactly one fused CAW may win (test-and-set)");
    }

    /// Generic NVM I/O SQE: opcode at byte 0, CID at 2, NSID = 1 at
    /// byte 4, SLBA split across CDW10/11, NLB (zero-based) in CDW12.
    fn sqe_io(opcode: NvmOpcode, slba: u64, nlb_zero_based: u16) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = opcode as u8;
        b[2] = 0x05;
        b[4] = 0x01;
        b[40..44].copy_from_slice(&((slba & 0xFFFF_FFFF) as u32).to_le_bytes());
        b[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        b[48..52].copy_from_slice(&u32::from(nlb_zero_based).to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    /// Admin SQE: opcode at byte 0, NSID at 4, CDW10 at bytes 40..44.
    fn sqe_admin(opcode: u8, nsid: u32, cdw10: u32) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = opcode;
        b[2] = 0x07;
        b[4..8].copy_from_slice(&nsid.to_le_bytes());
        b[40..44].copy_from_slice(&cdw10.to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    #[tokio::test]
    async fn flush_over_namespace_succeeds() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Flush, 0, 0),
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    #[tokio::test]
    async fn compare_matches_written_data() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let payload = vec![0xABu8; 4096];
        disp.handle_io(IoCommand {
            sqe: sqe_io(NvmOpcode::Write, 0, 0),
            data_out: Some(&payload),
            data_in_max: 0,
            session_volumes: None,
            host_id: None,
        })
        .await;

        let same = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Compare, 0, 0),
                data_out: Some(&payload),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(same.cqe.status, StatusField::SUCCESS);

        let differs = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Compare, 0, 0),
                data_out: Some(&vec![0x00u8; 4096]),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(differs.cqe.status, StatusField::compare_failure());
    }

    #[tokio::test]
    async fn compare_wrong_length_is_invalid_field() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Compare, 0, 0),
                data_out: Some(&[1, 2, 3]),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_field());
    }

    #[tokio::test]
    async fn write_zeroes_then_read_back_all_zero() {
        let (_tmp, disp) = fixture_dispatcher().await;
        // Dirty the sector first so the zero-fill is observable.
        disp.handle_io(IoCommand {
            sqe: sqe_io(NvmOpcode::Write, 0, 0),
            data_out: Some(&vec![0xFFu8; 4096]),
            data_in_max: 0,
            session_volumes: None,
            host_id: None,
        })
        .await;

        let wz = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::WriteZeroes, 0, 0),
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(wz.cqe.status, StatusField::SUCCESS);

        let rd = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Read, 0, 0),
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(rd.cqe.status, StatusField::SUCCESS);
        assert!(rd.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn dsm_deallocate_one_range_succeeds() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::DatasetManagement as u8;
        b[4] = 0x01; // NSID = 1
        // CDW10 NR = 0 (one range, zero-based); CDW11 bit 2 = AD.
        b[44] = 0x04;
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        // One 16-byte range descriptor: ctx(4) | nlb(4) | slba(8).
        let mut range = vec![0u8; 16];
        range[4..8].copy_from_slice(&1u32.to_le_bytes()); // nlb = 1 block
        let resp = disp
            .handle_io(IoCommand {
                sqe,
                data_out: Some(&range),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    #[tokio::test]
    async fn dsm_without_ad_bit_is_a_noop_success() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::DatasetManagement as u8;
        b[4] = 0x01;
        // No AD bit → integral-dataset hint path, no payload required.
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_io(IoCommand {
                sqe,
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    #[tokio::test]
    async fn verify_in_range_succeeds() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Verify, 0, 0),
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    // --- WORM enforcement (issue #79) ---------------------------------
    // A write-once namespace must refuse every mutating opcode with
    // Namespace Is Write Protected, mirroring the SBC WRITE PROTECTED
    // gate so the WORM guarantee holds over NVMe/TCP as well as iSCSI.

    #[tokio::test]
    async fn write_refused_on_worm_namespace() {
        let (_tmp, disp) = fixture_dispatcher_worm().await;
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Write, 0, 0),
                data_out: Some(&vec![0xFFu8; 4096]),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::namespace_write_protected());

        // The refused write must not have landed: the sector still reads
        // back as unallocated zeros.
        let rd = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Read, 0, 0),
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(rd.cqe.status, StatusField::SUCCESS);
        assert!(rd.data_in.iter().all(|&b| b == 0), "WORM write leaked");
    }

    #[tokio::test]
    async fn write_zeroes_refused_on_worm_namespace() {
        let (_tmp, disp) = fixture_dispatcher_worm().await;
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::WriteZeroes, 0, 0),
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::namespace_write_protected());
    }

    #[tokio::test]
    async fn dsm_deallocate_refused_on_worm_namespace() {
        let (_tmp, disp) = fixture_dispatcher_worm().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::DatasetManagement as u8;
        b[4] = 0x01; // NSID = 1
        // CDW10 NR = 0 (one range); CDW11 bit 2 = AD (Deallocate).
        b[44] = 0x04;
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let mut range = vec![0u8; 16];
        range[4..8].copy_from_slice(&1u32.to_le_bytes()); // nlb = 1 block
        let resp = disp
            .handle_io(IoCommand {
                sqe,
                data_out: Some(&range),
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::namespace_write_protected());
    }

    /// A non-deallocate DSM (no AD bit) is an advisory hint, not a
    /// mutation, so it still no-ops to success even on a WORM namespace.
    #[tokio::test]
    async fn dsm_hint_without_ad_still_noops_on_worm_namespace() {
        let (_tmp, disp) = fixture_dispatcher_worm().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::DatasetManagement as u8;
        b[4] = 0x01;
        // No AD bit set.
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_io(IoCommand {
                sqe,
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    /// Reads are never WORM-gated.
    #[tokio::test]
    async fn read_allowed_on_worm_namespace() {
        let (_tmp, disp) = fixture_dispatcher_worm().await;
        let resp = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Read, 0, 0),
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    #[tokio::test]
    async fn fused_compare_write_refused_on_worm_namespace() {
        let (_tmp, disp) = fixture_dispatcher_worm().await;
        let expected = vec![0u8; 64 * 1024];
        let new = vec![0xCCu8; 64 * 1024];
        let mut csqe = vec![0u8; nvme_base::SQE_SIZE];
        csqe[0] = NvmOpcode::Compare as u8;
        csqe[1] = 0b0000_0001; // FUSE = First
        csqe[2] = 0x71;
        csqe[4] = 0x01;
        csqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        let mut wsqe = vec![0u8; nvme_base::SQE_SIZE];
        wsqe[0] = NvmOpcode::Write as u8;
        wsqe[1] = 0b0000_0010; // FUSE = Second
        wsqe[2] = 0x72;
        wsqe[4] = 0x01;
        wsqe[48..52].copy_from_slice(&15u32.to_le_bytes());

        let (cqe_c, cqe_w) = disp
            .handle_fused_compare_write(
                IoCommand {
                    sqe: nvme_base::Sqe::parse(&csqe).unwrap(),
                    data_out: Some(&expected),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: None,
                },
                IoCommand {
                    sqe: nvme_base::Sqe::parse(&wsqe).unwrap(),
                    data_out: Some(&new),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: None,
                },
            )
            .await;
        assert_eq!(
            cqe_c.status,
            StatusField::namespace_write_protected(),
            "compare half write-protected",
        );
        assert_eq!(
            cqe_w.status,
            StatusField::aborted_due_to_failed_fused(),
            "write half aborted",
        );
    }

    #[tokio::test]
    async fn io_unknown_opcode_is_invalid_opcode() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = 0x7E; // not an NVM Command Set opcode
        b[4] = 0x01;
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_io(IoCommand {
                sqe,
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_opcode());
    }

    #[tokio::test]
    async fn identify_namespace_returns_4096_bytes() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_admin(AdminCommand {
                sqe: sqe_admin(0x06, 1, 0x00), // CNS = 0x00 Namespace
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert_eq!(resp.data_in.len(), 4096);
    }

    #[tokio::test]
    async fn identify_active_namespace_list_and_descriptors() {
        let (_tmp, disp) = fixture_dispatcher().await;
        for cns in [0x02u32, 0x03, 0x06] {
            let resp = disp
                .handle_admin(AdminCommand {
                    sqe: sqe_admin(0x06, 1, cns),
                    data_out: None,
                    data_in_max: 4096,
                    session_volumes: None,
                    cntlid: None,
                    local_addr: None,
                })
                .await;
            assert_eq!(resp.cqe.status, StatusField::SUCCESS, "CNS {cns:#x}");
            assert!(!resp.data_in.is_empty(), "CNS {cns:#x} returned no data");
        }
    }

    #[tokio::test]
    async fn identify_unsupported_cns_is_invalid_field() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_admin(AdminCommand {
                sqe: sqe_admin(0x06, 1, 0x55), // CNS 0x55 is unsupported
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_field());
    }

    #[tokio::test]
    async fn admin_keep_alive_succeeds() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_admin(AdminCommand {
                sqe: sqe_admin(0x18, 0, 0), // KeepAlive
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    #[tokio::test]
    async fn admin_abort_reports_command_not_aborted() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_admin(AdminCommand {
                sqe: sqe_admin(0x08, 0, 0), // Abort
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        // DW0 bit 0 = 1 means "command was not aborted".
        assert_eq!(resp.cqe.dw0 & 1, 1);
    }

    #[tokio::test]
    async fn admin_async_event_request_on_dispatch_path_is_invalid_opcode() {
        // The NVMe/TCP transport intercepts AER and parks it on the
        // ControllerRegistry before this dispatch path runs, so a real host never
        // reaches here. A non-transport caller (this test) hits the
        // synchronous fallback: there is no event to report inline, so
        // the dispatcher returns Invalid Opcode rather than blocking.
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_admin(AdminCommand {
                sqe: sqe_admin(0x0C, 0, 0), // AsyncEventRequest
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_opcode());
    }

    #[tokio::test]
    async fn admin_unknown_opcode_is_invalid_opcode() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_admin(AdminCommand {
                sqe: sqe_admin(0x03, 0, 0), // unassigned admin opcode
                data_out: None,
                data_in_max: 0,
                session_volumes: None,
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_opcode());
    }

    /// Fixture with two NSIDs (1 → "ns1", 2 → "ns2") so admission can
    /// fence one out.
    async fn fixture_two_ns() -> (TempDir, NvmeNvmDispatcher) {
        let tmp = TempDir::new().unwrap();
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = LocalBackend::new(&storage_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        let reg = TestRegistry::default();
        for (nsid, name) in [(1u32, "ns1"), (2u32, "ns2")] {
            VolumeManifest::new(
                name.into(),
                4 * (1u64 << 20),
                DEFAULT_SECTOR_BYTES,
                DEFAULT_PAGE_SIZE_BYTES,
                "primary".into(),
                DedupScope::Local,
                false,
                0,
            )
            .unwrap()
            .create(tmp.path())
            .unwrap();
            let writer =
                Arc::new(VolumeWriter::open(tmp.path(), name, Arc::clone(&backend)).unwrap());
            reg.register(nsid, PageCache::new(writer));
        }
        let aer = Arc::new(ControllerRegistry::new());
        let reservations = Arc::new(ReservationManager::new());
        reservations.register_observer(Arc::new(crate::aer::AerReservationSink::new(Arc::clone(
            &aer,
        ))));
        let disp = NvmeNvmDispatcher::new(
            Arc::new(reg),
            "nqn.2025-10.com.metebalci:thurvsa".into(),
            "TESTSN".into(),
            "ThurVSA Volume".into(),
            "0.1.0".into(),
            aer,
            reservations,
        );
        (tmp, disp)
    }

    #[tokio::test]
    async fn identify_active_namespace_list_filters_to_admitted() {
        let (_tmp, disp) = fixture_two_ns().await;
        let allow = vec!["ns2".to_string()];
        let resp = disp
            .handle_admin(AdminCommand {
                // sqe_admin(opcode, nsid, cdw10) — CNS=0x02 lives in
                // cdw10 low byte, starting_nsid=0 in nsid.
                sqe: sqe_admin(AdminOpcode::Identify as u8, 0, 0x02),
                data_out: None,
                data_in_max: nvme_base::IDENTIFY_DATA_SIZE as u32,
                session_volumes: Some(&allow),
                cntlid: None,
                local_addr: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        // First entry of the active NS list is a u32 LE — only ns2 (NSID 2)
        // should be present, ns1 (NSID 1) filtered out.
        let nsid0 = u32::from_le_bytes([
            resp.data_in[0],
            resp.data_in[1],
            resp.data_in[2],
            resp.data_in[3],
        ]);
        let nsid1 = u32::from_le_bytes([
            resp.data_in[4],
            resp.data_in[5],
            resp.data_in[6],
            resp.data_in[7],
        ]);
        assert_eq!(nsid0, 2);
        assert_eq!(nsid1, 0); // terminator
    }

    #[tokio::test]
    async fn dispatch_io_returns_invalid_namespace_for_non_admitted() {
        let (_tmp, disp) = fixture_two_ns().await;
        // Read NSID 1 (ns1) while admitted only to ns2.
        let allow = vec!["ns2".to_string()];
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = NvmOpcode::Read as u8;
        b[2] = 0x05; // CID
        b[4] = 0x01; // NSID = 1 (not admitted)
        let sqe = nvme_base::Sqe::parse(&b).unwrap();
        let resp = disp
            .handle_io(IoCommand {
                sqe,
                data_out: None,
                data_in_max: 4096,
                session_volumes: Some(&allow),
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_namespace());
    }

    // ---- NVMe reservations ----

    use nvme_base::reservation as wire;

    fn sqe_resv(opcode: NvmOpcode, cdw10: u32, cdw11: u32) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = opcode as u8;
        b[2] = 0x60; // CID
        b[4] = 0x01; // NSID = 1
        b[40..44].copy_from_slice(&cdw10.to_le_bytes());
        b[44..48].copy_from_slice(&cdw11.to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    fn keys16(crkey: u64, second: u64) -> Vec<u8> {
        let mut v = vec![0u8; 16];
        v[0..8].copy_from_slice(&crkey.to_le_bytes());
        v[8..16].copy_from_slice(&second.to_le_bytes());
        v
    }

    async fn resv_io(
        disp: &NvmeNvmDispatcher,
        sqe: nvme_base::Sqe,
        data_out: Option<&[u8]>,
        host_id: [u8; 16],
    ) -> NvmeResponse {
        disp.handle_io(IoCommand {
            sqe,
            data_out,
            data_in_max: 4096,
            session_volumes: None,
            host_id: Some(host_id),
        })
        .await
    }

    /// Register a key + acquire Write Exclusive, then a Reservation
    /// Report reflects the holder and key.
    #[tokio::test]
    async fn nvme_register_acquire_report_round_trip() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let host = [0xA1u8; 16];
        // Give the host a live controller so the Reservation Report can
        // map its HOSTID to a real CNTLID (rather than the 0 sentinel).
        let cntlid = controller_for(&disp, host);

        // Register key 0xCAFE (RREGA=Register, CRKEY=0, NRKEY=0xCAFE).
        let reg = sqe_resv(
            NvmOpcode::ReservationRegister,
            wire::RREGA_REGISTER as u32,
            0,
        );
        let r = resv_io(&disp, reg, Some(&keys16(0, 0xCAFE)), host).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);

        // Acquire Write Exclusive (NVMe RTYPE 1) with CRKEY=0xCAFE.
        let cdw10 = wire::RACQA_ACQUIRE as u32 | (1u32 << 8);
        let acq = sqe_resv(NvmOpcode::ReservationAcquire, cdw10, 0);
        let r = resv_io(&disp, acq, Some(&keys16(0xCAFE, 0)), host).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);

        // Report (NUMD large enough; EDS=0).
        let rep = sqe_resv(NvmOpcode::ReservationReport, 0x100, 0);
        let r = resv_io(&disp, rep, None, host).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);
        // RTYPE=1 (Write Exclusive); REGCTL=1; holder bit + key set.
        assert_eq!(r.data_in[4], 1, "RTYPE should be Write Exclusive");
        assert_eq!(&r.data_in[5..7], &1u16.to_le_bytes(), "one registrant");
        let entry = &r.data_in[wire::STATUS_HEADER_LEN..];
        assert_eq!(
            &entry[0..2],
            &cntlid.to_le_bytes(),
            "real CNTLID, not static 1"
        );
        assert_eq!(entry[2], 1, "holder bit set");
        // Spec offsets (issue #129): HOSTID at 8..16, RKEY at 16..24.
        assert_eq!(&entry[8..16], &host[0..8], "HOSTID low 64");
        assert_eq!(&entry[16..24], &0xCAFEu64.to_le_bytes(), "RKEY");
    }

    /// A registrant whose host has no live controller (registered, then
    /// its controller torn down — or never connected through the
    /// transport) is reported with CNTLID 0, the no-live-controller
    /// sentinel, while its HOSTID is intact.
    #[tokio::test]
    async fn nvme_report_cntlid_zero_without_live_controller() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let host = [0xC3u8; 16];
        // Register the host but give it no controller in the registry.
        let reg = sqe_resv(
            NvmOpcode::ReservationRegister,
            wire::RREGA_REGISTER as u32,
            0,
        );
        let r = resv_io(&disp, reg, Some(&keys16(0, 0xCAFE)), host).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);

        let rep = sqe_resv(NvmOpcode::ReservationReport, 0x100, 0);
        let r = resv_io(&disp, rep, None, host).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);
        assert_eq!(&r.data_in[5..7], &1u16.to_le_bytes(), "one registrant");
        let entry = &r.data_in[wire::STATUS_HEADER_LEN..];
        assert_eq!(
            &entry[0..2],
            &0u16.to_le_bytes(),
            "CNTLID 0 = no live controller"
        );
        assert_eq!(&entry[8..16], &host[0..8], "HOSTID still present");
    }

    /// Cross-host fencing: host A holds Write Exclusive; host B is
    /// denied writes but allowed reads; host A may write.
    #[tokio::test]
    async fn nvme_reservation_fences_other_host() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let a = [0xA1u8; 16];
        let b = [0xB2u8; 16];

        let reg = sqe_resv(
            NvmOpcode::ReservationRegister,
            wire::RREGA_REGISTER as u32,
            0,
        );
        resv_io(&disp, reg, Some(&keys16(0, 0xAAAA)), a).await;
        let acq = sqe_resv(
            NvmOpcode::ReservationAcquire,
            wire::RACQA_ACQUIRE as u32 | (1u32 << 8),
            0,
        );
        assert_eq!(
            resv_io(&disp, acq, Some(&keys16(0xAAAA, 0)), a)
                .await
                .cqe
                .status,
            StatusField::SUCCESS
        );

        // B's WRITE is fenced; B's READ is allowed under Write Exclusive.
        let payload = vec![0u8; 4096];
        let b_write = resv_io(&disp, sqe_io(NvmOpcode::Write, 0, 0), Some(&payload), b).await;
        assert_eq!(b_write.cqe.status, StatusField::reservation_conflict());
        let b_read = resv_io(&disp, sqe_io(NvmOpcode::Read, 0, 0), None, b).await;
        assert_eq!(b_read.cqe.status, StatusField::SUCCESS);
        // A may write.
        let a_write = resv_io(&disp, sqe_io(NvmOpcode::Write, 0, 0), Some(&payload), a).await;
        assert_eq!(a_write.cqe.status, StatusField::SUCCESS);
    }

    /// Acquire from an unregistered host → Reservation Conflict.
    #[tokio::test]
    async fn nvme_acquire_without_register_conflicts() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let acq = sqe_resv(
            NvmOpcode::ReservationAcquire,
            wire::RACQA_ACQUIRE as u32 | (1u32 << 8),
            0,
        );
        let r = resv_io(&disp, acq, Some(&keys16(0xAAAA, 0)), [0xA1u8; 16]).await;
        assert_eq!(r.cqe.status, StatusField::reservation_conflict());
    }

    /// CPTPL = "set PTPL" is rejected (PTPL not supported).
    #[tokio::test]
    async fn nvme_register_cptpl_persist_rejected() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let cdw10 = wire::RREGA_REGISTER as u32 | ((wire::CPTPL_PERSIST as u32) << 30);
        let reg = sqe_resv(NvmOpcode::ReservationRegister, cdw10, 0);
        let r = resv_io(&disp, reg, Some(&keys16(0, 0xCAFE)), [0xA1u8; 16]).await;
        assert_eq!(r.cqe.status, StatusField::invalid_field());
    }

    async fn admin_io(
        disp: &NvmeNvmDispatcher,
        sqe: nvme_base::Sqe,
        cntlid: Option<u16>,
    ) -> NvmeResponse {
        disp.handle_admin(AdminCommand {
            sqe,
            data_out: None,
            data_in_max: 4096,
            session_volumes: None,
            cntlid,
            local_addr: None,
        })
        .await
    }

    /// Register an admin controller for `host` in the dispatcher's
    /// registry and return its CNTLID — the per-controller key the
    /// admin AER commands (FID 0x82 / LID 0x80) are addressed by.
    fn controller_for(disp: &NvmeNvmDispatcher, host: [u8; 16]) -> u16 {
        disp.aer
            .connect_admin(host)
            .expect("cntlid available")
            .cntlid()
    }

    /// Get Log Page LID 0x80 for NSID 1, sized for one 64-byte entry
    /// (NUMD zero-based = 15 dwords).
    fn sqe_get_resv_log() -> nvme_base::Sqe {
        let cdw10 = u32::from(nvme_base::log_page::lid::RESERVATION_NOTIFICATION) | (15u32 << 16);
        sqe_admin(AdminOpcode::GetLogPage as u8, 1, cdw10)
    }

    /// A holds Write Exclusive on the helper's two-host fixture; B
    /// registers, then preempts. Drive the register + acquire for both.
    async fn setup_preempt(disp: &NvmeNvmDispatcher, a: [u8; 16], b: [u8; 16]) {
        resv_io(
            disp,
            sqe_resv(
                NvmOpcode::ReservationRegister,
                wire::RREGA_REGISTER as u32,
                0,
            ),
            Some(&keys16(0, 0xAAAA)),
            a,
        )
        .await;
        resv_io(
            disp,
            sqe_resv(
                NvmOpcode::ReservationAcquire,
                wire::RACQA_ACQUIRE as u32 | (1u32 << 8),
                0,
            ),
            Some(&keys16(0xAAAA, 0)),
            a,
        )
        .await;
        resv_io(
            disp,
            sqe_resv(
                NvmOpcode::ReservationRegister,
                wire::RREGA_REGISTER as u32,
                0,
            ),
            Some(&keys16(0, 0xBBBB)),
            b,
        )
        .await;
    }

    /// LID 0x80 is well-formed and empty when nothing has happened.
    #[tokio::test]
    async fn nvme_reservation_notif_log_empty_when_idle() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let ca = controller_for(&disp, [0xA1u8; 16]);
        let log = admin_io(&disp, sqe_get_resv_log(), Some(ca)).await;
        assert_eq!(log.cqe.status, StatusField::SUCCESS);
        assert_eq!(log.data_in.len(), 64);
        assert!(
            log.data_in.iter().all(|&x| x == 0),
            "empty page is all zero"
        );
    }

    /// End-to-end: B preempts A's Write Exclusive; host A learns via
    /// the Reservation Notification log; the issuer B is not notified.
    #[tokio::test]
    async fn nvme_preempt_emits_reservation_notification() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let a = [0xA1u8; 16];
        let b = [0xB2u8; 16];
        // Each host has a live controller (its admin queue); the
        // notification log is addressed per CNTLID.
        let ca = controller_for(&disp, a);
        let cb = controller_for(&disp, b);
        setup_preempt(&disp, a, b).await;

        let preempt = sqe_resv(
            NvmOpcode::ReservationAcquire,
            wire::RACQA_PREEMPT as u32 | (1u32 << 8),
            0,
        );
        let r = resv_io(&disp, preempt, Some(&keys16(0xBBBB, 0xAAAA)), b).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);

        // A held the reservation and lost it → Reservation Preempted (3).
        let log = admin_io(&disp, sqe_get_resv_log(), Some(ca)).await;
        assert_eq!(log.cqe.status, StatusField::SUCCESS);
        assert_eq!(log.data_in[8], 3, "Reservation Preempted");
        assert_eq!(
            u32::from_le_bytes(log.data_in[12..16].try_into().unwrap()),
            1,
            "nsid 1"
        );
        // Consumed on read → empty page next time.
        let again = admin_io(&disp, sqe_get_resv_log(), Some(ca)).await;
        assert_eq!(again.data_in[8], 0, "drained after consume");
        // Issuer B was never notified.
        let b_log = admin_io(&disp, sqe_get_resv_log(), Some(cb)).await;
        assert_eq!(b_log.data_in[8], 0, "issuer not notified");
    }

    /// FID 0x82 Set/Get round-trips and a set mask bit suppresses the
    /// matching notification class.
    #[tokio::test]
    async fn nvme_reservation_notif_mask_round_trip_and_suppresses() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let a = [0xA1u8; 16];
        let b = [0xB2u8; 16];
        let ca = controller_for(&disp, a);
        let cb = controller_for(&disp, b);

        // Set Features FID 0x82, NSID 1, mask Reservation Preempted (bit 3).
        let mut sb = vec![0u8; nvme_base::SQE_SIZE];
        sb[0] = AdminOpcode::SetFeatures as u8;
        sb[4..8].copy_from_slice(&1u32.to_le_bytes());
        sb[40..44].copy_from_slice(&u32::from(FID_RESERVATION_NOTIF_MASK).to_le_bytes());
        sb[44..48].copy_from_slice(&(1u32 << 3).to_le_bytes());
        let set = nvme_base::Sqe::parse(&sb).unwrap();
        let r = admin_io(&disp, set, Some(ca)).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);
        assert_eq!(r.cqe.dw0, 1 << 3, "Set echoes the stored mask");

        // Get Features FID 0x82 reflects the stored mask.
        let get_fid = u32::from(FID_RESERVATION_NOTIF_MASK);
        let get_a = sqe_admin(AdminOpcode::GetFeatures as u8, 1, get_fid);
        assert_eq!(admin_io(&disp, get_a, Some(ca)).await.cqe.dw0, 1 << 3);

        // A different controller's mask is independent (defaults to 0).
        let get_b = sqe_admin(AdminOpcode::GetFeatures as u8, 1, get_fid);
        assert_eq!(admin_io(&disp, get_b, Some(cb)).await.cqe.dw0, 0);

        // Preempt A; the masked Reservation Preempted is not queued.
        setup_preempt(&disp, a, b).await;
        let preempt = sqe_resv(
            NvmOpcode::ReservationAcquire,
            wire::RACQA_PREEMPT as u32 | (1u32 << 8),
            0,
        );
        resv_io(&disp, preempt, Some(&keys16(0xBBBB, 0xAAAA)), b).await;
        let log = admin_io(&disp, sqe_get_resv_log(), Some(ca)).await;
        assert_eq!(log.data_in[8], 0, "masked notification not queued");
    }

    /// Set Features FID 0x0B (Async Event Configuration) build helper —
    /// CDW11 carries the controller-wide config bitmap.
    fn sqe_set_aen_config(config: u32) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = AdminOpcode::SetFeatures as u8;
        b[40..44].copy_from_slice(&u32::from(FID_ASYNC_EVENT_CONFIG).to_le_bytes());
        b[44..48].copy_from_slice(&config.to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    /// Get Log Page LID 0x04 for the whole 4 KiB Changed Namespace List
    /// (NUMD zero-based = 1023 dwords).
    fn sqe_get_changed_ns_log() -> nvme_base::Sqe {
        let cdw10 = u32::from(nvme_base::log_page::lid::CHANGED_NAMESPACE_LIST) | (1023u32 << 16);
        sqe_admin(AdminOpcode::GetLogPage as u8, 0, cdw10)
    }

    /// FID 0x0B Set/Get round-trips per-controller and defaults to 0.
    #[tokio::test]
    async fn nvme_async_event_config_round_trip() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let ca = controller_for(&disp, [0xA1u8; 16]);
        let cb = controller_for(&disp, [0xB2u8; 16]);

        let get_fid = u32::from(FID_ASYNC_EVENT_CONFIG);
        // Default before any Set: nothing enabled.
        let get_a = sqe_admin(AdminOpcode::GetFeatures as u8, 0, get_fid);
        assert_eq!(admin_io(&disp, get_a, Some(ca)).await.cqe.dw0, 0);

        // Enable Namespace Attribute Notices (bit 8) on A.
        let set = sqe_set_aen_config(nvme_base::aer::AEN_CFG_NAMESPACE_ATTRIBUTE);
        let r = admin_io(&disp, set, Some(ca)).await;
        assert_eq!(r.cqe.status, StatusField::SUCCESS);
        assert_eq!(
            r.cqe.dw0,
            nvme_base::aer::AEN_CFG_NAMESPACE_ATTRIBUTE,
            "Set echoes the stored config"
        );

        // Get reflects it; the sibling controller is independent.
        let get_a2 = sqe_admin(AdminOpcode::GetFeatures as u8, 0, get_fid);
        assert_eq!(
            admin_io(&disp, get_a2, Some(ca)).await.cqe.dw0,
            nvme_base::aer::AEN_CFG_NAMESPACE_ATTRIBUTE
        );
        let get_b = sqe_admin(AdminOpcode::GetFeatures as u8, 0, get_fid);
        assert_eq!(admin_io(&disp, get_b, Some(cb)).await.cqe.dw0, 0);
    }

    /// LID 0x04 is well-formed + empty when idle; after a namespace
    /// change it lists the changed NSID and drains on read.
    #[tokio::test]
    async fn nvme_changed_namespace_list_reports_and_drains() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let ca = controller_for(&disp, [0xA1u8; 16]);

        // Idle: all-zero 4 KiB page.
        let idle = admin_io(&disp, sqe_get_changed_ns_log(), Some(ca)).await;
        assert_eq!(idle.cqe.status, StatusField::SUCCESS);
        assert_eq!(idle.data_in.len(), 4096);
        assert!(
            idle.data_in.iter().all(|&b| b == 0),
            "empty list is all zero"
        );

        // Opt in, then two volumes change out-of-band.
        admin_io(
            &disp,
            sqe_set_aen_config(nvme_base::aer::AEN_CFG_NAMESPACE_ATTRIBUTE),
            Some(ca),
        )
        .await;
        disp.aer.notify_namespace_attribute_changed(2);
        disp.aer.notify_namespace_attribute_changed(5);

        let log = admin_io(&disp, sqe_get_changed_ns_log(), Some(ca)).await;
        assert_eq!(log.cqe.status, StatusField::SUCCESS);
        assert_eq!(u32::from_le_bytes(log.data_in[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(log.data_in[4..8].try_into().unwrap()), 5);
        assert!(log.data_in[8..].iter().all(|&b| b == 0), "list terminates");

        // Drained on read → empty again.
        let again = admin_io(&disp, sqe_get_changed_ns_log(), Some(ca)).await;
        assert!(
            again.data_in.iter().all(|&b| b == 0),
            "drained after consume"
        );
    }

    /// The Release / Clear / Unregister adapter arms each drive a
    /// distinct notification to the *surviving* registrant, and never to
    /// the issuer. `reservations.rs` carries no inline tests and only
    /// Preempt is otherwise exercised through the integrated dispatcher
    /// path, so this pins the parse -> mgr-op -> event-derivation glue
    /// for the other three actions (issues #54 / #55).
    #[tokio::test]
    async fn nvme_release_clear_unregister_emit_correct_notifications() {
        use nvme_base::log_page::resv_notif_type;

        // (label, action SQE issued by A, notification type B observes).
        // Release leaves registrations -> survivors get Reservation
        // Released. Clear wipes them -> Reservation Preempted. Unregister
        // by the holder releases -> Reservation Released.
        for (label, sqe, expected) in [
            (
                "release",
                sqe_resv(
                    NvmOpcode::ReservationRelease,
                    wire::RRELA_RELEASE as u32 | (1u32 << 8), // RTYPE = Write Exclusive
                    0,
                ),
                resv_notif_type::RESERVATION_RELEASED,
            ),
            (
                "clear",
                sqe_resv(NvmOpcode::ReservationRelease, wire::RRELA_CLEAR as u32, 0),
                resv_notif_type::RESERVATION_PREEMPTED,
            ),
            (
                "unregister",
                sqe_resv(
                    NvmOpcode::ReservationRegister,
                    wire::RREGA_UNREGISTER as u32,
                    0,
                ),
                resv_notif_type::RESERVATION_RELEASED,
            ),
        ] {
            let (_tmp, disp) = fixture_dispatcher().await;
            let a = [0xA1u8; 16];
            let b = [0xB2u8; 16];
            // Both hosts hold a live controller; the notification log is
            // addressed per CNTLID.
            let ca = controller_for(&disp, a);
            let cb = controller_for(&disp, b);
            // A holds Write Exclusive (key 0xAAAA); B is a second
            // registrant (key 0xBBBB).
            setup_preempt(&disp, a, b).await;

            // A issues the action with its own current key.
            let r = resv_io(&disp, sqe, Some(&keys16(0xAAAA, 0)), a).await;
            assert_eq!(r.cqe.status, StatusField::SUCCESS, "{label} succeeds");

            // The surviving registrant B is notified with the right type.
            let b_log = admin_io(&disp, sqe_get_resv_log(), Some(cb)).await;
            assert_eq!(b_log.data_in[8], expected, "{label}: B notification type");
            assert_eq!(
                u32::from_le_bytes(b_log.data_in[12..16].try_into().unwrap()),
                1,
                "{label}: nsid 1",
            );

            // The issuer A is never notified of its own action.
            let a_log = admin_io(&disp, sqe_get_resv_log(), Some(ca)).await;
            assert_eq!(a_log.data_in[8], 0, "{label}: issuer A not notified");
        }
    }

    /// A fused Compare+Write from a non-holder is fenced by the
    /// write-side reservation gate (issue #54). Both fused tests above
    /// run with no reservation held, so that gate branch in
    /// `handle_fused_compare_write` is otherwise never executed — a
    /// regression would let a fenced host mutate the medium via the very
    /// op clusters use for test-and-set fencing.
    #[tokio::test]
    async fn fused_compare_write_fenced_for_nonholder() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let a = [0xA1u8; 16];
        let b = [0xB2u8; 16];

        // A registers and acquires Write Exclusive.
        resv_io(
            &disp,
            sqe_resv(
                NvmOpcode::ReservationRegister,
                wire::RREGA_REGISTER as u32,
                0,
            ),
            Some(&keys16(0, 0xAAAA)),
            a,
        )
        .await;
        assert_eq!(
            resv_io(
                &disp,
                sqe_resv(
                    NvmOpcode::ReservationAcquire,
                    wire::RACQA_ACQUIRE as u32 | (1u32 << 8),
                    0,
                ),
                Some(&keys16(0xAAAA, 0)),
                a,
            )
            .await
            .cqe
            .status,
            StatusField::SUCCESS,
        );

        // Non-holder B issues a fused Compare+Write. The gate denies the
        // compare half (reservation conflict) and aborts the write half
        // before any data buffer is read.
        let expected = vec![0u8; 64 * 1024];
        let new = vec![0u8; 64 * 1024];
        let mut csqe = vec![0u8; nvme_base::SQE_SIZE];
        csqe[0] = NvmOpcode::Compare as u8;
        csqe[1] = 0b0000_0001; // FUSE = First
        csqe[2] = 0x61;
        csqe[4] = 0x01;
        csqe[48..52].copy_from_slice(&15u32.to_le_bytes());
        let mut wsqe = vec![0u8; nvme_base::SQE_SIZE];
        wsqe[0] = NvmOpcode::Write as u8;
        wsqe[1] = 0b0000_0010; // FUSE = Second
        wsqe[2] = 0x62;
        wsqe[4] = 0x01;
        wsqe[48..52].copy_from_slice(&15u32.to_le_bytes());

        let (cqe_c, cqe_w) = disp
            .handle_fused_compare_write(
                IoCommand {
                    sqe: nvme_base::Sqe::parse(&csqe).unwrap(),
                    data_out: Some(&expected),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: Some(b),
                },
                IoCommand {
                    sqe: nvme_base::Sqe::parse(&wsqe).unwrap(),
                    data_out: Some(&new),
                    data_in_max: 0,
                    session_volumes: None,
                    host_id: Some(b),
                },
            )
            .await;
        assert_eq!(
            cqe_c.status,
            StatusField::reservation_conflict(),
            "compare half fenced",
        );
        assert_eq!(
            cqe_w.status,
            StatusField::aborted_due_to_failed_fused(),
            "write half aborted",
        );
        assert_eq!(cqe_c.cid, 0x61);
        assert_eq!(cqe_w.cid, 0x62);
    }

    #[test]
    fn failure_statuses_follow_the_fault_class_split() {
        // Issue #109: media faults get media-class statuses (SCT 2h)
        // instead of the generic Data Transfer Error, infrastructure
        // faults Internal Error, backpressure the retryable Namespace
        // Not Ready — mirroring the SBC mappers exactly.
        use core_block::BackpressureError;
        use core_block::chunk_pool::ChunkPoolError;
        use std::io;

        let hash_mismatch = UploaderError::ChunkPool(ChunkPoolError::HashMismatch {
            expected: "aa".into(),
            actual: "bb".into(),
        });
        assert_eq!(
            read_failure_status(&hash_mismatch),
            StatusField::unrecovered_read_error()
        );
        assert_eq!(
            write_failure_status(&hash_mismatch),
            StatusField::write_fault()
        );

        let decrypt = UploaderError::Decrypt("gcm auth tag mismatch");
        assert_eq!(
            read_failure_status(&decrypt),
            StatusField::unrecovered_read_error()
        );

        let eio = UploaderError::Io(io::Error::from(io::ErrorKind::Other));
        assert_eq!(read_failure_status(&eio), StatusField::internal_error());
        assert_eq!(write_failure_status(&eio), StatusField::internal_error());
        assert_eq!(
            write_failure_status(&UploaderError::UploadsFailed),
            StatusField::internal_error()
        );

        let backpressured = UploaderError::Backpressured(BackpressureError {
            pool_used_bytes: 1,
            pool_cap_bytes: 1,
            waited_secs: 1,
        });
        assert_eq!(
            write_failure_status(&backpressured),
            StatusField::namespace_not_ready()
        );
    }
}
