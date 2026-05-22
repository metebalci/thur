// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! HTTP surface for alerting introspection.
//!
//! One endpoint today: `GET /api/v1/system/alerting` returns the
//! configured dispatcher state (enabled, dedup window, sink list).
//! Mounted by both daemons on their admin Unix socket router. Gated
//! behind the optional `http` cargo feature so non-daemon consumers
//! (CLI, build.rs, tests) don't pay the axum compile cost.

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::json;

/// Empty state marker. The handler reads from the process-global
/// dispatcher, so it carries no per-request context.
#[derive(Clone, Copy)]
pub struct AlertingHttpState;

pub async fn alerting_list(State(_): State<AlertingHttpState>) -> impl IntoResponse {
    let Some(d) = crate::global() else {
        return Json(json!({
            "enabled": false,
            "dedup_window_seconds": 0,
            "sinks": Vec::<serde_json::Value>::new(),
        }));
    };
    // The dispatcher doesn't carry the sink-type tag back — for v1
    // we just label everything "configured". A follow-up could extend
    // `AlertingDispatcher::sink_specs()` if operators need the type
    // distinction without re-reading the YAML.
    let sinks: Vec<serde_json::Value> = d
        .sink_names()
        .into_iter()
        .map(|n| json!({ "name": n, "type": "configured" }))
        .collect();
    Json(json!({
        "enabled": true,
        "dedup_window_seconds": d.dedup_window_seconds(),
        "sinks": sinks,
    }))
}
