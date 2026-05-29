// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `/api/v1/*` read-only handlers.
//!
//! Live-state inspection — every CLI command in the read-only bucket
//! lands here. Each handler locks the daemon's `Arc<Mutex<Library>>`
//! briefly for the inventory snapshot; cartridge-info also reads
//! `<tapes_root>/<barcode>/manifest.json` directly from disk for the
//! per-cartridge stats. The lock is released before any IO so a
//! single slow manifest read can't block iSCSI traffic.

use axum::{
    Json,
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use core_mediachanger::{
    AuditActor, AuditResult, Cartridge, ChunkingMode, DedupScope, FASTCDC_DEFAULT_AVG,
    FASTCDC_DEFAULT_MAX, ObjectStoreBackend, TapeEvent, apply_cartridge_legal_hold,
    collect_cartridge_keys, find_drive_for_loaded_cartridge, generate_cartridge_uuid,
    read_cartridge_held, read_legal_hold_for_keys,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::iscsi::unit_attention::UnitAttentionCode;
use crate::state::DaemonState;
use shared_admin_server::PeerCred;

/// Shared state pulled in by every admin handler. Future mutating
/// endpoints will share the same shape — peer credentials and the
/// audit handle hang off `DaemonState`.
#[derive(Clone)]
pub struct AdminState {
    pub daemon: Arc<DaemonState>,
}

// `system.monitor` per-tick view. The handler in `shared-admin-monitor`
// calls these accessors once per second to compose the JSON payload.
impl shared_admin_monitor::MonitorState for AdminState {
    fn daemon_name(&self) -> &str {
        "thurvtld"
    }
    fn version(&self) -> &str {
        crate::THURVTL_VERSION_STR
    }
    fn started_at_unix(&self) -> i64 {
        self.daemon.started_at_unix
    }
    fn live_stats(&self) -> Arc<shared_telemetry::LiveStats> {
        // The global is always set on the daemon side (see main.rs
        // boot); the fallback keeps the `--test` smoke path harmless.
        shared_telemetry::global()
            .map(|t| t.live_stats())
            .unwrap_or_else(|| Arc::new(shared_telemetry::LiveStats::default()))
    }
    fn pool_budgets(
        &self,
    ) -> std::collections::HashMap<String, Arc<core_mediachanger::PoolBudget>> {
        self.daemon.pool_budgets.clone()
    }
    fn pool_namespace_label(&self, _backend: &str, namespace: &str) -> Option<String> {
        // VTL namespace = the cartridge's `Manifest.label` (the
        // barcode); operators already recognise the label, no lookup
        // needed.
        Some(namespace.to_string())
    }
    fn snapshot_product(&self) -> shared_admin_monitor::ProductSnapshot {
        let (cartridges_loaded, cartridges_total, drives_busy, drives_total) = {
            let lib = self
                .daemon
                .library
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let storage_occupied = lib.storage_slots().iter().filter(|s| s.occupied).count();
            let mail_occupied = lib.mail_slots().iter().filter(|s| s.occupied).count();
            let drives_occupied = lib.drives().iter().filter(|d| d.occupied).count();
            let total = lib.storage_slots().len() + lib.mail_slots().len() + lib.drives().len();
            (
                storage_occupied + mail_occupied + drives_occupied,
                total,
                drives_occupied,
                lib.drives().len(),
            )
        };
        let sessions_active = self.daemon.session_manager.session_count() as u64;
        shared_admin_monitor::ProductSnapshot::Vtl {
            cartridges_loaded: cartridges_loaded as u64,
            cartridges_total: cartridges_total as u64,
            drives_busy: drives_busy as u64,
            drives_total: drives_total as u64,
            sessions_active,
        }
    }
}

// ---------------------------------------------------------------------------
// /api/v1/library/info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct LibraryInfo {
    pub storage_slots: usize,
    pub mail_slots: usize,
    pub drives: usize,
    pub lto_generation: u8,
    pub firmware: String,
    /// Logical partition layout. Empty when the library is in
    /// legacy single-partition mode.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<core_mediachanger::LibraryPartition>,
    /// Library-wide byte-counter aggregate. Present only when the
    /// caller passes `?with_cartridges=true`; the default response
    /// is topology-only (cheap — no per-cartridge file reads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cartridge_totals: Option<CartridgeTotals>,
}

/// The four cartridge byte counters summed across every cartridge in
/// the library. Each cartridge contributes its `runtime.json`
/// sidecar; a cartridge loaded in a drive contributes its
/// last-persisted values (see [`CartridgeInfo`]).
#[derive(Serialize)]
pub struct CartridgeTotals {
    /// Number of cartridges summed (every barcode in the library
    /// inventory — storage slots, mail slots, drives).
    pub cartridges: usize,
    pub host_bytes_written: u64,
    pub host_bytes_read: u64,
    pub backend_bytes_written: u64,
    pub backend_bytes_read: u64,
}

#[derive(Deserialize)]
pub struct LibraryInfoQuery {
    /// When true, also walk every cartridge's `runtime.json` and
    /// return the [`CartridgeTotals`] aggregate.
    #[serde(default)]
    pub with_cartridges: bool,
}

pub async fn library_info(
    State(state): State<AdminState>,
    Query(q): Query<LibraryInfoQuery>,
) -> impl IntoResponse {
    // Snapshot topology + (optionally) every barcode under the lock,
    // then release it before any file IO.
    let (storage_slots, mail_slots, drives, lto_generation, firmware, partitions, barcodes) = {
        let lib = state
            .daemon
            .library
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let barcodes: Vec<String> = if q.with_cartridges {
            let mut v: Vec<String> = Vec::new();
            v.extend(lib.storage_slots().iter().filter_map(|s| s.barcode.clone()));
            v.extend(lib.mail_slots().iter().filter_map(|s| s.barcode.clone()));
            v.extend(lib.drives().iter().filter_map(|d| d.barcode.clone()));
            v
        } else {
            Vec::new()
        };
        (
            lib.storage_slots().len(),
            lib.mail_slots().len(),
            lib.drives().len(),
            lib.lto_generation(),
            lib.drive_firmware().to_string(),
            lib.partitions().to_vec(),
            barcodes,
        )
    };

    let cartridge_totals = if q.with_cartridges {
        let tapes_root: PathBuf = state.daemon.data_dir.join("tapes");
        let mut totals = CartridgeTotals {
            cartridges: barcodes.len(),
            host_bytes_written: 0,
            host_bytes_read: 0,
            backend_bytes_written: 0,
            backend_bytes_read: 0,
        };
        for barcode in &barcodes {
            let runtime_path = tapes_root.join(barcode).join("runtime.json");
            // A missing / unparseable sidecar contributes 0 — keep the
            // aggregate best-effort rather than failing the whole call.
            let runtime: serde_json::Value = tokio::fs::read_to_string(&runtime_path)
                .await
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            let counter = |key: &str| runtime.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
            totals.host_bytes_written += counter("host_bytes_written");
            totals.host_bytes_read += counter("host_bytes_read");
            totals.backend_bytes_written += counter("backend_bytes_written");
            totals.backend_bytes_read += counter("backend_bytes_read");
        }
        Some(totals)
    } else {
        None
    };

    Json(LibraryInfo {
        storage_slots,
        mail_slots,
        drives,
        lto_generation,
        firmware,
        partitions,
        cartridge_totals,
    })
}

// ---------------------------------------------------------------------------
// /api/v1/library/bounds
// ---------------------------------------------------------------------------

/// Read-only view of the slot/drive sizing envelope: current YAML
/// counts, minimum the operator can shrink to without losing
/// cartridges (computed against live inventory), and the absolute
/// ceiling the SCSI / iSCSI wire formats allow. Powers
/// `thurvtl library bounds` — the operator's check before editing
/// the YAML `library:` block.
pub async fn library_bounds(State(state): State<AdminState>) -> impl IntoResponse {
    let bounds = {
        let lib = state
            .daemon
            .library
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        core_mediachanger::library::reconcile::compute_bounds(&lib)
    };
    Json(bounds)
}

// ---------------------------------------------------------------------------
// /api/v1/cartridges  (list)
// ---------------------------------------------------------------------------

/// Where a cartridge currently lives. Lowercase string discriminant
/// in JSON for ergonomic CLI rendering.
#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CartridgeLocation {
    Storage,
    Mail,
    Drive,
}

#[derive(Serialize)]
pub struct CartridgeListItem {
    pub barcode: String,
    pub location: CartridgeLocation,
    pub slot_id: u32,
}

#[derive(Serialize)]
pub struct CartridgeList {
    pub cartridges: Vec<CartridgeListItem>,
}

#[derive(Deserialize)]
pub struct CartridgesQuery {
    pub filter: Option<String>,
}

pub async fn cartridges_list(
    State(state): State<AdminState>,
    Query(q): Query<CartridgesQuery>,
) -> impl IntoResponse {
    let lib = state
        .daemon
        .library
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    let pattern = q.filter.as_deref();

    let push_if_match =
        |out: &mut Vec<CartridgeListItem>, barcode: &str, id: u32, loc: CartridgeLocation| {
            if let Some(p) = pattern
                && !barcode.contains(p)
            {
                return;
            }
            out.push(CartridgeListItem {
                barcode: barcode.to_string(),
                location: loc,
                slot_id: id,
            });
        };

    for slot in lib.storage_slots() {
        if let Some(barcode) = slot.barcode.as_deref() {
            push_if_match(&mut out, barcode, slot.id, CartridgeLocation::Storage);
        }
    }
    for slot in lib.mail_slots() {
        if let Some(barcode) = slot.barcode.as_deref() {
            push_if_match(&mut out, barcode, slot.id, CartridgeLocation::Mail);
        }
    }
    for drive in lib.drives() {
        if let Some(barcode) = drive.barcode.as_deref() {
            push_if_match(&mut out, barcode, drive.id, CartridgeLocation::Drive);
        }
    }

    Json(CartridgeList { cartridges: out })
}

// ---------------------------------------------------------------------------
// /api/v1/cartridges/{identifier}  (info)
// ---------------------------------------------------------------------------

/// Per-cartridge detail. The `total_blocks` / `filemarks` /
/// `data_blocks` / `data_bytes` / `chunk_count` fields are derived
/// from `manifest.json`; the four byte counters are read from the
/// `runtime.json` sidecar — so the CLI never has to walk the blocks
/// array or open the runtime file client-side. For a cartridge
/// currently loaded in a drive the counters are as-of the last
/// `runtime.json` persist (cartridge unload, or any `persist_runtime`
/// boundary); the in-memory `Cartridge` may hold fresher values.
#[derive(Serialize)]
pub struct CartridgeInfo {
    pub barcode: String,
    pub location: Option<CartridgeLocation>,
    pub slot_id: Option<u32>,
    pub backend: String,
    pub worm: bool,
    pub total_blocks: usize,
    pub filemarks: usize,
    pub data_blocks: usize,
    pub data_bytes: u64,
    pub chunk_count: usize,
    /// Lifetime front-end bytes the host has written — pre-dedup,
    /// pre-compression.
    pub host_bytes_written: u64,
    /// Lifetime plaintext bytes served to the host on READ.
    pub host_bytes_read: u64,
    /// Lifetime on-wire bytes PUT to cloud — post-dedup,
    /// post-compression.
    pub backend_bytes_written: u64,
    /// Lifetime bytes fetched from cloud on a chunk cache miss.
    pub backend_bytes_read: u64,
}

pub async fn cartridge_info(
    State(state): State<AdminState>,
    AxumPath(identifier): AxumPath<String>,
) -> impl IntoResponse {
    // Resolve identifier (barcode or numeric slot ID) to a canonical
    // barcode + location while holding the library lock briefly.
    let resolved = {
        let lib = state
            .daemon
            .library
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut found: Option<(String, CartridgeLocation, u32)> = None;

        // Match by barcode first.
        for slot in lib.storage_slots() {
            if let Some(b) = slot.barcode.as_deref()
                && b == identifier
            {
                found = Some((b.to_string(), CartridgeLocation::Storage, slot.id));
                break;
            }
        }
        if found.is_none() {
            for slot in lib.mail_slots() {
                if let Some(b) = slot.barcode.as_deref()
                    && b == identifier
                {
                    found = Some((b.to_string(), CartridgeLocation::Mail, slot.id));
                    break;
                }
            }
        }
        if found.is_none() {
            for drive in lib.drives() {
                if let Some(b) = drive.barcode.as_deref()
                    && b == identifier
                {
                    found = Some((b.to_string(), CartridgeLocation::Drive, drive.id));
                    break;
                }
            }
        }
        // Fall back to numeric slot ID lookup.
        if found.is_none()
            && let Ok(slot_num) = identifier.parse::<u32>()
            && let Some(slot) = lib.get_storage_slot(slot_num)
            && let Some(b) = slot.barcode.as_deref()
        {
            found = Some((b.to_string(), CartridgeLocation::Storage, slot.id));
        }
        found
    };

    let (barcode, location, slot_id) = match resolved {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("cartridge '{}' not found", identifier),
                })),
            )
                .into_response();
        }
    };

    // Read manifest off disk. Tapes root lives under data_dir; use
    // the daemon's canonical layout.
    let tapes_root: PathBuf = state.daemon.data_dir.join("tapes");
    let manifest_path = tapes_root.join(&barcode).join("manifest.json");

    let manifest_str = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("reading manifest for {}: {}", barcode, e),
                })),
            )
                .into_response();
        }
    };

    let manifest: serde_json::Value = match serde_json::from_str(&manifest_str) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("parsing manifest for {}: {}", barcode, e),
                })),
            )
                .into_response();
        }
    };

    let blocks = manifest["blocks"].as_array();
    let total_blocks = blocks.map_or(0, |b| b.len());
    let filemarks = blocks.map_or(0, |b| {
        b.iter()
            .filter(|x| x["kind"].as_str() == Some("Filemark"))
            .count()
    });
    let data_bytes: u64 = blocks.map_or(0, |b| b.iter().filter_map(|x| x["len"].as_u64()).sum());
    let chunk_count = manifest["chunks"].as_array().map_or(0, |c| c.len());
    let backend = manifest["backend"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("(missing)")
        .to_string();
    let worm = manifest["worm"].as_bool().unwrap_or(false);

    // Byte counters live in the `runtime.json` sidecar, not the
    // creation-frozen manifest. A missing / unparseable sidecar
    // degrades to zeros rather than failing the whole info call.
    let runtime_path = tapes_root.join(&barcode).join("runtime.json");
    let runtime: serde_json::Value = tokio::fs::read_to_string(&runtime_path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let counter = |key: &str| runtime.get(key).and_then(|v| v.as_u64()).unwrap_or(0);

    Json(CartridgeInfo {
        barcode,
        location: Some(location),
        slot_id: Some(slot_id),
        backend,
        worm,
        total_blocks,
        filemarks,
        data_blocks: total_blocks.saturating_sub(filemarks),
        data_bytes,
        chunk_count,
        host_bytes_written: counter("host_bytes_written"),
        host_bytes_read: counter("host_bytes_read"),
        backend_bytes_written: counter("backend_bytes_written"),
        backend_bytes_read: counter("backend_bytes_read"),
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// /api/v1/changer/inventory
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct InventoryItem {
    pub slot_id: u32,
    pub slot_type: CartridgeLocation,
    pub barcode: String,
}

#[derive(Serialize)]
pub struct ChangerInventory {
    pub entries: Vec<InventoryItem>,
}

// ---------------------------------------------------------------------------
// /api/v1/changer/move,load,unload  (mutating)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ChangerMoveRequest {
    pub from: u32,
    pub to: u32,
    /// Allow the move when source and destination belong to different
    /// logical partitions. Default refuses such moves so cross-
    /// partition relocations don't happen by accident; operators that
    /// genuinely need to rebalance pass this flag and the audit log
    /// records `cross_partition: true`. No-op on unpartitioned
    /// libraries.
    #[serde(default)]
    pub cross_partition: bool,
}

#[derive(Deserialize)]
pub struct ChangerLoadRequest {
    pub slot: u32,
    pub drive: u32,
    /// Allow loading from a slot in one partition into a drive in
    /// another. Same audit-tag treatment as `ChangerMoveRequest`.
    #[serde(default)]
    pub cross_partition: bool,
}

#[derive(Deserialize)]
pub struct ChangerUnloadRequest {
    pub drive: u32,
    /// Destination storage slot. `None` = let the daemon pick (home
    /// slot if free, else first empty slot).
    pub slot: Option<u32>,
    /// Bypass the host-asserted PREVENT MEDIUM REMOVAL gate. Mirrors
    /// the operator-console "force unload" affordance — only mean-
    /// ingful when the source drive's data-transport-removal bit is
    /// set by an active iSCSI session.
    #[serde(default)]
    pub force: bool,
    /// Allow unloading into a storage slot in a different partition.
    /// Same audit-tag treatment as `ChangerMoveRequest`. No-op on
    /// unpartitioned libraries.
    #[serde(default)]
    pub cross_partition: bool,
}

#[derive(Serialize)]
pub struct ChangerMutationOk {
    pub action: &'static str,
    pub barcode: Option<String>,
    pub from: u32,
    pub to: u32,
    /// `true` when the operator passed `cross_partition: true` and
    /// the move actually crossed partition boundaries. Surfaced so
    /// the CLI can render a "(crossed partitions)" hint and the
    /// audit log records the explicit acknowledgement.
    pub cross_partition: bool,
}

/// Append an audit entry. Failure to write the audit log never
/// affects the SCSI/admin response — same philosophy as the iSCSI
/// path's `audit_append`.
fn audit_append(
    state: &DaemonState,
    op: &str,
    actor: AuditActor,
    params: serde_json::Value,
    result: AuditResult,
) {
    if let Some(chan) = state.audit_log.as_ref() {
        chan.try_append(op, actor, params, result);
    }
}

/// Queue MEDIUM MAY HAVE CHANGED on the drive LUNs whose cartridge
/// just changed. Mirrors the iSCSI MOVE MEDIUM path. Empty slice =
/// no drive's cartridge changed (slot-to-slot move) so nothing is
/// queued. Broadcasting across every drive LUN would preempt the
/// host's next command on unrelated drives, and a host that ignores
/// the resulting CHECK CONDITION (e.g. `mt rewind 2>/dev/null`)
/// would never reset the daemon-side head position — surfaces as
/// issue #37 (stale filemark in the block index between writes).
/// Best-effort: UA queue mutex poisoned just gets logged.
fn raise_medium_may_have_changed(state: &DaemonState, drive_ids: &[u32]) {
    if drive_ids.is_empty() {
        return;
    }
    let ua = match state.ua_tracker.lock() {
        Ok(g) => g,
        Err(_) => {
            warn!("UA tracker mutex poisoned, skipping medium-may-have-changed broadcast");
            return;
        }
    };
    for drive_id in drive_ids {
        let drive_lun = (*drive_id as u8) + 1;
        ua.add_ua_all_sessions(drive_lun, UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED);
    }
}

pub async fn changer_move(
    State(state): State<AdminState>,
    cred: PeerCred,
    Json(req): Json<ChangerMoveRequest>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());

    let mut crossed_partitions = false;

    let result: anyhow::Result<Option<String>> = (|| {
        let mut lib = state
            .daemon
            .library
            .lock()
            .map_err(|_| anyhow::anyhow!("library mutex poisoned"))?;

        // Partition fence on chassis-assembly authority. Default
        // refuses cross-partition moves; operators that genuinely
        // need to rebalance pass `cross_partition: true` and the
        // audit log records the acknowledgement.
        if !lib.partitions().is_empty() {
            let from_part = lib.partition_for_storage_slot(req.from).map(str::to_string);
            let to_part = lib.partition_for_storage_slot(req.to).map(str::to_string);
            if from_part != to_part {
                if !req.cross_partition {
                    anyhow::bail!(
                        "slot {} (partition {:?}) and slot {} (partition {:?}) belong to different partitions; pass cross_partition=true to override",
                        req.from,
                        from_part,
                        req.to,
                        to_part,
                    );
                }
                crossed_partitions = true;
            }
        }

        let barcode = lib
            .get_storage_slot(req.from)
            .and_then(|s| s.barcode.clone());

        lib.move_cartridge(req.from, req.to)
            .map_err(anyhow::Error::from)?;
        Ok(barcode)
    })();

    match result {
        Ok(barcode) => {
            // Slot-to-slot move; no drive's cartridge changed.
            raise_medium_may_have_changed(&state.daemon, &[]);
            audit_append(
                &state.daemon,
                "changer.move",
                actor,
                serde_json::json!({
                    "from": req.from,
                    "to": req.to,
                    "barcode": barcode,
                    "cross_partition": crossed_partitions,
                }),
                AuditResult::Ok,
            );
            info!(
                "admin: changer move {} -> {} (barcode={:?}, cross_partition={})",
                req.from, req.to, barcode, crossed_partitions
            );
            Json(ChangerMutationOk {
                action: "move",
                barcode,
                from: req.from,
                to: req.to,
                cross_partition: crossed_partitions,
            })
            .into_response()
        }
        Err(e) => {
            audit_append(
                &state.daemon,
                "changer.move",
                actor,
                serde_json::json!({"from": req.from, "to": req.to}),
                AuditResult::Error(e.to_string()),
            );
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

pub async fn changer_load(
    State(state): State<AdminState>,
    cred: PeerCred,
    Json(req): Json<ChangerLoadRequest>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());
    let mut crossed_partitions = false;

    let result: anyhow::Result<Option<String>> = (|| {
        let mut lib = state
            .daemon
            .library
            .lock()
            .map_err(|_| anyhow::anyhow!("library mutex poisoned"))?;

        if !lib.partitions().is_empty() {
            let slot_part = lib.partition_for_storage_slot(req.slot).map(str::to_string);
            let drive_part = lib.partition_for_drive(req.drive).map(str::to_string);
            if slot_part != drive_part {
                if !req.cross_partition {
                    anyhow::bail!(
                        "slot {} (partition {:?}) and drive {} (partition {:?}) belong to different partitions; pass cross_partition=true to override",
                        req.slot,
                        slot_part,
                        req.drive,
                        drive_part,
                    );
                }
                crossed_partitions = true;
            }
        }

        lib.load_to_drive(req.slot, req.drive)
            .map_err(anyhow::Error::from)?;

        let barcode = lib.get_drive(req.drive).and_then(|d| d.barcode.clone());
        // Mirror to DriveManager while still under the library
        // lock so iSCSI handlers see a consistent view (Library
        // says drive is loaded, DriveManager has the cartridge
        // attached).
        if let Some(ref b) = barcode
            && let Err(e) = state
                .daemon
                .drive_manager
                .load_cartridge(req.drive as usize, b)
        {
            error!(
                "drive_manager.load_cartridge failed for drive {}: {}",
                req.drive, e
            );
        }
        Ok(barcode)
    })();

    match result {
        Ok(barcode) => {
            if let Some(ref b) = barcode {
                let _ = state.daemon.event_tx.send(TapeEvent::CartridgeLoaded {
                    tape_id: b.clone(),
                    drive_num: req.drive as u8,
                });
            }
            // Only the destination drive's cartridge changed.
            raise_medium_may_have_changed(&state.daemon, &[req.drive]);
            audit_append(
                &state.daemon,
                "changer.load",
                actor,
                serde_json::json!({
                    "slot": req.slot,
                    "drive": req.drive,
                    "barcode": barcode,
                    "cross_partition": crossed_partitions,
                }),
                AuditResult::Ok,
            );
            info!(
                "admin: changer load slot {} -> drive {} (barcode={:?}, cross_partition={})",
                req.slot, req.drive, barcode, crossed_partitions
            );
            Json(ChangerMutationOk {
                action: "load",
                barcode,
                from: req.slot,
                to: req.drive,
                cross_partition: crossed_partitions,
            })
            .into_response()
        }
        Err(e) => {
            audit_append(
                &state.daemon,
                "changer.load",
                actor,
                serde_json::json!({"slot": req.slot, "drive": req.drive}),
                AuditResult::Error(e.to_string()),
            );
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

pub async fn changer_unload(
    State(state): State<AdminState>,
    cred: PeerCred,
    Json(req): Json<ChangerUnloadRequest>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());

    // PREVENT/ALLOW MEDIUM REMOVAL gate. Bit 1 (mechanical) is the
    // front-panel-eject analog: an admin /changer/unload is the
    // operator-console equivalent of pressing eject, so it gates on
    // bit 1. (SCSI UNLOAD / MOVE MEDIUM still gate on bit 0 — the
    // host-side lock — in shared/ssc/src/dispatch/handlers.rs and
    // vtl/daemon/src/iscsi/protocol.rs.) `force: true` overrides
    // either bit; same stuck-tape ergonomic as today.
    if !req.force
        && state
            .daemon
            .drive_manager
            .is_mechanical_prevented(req.drive as usize)
    {
        let err = "PREVENT MEDIUM REMOVAL (mechanical eject) is asserted on this drive (pass force: true to override)";
        audit_append(
            &state.daemon,
            "changer.unload",
            actor,
            serde_json::json!({
                "drive": req.drive,
                "slot": req.slot,
                "force": false,
                "refused": "mechanical_eject_prevented",
            }),
            AuditResult::Error("mechanical eject prevented".to_string()),
        );
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": err})),
        )
            .into_response();
    }

    let mut crossed_partitions = false;

    let result: anyhow::Result<(Option<String>, u32)> = (|| {
        let mut lib = state
            .daemon
            .library
            .lock()
            .map_err(|_| anyhow::anyhow!("library mutex poisoned"))?;

        let drive_info = lib
            .get_drive(req.drive)
            .ok_or_else(|| anyhow::anyhow!("drive {} not found", req.drive))?
            .clone();
        let barcode = drive_info.barcode.clone();
        if !drive_info.occupied || barcode.is_none() {
            anyhow::bail!("drive {} is empty", req.drive);
        }

        // Pick destination slot: caller-supplied, else home slot
        // if it's free, else first empty storage slot.
        let dest_slot = if let Some(s) = req.slot {
            s
        } else if let Some(home) = drive_info.home_slot {
            let home_u32 = home as u32;
            let home_free = lib
                .get_storage_slot(home_u32)
                .map(|s| !s.occupied)
                .unwrap_or(false);
            if home_free {
                home_u32
            } else {
                lib.storage_slots()
                    .iter()
                    .find(|s| !s.occupied)
                    .map(|s| s.id)
                    .ok_or_else(|| anyhow::anyhow!("no empty storage slot available"))?
            }
        } else {
            lib.storage_slots()
                .iter()
                .find(|s| !s.occupied)
                .map(|s| s.id)
                .ok_or_else(|| anyhow::anyhow!("no empty storage slot available"))?
        };

        // Partition fence: drive's partition vs destination slot's
        // partition. The auto-pick branches above happily land on a
        // free slot in another partition, so this check has to fire
        // *after* the destination is chosen.
        if !lib.partitions().is_empty() {
            let drive_part = lib.partition_for_drive(req.drive).map(str::to_string);
            let slot_part = lib
                .partition_for_storage_slot(dest_slot)
                .map(str::to_string);
            if drive_part != slot_part {
                if !req.cross_partition {
                    anyhow::bail!(
                        "drive {} (partition {:?}) and destination slot {} (partition {:?}) belong to different partitions; pass cross_partition=true to override",
                        req.drive,
                        drive_part,
                        dest_slot,
                        slot_part,
                    );
                }
                crossed_partitions = true;
            }
        }

        lib.unload_from_drive(req.drive, dest_slot)
            .map_err(anyhow::Error::from)?;

        if let Err(e) = state
            .daemon
            .drive_manager
            .unload_cartridge(req.drive as usize)
        {
            warn!(
                "drive_manager.unload_cartridge for drive {}: {}",
                req.drive, e
            );
        }

        Ok((barcode, dest_slot))
    })();

    match result {
        Ok((barcode, dest_slot)) => {
            if let Some(ref b) = barcode {
                let _ = state.daemon.event_tx.send(TapeEvent::CartridgeUnloaded {
                    tape_id: b.clone(),
                    drive_num: req.drive as u8,
                });
            }
            // Only the source drive's cartridge changed.
            raise_medium_may_have_changed(&state.daemon, &[req.drive]);
            audit_append(
                &state.daemon,
                "changer.unload",
                actor,
                serde_json::json!({
                    "drive": req.drive,
                    "slot": dest_slot,
                    "barcode": barcode,
                    "force": req.force,
                    "cross_partition": crossed_partitions,
                }),
                AuditResult::Ok,
            );
            info!(
                "admin: changer unload drive {} -> slot {} (barcode={:?}, force={}, cross_partition={})",
                req.drive, dest_slot, barcode, req.force, crossed_partitions
            );
            Json(ChangerMutationOk {
                action: "unload",
                barcode,
                from: req.drive,
                to: dest_slot,
                cross_partition: crossed_partitions,
            })
            .into_response()
        }
        Err(e) => {
            audit_append(
                &state.daemon,
                "changer.unload",
                actor,
                serde_json::json!({"drive": req.drive, "slot": req.slot, "force": req.force}),
                AuditResult::Error(e.to_string()),
            );
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// /api/v1/cartridges  (create — POST)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CartridgeCreateRequest {
    pub barcode: String,
    /// LTO generation override. `None` falls back to the barcode
    /// suffix (`L7` / `L8`; `M8` is also accepted and maps to LTO-8 —
    /// the LTO-7-substrate physical-media distinction is meaningless
    /// for a VTL); if no recognized suffix, falls back to the
    /// library's configured generation.
    #[serde(default)]
    pub lto_generation: Option<u8>,
    /// Per-cartridge chunk size in bytes. Required. For fixed mode
    /// this is the exact chunk size; for fastcdc it's the target avg.
    pub chunk_size_bytes: u64,
    /// Chunking strategy (`"fixed"` or `"fastcdc"`). Required.
    pub chunking: String,
    /// FastCDC minimum chunk size in bytes. Optional override; when
    /// absent the daemon derives `(avg/8).max(64 KiB)`. Rejected on
    /// fixed mode. Must satisfy `min <= avg <= max`.
    #[serde(default)]
    pub chunking_min_bytes: Option<u64>,
    /// FastCDC maximum chunk size in bytes. Optional override; when
    /// absent the daemon derives `(avg*4).max(FASTCDC_DEFAULT_MAX)`.
    /// Rejected on fixed mode. Must satisfy `min <= avg <= max`.
    #[serde(default)]
    pub chunking_max_bytes: Option<u64>,
    /// Number of cartridges to create starting at `barcode`. The
    /// trailing decimal digit run gets incremented `multi - 1` times,
    /// preserving zero-pad width.
    #[serde(default = "default_multi")]
    pub multi: u32,
    /// Cloud backend name. Required when 2+ backends configured;
    /// inferred when only one backend exists.
    #[serde(default)]
    pub backend: Option<String>,
    /// Make the cartridge WORM. Backend must have retention_mode set.
    #[serde(default)]
    pub worm: bool,
    /// Dedup scope (`"local"` or `"global"`). Default `"global"`.
    #[serde(default = "default_dedup")]
    pub dedup: String,
    /// Operator passed `--encrypt`. Opt-in at-rest encryption; when
    /// `true` the daemon mints a per-cartridge DEK wrapped by the
    /// `keystore` backend. Requires `keystore` to be set.
    #[serde(default)]
    pub encrypt: bool,
    /// Keystore-backend name for at-rest encryption. Required when
    /// `encrypt: true`; ignored otherwise. Names an entry under
    /// `keystore.backends:` in the daemon YAML.
    #[serde(default)]
    pub keystore: Option<String>,
}

fn default_multi() -> u32 {
    1
}
fn default_dedup() -> String {
    "global".to_string()
}

#[derive(Serialize)]
pub struct CartridgeCreateOk {
    pub created: Vec<CreatedCartridge>,
}

#[derive(Serialize)]
pub struct CreatedCartridge {
    pub barcode: String,
    pub slot: u32,
    pub backend: String,
    pub lto_generation: u8,
    pub worm: bool,
    pub chunking: String,
    pub chunk_size_bytes: u64,
    /// At-rest encryption keystore-backend name. `None` for
    /// cartridges created plaintext (no `--encrypt`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keystore: Option<String>,
}

pub async fn cartridge_create(
    State(state): State<AdminState>,
    cred: PeerCred,
    Json(req): Json<CartridgeCreateRequest>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());

    // ---- input validation ----
    let dedup: DedupScope = match req.dedup.parse() {
        Ok(d) => d,
        Err(e) => return bad_request(format!("dedup: {}", e)),
    };

    let (chunking, chunking_str) = match parse_chunking_mode(&req) {
        Ok(x) => x,
        Err(e) => return bad_request(e),
    };

    // ---- backend resolution ----
    let resolved_backend = match resolve_backend(&state, &req.backend) {
        Ok(name) => name,
        Err(e) => return bad_request(e),
    };

    // ---- WORM gate ----
    if req.worm {
        let mode = state
            .daemon
            .storage_config
            .retention_mode_named(&resolved_backend);
        if !mode.requires_lock() {
            return bad_request(format!(
                "worm requires backend '{}' to have retention_mode set to governance or compliance (currently: {})",
                resolved_backend,
                mode.label()
            ));
        }
    }

    // ---- multi-barcode expansion ----
    let barcodes = match expand_barcodes(&req.barcode, req.multi) {
        Ok(bs) => bs,
        Err(e) => return bad_request(e),
    };

    let tapes_root = state.daemon.data_dir.join("tapes");

    // ---- at-rest keystore resolution ----
    //
    // Encryption is opt-in: `encrypt` is the sole trigger and it
    // requires a keystore. Mirror the CLI's clap `requires` here so
    // the raw admin API can't slip past it.
    if req.encrypt && req.keystore.is_none() {
        return bad_request("encrypt requires a keystore — set `keystore`");
    }
    if !req.encrypt && req.keystore.is_some() {
        return bad_request("keystore requires `encrypt: true`");
    }
    //
    // Run this BEFORE locking the library because
    // `KeyStoreBackend::generate_and_wrap` is async (KMS / Vault /
    // KMIP all make network calls) and we must not hold the library
    // mutex across an `.await`. We pre-mint one (uuid, plain_dek,
    // wrapped_dek) tuple per barcode upfront; if the create loop
    // fails downstream the tuples are just discarded (nothing was
    // persisted server-side beyond cloud RPCs that the wrapped blob
    // makes opaque).
    let resolved_keystore: Option<&str> = if req.encrypt {
        match state
            .daemon
            .keystore_config
            .resolve_name(req.keystore.as_deref())
        {
            Ok(name) => Some(name),
            Err(e) => return bad_request(format!("keystore selection: {e}")),
        }
    } else {
        None
    };
    let at_rest_params: Vec<Option<core_mediachanger::AtRestCreateParams>> = match resolved_keystore
    {
        Some(name) => {
            let backend = match state
                .daemon
                .keystore_config
                .create_backend_named(name, &state.daemon.data_dir)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    return bad_request(format!("keystore backend '{name}' not usable: {e}"));
                }
            };
            let mut out = Vec::with_capacity(barcodes.len());
            for _ in &barcodes {
                let uuid = generate_cartridge_uuid();
                let (plain_dek, wrapped_dek) = match backend
                    .generate_and_wrap(&uuid, shared_keystore::DekSource::Daemon)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        return bad_request(format!(
                            "keystore '{name}' generate_and_wrap failed: {e}"
                        ));
                    }
                };
                let wrapped_dek_b64 = if backend.manages_local_blob() {
                    None
                } else {
                    use base64::Engine as _;
                    Some(base64::engine::general_purpose::STANDARD.encode(&wrapped_dek))
                };
                out.push(Some(core_mediachanger::AtRestCreateParams {
                    uuid,
                    meta: core_mediachanger::CartridgeEncryptionMeta {
                        algorithm: core_mediachanger::CartridgeEncryptionAlgorithm::Aes256Gcm,
                        keystore_backend: name.to_string(),
                        wrapped_dek: wrapped_dek_b64,
                    },
                    plain_dek: *plain_dek.as_bytes(),
                }));
            }
            out
        }
        None => barcodes.iter().map(|_| None).collect(),
    };

    // Lock library for the duration of the create batch — keeps the
    // free-slot count stable and prevents racing iSCSI MOVE MEDIUM
    // ops from grabbing slots out from under us.
    let mut lib = match state.daemon.library.lock() {
        Ok(g) => g,
        Err(_) => return server_error("library mutex poisoned"),
    };

    let free_slots = lib.storage_slots().iter().filter(|s| !s.occupied).count();
    if free_slots < barcodes.len() {
        return bad_request(format!(
            "not enough free slots: requested {}, free {}",
            barcodes.len(),
            free_slots
        ));
    }

    let lto_gen = match resolve_lto_generation(&req, &lib) {
        Ok(x) => x,
        Err(e) => return bad_request(e),
    };

    // ---- create loop with rollback on failure ----
    let mut committed: Vec<(String, u32)> = Vec::with_capacity(barcodes.len());
    let mut created_resp: Vec<CreatedCartridge> = Vec::with_capacity(barcodes.len());

    let create_result: anyhow::Result<()> = (|| {
        for (bc, at_rest) in barcodes.iter().zip(at_rest_params.into_iter()) {
            let half_built = tapes_root.join(bc);
            // Stash the plaintext DEK in the daemon's cartridge-key
            // cache BEFORE create — that way any drive-load that
            // races with create finds the key. Removed on rollback.
            if let Some(p) = at_rest.as_ref() {
                state
                    .daemon
                    .drive_manager
                    .set_cartridge_dek(bc, p.plain_dek);
            }
            let _ = Cartridge::create_with_chunking_and_at_rest(
                &tapes_root,
                bc,
                chunking,
                lto_gen,
                &resolved_backend,
                req.worm,
                dedup,
                at_rest.clone(),
            )
            .map_err(|e| {
                if half_built.exists() {
                    let _ = std::fs::remove_dir_all(&half_built);
                }
                state.daemon.drive_manager.forget_cartridge_dek(bc);
                anyhow::anyhow!("create '{}': {}", bc, e)
            })?;

            let slot = match lib.add_or_create_tape(bc, &resolved_backend) {
                Ok(s) => s,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(tapes_root.join(bc));
                    state.daemon.drive_manager.forget_cartridge_dek(bc);
                    return Err(anyhow::anyhow!("library add '{}': {}", bc, e));
                }
            };
            committed.push((bc.clone(), slot));
            let keystore_name = at_rest.as_ref().map(|p| p.meta.keystore_backend.clone());
            created_resp.push(CreatedCartridge {
                barcode: bc.clone(),
                slot,
                backend: resolved_backend.clone(),
                lto_generation: lto_gen,
                worm: req.worm,
                chunking: chunking_str.clone(),
                chunk_size_bytes: req.chunk_size_bytes,
                keystore: keystore_name.clone(),
            });
            let mut payload = serde_json::json!({
                "barcode": bc,
                "lto_generation": lto_gen,
                "backend": resolved_backend,
                "worm": req.worm,
                "slot": slot,
            });
            if let Some(name) = keystore_name {
                payload["encryption"] = serde_json::json!({
                    "algorithm": "aes_256_gcm",
                    "keystore_backend": name,
                });
            }
            audit_append(
                &state.daemon,
                "cartridge.create",
                actor.clone(),
                payload,
                AuditResult::Ok,
            );
        }
        Ok(())
    })();

    if let Err(e) = create_result {
        // Rollback: free committed slots + remove dirs in reverse.
        for (bc, slot_id) in committed.iter().rev() {
            let _ = lib.remove_from_slot(*slot_id);
            let dir = tapes_root.join(bc);
            if dir.exists() {
                let _ = std::fs::remove_dir_all(&dir);
            }
            audit_append(
                &state.daemon,
                "cartridge.create.rollback",
                actor.clone(),
                serde_json::json!({"barcode": bc, "slot": slot_id}),
                AuditResult::Ok,
            );
        }
        audit_append(
            &state.daemon,
            "cartridge.create",
            actor,
            serde_json::json!({"barcode": req.barcode, "multi": req.multi}),
            AuditResult::Error(e.to_string()),
        );
        return bad_request(e.to_string());
    }

    info!(
        "admin: cartridge create batch ({} cartridges, backend={}, worm={})",
        created_resp.len(),
        resolved_backend,
        req.worm
    );
    Json(CartridgeCreateOk {
        created: created_resp,
    })
    .into_response()
}

/// Parse `req.chunking` + `req.chunk_size_bytes` into a `ChunkingMode`.
/// Returns the parsed mode plus the lowercase strategy label that
/// shows up in the response and audit payloads. The Err string is
/// the operator-facing message the caller wraps in `bad_request`.
///
/// FastCDC: operator-supplied `chunking_min_bytes` / `chunking_max_bytes`
/// override the avg/8 and avg*4 derivation. Fixed: both overrides are
/// rejected — they're meaningless when every chunk is the same size.
fn parse_chunking_mode(req: &CartridgeCreateRequest) -> Result<(ChunkingMode, String), String> {
    let chunking_str = req.chunking.to_lowercase();
    let chunking = match chunking_str.as_str() {
        "fixed" => {
            if req.chunking_min_bytes.is_some() || req.chunking_max_bytes.is_some() {
                return Err(
                    "chunking_min_bytes / chunking_max_bytes are only valid with chunking=fastcdc"
                        .to_string(),
                );
            }
            ChunkingMode::Fixed {
                size_bytes: req.chunk_size_bytes,
            }
        }
        "fastcdc" => {
            let avg = if req.chunk_size_bytes > 0 {
                req.chunk_size_bytes
            } else {
                FASTCDC_DEFAULT_AVG as u64
            };
            let min = req
                .chunking_min_bytes
                .unwrap_or_else(|| (avg / 8).max(64 * 1024).min(avg));
            let max = req
                .chunking_max_bytes
                .unwrap_or_else(|| (avg * 4).max(FASTCDC_DEFAULT_MAX as u64));
            if min == 0 || max == 0 {
                return Err("fastcdc min/max must be > 0".to_string());
            }
            if min > avg {
                return Err(format!("fastcdc min ({}) exceeds avg ({})", min, avg));
            }
            if avg > max {
                return Err(format!("fastcdc avg ({}) exceeds max ({})", avg, max));
            }
            ChunkingMode::FastCdc { min, avg, max }
        }
        other => return Err(format!("unknown chunking strategy '{}'", other)),
    };
    Ok((chunking, chunking_str))
}

/// Resolve the cloud backend name: explicit request field wins;
/// otherwise auto-pick when exactly one backend is configured; refuse
/// when 2+ backends are configured without an explicit choice.
fn resolve_backend(state: &AdminState, req_backend: &Option<String>) -> Result<String, String> {
    let backend_names = state.daemon.storage_config.backend_names();
    match (req_backend, backend_names.len()) {
        (Some(name), _) => {
            if !backend_names.iter().any(|n| n == name) {
                return Err(format!(
                    "backend '{}' not configured. Available: {}",
                    name,
                    backend_names.join(", ")
                ));
            }
            Ok(name.clone())
        }
        (None, 1) => Ok(backend_names.into_iter().next().unwrap_or_default()),
        (None, _) => Err(format!(
            "backend is required when multiple cloud backends are configured. Available: {}",
            backend_names.join(", ")
        )),
    }
}

/// Resolve `lto_generation` from request field + library default.
/// Explicit request field wins; otherwise we use the library's
/// generation. Barcode inference is intentionally not consulted —
/// the barcode is just a label, the cartridge carries its own LTO
/// generation in the manifest.
fn resolve_lto_generation(
    req: &CartridgeCreateRequest,
    lib: &core_mediachanger::Library,
) -> Result<u8, String> {
    let lto_gen = req.lto_generation.unwrap_or_else(|| lib.lto_generation());
    if lto_gen != 8 {
        return Err(format!("invalid LTO generation {}: must be 8", lto_gen));
    }
    Ok(lto_gen)
}

fn bad_request(msg: impl Into<String>) -> axum::response::Response {
    let m = msg.into();
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": m})),
    )
        .into_response()
}

fn server_error(msg: impl Into<String>) -> axum::response::Response {
    let m = msg.into();
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": m})),
    )
        .into_response()
}

fn cartridge_present(lib: &core_mediachanger::Library, barcode: &str) -> bool {
    let in_storage = lib
        .storage_slots()
        .iter()
        .any(|s| s.barcode.as_deref() == Some(barcode));
    let in_mail = lib
        .mail_slots()
        .iter()
        .any(|s| s.barcode.as_deref() == Some(barcode));
    let in_drive = lib
        .drives()
        .iter()
        .any(|d| d.barcode.as_deref() == Some(barcode));
    in_storage || in_mail || in_drive
}

/// Expand `(barcode, multi)` into the list of barcodes to create.
/// Mirrors the CLI's helper exactly: trailing decimal-digit run,
/// preserve zero-pad width, error on overflow.
fn expand_barcodes(barcode: &str, multi: u32) -> Result<Vec<String>, String> {
    if multi == 0 {
        return Err("multi must be >= 1".to_string());
    }
    if multi == 1 {
        return Ok(vec![barcode.to_string()]);
    }
    let suffix_start = barcode
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    let prefix = &barcode[..suffix_start];
    let digits = &barcode[suffix_start..];
    if digits.is_empty() {
        return Err(format!(
            "barcode '{}' must end in a numeric suffix when multi > 1",
            barcode
        ));
    }
    let width = digits.len();
    let start: u64 = digits.parse().map_err(|e| format!("parse suffix: {}", e))?;
    let max_for_width = 10u64.pow(width as u32);
    let end = start + (multi as u64);
    if end > max_for_width {
        return Err(format!(
            "barcode suffix would overflow {}-digit field at multi={}",
            width, multi
        ));
    }
    Ok((start..end)
        .map(|n| format!("{}{:0>width$}", prefix, n, width = width))
        .collect())
}

// ---------------------------------------------------------------------------
// /api/v1/cartridges/import  (POST)
// /api/v1/cartridges/{barcode}/export  (POST)
//
// Both take a server-side filesystem path. The daemon owns the
// data directory and the operator already has shell access to the
// daemon host (deployment model assumes this), so streaming bytes
// over the admin socket would be needless complexity.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CartridgeImportRequest {
    /// Path on the daemon host's filesystem to a cartridge directory
    /// (must contain `manifest.json`). The daemon copies it into
    /// `<data_dir>/tapes/<barcode>/`.
    pub path: String,
    /// Preferred destination slot. Daemon honors it when free,
    /// otherwise falls back to the first empty slot.
    #[serde(default)]
    pub slot: Option<u32>,
}

#[derive(Deserialize)]
pub struct CartridgeExportRequest {
    /// Destination path on the daemon host's filesystem. Must not
    /// already exist.
    pub path: String,
}

#[derive(Serialize)]
pub struct CartridgeImportOk {
    pub barcode: String,
    pub backend: String,
    pub slot: u32,
    /// `true` if `slot` differs from the operator's requested slot
    /// (free-slot fallback fired).
    pub fallback_slot: bool,
}

#[derive(Serialize)]
pub struct CartridgeExportOk {
    pub barcode: String,
    pub slot: u32,
    pub dest_path: String,
}

pub async fn cartridge_import(
    State(state): State<AdminState>,
    cred: PeerCred,
    Json(req): Json<CartridgeImportRequest>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());
    let source_path = std::path::PathBuf::from(&req.path);

    if !source_path.exists() {
        return bad_request(format!("source path does not exist: {}", req.path));
    }

    let manifest_path = source_path.join("manifest.json");
    if !manifest_path.exists() {
        return bad_request(format!(
            "not a valid cartridge directory (no manifest.json): {}",
            req.path
        ));
    }

    let manifest_str = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => return bad_request(format!("read manifest: {}", e)),
    };
    let manifest: serde_json::Value = match serde_json::from_str(&manifest_str) {
        Ok(v) => v,
        Err(e) => return bad_request(format!("parse manifest: {}", e)),
    };
    let barcode = match manifest["label"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return bad_request("manifest missing 'label' field"),
    };
    let imported_backend = match manifest["backend"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return bad_request(format!(
                "imported cartridge '{}' manifest has no `backend` field — refuse to import",
                barcode
            ));
        }
    };

    let tapes_root = state.daemon.data_dir.join("tapes");
    let dest_path = tapes_root.join(&barcode);

    if dest_path.exists() {
        return bad_request(format!("cartridge '{}' already in tapes dir", barcode));
    }

    if let Err(e) = copy_dir_all(&source_path, &dest_path) {
        let _ = std::fs::remove_dir_all(&dest_path);
        let msg = format!("copy {} -> {}: {}", req.path, dest_path.display(), e);
        audit_append(
            &state.daemon,
            "cartridge.import",
            actor,
            serde_json::json!({
                "barcode": barcode,
                "source_path": req.path,
                "backend": imported_backend,
            }),
            AuditResult::Error(msg.clone()),
        );
        return bad_request(msg);
    }

    let mut lib = match state.daemon.library.lock() {
        Ok(g) => g,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dest_path);
            audit_append(
                &state.daemon,
                "cartridge.import",
                actor,
                serde_json::json!({
                    "barcode": barcode,
                    "source_path": req.path,
                    "backend": imported_backend,
                }),
                AuditResult::Error("library mutex poisoned".to_string()),
            );
            return server_error("library mutex poisoned");
        }
    };

    let actual_slot = match lib.add_or_create_tape(&barcode, &imported_backend) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest_path);
            let msg = format!("library add '{}': {}", barcode, e);
            audit_append(
                &state.daemon,
                "cartridge.import",
                actor,
                serde_json::json!({
                    "barcode": barcode,
                    "source_path": req.path,
                    "backend": imported_backend,
                }),
                AuditResult::Error(msg.clone()),
            );
            return bad_request(msg);
        }
    };
    let fallback_slot = req.slot.is_some() && req.slot != Some(actual_slot);

    audit_append(
        &state.daemon,
        "cartridge.import",
        actor,
        serde_json::json!({
            "barcode": barcode,
            "source_path": req.path,
            "backend": imported_backend,
            "slot": actual_slot,
        }),
        AuditResult::Ok,
    );
    info!(
        "admin: cartridge import '{}' from {} -> slot {}",
        barcode, req.path, actual_slot
    );

    Json(CartridgeImportOk {
        barcode,
        backend: imported_backend,
        slot: actual_slot,
        fallback_slot,
    })
    .into_response()
}

pub async fn cartridge_export(
    State(state): State<AdminState>,
    cred: PeerCred,
    AxumPath(slot): AxumPath<u32>,
    Json(req): Json<CartridgeExportRequest>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());
    let dest_path = std::path::PathBuf::from(&req.path);

    if dest_path.exists() {
        return bad_request(format!("destination path already exists: {}", req.path));
    }

    let lib = match state.daemon.library.lock() {
        Ok(g) => g,
        Err(_) => return server_error("library mutex poisoned"),
    };
    let slot_info = match lib.get_storage_slot(slot) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("slot {} not found", slot)})),
            )
                .into_response();
        }
    };
    let Some(barcode) = slot_info.barcode.clone() else {
        return bad_request(format!("slot {} is empty", slot));
    };
    if !slot_info.occupied {
        return bad_request(format!("slot {} is empty", slot));
    }
    drop(lib);

    let source_path = state.daemon.data_dir.join("tapes").join(&barcode);
    if !source_path.exists() {
        return server_error(format!(
            "cartridge dir missing on disk: {}",
            source_path.display()
        ));
    }

    if let Err(e) = copy_dir_all(&source_path, &dest_path) {
        let _ = std::fs::remove_dir_all(&dest_path);
        let msg = format!(
            "export {} -> {}: {}",
            source_path.display(),
            dest_path.display(),
            e
        );
        audit_append(
            &state.daemon,
            "cartridge.export",
            actor,
            serde_json::json!({
                "barcode": barcode,
                "slot": slot,
                "dest_path": req.path,
            }),
            AuditResult::Error(msg.clone()),
        );
        return server_error(msg);
    }

    audit_append(
        &state.daemon,
        "cartridge.export",
        actor,
        serde_json::json!({
            "barcode": barcode,
            "slot": slot,
            "dest_path": req.path,
        }),
        AuditResult::Ok,
    );
    info!(
        "admin: cartridge export slot {} '{}' -> {}",
        slot, barcode, req.path
    );

    Json(CartridgeExportOk {
        barcode,
        slot,
        dest_path: req.path,
    })
    .into_response()
}

/// Recursive directory copy. Mirror of the helper that used to live
/// in thurvtl — moved into the daemon so import/export are
/// fully owner-side operations.
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// /api/v1/cartridges/{barcode}/legal-hold  (set / clear / status)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct LegalHoldMutate {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct LegalHoldStatusQuery {
    #[serde(default)]
    pub full: bool,
}

#[derive(Serialize)]
pub struct LegalHoldMutateResponse {
    pub barcode: String,
    pub backend: String,
    pub key_count: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub sentinel_present: bool,
    pub failures: Vec<LegalHoldFailure>,
}

#[derive(Serialize)]
pub struct LegalHoldFailure {
    pub key: String,
    pub error: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum LegalHoldStatusResponse {
    Sentinel {
        barcode: String,
        backend: String,
        held: bool,
    },
    Full {
        barcode: String,
        backend: String,
        held: usize,
        not_held: usize,
        errors: usize,
        details: Vec<LegalHoldKeyState>,
    },
    Empty {
        barcode: String,
        backend: String,
    },
}

#[derive(Serialize)]
pub struct LegalHoldKeyState {
    pub key: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Resolve a cartridge's bound backend, validate that it supports
/// legal hold, and enumerate the keys covered. Returns owned values
/// (`Arc<dyn ObjectStoreBackend>`, `CartridgeKeys`) so the handler can
/// call into `apply_cartridge_legal_hold` etc. without holding `&dyn`
/// borrows across awaits.
async fn open_backend_for_cartridge(
    state: Arc<DaemonState>,
    barcode: String,
) -> anyhow::Result<(
    Arc<dyn ObjectStoreBackend>,
    core_mediachanger::CartridgeKeys,
)> {
    let tapes_root = state.data_dir.join("tapes");
    let manifest_path = tapes_root.join(&barcode).join("manifest.json");
    if !manifest_path.exists() {
        anyhow::bail!(
            "cartridge '{}' not found at {}",
            barcode,
            manifest_path.display()
        );
    }
    let manifest_str = tokio::fs::read_to_string(&manifest_path)
        .await
        .map_err(|e| anyhow::anyhow!("read {}: {}", manifest_path.display(), e))?;
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_str)
        .map_err(|e| anyhow::anyhow!("manifest parse: {}", e))?;
    let bound_backend = manifest_json["backend"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("cartridge '{}' manifest has no `backend` field", barcode))?
        .to_string();

    let backend_box = state
        .storage_config
        .create_backend_named(&bound_backend)
        .await
        .map_err(|e| anyhow::anyhow!("construct backend '{}': {}", bound_backend, e))?;

    if !backend_box.supports_legal_hold() {
        anyhow::bail!(
            "backend '{}' (type: {}) does not support legal hold",
            bound_backend,
            backend_box.backend_type()
        );
    }

    let backend: Arc<dyn ObjectStoreBackend> = Arc::from(backend_box);
    let keys = collect_cartridge_keys(&tapes_root, barcode.clone(), Arc::clone(&backend))
        .await
        .map_err(|e| anyhow::anyhow!("enumerate keys for '{}': {}", barcode, e))?;

    Ok((backend, keys))
}

fn refuse_if_loaded_legal(state: &DaemonState, barcode: &str) -> anyhow::Result<()> {
    if let Some(drive_id) = find_drive_for_loaded_cartridge(&state.data_dir, barcode)
        .map_err(|e| anyhow::anyhow!("inventory check: {}", e))?
    {
        anyhow::bail!(
            "cartridge '{}' is loaded on drive {} — unload it before changing legal-hold state",
            barcode,
            drive_id
        );
    }
    Ok(())
}

pub async fn legal_hold_set(
    State(state): State<AdminState>,
    cred: PeerCred,
    AxumPath(barcode): AxumPath<String>,
    Json(req): Json<LegalHoldMutate>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());
    legal_hold_apply(state, actor, barcode, req, true).await
}

pub async fn legal_hold_clear(
    State(state): State<AdminState>,
    cred: PeerCred,
    AxumPath(barcode): AxumPath<String>,
    Json(req): Json<LegalHoldMutate>,
) -> impl IntoResponse {
    let actor = AuditActor::cli(cred.audit_descriptor());
    legal_hold_apply(state, actor, barcode, req, false).await
}

async fn legal_hold_apply(
    state: AdminState,
    actor: AuditActor,
    barcode: String,
    req: LegalHoldMutate,
    set_hold: bool,
) -> axum::response::Response {
    let op = if set_hold {
        "cartridge.legal_hold.set"
    } else {
        "cartridge.legal_hold.clear"
    };

    if let Err(e) = refuse_if_loaded_legal(&state.daemon, &barcode) {
        audit_append(
            &state.daemon,
            op,
            actor,
            serde_json::json!({"barcode": barcode, "id": req.id, "reason": req.reason}),
            AuditResult::Error(e.to_string()),
        );
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }

    let (backend, keys) =
        match open_backend_for_cartridge(Arc::clone(&state.daemon), barcode.clone()).await {
            Ok(t) => t,
            Err(e) => {
                audit_append(
                    &state.daemon,
                    op,
                    actor,
                    serde_json::json!({"barcode": barcode, "id": req.id, "reason": req.reason}),
                    AuditResult::Error(e.to_string()),
                );
                let status = if e.to_string().contains("not found") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                };
                return (status, Json(serde_json::json!({"error": e.to_string()}))).into_response();
            }
        };

    let backend_type = backend.backend_type().to_string();
    let total_keys = keys.others.len() + usize::from(keys.sentinel.is_some());
    let sentinel_present = keys.sentinel.is_some();
    if total_keys == 0 {
        audit_append(
            &state.daemon,
            op,
            actor,
            serde_json::json!({
                "barcode": barcode,
                "id": req.id,
                "reason": req.reason,
                "key_count": 0,
            }),
            AuditResult::Ok,
        );
        return Json(LegalHoldMutateResponse {
            barcode,
            backend: backend_type,
            key_count: 0,
            succeeded: 0,
            failed: 0,
            sentinel_present,
            failures: Vec::new(),
        })
        .into_response();
    }

    let report = apply_cartridge_legal_hold(backend, &keys, set_hold, 8).await;
    let failures: Vec<LegalHoldFailure> = report
        .failures
        .iter()
        .map(|f| LegalHoldFailure {
            key: f.key.clone(),
            error: f
                .result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default(),
        })
        .collect();

    let audit_params = serde_json::json!({
        "barcode": barcode,
        "id": req.id,
        "reason": req.reason,
        "key_count": report.total,
        "succeeded": report.successes,
        "failed": report.failures.len(),
        "sentinel_present": sentinel_present,
    });
    if report.failures.is_empty() {
        audit_append(&state.daemon, op, actor, audit_params, AuditResult::Ok);
    } else {
        audit_append(
            &state.daemon,
            op,
            actor,
            audit_params,
            AuditResult::Error(format!(
                "{} of {} keys failed",
                report.failures.len(),
                report.total
            )),
        );
    }

    Json(LegalHoldMutateResponse {
        barcode,
        backend: backend_type,
        key_count: report.total,
        succeeded: report.successes,
        failed: report.failures.len(),
        sentinel_present,
        failures,
    })
    .into_response()
}

pub async fn legal_hold_status(
    State(state): State<AdminState>,
    AxumPath(barcode): AxumPath<String>,
    Query(q): Query<LegalHoldStatusQuery>,
) -> impl IntoResponse {
    let (backend, keys) =
        match open_backend_for_cartridge(Arc::clone(&state.daemon), barcode.clone()).await {
            Ok(t) => t,
            Err(e) => {
                let status = if e.to_string().contains("not found") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                };
                return (status, Json(serde_json::json!({"error": e.to_string()}))).into_response();
            }
        };

    let backend_type = backend.backend_type().to_string();
    let total_keys = keys.others.len() + usize::from(keys.sentinel.is_some());
    if total_keys == 0 {
        return Json(LegalHoldStatusResponse::Empty {
            barcode,
            backend: backend_type,
        })
        .into_response();
    }

    if !q.full {
        if keys.sentinel.is_none() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "no manifest-latest.json sentinel found — use ?full=true to sweep every key"
                })),
            )
                .into_response();
        }
        match read_cartridge_held(backend, barcode.clone()).await {
            Ok(held) => Json(LegalHoldStatusResponse::Sentinel {
                barcode,
                backend: backend_type,
                held,
            })
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("sentinel read failed: {}", e)})),
            )
                .into_response(),
        }
    } else {
        let mut all_keys: Vec<String> = keys.others;
        if let Some(s) = keys.sentinel {
            all_keys.push(s);
        }
        let outcomes = read_legal_hold_for_keys(backend, all_keys, 8).await;
        let mut held = 0usize;
        let mut not_held = 0usize;
        let mut errors = 0usize;
        let mut details = Vec::with_capacity(outcomes.len());
        for (key, result) in outcomes {
            match result {
                Ok(true) => {
                    held += 1;
                    details.push(LegalHoldKeyState {
                        key,
                        state: "held".to_string(),
                        error: None,
                    });
                }
                Ok(false) => {
                    not_held += 1;
                    details.push(LegalHoldKeyState {
                        key,
                        state: "not_held".to_string(),
                        error: None,
                    });
                }
                Err(e) => {
                    errors += 1;
                    details.push(LegalHoldKeyState {
                        key,
                        state: "error".to_string(),
                        error: Some(e.to_string()),
                    });
                }
            }
        }
        Json(LegalHoldStatusResponse::Full {
            barcode,
            backend: backend_type,
            held,
            not_held,
            errors,
            details,
        })
        .into_response()
    }
}

pub async fn changer_inventory(
    State(state): State<AdminState>,
    Query(q): Query<CartridgesQuery>,
) -> impl IntoResponse {
    let lib = state
        .daemon
        .library
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let pattern = q.filter.as_deref();
    let mut entries = Vec::new();

    let push =
        |entries: &mut Vec<InventoryItem>, id: u32, barcode: &str, loc: CartridgeLocation| {
            if let Some(p) = pattern
                && !barcode.contains(p)
            {
                return;
            }
            entries.push(InventoryItem {
                slot_id: id,
                slot_type: loc,
                barcode: barcode.to_string(),
            });
        };

    for slot in lib.storage_slots() {
        if let Some(b) = slot.barcode.as_deref() {
            push(&mut entries, slot.id, b, CartridgeLocation::Storage);
        }
    }
    for slot in lib.mail_slots() {
        if let Some(b) = slot.barcode.as_deref() {
            push(&mut entries, slot.id, b, CartridgeLocation::Mail);
        }
    }
    for drive in lib.drives() {
        if let Some(b) = drive.barcode.as_deref() {
            push(&mut entries, drive.id, b, CartridgeLocation::Drive);
        }
    }

    Json(ChangerInventory { entries })
}

// ---------------------------------------------------------------------------
// /api/v1/drives  +  /api/v1/drives/{id}
// ---------------------------------------------------------------------------

/// Operator-friendly drive shape. The TCP `/drives` endpoint returns
/// an iSCSI-flavored view (lock state, owning session); this is the
/// "what's loaded?" view the CLI's `drive status` command uses.
#[derive(Serialize)]
pub struct DriveStatus {
    pub id: u32,
    pub loaded: bool,
    pub barcode: Option<String>,
    pub home_slot: Option<u16>,
    pub next_lba: Option<u64>,
    pub total_blocks: Option<usize>,
}

#[derive(Serialize)]
pub struct DrivesList {
    pub drives: Vec<DriveStatus>,
}

pub async fn drives_list(State(state): State<AdminState>) -> impl IntoResponse {
    let mut out = Vec::new();
    // Snapshot drive metadata (lock-then-collect, so we don't hold
    // the library lock across the per-drive manifest reads below).
    let snapshots: Vec<(u32, bool, Option<String>, Option<u16>)> = {
        let lib = state
            .daemon
            .library
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        lib.drives()
            .iter()
            .map(|d| (d.id, d.occupied, d.barcode.clone(), d.home_slot))
            .collect()
    };
    for (id, loaded, barcode, home_slot) in snapshots {
        let (next_lba, total_blocks) = if let Some(ref b) = barcode {
            read_position(&state.daemon.data_dir, b).await
        } else {
            (None, None)
        };
        out.push(DriveStatus {
            id,
            loaded,
            barcode,
            home_slot,
            next_lba,
            total_blocks,
        });
    }
    Json(DrivesList { drives: out })
}

pub async fn drive_status(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<u32>,
) -> impl IntoResponse {
    let snapshot = {
        let lib = state
            .daemon
            .library
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        lib.get_drive(id)
            .map(|d| (d.id, d.occupied, d.barcode.clone(), d.home_slot))
    };

    let (drive_id, loaded, barcode, home_slot) = match snapshot {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("drive {} not found", id),
                })),
            )
                .into_response();
        }
    };

    let (next_lba, total_blocks) = if let Some(ref b) = barcode {
        read_position(&state.daemon.data_dir, b).await
    } else {
        (None, None)
    };

    Json(DriveStatus {
        id: drive_id,
        loaded,
        barcode,
        home_slot,
        next_lba,
        total_blocks,
    })
    .into_response()
}

/// Best-effort read of `next_lba` and total block count from a
/// loaded cartridge's manifest. Errors collapse to `(None, None)` —
/// drive status is informational and shouldn't fail just because a
/// manifest momentarily isn't readable.
async fn read_position(data_dir: &std::path::Path, barcode: &str) -> (Option<u64>, Option<usize>) {
    let manifest_path = data_dir.join("tapes").join(barcode).join("manifest.json");
    let content = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let next_lba = v["next_lba"].as_u64();
    let total_blocks = v["blocks"].as_array().map(|b| b.len());
    (next_lba, total_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `CartridgeCreateRequest` with the given chunking string and
    /// chunk size; every other field at its default.
    fn create_req(chunking: &str, chunk_size_bytes: u64) -> CartridgeCreateRequest {
        CartridgeCreateRequest {
            barcode: "TAPE001".to_string(),
            lto_generation: None,
            chunk_size_bytes,
            chunking: chunking.to_string(),
            chunking_min_bytes: None,
            chunking_max_bytes: None,
            multi: default_multi(),
            backend: None,
            worm: false,
            dedup: default_dedup(),
            encrypt: false,
            keystore: None,
        }
    }

    #[test]
    fn default_multi_is_one_and_default_dedup_is_global() {
        assert_eq!(default_multi(), 1);
        assert_eq!(default_dedup(), "global");
    }

    #[test]
    fn parse_chunking_mode_fixed() {
        let req = create_req("fixed", 128 * 1024 * 1024);
        let (mode, label) = parse_chunking_mode(&req).expect("fixed mode parses");
        assert_eq!(label, "fixed");
        assert!(matches!(
            mode,
            ChunkingMode::Fixed { size_bytes } if size_bytes == 128 * 1024 * 1024
        ));
    }

    #[test]
    fn parse_chunking_mode_fixed_rejects_fastcdc_bounds() {
        let mut req = create_req("fixed", 1024);
        req.chunking_min_bytes = Some(512);
        let err = parse_chunking_mode(&req);
        assert!(err.is_err());
    }

    #[test]
    fn parse_chunking_mode_fastcdc_derives_min_max() {
        let req = create_req("fastcdc", 8 * 1024 * 1024);
        let (mode, label) = parse_chunking_mode(&req).expect("fastcdc parses");
        assert_eq!(label, "fastcdc");
        assert!(matches!(
            mode,
            ChunkingMode::FastCdc { min, avg, max }
                if min <= avg && avg <= max && avg == 8 * 1024 * 1024
        ));
    }

    #[test]
    fn parse_chunking_mode_fastcdc_zero_size_uses_default_avg() {
        let req = create_req("fastcdc", 0);
        let (mode, _) = parse_chunking_mode(&req).expect("fastcdc with default avg");
        assert!(matches!(mode, ChunkingMode::FastCdc { .. }));
    }

    #[test]
    fn parse_chunking_mode_fastcdc_rejects_min_over_avg() {
        let mut req = create_req("fastcdc", 1024 * 1024);
        req.chunking_min_bytes = Some(8 * 1024 * 1024);
        let err = parse_chunking_mode(&req);
        assert!(err.is_err());
        assert!(err.expect_err("min>avg fails").contains("exceeds avg"));
    }

    #[test]
    fn parse_chunking_mode_fastcdc_rejects_avg_over_max() {
        let mut req = create_req("fastcdc", 64 * 1024 * 1024);
        req.chunking_max_bytes = Some(1024 * 1024);
        let err = parse_chunking_mode(&req);
        assert!(err.is_err());
        assert!(err.expect_err("avg>max fails").contains("exceeds max"));
    }

    #[test]
    fn parse_chunking_mode_rejects_unknown_strategy() {
        let req = create_req("bogus", 1024);
        let err = parse_chunking_mode(&req);
        assert!(err.is_err());
        assert!(
            err.expect_err("unknown strategy fails")
                .contains("unknown chunking strategy")
        );
    }

    #[test]
    fn expand_barcodes_multi_zero_is_error() {
        assert!(expand_barcodes("TAPE001", 0).is_err());
    }

    #[test]
    fn expand_barcodes_multi_one_returns_single() {
        let v = expand_barcodes("TAPE001", 1).expect("multi=1");
        assert_eq!(v, vec!["TAPE001".to_string()]);
    }

    #[test]
    fn expand_barcodes_increments_and_preserves_width() {
        let v = expand_barcodes("TAPE001", 3).expect("multi=3");
        assert_eq!(
            v,
            vec![
                "TAPE001".to_string(),
                "TAPE002".to_string(),
                "TAPE003".to_string()
            ]
        );
    }

    #[test]
    fn expand_barcodes_requires_numeric_suffix() {
        let err = expand_barcodes("TAPE", 2);
        assert!(err.is_err());
        assert!(err.expect_err("no suffix fails").contains("numeric suffix"));
    }

    #[test]
    fn expand_barcodes_detects_width_overflow() {
        // 1-digit suffix starting at 9, multi 5 -> would reach 14,
        // past the single-digit field's 0..9 range.
        let err = expand_barcodes("TAPE9", 5);
        assert!(err.is_err());
        assert!(err.expect_err("overflow fails").contains("overflow"));
    }

    #[test]
    fn bad_request_carries_400_status() {
        let resp = bad_request("nope");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn server_error_carries_500_status() {
        let resp = server_error("boom");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn copy_dir_all_replicates_a_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::create_dir_all(src.join("nested")).expect("mkdir nested");
        std::fs::write(src.join("a.txt"), b"alpha").expect("write a");
        std::fs::write(src.join("nested").join("b.txt"), b"beta").expect("write b");
        copy_dir_all(&src, &dst).expect("copy tree");
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).expect("read a"),
            "alpha"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("nested").join("b.txt")).expect("read b"),
            "beta"
        );
    }

    #[test]
    fn resolve_lto_generation_accepts_8_rejects_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib = core_mediachanger::Library::initialize(
            &dir.path().join("library"),
            &dir.path().join("tapes"),
            5,
            0,
            1,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .expect("init library");
        let req8 = create_req("fixed", 1024);
        assert_eq!(resolve_lto_generation(&req8, &lib), Ok(8));
        let mut req9 = create_req("fixed", 1024);
        req9.lto_generation = Some(9);
        assert!(resolve_lto_generation(&req9, &lib).is_err());
    }

    #[test]
    fn cartridge_present_false_on_empty_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib = core_mediachanger::Library::initialize(
            &dir.path().join("library"),
            &dir.path().join("tapes"),
            5,
            0,
            1,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .expect("init library");
        assert!(!cartridge_present(&lib, "TAPE001"));
    }

    #[test]
    fn read_position_missing_runtime_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let (lba, blocks) = rt.block_on(read_position(dir.path(), "ABSENT"));
        assert!(lba.is_none());
        assert!(blocks.is_none());
    }

    #[test]
    fn cartridge_create_request_deserializes_minimal() {
        let req: CartridgeCreateRequest = serde_json::from_value(serde_json::json!({
            "barcode": "T1",
            "chunk_size_bytes": 8388608,
            "chunking": "fastcdc",
        }))
        .expect("minimal create request");
        assert_eq!(req.barcode, "T1");
        assert_eq!(req.multi, 1);
        assert_eq!(req.dedup, "global");
        assert!(!req.worm);
        assert!(!req.encrypt);
    }
}
