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
pub mod cloud_check;
pub mod gc;
pub mod migrate;
pub mod restore_archive;
pub mod self_test;
pub mod stats;
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
            tokio::spawn(cloud_check::run(emitter, body, state));
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
        other => return Err(format!("unknown job kind: {}", other)),
    }
    Ok(())
}
