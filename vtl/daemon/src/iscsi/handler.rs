// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `shared_iscsi::ScsiHandler` impl for the tape product.
//!
//! Bridges the shared transport (PDU framing, login, R2T loop) to the
//! existing thurvtl dispatch tree (`protocol::handle_scsi_command` +
//! the 50+ per-opcode `handle_*` arms below it). The handler owns
//! `Arc` handles into every subsystem dispatch needs (DriveManager,
//! Library, UnitAttentionTracker, DiagnosticStore, audit channel,
//! cloud backend registry) — those used to live as scattered locals
//! inside the old `serve_connection` loop.
//!
//! Pre- / post-dispatch hooks that sit *around* the SCSI layer (not
//! part of the SCSI surface itself) live here too:
//!
//! - **Cloud-prefetch hook** (READ on a tape LUN): pulls the chunk
//!   backing the next-read LBA from cloud into the local pool before
//!   the sync read path runs. Best-effort — failures fall through.
//! - **SEND DIAGNOSTIC self-test pre-hook**: runs the async LUN-routed
//!   self-test and stamps the result in `DiagnosticStore` so the sync
//!   `handle_send_diagnostic` arm can read it back.
//! - **MOVE MEDIUM legal-hold post-hook**: after a successful
//!   load-into-drive, reads the cloud sentinel
//!   (`manifests/<barcode>/manifest-latest.json`) and stamps the
//!   volatile `legal_held` flag on the drive.
//!
//! Session lifecycle hooks via [`Self::on_session_close`] release
//! per-I_T-nexus drive locks and PREVENT/ALLOW state — same
//! semantics as the old `SessionGuard` drop in `serve_connection`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use core_mediachanger::{
    AuditChannel, AuditRateLimiter, Library, ObjectStoreConfig, PoolBudget, TapeEvent,
};
use shared_iscsi::{ScsiHandler, ScsiRequest, ScsiResponse, ScsiStatus};
use tokio::sync::broadcast;

use super::drive_manager::DriveManager;
use super::protocol::{self, Pdu, ScsiResp};
use super::server::ObjectStoreRegistry;
use super::unit_attention::UnitAttentionTracker;
use crate::diagnostics::DiagnosticStore;
use scsi_smc::changer::{ElementAddressConfig, ElementType};

/// `shared_iscsi::ScsiHandler` impl for the tape / library product.
/// Holds `Arc`s into the shared `DaemonState` so every dispatch can
/// reach the same handles the old `protocol::handle_connection`
/// threaded through as 13 parameters.
pub struct IscsiLibraryHandler {
    pub(crate) drive_manager: Arc<DriveManager>,
    pub(crate) library: Arc<Mutex<Library>>,
    pub(crate) ua_tracker: Arc<Mutex<UnitAttentionTracker>>,
    pub(crate) element_config: ElementAddressConfig,
    pub(crate) event_tx: broadcast::Sender<TapeEvent>,
    pub(crate) data_dir: PathBuf,
    pub(crate) audit_log: Option<AuditChannel>,
    pub(crate) audit_ratelimiter: Arc<AuditRateLimiter>,
    pub(crate) cloud_backends: ObjectStoreRegistry,
    pub(crate) storage_config: Arc<ObjectStoreConfig>,
    /// Per-backend pool budgets. The read-prefetch hook releases a
    /// reservation against the matching backend when it warms a
    /// cloud-fetched chunk into the local pool, keeping
    /// `current_bytes()` exact for the eviction worker.
    pub(crate) pool_budgets: HashMap<String, Arc<PoolBudget>>,
    pub(crate) diagnostic_store: Arc<DiagnosticStore>,
    /// iSCSI target IQN advertised in Login / SendTargets. Resolved
    /// at boot from `iscsi.target_iqn`.
    pub(crate) target_iqn: String,
    /// ALUA topology built from `iscsi.listen_portals` at daemon
    /// startup. Threaded into the SCSI dispatcher's `ScsiCtx::alua`
    /// for VPD 0x83 TargetPort designators + MAINTENANCE IN SA 0x0A
    /// REPORT TARGET PORT GROUPS.
    pub(crate) alua: Arc<shared_iscsi::alua::AluaTopology>,
}

impl IscsiLibraryHandler {
    /// Convert thurvtl's `ScsiResp` (Data-In bytes lives in
    /// `data_out`, sense optional) into the transport-facing
    /// [`ScsiResponse`]. The legacy thurvtl pipeline produces sense
    /// as a `Vec<u8>` from `SenseDataBuilder::build()`; the
    /// canonical scsi-spc shape carries structured
    /// `Option<SenseData>` so the transport can pick fixed vs
    /// descriptor format at PDU-wrap time. Lift via
    /// `SenseData::from_wire_bytes` — every legacy site emits
    /// fixed-format (response code 0x70) which round-trips cleanly.
    fn into_shared_response(resp: ScsiResp) -> ScsiResponse {
        let status = match resp.status {
            protocol::ScsiStatus::Good => ScsiStatus::Good,
            protocol::ScsiStatus::CheckCondition => ScsiStatus::CheckCondition,
        };
        let sense = resp
            .sense
            .as_deref()
            .and_then(scsi_spc::SenseData::from_wire_bytes);
        ScsiResponse {
            status,
            sense,
            data_in: resp.data_out,
        }
    }
}

#[async_trait]
impl ScsiHandler for IscsiLibraryHandler {
    fn target_iqn(&self) -> &str {
        &self.target_iqn
    }

    fn on_session_close(&self, tsih: u16, _cid: u16) {
        // Release every drive lock the session held, then drop
        // PREVENT/ALLOW state. Same teardown the old SessionGuard
        // ran when the connection's TCP stream closed.
        self.drive_manager.release_session_locks(tsih);
        self.drive_manager.clear_prevent_for_session(tsih);
    }

    async fn dispatch(&self, req: ScsiRequest<'_>) -> ScsiResponse {
        let cdb_opcode = req.cdb.first().copied().unwrap_or(0);
        // Canonical ScsiRequest carries LUN as u64 (SAM-5 flat-space-
        // friendly); narrow for thurvtl's single-level (LUNs < 256)
        // call sites (DriveManager indexing, diagnostics, …).
        let pdu_lun = (req.lun & 0xFF) as u8;
        let tsih = req.tsih;

        // Cloud-prefetch hook for tape READs (CDB op 0x08, LUN >= 1).
        // Pulls the chunk backing the next read LBA from cloud into
        // the local pool if missing — best-effort, errors fall
        // through.
        if pdu_lun >= 1 && cdb_opcode == 0x08 {
            let drive_id = (pdu_lun - 1) as usize;
            if let Err(e) = protocol::ensure_chunk_local_for_next_read(
                &self.drive_manager,
                drive_id,
                tsih,
                &self.cloud_backends,
                &self.storage_config,
                &self.pool_budgets,
            )
            .await
            {
                tracing::debug!(
                    "iSCSI prefetch: refetch hook returned {} - read will proceed without cloud refetch",
                    e
                );
            }
        }

        // SEND DIAGNOSTIC pre-hook: SELFTEST=1 (CDB byte 1 bit 2)
        // runs the LUN-routed self-test and stamps the result into
        // `diagnostic_store` so the sync `handle_send_diagnostic`
        // arm reads it back as the freshest entry.
        let send_diag_selftest =
            cdb_opcode == 0x1D && req.cdb.get(1).map(|b| (b & 0x04) != 0).unwrap_or(false);
        if send_diag_selftest {
            crate::diagnostics::run_and_record(
                pdu_lun,
                &self.drive_manager,
                &self.storage_config,
                &self.data_dir,
                &self.diagnostic_store,
            )
            .await;
        }

        // MOVE MEDIUM (LUN 0, 0xA5) load-to-drive detection: capture
        // the destination drive_id pre-call so the legal-hold
        // sentinel HEAD post-call can run. src must be Storage or
        // ImportExport, dst must be DataTransfer.
        let move_medium_loaded_drive: Option<usize> =
            if pdu_lun == 0 && cdb_opcode == 0xA5 && req.cdb.len() >= 6 {
                let cdb_src = u16::from_be_bytes([req.cdb[2], req.cdb[3]]);
                let cdb_dst = u16::from_be_bytes([req.cdb[4], req.cdb[5]]);
                let src_t = self.element_config.element_type_from_address(cdb_src);
                let dst_t = self.element_config.element_type_from_address(cdb_dst);
                match (src_t, dst_t) {
                    (Some(ElementType::Storage), Some(ElementType::DataTransfer))
                    | (Some(ElementType::ImportExport), Some(ElementType::DataTransfer)) => self
                        .element_config
                        .address_to_drive_id(cdb_dst)
                        .map(|d| d as usize),
                    _ => None,
                }
            } else {
                None
            };

        // SCSI dispatch on the blocking pool — `cart.write_data` can
        // park on `PoolBudget` for up to
        // `backpressure_max_wait_seconds`; running that on a tokio
        // worker would wedge the runtime.
        let dm = Arc::clone(&self.drive_manager);
        let lib = Arc::clone(&self.library);
        let ua = Arc::clone(&self.ua_tracker);
        let elem = self.element_config;
        let ev = self.event_tx.clone();
        let data_dir = self.data_dir.clone();
        let audit = self.audit_log.clone();
        let ratelim = Arc::clone(&self.audit_ratelimiter);
        let iqn = req.initiator_iqn.map(str::to_string);
        let peer = req.peer.to_string();
        let partition = req.session_partition.map(str::to_string);
        let diag = Arc::clone(&self.diagnostic_store);
        let alua = Arc::clone(&self.alua);
        let mut pdu = Pdu::synth(req.cdb, req.lun, req.data_in_max, req.data_out);

        let resp_result: Result<ScsiResp> = tokio::task::spawn_blocking(move || {
            protocol::handle_scsi_command(
                &mut pdu,
                tsih,
                dm,
                lib,
                ua,
                elem,
                ev,
                &data_dir,
                &audit,
                ratelim,
                iqn.as_deref(),
                &peer,
                partition,
                diag,
                Some(alua),
            )
        })
        .await
        .unwrap_or_else(|e| Err(anyhow!("SCSI dispatch task panicked: {}", e)));

        let resp = match resp_result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    "SCSI dispatch error (LUN={} op=0x{:02x}): {}",
                    pdu_lun,
                    cdb_opcode,
                    e
                );
                ScsiResp {
                    status: protocol::ScsiStatus::CheckCondition,
                    data_out: Vec::new(),
                    sense: None,
                }
            }
        };

        // MOVE MEDIUM legal-hold post-hook: if a cartridge was just
        // loaded into a drive, read its cloud sentinel and stamp the
        // volatile `legal_held` flag on the drive so host writes
        // return WRITE PROTECTED for the duration of the load.
        if matches!(resp.status, protocol::ScsiStatus::Good)
            && let Some(drive_id) = move_medium_loaded_drive
            && let Ok(Some((barcode, backend_name))) =
                self.drive_manager.get_loaded_cartridge_info(drive_id)
        {
            let held = protocol::read_legal_hold_at_load(
                &self.cloud_backends,
                &self.storage_config,
                &backend_name,
                &barcode,
            )
            .await;
            if let Err(e) = self.drive_manager.set_legal_held(drive_id, held) {
                tracing::warn!(
                    "iSCSI legal-hold post-hook: failed to set flag on drive {}: {}",
                    drive_id,
                    e
                );
            }
        }

        Self::into_shared_response(resp)
    }
}
