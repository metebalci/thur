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
//! Set Features (FID 0x07 Number of Queues + FID 0x0F Keep Alive
//! Timer), Get Log Page (Error / SMART / FW Slot), Keep Alive,
//! Abort. AER returns Invalid Opcode (NVMe Base §5.2 lets that
//! terminate the host's AER loop cleanly).
//!
//! NVM I/O commands: Read, Write, Flush, Compare, Write Zeroes,
//! DSM Deallocate, Verify, fused Compare+Write (the transport
//! pairs the two SQEs and invokes
//! `handle_fused_compare_write`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use async_trait::async_trait;

use core_block::PageCache;
use nvme_base::identify::CNS;
use nvme_base::{AdminOpcode, Cqe, IdentifyController, IdentifyNamespace, StatusField};

use crate::NamespaceLookup;
use crate::handler::{AdminCommand, IoCommand, NvmeCommandHandler, NvmeResponse};
use crate::opcode::NvmOpcode;

/// Default I/O queue cap advertised via Get/Set Features
/// (FID 0x07 Number of Queues). Each I/O queue corresponds to one
/// NVMe/TCP connection; 64 matches typical kernel host expectations
/// without being so high that an aggressive host floods us with
/// connections.
const DEFAULT_NUM_IO_QUEUES: u16 = 64;

/// Feature Identifier for Number of Queues (NVMe Base §5.21.1.7).
const FID_NUMBER_OF_QUEUES: u8 = 0x07;

/// Feature Identifier for Keep Alive Timer (NVMe Base §5.21.1.15).
/// Linux nvme-tcp's `nvme_set_keep_alive` issues Set Features 0x0F
/// with the negotiated KATO (in ms) on every controller bring-up;
/// without this handler, Identify completes but the very next admin
/// command fails and the host aborts the session.
const FID_KEEP_ALIVE_TIMER: u8 = 0x0F;

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
    /// Number of I/O Submission Queues granted to the host
    /// (zero-based). Set Features can lower this; it tops out at
    /// [`DEFAULT_NUM_IO_QUEUES`] minus 1. Atomic so concurrent
    /// command dispatch is safe.
    num_io_sqs_zero_based: AtomicU16,
    /// Same shape for Completion Queues.
    num_io_cqs_zero_based: AtomicU16,
    /// Host-negotiated Keep Alive Timeout, in ms. Stored on Set
    /// Features 0x0F, echoed on Get Features 0x0F. We don't enforce
    /// the timer ourselves — the dispatcher just acknowledges Keep
    /// Alive admin commands when they arrive — so this is purely
    /// host-visible state.
    kato_ms: AtomicU32,
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
    ) -> Self {
        Self {
            registry,
            subnqn,
            controller_sn,
            controller_mn,
            controller_fr,
            num_io_sqs_zero_based: AtomicU16::new(DEFAULT_NUM_IO_QUEUES - 1),
            num_io_cqs_zero_based: AtomicU16::new(DEFAULT_NUM_IO_QUEUES - 1),
            kato_ms: AtomicU32::new(0),
        }
    }

    async fn dispatch_io(&self, cmd: IoCommand<'_>) -> NvmeResponse {
        let sqe = &cmd.sqe;
        let cid = sqe.cid;
        let Some(opcode) = NvmOpcode::from_u8(sqe.opcode) else {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode()));
        };
        let Some(cache) = self.registry.get(sqe.nsid) else {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_namespace()));
        };
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
            AdminOpcode::Identify => self.cmd_identify(cid, sqe).await,
            AdminOpcode::GetFeatures => self.cmd_get_features(cid, sqe),
            AdminOpcode::SetFeatures => self.cmd_set_features(cid, sqe),
            AdminOpcode::GetLogPage => self.cmd_get_log_page(cid, sqe),
            AdminOpcode::KeepAlive => {
                // No-op — host pings us on the admin queue to confirm
                // the controller is alive. We have nothing to update
                // on the controller side today; future Discovery /
                // AER work would touch a per-connection timer here.
                NvmeResponse::just(Cqe::success(cid, 0, 0, 0))
            }
            AdminOpcode::AsyncEventRequest => {
                // VSA produces no asynchronous events today (no
                // namespace add/remove notifications, no firmware
                // activation, no thermal events). Per NVMe Base §5.2
                // AER has no timeout — never completing is legal —
                // but Linux nvme-tcp posts AERs and logs warnings on
                // controller bring-up if it doesn't see them
                // completed within a short window. Return Invalid
                // Command Opcode so the host stops resubmitting and
                // logs a single, clear "AER not supported" notice.
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

    fn cmd_get_features(&self, cid: u16, sqe: &nvme_base::Sqe) -> NvmeResponse {
        // CDW10[7:0] FID, [10:8] SEL (0=current, 1=default, 2=saved,
        // 3=supported). We treat all SELs as "current" for now —
        // host most commonly uses SEL=0 and ours have no separate
        // default / saved state to distinguish.
        let fid = (sqe.cdw10 & 0xFF) as u8;
        match fid {
            FID_NUMBER_OF_QUEUES => {
                let nsq = self.num_io_sqs_zero_based.load(Ordering::Acquire);
                let ncq = self.num_io_cqs_zero_based.load(Ordering::Acquire);
                let dw0 = u32::from(ncq) | (u32::from(nsq) << 16);
                NvmeResponse::just(Cqe::success(cid, 0, 0, dw0))
            }
            FID_KEEP_ALIVE_TIMER => {
                let kato = self.kato_ms.load(Ordering::Acquire);
                NvmeResponse::just(Cqe::success(cid, 0, 0, kato))
            }
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field())),
        }
    }

    fn cmd_set_features(&self, cid: u16, sqe: &nvme_base::Sqe) -> NvmeResponse {
        let fid = (sqe.cdw10 & 0xFF) as u8;
        tracing::debug!(
            fid = format!("0x{:02X}", fid),
            cdw11 = format!("0x{:08X}", sqe.cdw11),
            "nvme: Set Features",
        );
        match fid {
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
                self.num_io_cqs_zero_based
                    .store(granted_ncq, Ordering::Release);
                self.num_io_sqs_zero_based
                    .store(granted_nsq, Ordering::Release);
                let dw0 = u32::from(granted_ncq) | (u32::from(granted_nsq) << 16);
                NvmeResponse::just(Cqe::success(cid, 0, 0, dw0))
            }
            FID_KEEP_ALIVE_TIMER => {
                // CDW11 carries the timer in ms (Linux nvme-tcp's
                // nvme_set_keep_alive passes `kato * 1000`). We don't
                // run a watchdog — Keep Alive admin commands are
                // acknowledged unconditionally — so the value is
                // stored only for Get Features symmetry.
                self.kato_ms.store(sqe.cdw11, Ordering::Release);
                NvmeResponse::just(Cqe::success(cid, 0, 0, sqe.cdw11))
            }
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field())),
        }
    }

    fn cmd_get_log_page(&self, cid: u16, sqe: &nvme_base::Sqe) -> NvmeResponse {
        // CDW10[7:0] LID, [15:8] LSP, [31:16] NUMDL (zero-based).
        // CDW11[15:0] NUMDU (zero-based). Total dwords = (NUMDL |
        // (NUMDU << 16)) + 1.
        let lid = (sqe.cdw10 & 0xFF) as u8;
        let numdl = (sqe.cdw10 >> 16) & 0xFFFF;
        let numdu = sqe.cdw11 & 0xFFFF;
        let total_dwords = numdl | (numdu << 16);
        let total_bytes = total_dwords.saturating_add(1).saturating_mul(4) as usize;
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
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload)
    }

    async fn cmd_identify(&self, cid: u16, sqe: &nvme_base::Sqe) -> NvmeResponse {
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
                let ic = match IdentifyController::new(
                    self.controller_sn.clone(),
                    self.controller_mn.clone(),
                    self.controller_fr.clone(),
                    1,
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
                let id = match IdentifyNamespace::from_volume(size_bytes, lba_bytes, nguid) {
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
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), id.to_bytes().to_vec())
            }
            CNS::ActiveNamespaceList => {
                let mut nsids = self.registry.active_namespaces();
                nsids.sort_unstable();
                let payload = nvme_base::identify::active_namespace_list(&nsids, sqe.nsid);
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload.to_vec())
            }
            CNS::NamespaceIdDescList => {
                // Validate NSID exists; per spec the Descriptor list
                // is per-namespace and an invalid NSID should error.
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
                NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::internal_error()))
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
        let lba_bytes = cache.sector_size();
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let len_bytes = u64::from(nlb) * lba_bytes;
        if let Some(resp) = check_range(cid, cache, slba, len_bytes) {
            return resp;
        }
        if len_bytes > u64::from(data_in_max) {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
        }
        let byte_off = slba * lba_bytes;
        match cache.read_bytes(byte_off, len_bytes as usize).await {
            Ok(buf) => NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), buf),
            Err(e) => {
                tracing::warn!(error = %e, "read failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::data_transfer_error()))
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
        let lba_bytes = cache.sector_size();
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let len_bytes = u64::from(nlb) * lba_bytes;
        if let Some(resp) = check_range(cid, cache, slba, len_bytes) {
            return resp;
        }
        let payload = match data_out {
            Some(p) if p.len() as u64 == len_bytes => p,
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        let byte_off = slba * lba_bytes;
        match cache.write_bytes(byte_off, payload).await {
            Ok(()) => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "write failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::data_transfer_error()))
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
        let lba_bytes = cache.sector_size();
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let len_bytes = u64::from(nlb) * lba_bytes;
        if let Some(resp) = check_range(cid, cache, slba, len_bytes) {
            return resp;
        }
        let expected = match data_out {
            Some(p) if p.len() as u64 == len_bytes => p,
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        let byte_off = slba * lba_bytes;
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
                NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::data_transfer_error()))
            }
        }
    }

    async fn cmd_write_zeroes(
        &self,
        cid: u16,
        sqe: &nvme_base::Sqe,
        cache: &PageCache,
    ) -> NvmeResponse {
        let lba_bytes = cache.sector_size();
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let len_bytes = u64::from(nlb) * lba_bytes;
        if let Some(resp) = check_range(cid, cache, slba, len_bytes) {
            return resp;
        }
        // VSA's write path dedups identical pages, so a zero buffer
        // collapses to the canonical zero-chunk reference. unmap is
        // a possible optimization but core-block doesn't yet expose
        // a hint that distinguishes deallocate vs zero-fill semantics,
        // and zero-write preserves the read-back contract NVMe writes
        // expect (all zeros, not "unallocated").
        let zeros = vec![0u8; len_bytes as usize];
        let byte_off = slba * lba_bytes;
        match cache.write_bytes(byte_off, &zeros).await {
            Ok(()) => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "write-zeroes failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::data_transfer_error()))
            }
        }
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
        let nr_zero_based = (sqe.cdw10 & 0xFF) as usize;
        let count = nr_zero_based + 1;
        let expected_bytes = count * 16;
        let payload = match data_out {
            Some(p) if p.len() == expected_bytes => p,
            _ => {
                return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
            }
        };
        let lba_bytes = cache.sector_size();
        for chunk in payload.chunks_exact(16) {
            let nlb = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let slba = u64::from_le_bytes([
                chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14],
                chunk[15],
            ]);
            let len = u64::from(nlb) * lba_bytes;
            if let Some(resp) = check_range(cid, cache, slba, len) {
                return resp;
            }
            let byte_off = slba * lba_bytes;
            if let Err(e) = cache.unmap_bytes(byte_off, len).await {
                tracing::warn!(error = %e, "dsm deallocate failed");
                return NvmeResponse::just(Cqe::failure(
                    cid,
                    0,
                    0,
                    StatusField::data_transfer_error(),
                ));
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
        let lba_bytes = cache.sector_size();
        let slba = read_slba(sqe);
        let nlb = read_nlb(sqe);
        let len_bytes = u64::from(nlb) * lba_bytes;
        if let Some(resp) = check_range(cid, cache, slba, len_bytes) {
            return resp;
        }
        let byte_off = slba * lba_bytes;
        match cache.read_bytes(byte_off, len_bytes as usize).await {
            Ok(_) => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "verify failed");
                NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::data_transfer_error()))
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

/// Range-check an LBA span. Returns a populated NvmeResponse on
/// error so callers `return` straight through; returns None on a
/// valid range.
fn check_range(cid: u16, cache: &PageCache, slba: u64, len_bytes: u64) -> Option<NvmeResponse> {
    let size = cache.size_bytes();
    let lba_bytes = cache.sector_size();
    if lba_bytes == 0 {
        return Some(NvmeResponse::just(Cqe::failure(
            cid,
            0,
            0,
            StatusField::internal_error(),
        )));
    }
    let Some(end) = slba
        .checked_mul(lba_bytes)
        .and_then(|s| s.checked_add(len_bytes))
    else {
        return Some(NvmeResponse::just(Cqe::failure(
            cid,
            0,
            0,
            StatusField::lba_out_of_range(),
        )));
    };
    if end > size {
        return Some(NvmeResponse::just(Cqe::failure(
            cid,
            0,
            0,
            StatusField::lba_out_of_range(),
        )));
    }
    None
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
        let Some(cache) = self.registry.get(compare.sqe.nsid) else {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::invalid_namespace()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        };
        let lba_bytes = cache.sector_size();
        let slba = read_slba(&compare.sqe);
        let nlb = read_nlb(&compare.sqe);
        let len_bytes = u64::from(nlb) * lba_bytes;
        if let Some(_resp) = check_range(compare_cid, &cache, slba, len_bytes) {
            return (
                Cqe::failure(compare_cid, 0, 0, StatusField::lba_out_of_range()),
                Cqe::failure(write_cid, 0, 0, StatusField::aborted_due_to_failed_fused()),
            );
        }
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
        let byte_off = slba * lba_bytes;
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
                (
                    Cqe::failure(compare_cid, 0, 0, StatusField::internal_error()),
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
    use shared_cloud::{CloudBackend, LocalBackend};
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
    }

    async fn fixture_dispatcher() -> (TempDir, NvmeNvmDispatcher) {
        let tmp = TempDir::new().unwrap();
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn CloudBackend> = Arc::new(backend);

        VolumeManifest::new(
            "ns1".into(),
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
        let writer = Arc::new(VolumeWriter::open(tmp.path(), "ns1", backend).unwrap());
        let cache = PageCache::new(writer);

        let reg = TestRegistry::default();
        reg.register(1, cache);
        let disp = NvmeNvmDispatcher::new(
            Arc::new(reg),
            "nqn.2025-10.com.metebalci:thurvsa".into(),
            "TESTSN".into(),
            "ThurVSA Volume".into(),
            "0.1.0".into(),
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
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS, "write failed");

        let rsqe = sqe_read(0, 15);
        let resp = disp
            .handle_io(IoCommand {
                sqe: rsqe,
                data_out: None,
                data_in_max: 64 * 1024,
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
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert_eq!(resp.data_in.len(), 4096);
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
                },
                IoCommand {
                    sqe: wsqe2,
                    data_out: Some(&new),
                    data_in_max: 0,
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
                },
                IoCommand {
                    sqe: nvme_base::Sqe::parse(&wsqe2).unwrap(),
                    data_out: Some(&new),
                    data_in_max: 0,
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
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_namespace());
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
        })
        .await;

        let same = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Compare, 0, 0),
                data_out: Some(&payload),
                data_in_max: 0,
            })
            .await;
        assert_eq!(same.cqe.status, StatusField::SUCCESS);

        let differs = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Compare, 0, 0),
                data_out: Some(&vec![0x00u8; 4096]),
                data_in_max: 0,
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
        })
        .await;

        let wz = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::WriteZeroes, 0, 0),
                data_out: None,
                data_in_max: 0,
            })
            .await;
        assert_eq!(wz.cqe.status, StatusField::SUCCESS);

        let rd = disp
            .handle_io(IoCommand {
                sqe: sqe_io(NvmOpcode::Read, 0, 0),
                data_out: None,
                data_in_max: 4096,
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
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
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
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        // DW0 bit 0 = 1 means "command was not aborted".
        assert_eq!(resp.cqe.dw0 & 1, 1);
    }

    #[tokio::test]
    async fn admin_async_event_request_is_rejected() {
        let (_tmp, disp) = fixture_dispatcher().await;
        let resp = disp
            .handle_admin(AdminCommand {
                sqe: sqe_admin(0x0C, 0, 0), // AsyncEventRequest
                data_out: None,
                data_in_max: 0,
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
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_opcode());
    }
}
