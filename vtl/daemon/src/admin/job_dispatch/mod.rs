// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-kind handlers for `POST /api/v1/jobs/<kind>`.
//!
//! Each kind has its own file under this directory; [`dispatch`]
//! routes by name and spawns the right `run` function. The worker
//! emits events through [`JobEmitter`] and the HTTP layer picks them
//! up via the `GET /jobs/{id}/events` stream.
//!
//! Adding a new kind: drop a new module here exposing
//! `pub async fn run(emitter, body, state)`, then add a match arm
//! in [`dispatch`].

pub mod alerting;
pub mod archive;
pub mod gc;
pub mod migrate;
pub mod restore_archive;
pub mod self_test;
pub mod stats;
pub mod tiering;
pub mod verify;

use std::sync::Arc;

use crate::state::DaemonState;
use shared_admin_server::JobEmitter;

/// Spawn the worker task for `kind`. The body of the POST request is
/// parsed inside each kind module so the input type can vary.
/// Returns `Err(reason)` on a bad/unknown kind so the HTTP handler
/// can return 400 before any job is registered.
pub fn dispatch(
    kind: &str,
    body: serde_json::Value,
    emitter: JobEmitter,
    state: Arc<DaemonState>,
) -> Result<(), String> {
    match kind {
        "system.cloud_check" => {
            // Handler lifted to the shared crate so VSA mounts the same
            // job; the per-product input is just the storage config.
            let _ = body;
            tokio::spawn(shared_admin_cloud_check::run_cloud_check(
                emitter,
                Arc::clone(&state.storage_config),
            ));
        }
        "system.verify" => {
            tokio::spawn(verify::run(emitter, body, state));
        }
        "system.stats" => {
            tokio::spawn(stats::run(emitter, body, state));
        }
        "system.gc" => {
            tokio::spawn(gc::run(emitter, body, state));
        }
        "system.tiering.plan" => {
            tokio::spawn(tiering::run(emitter, body, state));
        }
        "system.tiering.run" => {
            tokio::spawn(tiering::run_apply(emitter, body, state));
        }
        "system.audit.tail" => {
            tokio::spawn(shared_admin_audit::run_tail(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
        }
        "system.audit.export" => {
            tokio::spawn(shared_admin_audit::run_export(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
        }
        "system.audit.verify" => {
            tokio::spawn(shared_admin_audit::run_verify(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
        }
        "system.audit.rotate" => {
            tokio::spawn(shared_admin_audit::run_rotate(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
        }
        "system.alerting.test" => {
            tokio::spawn(alerting::run(emitter, body, state));
        }
        "system.library.self_test" => {
            tokio::spawn(self_test::run_library(emitter, body, state));
        }
        "system.drive.self_test" => {
            tokio::spawn(self_test::run_drive(emitter, body, state));
        }
        "cartridge.migrate" => {
            tokio::spawn(migrate::run(emitter, body, state));
        }
        "cartridge.archive" => {
            tokio::spawn(archive::run(emitter, body, state));
        }
        "library.restore_archive" => {
            tokio::spawn(restore_archive::run(emitter, body, state));
        }
        "system.monitor" => {
            // The shared handler is generic over a `MonitorState` impl;
            // `AdminState` (which wraps the same `Arc<DaemonState>` we
            // already have) is the type that carries the impl. Building
            // it here keeps `shared-admin-monitor` from depending on
            // VTL's daemon crate.
            let admin_state = crate::admin::handlers::AdminState { daemon: state };
            tokio::spawn(shared_admin_monitor::run_monitor(
                emitter,
                body,
                admin_state,
            ));
        }
        other => return Err(format!("unknown job kind: {}", other)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::{AuditRateLimiter, ObjectStoreConfig};
    use scsi_smc::changer::ElementAddressConfig;
    use shared_admin_server::JobRegistry;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::{Mutex as TokioMutex, broadcast};

    use crate::state::{DaemonState, DaemonStateConfig};

    fn minimal_state(dir: &std::path::Path) -> Arc<DaemonState> {
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
            listen_addresses: vec!["0.0.0.0:3260".to_string()],
            event_tx,
            audit_log: None,
            audit_dir: dir.join("audit"),
            audit_ratelimiter: Arc::new(AuditRateLimiter::new(Duration::from_secs(60))),
            cloud_backends: Arc::new(TokioMutex::new(HashMap::new())),
            storage_config: Arc::new(ObjectStoreConfig::default()),
            tiering: Arc::new(core_mediachanger::TieringConfig::default()),
            keystore_config: Arc::new(shared_keystore::KeystoreYamlConfig::default()),
            num_drives: 1,
            drive_compression_algorithm: core_mediachanger::CompressionAlgo::Lz4,
            drive_compression_zstd_level: 3,
            pool_budgets: HashMap::new(),
            ghost_lists: HashMap::new(),
            backpressure_max_wait: Duration::from_secs(30),
        };
        Arc::new(DaemonState::new(cfg))
    }

    #[tokio::test]
    async fn dispatch_unknown_kind_returns_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = minimal_state(dir.path());
        let registry = JobRegistry::new();
        let (_id, _started, emitter) = registry.create("bogus.kind").await;
        let result = dispatch("bogus.kind", serde_json::json!({}), emitter, state);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap_or_default()
                .contains("unknown job kind")
        );
    }
}
