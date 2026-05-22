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
//!   listen address).
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

use crate::registry::VolumeRegistry;

/// Default TCP listen address for the HTTP server. Distinct from
/// thurvtl's :9090 so co-resident installs don't fight for the port.
pub const DEFAULT_HTTP_LISTEN_ADDRESS: &str = "0.0.0.0:9090";

#[derive(Clone)]
pub struct HttpState {
    pub telemetry: Arc<Telemetry>,
    pub registry: Arc<VolumeRegistry>,
    pub sessions: Arc<SessionManager>,
    pub listen_address: String,
    /// Resolved iSCSI target IQN (`iscsi.target_iqn` or the default).
    pub target_iqn: String,
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
            listen_address: state.listen_address.clone(),
        }
    }
}

/// Construct the axum Router for the daemon's TCP HTTP listener.
/// `shared-admin-http` owns the bind/serve glue.
pub fn build_router(state: HttpState) -> Router {
    Router::new()
        .route("/health", get(shared_health::health_handler))
        .route("/metrics", get(shared_telemetry::http::metrics_handler))
        .route("/sessions", get(shared_iscsi::http::sessions_handler))
        .route("/info", get(info_handler))
        .with_state(state)
}

/// Emit the per-route URL listing operators see in journalctl at boot.
/// `scheme` is "http" or "https" — picked by the caller from whether
/// `http.tls` is configured.
pub fn log_route_table(listen: &str, scheme: &str) {
    info!("HTTP server listening on {scheme}://{listen}");
    for route in ["health", "metrics", "sessions", "info"] {
        info!("  - {route}: {scheme}://{listen}/{route}");
    }
}

/// Handler for `/info` — read-only summary of the running daemon's
/// volume count + the configured iSCSI coordinates a host needs to
/// reach the target. Per-volume detail stays on the admin socket.
async fn info_handler(State(state): State<HttpState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "volume_count": state.registry.len(),
        "iqn": state.target_iqn,
        "listen_address": state.listen_address,
    }))
}
