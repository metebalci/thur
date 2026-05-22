// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Admin Unix-socket router for `thurvsad`.
//!
//! Always binds `/run/thurvsa/admin.sock` (overridable via the
//! `THURVSA_ADMIN_SOCKET` env var for dev / tests where `/run/`
//! isn't writable). Mode 0660; the systemd unit's
//! `RuntimeDirectory=thurvsa` provisions the parent at boot. The
//! co-resident thurvtld binds `/run/thurvtl/admin.sock` —
//! the two products distinguish by directory name, same way they
//! do on iSCSI ports (3260 (per-product override)).
//!
//! Transport (bind / chmod / SO_PEERCRED capture / accept loop) and
//! the long-running-job HTTP machinery live in `shared-admin-server`.
//! This module just builds the product router, merges the shared
//! jobs router, and hands the result off.

pub mod handlers;
pub mod iscsi_target;
pub mod iscsi_users;
pub mod job_dispatch;
pub mod nvmetcp_psks;

use anyhow::Result;
use axum::{
    Router,
    routing::{get, post},
};
use shared_admin_server::HasJobs;
use std::path::PathBuf;

use handlers::AdminState;

/// Canonical admin socket path. Sourced from [`shared_naming::DISK`]
/// so the daemon and CLI agree without duplicating the literal.
pub const CANONICAL_ADMIN_SOCKET: &str = shared_naming::DISK.admin_socket;

/// Env var that overrides `CANONICAL_ADMIN_SOCKET`. Same name
/// honored by the CLI client.
pub const ADMIN_SOCKET_ENV: &str = "THURVSA_ADMIN_SOCKET";

/// Resolve the admin-socket path the daemon should bind. Reads
/// `THURVSA_ADMIN_SOCKET` if set, otherwise returns the canonical
/// path.
pub fn admin_socket_path() -> PathBuf {
    match std::env::var(ADMIN_SOCKET_ENV) {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => PathBuf::from(CANONICAL_ADMIN_SOCKET),
    }
}

impl HasJobs for AdminState {
    fn jobs(&self) -> &shared_admin_server::JobRegistry {
        &self.jobs
    }
}

/// Build the product router, merge in the shared jobs router, and
/// hand the result off to the shared transport runner.
pub async fn run_admin_server(socket_path: PathBuf, state: AdminState) -> Result<()> {
    let product_routes = Router::new()
        .route("/api/v1/health", get(handlers::health))
        .route(
            "/api/v1/volumes",
            get(handlers::list).post(handlers::create),
        )
        .route(
            "/api/v1/volumes/:name",
            get(handlers::info).delete(handlers::destroy),
        )
        .route(
            "/api/v1/volumes/:name/sync-after",
            post(handlers::set_sync_after),
        )
        // iSCSI CHAP users (list / add / remove / disable / enable / rotate)
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
        // mutual-CHAP target credential (singleton)
        .route(
            "/api/v1/iscsi/target",
            get(iscsi_target::show).post(iscsi_target::set),
        )
        .route("/api/v1/iscsi/target/clear", post(iscsi_target::clear))
        // NVMe-TCP TLS-PSK lifecycle
        .route(
            "/api/v1/nvmetcp/psks",
            get(nvmetcp_psks::list).post(nvmetcp_psks::add),
        )
        .route("/api/v1/nvmetcp/psks/remove", post(nvmetcp_psks::remove))
        .route("/api/v1/nvmetcp/psks/disable", post(nvmetcp_psks::disable))
        .route("/api/v1/nvmetcp/psks/enable", post(nvmetcp_psks::enable))
        .route("/api/v1/nvmetcp/psks/rotate", post(nvmetcp_psks::rotate))
        .route(
            "/api/v1/nvmetcp/psks/rotate/cancel",
            post(nvmetcp_psks::rotate_cancel),
        )
        .with_state(state.clone());

    let jobs = shared_admin_server::jobs_router(state, |kind, body, emitter, st| {
        job_dispatch::dispatch(kind, body, emitter, st)
    });

    // `system alerting list` queries this. Process-global dispatcher
    // means the handler doesn't need per-daemon state — empty marker.
    let alerting = axum::Router::new()
        .route(
            "/api/v1/system/alerting",
            axum::routing::get(shared_alerting::http::alerting_list),
        )
        .with_state(shared_alerting::http::AlertingHttpState);

    let app = product_routes.merge(jobs).merge(alerting);

    shared_admin_server::run_admin_server(socket_path, app).await
}
