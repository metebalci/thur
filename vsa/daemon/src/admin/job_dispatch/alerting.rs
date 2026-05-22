// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.alerting.test` job — thin trampoline over the cross-
//! product [`shared_alerting::admin_job::run_test`] implementation.
//! The alerting dispatcher is process-global; the daemon's
//! `AdminState` carries no per-call context.

use shared_admin_server::JobEmitter;

use crate::admin::handlers::AdminState;

pub async fn run_test(emitter: JobEmitter, body: serde_json::Value, _state: AdminState) {
    shared_alerting::admin_job::run_test(emitter, body).await;
}
