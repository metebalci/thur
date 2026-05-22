// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-kind handlers for `POST /api/v1/jobs/<kind>` on thurvsad.
//!
//! Mirrors `vtl/daemon/src/admin/job_dispatch/mod.rs`. Each kind has
//! its own file under this directory; [`dispatch`] routes by name and
//! spawns the right `run` function. The worker emits events through
//! [`JobEmitter`] and the HTTP layer picks them up via the
//! `GET /jobs/{id}/events` stream.
//!
//! Adding a new kind: drop a new module here exposing
//! `pub async fn run(emitter, body, state)`, then add a match arm in
//! [`dispatch`].

pub mod alerting;
pub mod gc;
pub mod stats;
pub mod verify;

use shared_admin_server::JobEmitter;

use crate::admin::handlers::AdminState;

/// Spawn the worker task for `kind`. The body of the POST request is
/// parsed inside each kind module. Returns `Err(reason)` on an
/// unknown kind so the HTTP handler can return 400 before any job is
/// registered.
pub fn dispatch(
    kind: &str,
    body: serde_json::Value,
    emitter: JobEmitter,
    state: AdminState,
) -> Result<(), String> {
    match kind {
        "system.alerting.test" => {
            tokio::spawn(alerting::run_test(emitter, body, state));
            Ok(())
        }
        "system.gc" => {
            tokio::spawn(gc::run(emitter, body, state));
            Ok(())
        }
        "system.stats" => {
            tokio::spawn(stats::run(emitter, body, state));
            Ok(())
        }
        "system.verify" => {
            tokio::spawn(verify::run(emitter, body, state));
            Ok(())
        }
        "system.audit.tail" => {
            tokio::spawn(shared_admin_audit::run_tail(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
            Ok(())
        }
        "system.audit.export" => {
            tokio::spawn(shared_admin_audit::run_export(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
            Ok(())
        }
        "system.audit.verify" => {
            tokio::spawn(shared_admin_audit::run_verify(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
            Ok(())
        }
        "system.audit.rotate" => {
            tokio::spawn(shared_admin_audit::run_rotate(
                emitter,
                body,
                state.audit_dir.clone(),
            ));
            Ok(())
        }
        other => Err(format!("unknown job kind: {}", other)),
    }
}
