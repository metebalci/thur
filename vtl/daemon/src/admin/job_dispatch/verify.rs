// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.verify` job — library-wide consistency check.
//!
//! Walks library/inventory, every cartridge's manifest /
//! `chunks.idx` / `blocks-p<N>.idx`, then sweeps each per-backend
//! pool. Optional cloud HEAD pass (default on; `skip_cloud=true`
//! skips it). Emits the full structured `VerifyReport` as a Result
//! event so the CLI can render verbose / json variants without re-
//! contacting the daemon.

use std::sync::Arc;

use crate::state::DaemonState;
use core_mediachanger::verify::{VerifyScope, verify_local, verify_with_cloud};
use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};

#[derive(Debug, Deserialize, Default)]
pub struct VerifyParams {
    #[serde(default)]
    pub skip_cloud: bool,
    #[serde(default)]
    pub barcodes: Vec<String>,
}

pub async fn run(emitter: JobEmitter, body: serde_json::Value, state: Arc<DaemonState>) {
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
        barcodes: params.barcodes,
    };

    // Verify is a sync walker that can chew on lots of disk. Run it
    // on the blocking thread pool so the runtime stays responsive
    // for other admin requests + iSCSI traffic.
    let data_dir = state.data_dir.clone();

    if params.skip_cloud {
        emitter
            .info(format!(
                "Verifying library at {} (cloud sweep skipped)",
                data_dir.display()
            ))
            .await;
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
        let cloud_cfg = state.cloud_config.as_ref().clone();
        emitter
            .info(format!(
                "Verifying library at {} (cloud HEAD sweep enabled)",
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

async fn finish(emitter: &JobEmitter, report: core_mediachanger::verify::VerifyReport) {
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
    fn verify_params_default() {
        let p = VerifyParams::default();
        assert!(!p.skip_cloud);
        assert!(p.barcodes.is_empty());
    }

    #[test]
    fn verify_params_empty_json_uses_defaults() {
        let p: VerifyParams = serde_json::from_value(serde_json::json!({})).expect("empty body");
        assert!(!p.skip_cloud);
        assert!(p.barcodes.is_empty());
    }

    #[test]
    fn verify_params_parses_skip_cloud_and_barcodes() {
        let p: VerifyParams =
            serde_json::from_value(serde_json::json!({"skip_cloud": true, "barcodes": ["A", "B"]}))
                .expect("explicit body");
        assert!(p.skip_cloud);
        assert_eq!(p.barcodes, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn verify_params_rejects_non_array_barcodes() {
        assert!(
            serde_json::from_value::<VerifyParams>(serde_json::json!({"barcodes": "A"})).is_err()
        );
    }
}
