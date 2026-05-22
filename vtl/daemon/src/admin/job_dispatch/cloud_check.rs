// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.cloud_check` job — verify reachability/auth on every
//! configured cloud backend.
//!
//! Body: `{}` (no parameters — backends come from the daemon's
//! already-loaded `cloud_config`).

use std::sync::Arc;

use crate::state::DaemonState;
use core_mediachanger::{CloudCheckStep, validate_cloud_backend};
use shared_admin_server::{JobEmitter, JobEvent};

pub async fn run(emitter: JobEmitter, _body: serde_json::Value, state: Arc<DaemonState>) {
    let cfg = state.cloud_config.as_ref();
    let names = cfg.backend_names();

    emitter
        .info(format!(
            "Configured backends to check: {}",
            names.join(", ")
        ))
        .await;
    emitter.info("").await;

    let mut failed: Vec<String> = Vec::new();

    for name in &names {
        emitter.info(format!("=== Backend: {} ===", name)).await;
        if let Some(target) = cfg.target_label_named(name)
            && !target.is_empty()
        {
            emitter.info(format!("Target: {}", target)).await;
        }

        // Bridge the sync `validate_cloud_backend` step callback into
        // async event emission. The callback is sync; we collect into
        // a Vec then ship the lines after each backend completes.
        // (Cloud check steps are a handful per backend — buffering is
        // fine. If a future kind has thousands of sync callbacks
        // we'd want a sync->async bridge instead.)
        let mut steps: Vec<CloudCheckStep> = Vec::new();
        let result = validate_cloud_backend(cfg, name, |step| {
            steps.push(step);
        })
        .await;

        for s in &steps {
            emitter
                .info(format!("  [PASS] {:<6} {}", s.name, s.detail))
                .await;
        }

        match result {
            Ok(()) => {
                emitter.info("  Result: PASS").await;
                shared_alerting::record::backend_reachability(name, "recovery", None);
            }
            Err(e) => {
                let kind_label = e.kind().label();
                let step_label = e.step();
                shared_alerting::record::backend_reachability(
                    name,
                    "failure",
                    Some(&e.to_string()),
                );
                emitter
                    .info(format!("  [FAIL] {:<6} {}", step_label, kind_label))
                    .await;
                emitter
                    .info(format!("  Diagnosis: {}", e.kind().diagnosis()))
                    .await;
                emitter.info("  Hints to check:").await;
                for line in e.kind().hints().lines() {
                    emitter.info(format!("    {}", line.trim_start())).await;
                }
                emitter.info("  Raw error:").await;
                emitter.info(format!("    {}", e)).await;
                // Materialize the source chain into owned strings
                // before any await — `&dyn Error` is `!Send`, so we
                // can't hold one across an await boundary in a future
                // that tokio::spawn requires to be Send.
                let mut chain: Vec<String> = Vec::new();
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    chain.push(s.to_string());
                    src = s.source();
                }
                for line in chain {
                    emitter.info(format!("    caused by: {}", line)).await;
                }
                failed.push(name.clone());
            }
        }
        emitter.info("").await;
    }

    if failed.is_empty() {
        emitter
            .info(format!(
                "Cloud check passed for all configured backends ({}).",
                names.join(", ")
            ))
            .await;
        emitter
            .emit(JobEvent::result(serde_json::json!({
                "passed": names,
                "failed": Vec::<String>::new(),
            })))
            .await;
        emitter.emit(JobEvent::done(0)).await;
    } else {
        let summary = format!(
            "Cloud check FAILED for: {}  (passed: {}/{})",
            failed.join(", "),
            names.len() - failed.len(),
            names.len(),
        );
        emitter.error(summary.clone()).await;
        emitter
            .emit(JobEvent::result(serde_json::json!({
                "passed": names.iter().filter(|n| !failed.contains(n)).cloned().collect::<Vec<_>>(),
                "failed": failed,
            })))
            .await;
        emitter.emit(JobEvent::done_with_error(1, summary)).await;
    }
}
