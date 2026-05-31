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
pub mod nvmetcp_dhchap;
pub mod nvmetcp_host_file;
pub mod nvmetcp_psks;

use anyhow::Result;
use axum::{
    Router,
    routing::{get, post},
};
use shared_admin_server::HasJobs;
use std::path::PathBuf;

use handlers::AdminState;
use nvmetcp_dhchap::DhchapSurface;
use nvmetcp_host_file as host_file;
use nvmetcp_psks::PsksSurface;

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
        .route("/api/v1/iscsi/users/grant", post(iscsi_users::grant))
        .route("/api/v1/iscsi/users/revoke", post(iscsi_users::revoke))
        // mutual-CHAP target credential (singleton)
        .route(
            "/api/v1/iscsi/target",
            get(iscsi_target::show).post(iscsi_target::set),
        )
        .route("/api/v1/iscsi/target/clear", post(iscsi_target::clear))
        // NVMe-TCP TLS-PSK lifecycle. The mutating verbs are the
        // generic host-credential handlers parameterized on the
        // TLS-PSK surface; only `list` carries a surface-specific
        // response envelope.
        .route(
            "/api/v1/nvmetcp/psks",
            get(nvmetcp_psks::list).post(host_file::add::<PsksSurface>),
        )
        .route(
            "/api/v1/nvmetcp/psks/remove",
            post(host_file::remove::<PsksSurface>),
        )
        .route(
            "/api/v1/nvmetcp/psks/disable",
            post(host_file::disable::<PsksSurface>),
        )
        .route(
            "/api/v1/nvmetcp/psks/enable",
            post(host_file::enable::<PsksSurface>),
        )
        .route(
            "/api/v1/nvmetcp/psks/rotate",
            post(host_file::rotate::<PsksSurface>),
        )
        .route(
            "/api/v1/nvmetcp/psks/rotate/cancel",
            post(host_file::rotate_cancel::<PsksSurface>),
        )
        .route(
            "/api/v1/nvmetcp/psks/grant",
            post(host_file::grant::<PsksSurface>),
        )
        .route(
            "/api/v1/nvmetcp/psks/revoke",
            post(host_file::revoke::<PsksSurface>),
        )
        // NVMe-TCP DH-HMAC-CHAP lifecycle (same generic handlers on the
        // DH-HMAC-CHAP surface; `list` + the ctrl-key verbs are
        // surface-specific).
        .route(
            "/api/v1/nvmetcp/dhchap",
            get(nvmetcp_dhchap::list).post(host_file::add::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/remove",
            post(host_file::remove::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/disable",
            post(host_file::disable::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/enable",
            post(host_file::enable::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/rotate",
            post(host_file::rotate::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/rotate/cancel",
            post(host_file::rotate_cancel::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/grant",
            post(host_file::grant::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/revoke",
            post(host_file::revoke::<DhchapSurface>),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/ctrl-key/set",
            post(nvmetcp_dhchap::set_ctrl_key),
        )
        .route(
            "/api/v1/nvmetcp/dhchap/ctrl-key/clear",
            post(nvmetcp_dhchap::clear_ctrl_key),
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
