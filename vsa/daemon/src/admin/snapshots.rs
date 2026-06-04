// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! thurvsa snapshot admin-socket handlers (issue #13).
//!
//! - `POST   /api/v1/volumes/{name}/snapshots`        — create snapshot
//! - `GET    /api/v1/volumes/{name}/snapshots`        — list snapshots
//! - `DELETE /api/v1/volumes/{name}/snapshots/{snap}` — destroy snapshot
//!
//! A snapshot is a frozen byte-for-byte copy of the volume's `pages.idx`
//! plus a small read-only [`SnapshotManifest`]. It is **not** a
//! host-visible LUN — to access its data, `thurvsa volume clone` it into
//! a new writable volume (`POST /api/v1/volumes/{name}/clone`, in
//! `handlers.rs`). The frozen index keeps the parent's pre-overwrite
//! chunks alive via the existing manifest-walking GC, which is what
//! makes copy-on-write reclaimable without any hot-path change.
//!
//! All three verbs are instant (no job protocol): create is a flush +
//! a sparse index copy, destroy is a directory removal.

use axum::{Json, extract::Path as AxumPath, extract::State, http::StatusCode};
use core_block::{PageIndex, SnapshotManifest, VolumeManifest};
use serde::Deserialize;
use serde_json::json;
use shared_admin_server::PeerCred;
use shared_audit::{AuditActor, AuditResult};
use tracing::{info, warn};

use super::handlers::AdminState;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Debug, Deserialize)]
pub struct CreateSnapshotRequest {
    /// Operator-chosen snapshot name, unique within this volume.
    pub snapshot: String,
}

/// `POST /api/v1/volumes/{name}/snapshots` — freeze a point-in-time
/// snapshot of a live volume's page table.
///
/// Requires the volume to be registered (live): snapshot-create flushes
/// the volume's cache and awaits its pending cloud uploads so the frozen
/// index references only cloud-durable chunks. The copy briefly pauses
/// the volume's host I/O (it runs under the cache's inner lock); the
/// `pages.idx` is sparse, so the pause scales with allocated pages.
pub async fn create(
    State(state): State<AdminState>,
    peer: PeerCred,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<CreateSnapshotRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let cache = state.registry.get_by_name(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("volume '{name}' is not registered") })),
        )
    })?;

    // Reject a duplicate snapshot name before we touch the disk.
    let snap_dir = SnapshotManifest::dir_for(&state.data_dir, &name, &body.snapshot);
    if snap_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("snapshot '{}' already exists for volume '{name}'", body.snapshot)
            })),
        ));
    }

    // Build the snapshot manifest from the parent + its live size. This
    // validates the snapshot name.
    let snap = SnapshotManifest::new(body.snapshot.clone(), cache.manifest(), cache.size_bytes())
        .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("invalid snapshot: {e}") })),
        )
    })?;

    std::fs::create_dir_all(&snap_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("create snapshot dir: {e}") })),
        )
    })?;

    // Freeze the page index (flush + quiesce + copy). Roll back the
    // snapshot dir on any failure so a stuck create leaves nothing.
    if let Err(e) = cache
        .snapshot_pages_idx(PageIndex::path_for(&snap_dir))
        .await
    {
        let _ = std::fs::remove_dir_all(&snap_dir);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("freeze page index: {e}") })),
        ));
    }

    // The manifest rename is the commit point: a crash before it leaves
    // a stray pages.idx with no snap.json, which every walker skips.
    if let Err(e) = snap.persist(&snap_dir) {
        let _ = std::fs::remove_dir_all(&snap_dir);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("persist snapshot manifest: {e}") })),
        ));
    }

    info!(
        volume = name.as_str(),
        snapshot = body.snapshot.as_str(),
        size_bytes = snap.size_bytes,
        "admin: created snapshot uid={} pid={:?}",
        peer.uid,
        peer.pid,
    );

    if let Some(channel) = state.audit.as_ref() {
        channel.try_append(
            "snapshot.create",
            AuditActor::cli(peer.audit_descriptor()),
            json!({
                "volume": name,
                "snapshot": body.snapshot,
                "size_bytes": snap.size_bytes,
                "uuid": hex::encode(snap.uuid),
            }),
            AuditResult::Ok,
        );
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "volume": name,
            "snapshot": snap.name,
            "size_bytes": snap.size_bytes,
            "created_at": snap.created_at,
        })),
    ))
}

/// `GET /api/v1/volumes/{name}/snapshots` — list a volume's snapshots.
///
/// Reads the on-disk `snapshots/` directory; works whether or not the
/// volume is currently registered. 404 if the volume directory is gone.
pub async fn list(
    State(state): State<AdminState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !VolumeManifest::dir_for(&state.data_dir, &name).is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("volume '{name}' not found") })),
        ));
    }
    let names = SnapshotManifest::list(&state.data_dir, &name).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("list snapshots: {e}") })),
        )
    })?;
    let mut snapshots = Vec::new();
    for snap in names {
        match SnapshotManifest::load(&state.data_dir, &name, &snap) {
            Ok(m) => snapshots.push(json!({
                "snapshot": m.name,
                "size_bytes": m.size_bytes,
                "created_at": m.created_at,
            })),
            Err(e) => warn!(
                volume = name.as_str(),
                snapshot = snap.as_str(),
                error = %e,
                "admin: skipping unreadable snapshot manifest in list"
            ),
        }
    }
    Ok(Json(json!({ "volume": name, "snapshots": snapshots })))
}

/// `DELETE /api/v1/volumes/{name}/snapshots/{snap}` — remove a snapshot.
///
/// Deletes the snapshot directory (manifest + frozen index). Chunks the
/// snapshot was holding alive that no other family member references
/// become orphans the next `system gc` reclaims — the same leave-for-GC
/// contract as `volume destroy`. No LUN / registry / NVMe interaction:
/// a snapshot is not host-visible.
pub async fn destroy(
    State(state): State<AdminState>,
    peer: PeerCred,
    AxumPath((name, snap)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snap_dir = SnapshotManifest::dir_for(&state.data_dir, &name, &snap);
    if !snap_dir.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("snapshot '{snap}' not found for volume '{name}'")
            })),
        ));
    }
    std::fs::remove_dir_all(&snap_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("remove snapshot dir: {e}") })),
        )
    })?;

    info!(
        volume = name.as_str(),
        snapshot = snap.as_str(),
        "admin: destroyed snapshot uid={} pid={:?}",
        peer.uid,
        peer.pid,
    );

    if let Some(channel) = state.audit.as_ref() {
        channel.try_append(
            "snapshot.destroy",
            AuditActor::cli(peer.audit_descriptor()),
            json!({ "volume": name, "snapshot": snap }),
            AuditResult::Ok,
        );
    }

    Ok(Json(json!({
        "volume": name,
        "snapshot": snap,
        "status": "destroyed",
    })))
}
