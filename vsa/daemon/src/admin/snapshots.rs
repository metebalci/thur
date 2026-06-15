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

use axum::{
    Json,
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
};
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
/// the volume's cache and awaits its pending storage uploads so the frozen
/// index references only storage-durable chunks. The copy briefly pauses
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

#[derive(Debug, Deserialize)]
pub struct FindSnapshotQuery {
    /// Snapshot name to locate across all volumes.
    pub name: String,
}

/// `GET /api/v1/snapshots?name=<snap>` — find a snapshot by name across
/// every volume.
///
/// The daemon scopes snapshot names per-volume, so a name can in
/// principle exist on more than one volume; this returns the first match
/// in the same sorted `(volume, snapshot)` order the GC walk uses, so the
/// result is deterministic. It exists for the CSI driver, whose snapshot
/// Name must be globally unique: a single round trip here replaces the
/// driver's prior O(volumes) `ListSnapshots` fan-out (issue #294). 404
/// when no volume carries a snapshot with that name. The on-disk walk is
/// offloaded to a blocking thread so it can't stall a runtime worker
/// shared with the data path.
pub async fn find(
    State(state): State<AdminState>,
    Query(q): Query<FindSnapshotQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if q.name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "query parameter 'name' must be non-empty" })),
        ));
    }
    let data_dir = state.data_dir.clone();
    let target = q.name.clone();
    let found = tokio::task::spawn_blocking(
        move || -> Result<Option<(String, SnapshotManifest)>, core_block::VolumeError> {
            for (parent, snap) in SnapshotManifest::list_all(&data_dir)? {
                if snap == target {
                    return Ok(Some((parent.clone(), SnapshotManifest::load(
                        &data_dir, &parent, &snap,
                    )?)));
                }
            }
            Ok(None)
        },
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("snapshot scan task: {e}") })),
        )
    })?
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("scan snapshots: {e}") })),
        )
    })?;

    match found {
        Some((parent, m)) => Ok(Json(json!({
            "volume": parent,
            "snapshot": m.name,
            "size_bytes": m.size_bytes,
            "created_at": m.created_at,
        }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no snapshot named '{}' found", q.name) })),
        )),
    }
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

/// `POST /api/v1/volumes/{name}/snapshots/{snap}/restore` — roll a
/// volume in place back to one of its snapshots (issue #85).
///
/// Destructive: discards every write to the volume since the snapshot.
/// The volume keeps its identity (uuid / lun / name / DEK) — only the
/// page table is rewound. Diverged post-snapshot chunks become orphans
/// the next `system gc` reclaims (the same leave-for-GC contract as
/// `volume destroy`).
///
/// Guards:
/// - the volume must be registered (live);
/// - a held SCSI persistent reservation refuses the restore (a cluster
///   is actively using the LUN) — mirrors `volume resize`'s shrink
///   guard. There is NO active-session check: the target can't see host
///   mount state, so quiescing the host before restore is the
///   operator's responsibility (the CLI requires `--force` and warns).
/// - without `resize` (issue #90), the snapshot's captured size must
///   equal the volume's current size; restore is page-table-only, so a
///   resized volume must be resized back first. With `resize`, the
///   handler rolls the logical size back to the snapshot's captured size
///   after the page-table rewrite and signals connected hosts to re-read
///   capacity — except a WORM volume, whose size is grow-only, so a
///   shrink-back is refused up front.
pub async fn restore(
    State(state): State<AdminState>,
    peer: PeerCred,
    AxumPath((name, snap)): AxumPath<(String, String)>,
    Json(body): Json<RestoreSnapshotRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cache = state.registry.get_by_name(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("volume '{name}' is not registered") })),
        )
    })?;

    let manifest = SnapshotManifest::load(&state.data_dir, &name, &snap).map_err(|e| {
        let status = match e {
            core_block::VolumeError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(json!({ "error": format!("load snapshot '{snap}': {e}") })),
        )
    })?;

    // A held persistent reservation blocks restore: silently swapping the
    // data under a registrant (a cluster member) is a least-surprise
    // violation. Mirrors `resize` (handlers.rs).
    let lun = cache.manifest().lun;
    if !state.reservations.snapshot(lun).registrants.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "volume '{name}' has an active persistent reservation; \
                     clear it before restoring"
                )
            })),
        ));
    }

    // Restore is page-table-only by default. If the volume was resized
    // after the snapshot, the live size and the captured size diverge.
    // Without `resize` (issue #90), refuse and tell the operator to
    // resize back first rather than silently leaving a
    // coherent-but-surprising size/extent mismatch; with it, roll the
    // size back too after the page-table rewrite below. Compare against
    // the live shadow, not the boot-snapshot manifest (issue #76).
    let live_size = cache.size_bytes();
    let target_size = manifest.size_bytes;
    let size_rollback = body.resize && target_size != live_size;
    if target_size != live_size && !body.resize {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "snapshot '{snap}' captured size {} B != volume '{name}' size {} B; \
                     pass --resize to roll the size back too, or resize the volume to {} B first",
                    target_size, live_size, target_size
                )
            })),
        ));
    }

    // A size rollback that shrinks a WORM volume is refused up front —
    // before the page table is touched — so a refusal leaves the volume
    // wholly untouched rather than data-rolled-back-but-size-stale. A WORM
    // volume can only have been *grown* after the snapshot (shrink is
    // forbidden), so the rollback would be a shrink, which `set_size`
    // rejects with the same WORM guard. Mirrors `resize` (handlers.rs).
    if size_rollback && target_size < live_size && cache.manifest().worm {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "volume '{name}' is WORM; restore cannot shrink its size back to the \
                     snapshot's {} B (current {} B). WORM size is grow-only.",
                    target_size, live_size
                )
            })),
        ));
    }

    let snap_idx = SnapshotManifest::page_index_path(&state.data_dir, &name, &snap);
    cache.restore_from_snapshot(snap_idx).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("restore snapshot: {e}") })),
        )
    })?;

    // Roll the logical size back to the snapshot's captured size (issue
    // #90). Ordered AFTER the page-table rewrite so the shrink guard rails
    // hold by construction: the restored index's high-water mark is
    // already the snapshot's, so nothing sits past the snapshot-era size
    // and `set_size` cannot trip ResizeWouldDiscardData. The pre-checks
    // above ruled out the only other way `set_size` could fail here (a
    // WORM shrink); alignment / page-floor hold because `target_size` was
    // itself a valid live size when the snapshot froze it. The
    // capacity-change notice mirrors `volume resize` (issue #76).
    if size_rollback {
        cache.writer().set_size(target_size).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("roll size back to snapshot: {e}") })),
            )
        })?;
        state.notify_capacity_changed(lun);
    }

    info!(
        volume = name.as_str(),
        snapshot = snap.as_str(),
        size_bytes = target_size,
        resized = size_rollback,
        "admin: restored volume to snapshot uid={} pid={:?}",
        peer.uid,
        peer.pid,
    );

    if let Some(channel) = state.audit.as_ref() {
        channel.try_append(
            "snapshot.restore",
            AuditActor::cli(peer.audit_descriptor()),
            json!({
                "volume": name,
                "snapshot": snap,
                "size_bytes": target_size,
                "previous_size_bytes": live_size,
                "resized": size_rollback,
            }),
            AuditResult::Ok,
        );
    }

    Ok(Json(json!({
        "volume": name,
        "snapshot": snap,
        "size_bytes": target_size,
        "previous_size_bytes": live_size,
        "resized": size_rollback,
        "status": "restored",
    })))
}

#[derive(Debug, Deserialize)]
pub struct RestoreSnapshotRequest {
    /// Roll the volume's logical size back to the snapshot's captured
    /// `size_bytes` as well (issue #90). Without it, restore is
    /// page-table-only and refuses on a size mismatch.
    #[serde(default)]
    pub resize: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The handler guard / orchestration paths (registered-volume
    // requirement, duplicate / size-mismatch / WORM / active-reservation
    // refusals, the rollback ordering) run against a live daemon and are
    // covered end-to-end by `vsa/scripts/test-snapshot.sh`. What is
    // genuinely unit-testable here is the request-body wire contract,
    // where the `resize` default decides whether a restore is
    // page-table-only or a destructive size rollback.

    #[test]
    fn restore_request_resize_defaults_to_false() {
        // Absent `resize` => page-table-only restore (the safe,
        // size-preserving default; a size mismatch then refuses).
        let req: RestoreSnapshotRequest = serde_json::from_value(json!({})).unwrap();
        assert!(!req.resize);
    }

    #[test]
    fn restore_request_honours_explicit_resize() {
        let on: RestoreSnapshotRequest = serde_json::from_value(json!({ "resize": true })).unwrap();
        assert!(on.resize);
        let off: RestoreSnapshotRequest =
            serde_json::from_value(json!({ "resize": false })).unwrap();
        assert!(!off.resize);
    }

    #[test]
    fn create_request_requires_a_snapshot_name() {
        let req: CreateSnapshotRequest =
            serde_json::from_value(json!({ "snapshot": "daily" })).unwrap();
        assert_eq!(req.snapshot, "daily");
        // The name is mandatory — an empty body is a deserialize error,
        // not a silent default.
        assert!(serde_json::from_value::<CreateSnapshotRequest>(json!({})).is_err());
    }

    #[test]
    fn find_query_requires_the_name_param() {
        // The cross-volume find verb (issue #294) keys off a single
        // required `name` query parameter. The Deserialize derive is what
        // axum's Query extractor drives; confirm the field is mandatory
        // (a missing key is an error, not a silent default) — the handler
        // additionally rejects an explicit empty value with 400.
        let q: FindSnapshotQuery = serde_json::from_value(json!({ "name": "daily" })).unwrap();
        assert_eq!(q.name, "daily");
        assert!(serde_json::from_value::<FindSnapshotQuery>(json!({})).is_err());
    }
}
