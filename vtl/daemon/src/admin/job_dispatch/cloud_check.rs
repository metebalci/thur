// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.cloud_check` job — verify reachability/auth on every
//! configured cloud backend.
//!
//! Body: `{}` (no parameters — backends come from the daemon's
//! already-loaded `storage_config`).

use std::sync::Arc;

use crate::state::DaemonState;
use core_mediachanger::{ObjectStoreCheckStep, validate_object_store_backend};
use shared_admin_server::{JobEmitter, JobEvent};

pub async fn run(emitter: JobEmitter, _body: serde_json::Value, state: Arc<DaemonState>) {
    let cfg = state.storage_config.as_ref();
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

        // Bridge the sync `validate_object_store_backend` step callback into
        // async event emission. The callback is sync; we collect into
        // a Vec then ship the lines after each backend completes.
        // (Cloud check steps are a handful per backend — buffering is
        // fine. If a future kind has thousands of sync callbacks
        // we'd want a sync->async bridge instead.)
        let mut steps: Vec<ObjectStoreCheckStep> = Vec::new();
        let result = validate_object_store_backend(cfg, name, |step| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DaemonState, DaemonStateConfig};
    use core_mediachanger::{AuditRateLimiter, ObjectStoreConfig};
    use scsi_smc::changer::ElementAddressConfig;
    use shared_admin_server::JobRegistry;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{Mutex as TokioMutex, broadcast};

    /// Build a `DaemonState` with an empty `ObjectStoreConfig` — the cloud
    /// check then iterates zero backends and takes the all-passed
    /// branch.
    fn empty_state(dir: &std::path::Path) -> Arc<DaemonState> {
        let lib_root = dir.join("library");
        let tapes_root = dir.join("tapes");
        let library = core_mediachanger::Library::initialize(
            &lib_root,
            &tapes_root,
            5,
            0,
            1,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .expect("init library");
        let (event_tx, _rx) = broadcast::channel(8);
        let cfg = DaemonStateConfig {
            data_dir: dir.to_path_buf(),
            tapes_root,
            library: Arc::new(Mutex::new(library)),
            element_config: ElementAddressConfig::new(0, 1001, 5, 101, 0, 1, 1),
            target_iqn: "iqn.2025-10.com.metebalci:thurvtl".to_string(),
            listen_address: "0.0.0.0:3260".to_string(),
            event_tx,
            audit_log: None,
            audit_dir: dir.join("audit"),
            audit_ratelimiter: Arc::new(AuditRateLimiter::new(Duration::from_secs(60))),
            cloud_backends: Arc::new(TokioMutex::new(HashMap::new())),
            storage_config: Arc::new(ObjectStoreConfig::default()),
            keystore_config: Arc::new(shared_keystore::KeystoreYamlConfig::default()),
            num_drives: 1,
            drive_compression_algorithm: core_mediachanger::CompressionAlgo::Lz4,
            drive_compression_zstd_level: 3,
            pool_budgets: HashMap::new(),
            backpressure_max_wait: Duration::from_secs(30),
        };
        Arc::new(DaemonState::new(cfg))
    }

    #[tokio::test]
    async fn cloud_check_passes_with_no_backends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = empty_state(dir.path());
        let registry = JobRegistry::new();
        let (_id, _started, emitter) = registry.create("system.cloud_check").await;
        run(emitter, serde_json::json!({}), state).await;
        // The job reaches a terminal Done event without panicking.
    }
}
