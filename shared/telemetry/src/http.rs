// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Axum handler for `GET /metrics` — Prometheus text-format render of
//! the current OTel snapshot. Daemons mount this on their TCP HTTP
//! server; the per-product router pulls `Arc<Telemetry>` from its own
//! `HttpState` via `axum::extract::FromRef`.

use std::sync::Arc;

use axum::{extract::State, http::header::CONTENT_TYPE, response::IntoResponse};

use crate::Telemetry;

pub async fn metrics_handler(State(t): State<Arc<Telemetry>>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        t.export_prometheus(),
    )
}
