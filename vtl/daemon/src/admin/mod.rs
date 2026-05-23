// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Admin Unix-socket router for `thurvtld`.
//!
//! Always binds `/run/thurvtl/admin.sock` (overridable via the
//! `THURVTL_ADMIN_SOCKET` env var for dev / tests where `/run/`
//! isn't writable). Mode 0660; the systemd unit's
//! `RuntimeDirectory=thurvtl` provisions the parent at boot.
//!
//! Transport (bind / chmod / SO_PEERCRED capture / accept loop) and
//! the long-running-job HTTP machinery live in `shared-admin-server`.
//! This module just builds the product-specific router (cartridges,
//! drives, changer, legal-hold, library, system endpoints), merges
//! the shared jobs router, and hands the result to the shared
//! transport runner.

pub mod handlers;
pub mod iscsi_users;
pub mod job_dispatch;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::json;
use shared_admin_server::HasJobs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::state::DaemonState;
use handlers::AdminState;

/// Canonical admin socket path. Sourced from
/// [`shared_naming::TAPE_LIBRARY`] so the daemon and CLI agree
/// without duplicating the literal.
pub const CANONICAL_ADMIN_SOCKET: &str = shared_naming::TAPE_LIBRARY.admin_socket;

/// Env var that overrides `CANONICAL_ADMIN_SOCKET`. Same name
/// honored by the CLI client (derives the env-var name from the
/// product identity at runtime).
pub const ADMIN_SOCKET_ENV: &str = "THURVTL_ADMIN_SOCKET";

/// Resolve the admin-socket path the daemon should bind. Reads
/// `THURVTL_ADMIN_SOCKET` if set, otherwise returns the canonical
/// path.
pub fn admin_socket_path() -> PathBuf {
    match std::env::var(ADMIN_SOCKET_ENV) {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => PathBuf::from(CANONICAL_ADMIN_SOCKET),
    }
}

impl HasJobs for AdminState {
    fn jobs(&self) -> &shared_admin_server::JobRegistry {
        &self.daemon.jobs
    }
}

/// Build the product router (cartridges / library / changer /
/// drives / system / health) and hand it off to the shared
/// transport runner.
pub async fn run_admin_server(socket_path: PathBuf, daemon_state: Arc<DaemonState>) -> Result<()> {
    let state = AdminState {
        daemon: daemon_state,
    };

    let product_routes = Router::new()
        .route("/api/v1/health", get(health_handler))
        .route("/api/v1/library/info", get(handlers::library_info))
        .route("/api/v1/library/bounds", get(handlers::library_bounds))
        .route(
            "/api/v1/cartridges",
            get(handlers::cartridges_list).post(handlers::cartridge_create),
        )
        .route(
            "/api/v1/cartridges/:identifier",
            get(handlers::cartridge_info),
        )
        .route(
            "/api/v1/changer/inventory",
            get(handlers::changer_inventory),
        )
        .route(
            "/api/v1/cartridges/import",
            axum::routing::post(handlers::cartridge_import),
        )
        .route(
            "/api/v1/cartridges/export/:slot",
            axum::routing::post(handlers::cartridge_export),
        )
        .route(
            "/api/v1/cartridges/:barcode/legal-hold",
            axum::routing::put(handlers::legal_hold_set)
                .delete(handlers::legal_hold_clear)
                .get(handlers::legal_hold_status),
        )
        .route(
            "/api/v1/changer/move",
            axum::routing::post(handlers::changer_move),
        )
        .route(
            "/api/v1/changer/load",
            axum::routing::post(handlers::changer_load),
        )
        .route(
            "/api/v1/changer/unload",
            axum::routing::post(handlers::changer_unload),
        )
        .route("/api/v1/drives", get(handlers::drives_list))
        .route("/api/v1/drives/:id", get(handlers::drive_status))
        // iSCSI CHAP users
        .route(
            "/api/v1/iscsi/users",
            get(iscsi_users::list).post(iscsi_users::add),
        )
        .route("/api/v1/iscsi/users/remove", post(iscsi_users::remove))
        .route("/api/v1/iscsi/users/disable", post(iscsi_users::disable))
        .route("/api/v1/iscsi/users/enable", post(iscsi_users::enable))
        .route("/api/v1/iscsi/users/rotate", post(iscsi_users::rotate))
        .route(
            "/api/v1/iscsi/users/rotate/cancel",
            post(iscsi_users::rotate_cancel),
        )
        // mutual-CHAP target credential
        .route(
            "/api/v1/iscsi/target",
            get(iscsi_users::target_show).post(iscsi_users::target_set),
        )
        .route(
            "/api/v1/iscsi/target/clear",
            post(iscsi_users::target_clear),
        )
        .with_state(state.clone());

    let jobs = shared_admin_server::jobs_router(state, |kind, body, emitter, st| {
        job_dispatch::dispatch(kind, body, emitter, Arc::clone(&st.daemon))
    });

    // `system alerting list` queries this. Process-global dispatcher
    // means the handler doesn't need per-daemon state — empty marker.
    let alerting = axum::Router::new()
        .route(
            "/api/v1/system/alerting",
            get(shared_alerting::http::alerting_list),
        )
        .with_state(shared_alerting::http::AlertingHttpState);

    let app = product_routes.merge(jobs).merge(alerting);

    shared_admin_server::run_admin_server(socket_path, app).await
}

/// `GET /api/v1/health` — admin-side health probe.
///
/// Distinct from the TCP `/health` endpoint (the unauthenticated
/// liveness probe for /health curl loops on the management
/// LAN). This one is authenticated by the socket's filesystem
/// permissions and reports daemon-internal context the CLI client
/// uses to confirm it's talking to the right `data_dir`.
async fn health_handler(State(state): State<AdminState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "daemon": "thurvtld",
        "version": crate::THURVTL_VERSION_STR,
        "data_dir": state.daemon.data_dir,
        "api_version": "v1",
    }))
}
