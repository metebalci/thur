// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// HTTP Server Module (Phase 6: Unified HTTP Server)
//
// Provides a single HTTP server with essential endpoints:
// - /health   - liveness probe (shared crate)
// - /metrics  - Prometheus metrics (shared crate)
// - /sessions - iSCSI sessions (shared crate)
// - /info     - VTL-specific library topology snapshot
//
// Per-drive state is operator-diagnostic, not scrape-friendly, so it
// stays on the peer-cred-authed admin socket (`thurvtl drive
// status`) rather than the unauthenticated TCP listener.
//
// Bind / TLS / serve plumbing lives in `shared-admin-http`. This
// module only owns the per-product Router + state composition.

use axum::{
    Json, Router,
    extract::{FromRef, State},
    response::IntoResponse,
    routing::get,
};
use core_mediachanger::Telemetry;
use shared_health::HealthMeta;
use shared_iscsi::http::SessionsState;
use std::sync::Arc;
use tracing::info;

use crate::state::DaemonState;

/// Shared state for HTTP handlers
#[derive(Clone)]
pub struct HttpState {
    pub metrics: Arc<Telemetry>,
    pub daemon_state: Arc<DaemonState>,
}

// Lets the shared `/metrics` handler (in `shared_telemetry::http`)
// extract its `Arc<Telemetry>` from our composite state.
impl FromRef<HttpState> for Arc<Telemetry> {
    fn from_ref(state: &HttpState) -> Self {
        state.metrics.clone()
    }
}

// Lets the shared `/health` handler (in `shared_health`) extract its
// per-product identity from our composite state.
impl FromRef<HttpState> for HealthMeta {
    fn from_ref(_state: &HttpState) -> Self {
        HealthMeta {
            product: &shared_naming::TAPE_LIBRARY,
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

// Lets the shared `/sessions` handler (in `shared_iscsi::http`)
// extract the session-manager + target-coordinates bundle.
impl FromRef<HttpState> for SessionsState {
    fn from_ref(state: &HttpState) -> Self {
        SessionsState {
            sessions: state.daemon_state.session_manager.clone(),
            target_iqn: state.daemon_state.target_iqn.clone(),
            listen_address: state.daemon_state.listen_address.clone(),
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

/// Handler for `/info` — read-only snapshot of library topology
/// (chassis-level slot / drive counts, LTO generation, partition
/// names, chassis serial). No CHAP / cloud creds; sensitive
/// per-partition inventory stays on the admin socket.
async fn info_handler(State(state): State<HttpState>) -> impl IntoResponse {
    let library = state
        .daemon_state
        .library
        .lock()
        .expect("library mutex poisoned");
    let partition_names: Vec<&str> = library
        .partitions()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    Json(serde_json::json!({
        "slots": {
            "storage": library.storage_slots().len(),
            "mail": library.mail_slots().len(),
        },
        "drives": library.drives().len(),
        "lto_generation": library.lto_generation(),
        "partitions": partition_names,
        "chassis_serial": library.chassis_serial(),
    }))
}
