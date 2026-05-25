// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.verify` job — volume-wide consistency check.
//!
//! Block-side parallel of `vtl/daemon/src/admin/job_dispatch/verify.rs`.
//! Walks every volume's `pages.idx` (integrity + chunk presence), then
//! sweeps each per-backend pool. Optional cloud HEAD pass (default on;
//! `skip_cloud=true` skips it). Emits the structured
//! `VolumeVerifyReport` as a Result event so the CLI can render
//! verbose / json variants without re-contacting the daemon.

use core_block::verify::{VerifyScope, VolumeVerifyReport, verify_local, verify_with_cloud};
use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};

use crate::admin::handlers::AdminState;

#[derive(Debug, Deserialize, Default)]
pub struct VerifyParams {
    #[serde(default)]
    pub skip_storage: bool,
    #[serde(default)]
    pub volumes: Vec<String>,
}

pub async fn run(emitter: JobEmitter, body: serde_json::Value, state: AdminState) {
    let params: VerifyParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };

    let scope = VerifyScope {
        volumes: params.volumes,
    };
    let data_dir = state.data_dir.clone();

    if params.skip_storage {
        emitter
            .info(format!(
                "Verifying volumes at {} (cloud sweep skipped)",
                data_dir.display()
            ))
            .await;
        // verify_local is a sync walker — run it off the async pool.
        let report =
            match tokio::task::spawn_blocking(move || verify_local(&data_dir, &scope)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    emitter
                        .emit(JobEvent::done_with_error(
                            2,
                            format!("verify failed: {}", e),
                        ))
                        .await;
                    return;
                }
                Err(e) => {
                    emitter
                        .emit(JobEvent::done_with_error(
                            2,
                            format!("verify panicked: {}", e),
                        ))
                        .await;
                    return;
                }
            };
        finish(&emitter, report).await;
    } else {
        let cloud_cfg = state.storage.as_ref().clone();
        emitter
            .info(format!(
                "Verifying volumes at {} (cloud HEAD sweep enabled)",
                data_dir.display()
            ))
            .await;
        let report = match verify_with_cloud(&data_dir, &scope, &cloud_cfg).await {
            Ok(r) => r,
            Err(e) => {
                emitter
                    .emit(JobEvent::done_with_error(
                        2,
                        format!("verify failed: {}", e),
                    ))
                    .await;
                return;
            }
        };
        finish(&emitter, report).await;
    }
}

async fn finish(emitter: &JobEmitter, report: VolumeVerifyReport) {
    let errors = report.error_count();
    let warnings = report.warning_count();
    let hints = report.gc_hint_count();
    emitter
        .info(format!(
            "verify complete: {} error(s), {} warning(s), {} GC hint(s)",
            errors, warnings, hints
        ))
        .await;
    let report_json = match serde_json::to_value(&report) {
        Ok(v) => v,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("serialize report: {}", e),
                ))
                .await;
            return;
        }
    };
    emitter.emit(JobEvent::result(report_json)).await;
    let exit = if errors > 0 { 1 } else { 0 };
    emitter.emit(JobEvent::done(exit)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_params_default_when_body_empty() {
        let params: VerifyParams =
            serde_json::from_value(serde_json::json!({})).expect("empty body deserializes");
        assert!(!params.skip_storage);
        assert!(params.volumes.is_empty());
    }

    #[test]
    fn verify_params_round_trip_full_body() {
        let params: VerifyParams = serde_json::from_value(serde_json::json!({
            "skip_storage": true,
            "volumes": ["vol-a", "vol-b"],
        }))
        .expect("full body deserializes");
        assert!(params.skip_storage);
        assert_eq!(params.volumes, vec!["vol-a", "vol-b"]);
    }

    #[test]
    fn verify_params_default_impl_matches_empty_body() {
        let from_default = VerifyParams::default();
        assert!(!from_default.skip_storage);
        assert!(from_default.volumes.is_empty());
    }

    #[test]
    fn verify_params_rejects_wrong_type() {
        // `skip_cloud` is a bool — a string must fail to deserialize.
        let bad = serde_json::from_value::<VerifyParams>(serde_json::json!({
            "skip_storage": "yes",
        }));
        assert!(bad.is_err());
    }
}
