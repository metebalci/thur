// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product `system.alerting.test` admin job.
//!
//! Body params: `{ "sink": "<name>", "severity": "<info|warn|error>" }`.
//!
//! Looks up the process-global dispatcher, constructs a synthetic
//! [`Alert`] flagged as a test, and ships it through the named sink
//! only. Bypasses the rate limiter so two operator-invoked tests in
//! a row both go out.

use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};

use crate::alert::{Alert, AlertClass, Severity};

#[derive(Debug, Deserialize)]
pub struct TestParams {
    pub sink: String,
    #[serde(default = "default_severity")]
    pub severity: String,
}

fn default_severity() -> String {
    "warn".to_string()
}

pub async fn run_test(emitter: JobEmitter, body: serde_json::Value) {
    let params: TestParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {e}")))
                .await;
            return;
        }
    };

    let Some(dispatcher) = crate::global() else {
        emitter
            .emit(JobEvent::done_with_error(
                2,
                "alerting is disabled — set `alerting.enabled: true` in the daemon config and restart",
            ))
            .await;
        return;
    };

    let severity = match params.severity.as_str() {
        "info" => Severity::Info,
        "warn" => Severity::Warn,
        "error" => Severity::Error,
        other => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("unknown severity '{other}' (expected info | warn | error)"),
                ))
                .await;
            return;
        }
    };

    let mut fields = serde_json::Map::new();
    fields.insert("test".to_string(), serde_json::Value::Bool(true));
    let alert = Alert::new(
        AlertClass::AuditFailure, // synthetic; class is ignored by send_test
        severity,
        format!(
            "Test alert from {} ({})",
            dispatcher.product().display_name,
            dispatcher.product().name,
        ),
        fields,
        format!("test:{}", params.sink),
    );

    emitter
        .info(format!(
            "Sending test alert through sink '{}' ({}) ...",
            params.sink, params.severity,
        ))
        .await;
    match dispatcher.send_test(&params.sink, alert).await {
        Ok(()) => {
            emitter
                .emit(JobEvent::result(serde_json::json!({
                    "sink": params.sink,
                    "outcome": "success",
                })))
                .await;
            emitter
                .info(format!(
                    "OK: sink '{}' accepted the test alert",
                    params.sink
                ))
                .await;
            emitter.emit(JobEvent::done(0)).await;
        }
        Err(e) => {
            emitter
                .emit(JobEvent::result(serde_json::json!({
                    "sink": params.sink,
                    "outcome": "failure",
                    "error": e.to_string(),
                })))
                .await;
            emitter
                .emit(JobEvent::done_with_error(
                    1,
                    format!("sink '{}' send failed: {e}", params.sink),
                ))
                .await;
        }
    }
}
