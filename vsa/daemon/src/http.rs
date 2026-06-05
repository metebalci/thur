// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! thurvsa's TCP HTTP server.
//!
//! - `GET /health`   — liveness probe (`shared_health`).
//! - `GET /metrics`  — Prometheus text format. Renders the
//!   `shared-telemetry::Telemetry` snapshot, which carries the
//!   same `thur_*` instrument names thurvtld emits — both
//!   products live on one MeterProvider per process and the
//!   `service.name` resource attribute (set to `thurvsa` here, vs.
//!   `thurvtl` on the VTL daemon) is what distinguishes the two on
//!   shared OTLP backends.
//! - `GET /sessions` — iSCSI session inventory (`shared_iscsi::http`).
//! - `GET /info`     — VSA-specific summary (volume count + IQN +
//!   listen addresses).
//!
//! Default listen address is `0.0.0.0:9090` — mirrors thurvtl's
//! `:9090` posture so an operator running both daemons on the
//! same host can scrape each port independently. CHAP / cloud
//! secrets are out of band on this listener; volume mutations go
//! through the admin Unix socket instead.
//!
//! Bind / TLS / serve plumbing lives in `shared-admin-http`. This
//! module only owns the per-product Router + state composition.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{FromRef, State},
    response::IntoResponse,
    routing::get,
};
use shared_health::HealthMeta;
use shared_iscsi::http::SessionsState;
use shared_iscsi::session::SessionManager;
use shared_telemetry::Telemetry;
use tracing::info;

use crate::admin::handlers::{self, AdminState};
use crate::admin::snapshots;
use crate::registry::VolumeRegistry;

/// Default TCP listen address for the HTTP server. Distinct from
/// thurvtl's :9090 so co-resident installs don't fight for the port.
pub const DEFAULT_HTTP_LISTEN_ADDRESS: &str = "0.0.0.0:9090";

#[derive(Clone)]
pub struct HttpState {
    pub telemetry: Arc<Telemetry>,
    pub registry: Arc<VolumeRegistry>,
    pub sessions: Arc<SessionManager>,
    /// Every transport listen address (one for NVMe/TCP, one or more
    /// iSCSI portals). Always at least one entry.
    pub listen_addresses: Vec<String>,
    /// Resolved iSCSI target IQN (`iscsi.target_iqn` or the default).
    pub target_iqn: String,
    /// Live web-admin password verifier (issue #4) — the same handle
    /// the admin socket's `set-admin-password` setter writes. Drives the
    /// `require_admin_password` middleware on the protected route group.
    pub auth: shared_admin_auth::AuthState,
    /// The admin state, also handed to the Unix admin socket. The Web
    /// UI's read-only `/api/v1` GET subset (issue #5) runs against it;
    /// only GET handlers are mounted on the TCP listener, so the surface
    /// stays read-only (mutating handlers take `PeerCred`).
    pub admin: AdminState,
}

// Lets the shared `/metrics` handler (in `shared_telemetry::http`)
// extract its `Arc<Telemetry>` from our composite state.
impl FromRef<HttpState> for Arc<Telemetry> {
    fn from_ref(state: &HttpState) -> Self {
        state.telemetry.clone()
    }
}

// Lets the shared `/health` handler (in `shared_health`) extract its
// per-product identity from our composite state.
impl FromRef<HttpState> for HealthMeta {
    fn from_ref(_state: &HttpState) -> Self {
        HealthMeta {
            product: &shared_naming::DISK,
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

// Lets the shared `/sessions` handler (in `shared_iscsi::http`)
// extract the session-manager + target-coordinates bundle. VSA's
// IQN and listen address are both daemon config, threaded through
// `HttpState`.
impl FromRef<HttpState> for SessionsState {
    fn from_ref(state: &HttpState) -> Self {
        SessionsState {
            sessions: state.sessions.clone(),
            target_iqn: state.target_iqn.clone(),
            listen_addresses: state.listen_addresses.clone(),
        }
    }
}

/// Construct the axum Router for the daemon's TCP HTTP listener.
/// `shared-admin-http` owns the bind/serve glue.
///
/// Route groups, all merged into one listener:
/// - `open` (`/health` + `/metrics`) — unauthenticated, for liveness
///   probes + Prometheus scrape.
/// - `protected` (`/sessions`, `/info`) — gated by the web-admin
///   password (#4) via [`shared_admin_auth::require_admin_password`];
///   they expose the target IQN, listen addresses, and volume topology.
/// - When `webui.enabled`: the read-only `/api/v1` GET subset (volume
///   list / info / snapshots + the cross-product monitor / jobs / audit
///   handlers from `shared-admin-webui`) plus the static `/ui` bundle,
///   all gated by the same password. The read-only API runs against
///   [`AdminState`] — the same state the admin socket uses — but only
///   GET handlers are mounted, so the TCP surface stays read-only
///   (mutating handlers take `PeerCred` and are never mounted here).
pub fn build_router(state: HttpState, webui: &shared_admin_webui::WebuiConfig) -> Router {
    let auth = state.auth.clone();

    let open = Router::new()
        .route("/health", get(shared_health::health_handler))
        .route("/metrics", get(shared_telemetry::http::metrics_handler))
        .with_state(state.clone());

    let protected = Router::new()
        .route("/sessions", get(shared_iscsi::http::sessions_handler))
        .route("/info", get(info_handler))
        .route_layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            shared_admin_auth::require_admin_password,
        ))
        .with_state(state.clone());

    let mut router = open.merge(protected);

    if webui.enabled {
        let admin = state.admin.clone();
        let api = Router::new()
            .route("/api/v1/volumes", get(handlers::list))
            .route("/api/v1/volumes/:name", get(handlers::info))
            .route("/api/v1/volumes/:name/snapshots", get(snapshots::list))
            .route(
                "/api/v1/monitor",
                get(shared_admin_webui::monitor_snapshot_handler::<AdminState>),
            )
            .route(
                "/api/v1/jobs/recent",
                get(shared_admin_webui::jobs_recent_handler::<AdminState>),
            )
            .route(
                "/api/v1/audit/tail",
                get(shared_admin_webui::audit_tail_handler::<AdminState>),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                auth.clone(),
                shared_admin_auth::require_admin_password,
            ))
            .with_state(admin);
        router = router
            .merge(api)
            .merge(shared_admin_webui::webui_router(webui, auth));
    }

    router
}

/// Emit the per-route URL listing operators see in journalctl at boot.
/// `scheme` is "http" or "https" — picked by the caller from whether
/// `http.tls` is configured.
pub fn log_route_table(listen: &str, scheme: &str, webui_enabled: bool, password_required: bool) {
    info!("HTTP server listening on {scheme}://{listen}");
    let gate = if password_required {
        "admin password"
    } else {
        "no auth"
    };
    if !password_required {
        info!(
            "  web-admin auth DISABLED (http.auth.method: None) - protected routes served open; ensure the listener is on a trusted network"
        );
    }
    for route in ["health", "metrics"] {
        info!("  - {route}: {scheme}://{listen}/{route}");
    }
    for route in ["sessions", "info"] {
        info!("  - {route}: {scheme}://{listen}/{route} ({gate})");
    }
    if webui_enabled {
        info!("  - ui: {scheme}://{listen}/ui/ ({gate})");
        info!("  - api: {scheme}://{listen}/api/v1/* read-only ({gate})");
    }
}

/// Handler for `/info` — read-only summary of the running daemon's
/// volume count + the configured iSCSI coordinates a host needs to
/// reach the target. Per-volume detail stays on the admin socket.
async fn info_handler(State(state): State<HttpState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "product": "thurvsa",
        "volume_count": state.registry.len(),
        "iqn": state.target_iqn,
        "listen_addresses": state.listen_addresses,
    }))
}
