// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.alerting.test` job — thin trampoline over the cross-
//! product [`shared_alerting::admin_job::run_test`] implementation.

use std::sync::Arc;

use shared_admin_server::JobEmitter;

use crate::state::DaemonState;

pub async fn run(emitter: JobEmitter, body: serde_json::Value, _state: Arc<DaemonState>) {
    shared_alerting::admin_job::run_test(emitter, body).await;
}
