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

use std::future::Future;
use std::pin::Pin;

use shared_admin_server::JobEmitter;

use crate::admin::handlers::AdminState;

/// Spawn the worker task for `kind`. The body of the POST request is
/// parsed inside each kind module. Returns `Err(reason)` on an
/// unknown kind so the HTTP handler can return 400 before any job is
/// registered.
///
/// Every worker runs under [`JobEmitter::spawn_supervised`] so a panic
/// or early return without a terminal event still drives the job to a
/// terminal state, instead of hanging the CLI stream forever and leaking
/// the job past reap (issue #206).
pub fn dispatch(
    kind: &str,
    body: serde_json::Value,
    emitter: JobEmitter,
    state: AdminState,
) -> Result<(), String> {
    let supervisor = emitter.clone();
    let worker: Pin<Box<dyn Future<Output = ()> + Send>> = match kind {
        "system.alerting.test" => Box::pin(alerting::run_test(emitter, body, state)),
        "system.storage_check" => {
            // Same shared handler VTL mounts; the per-product input is
            // just the parsed storage config.
            let _ = body;
            Box::pin(shared_admin_storage_check::run_storage_check(
                emitter,
                std::sync::Arc::clone(&state.storage),
            ))
        }
        "system.gc" => Box::pin(gc::run(emitter, body, state)),
        "system.stats" => Box::pin(stats::run(emitter, body, state)),
        "system.verify" => Box::pin(verify::run(emitter, body, state)),
        "system.audit.tail" => Box::pin(shared_admin_audit::run_tail(
            emitter,
            body,
            state.audit_dir.clone(),
        )),
        "system.audit.export" => Box::pin(shared_admin_audit::run_export(
            emitter,
            body,
            state.audit_dir.clone(),
        )),
        "system.audit.verify" => Box::pin(shared_admin_audit::run_verify(
            emitter,
            body,
            state.audit_dir.clone(),
        )),
        "system.audit.rotate" => Box::pin(shared_admin_audit::run_rotate(
            emitter,
            body,
            state.audit_dir.clone(),
        )),
        "system.monitor" => {
            // AdminState already impls `MonitorState`; spawn directly.
            Box::pin(shared_admin_monitor::run_monitor(emitter, body, state))
        }
        other => return Err(format!("unknown job kind: {}", other)),
    };
    supervisor.spawn_supervised(worker);
    Ok(())
}
