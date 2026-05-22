// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.drive.self_test` and `system.library.self_test` jobs —
//! operator-initiated SPC-4 self-tests.
//!
//! The same diagnostic functions iSCSI's SEND DIAGNOSTIC handler
//! invokes (`diagnostics::run_library_diagnostic` /
//! `run_drive_diagnostic`). Routing through the admin API gives
//! operators a way to trigger and read back the result without an
//! iSCSI initiator on the host, and stamps the same
//! `DiagnosticStore` ring so a subsequent host RECEIVE DIAGNOSTIC
//! RESULTS sees the CLI-issued probe as the latest entry.

use std::sync::Arc;

use crate::diagnostics::{DiagnosticEntry, run_drive_diagnostic, run_library_diagnostic};
use crate::state::DaemonState;
use core_mediachanger::{AuditActor, AuditResult};
use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};

#[derive(Debug, Deserialize)]
pub struct DriveSelfTestParams {
    pub drive: u32,
}

pub async fn run_library(emitter: JobEmitter, _body: serde_json::Value, state: Arc<DaemonState>) {
    emitter.info("library self-test: starting").await;
    let entry = run_library_diagnostic(&state.data_dir, &state.cloud_config).await;
    state.diagnostic_store.record(0, entry.clone());

    audit(&state, "library.self_test", None, &entry);
    finish_self_test(&emitter, "library", None, entry).await;
}

pub async fn run_drive(emitter: JobEmitter, body: serde_json::Value, state: Arc<DaemonState>) {
    let params: DriveSelfTestParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };
    emitter
        .info(format!("drive {} self-test: starting", params.drive))
        .await;

    // run_drive_diagnostic is sync (per-drive Mutex + sync fs reads);
    // dispatch on the blocking pool so the runtime stays responsive.
    let dm = Arc::clone(&state.drive_manager);
    let tapes_root = state.data_dir.join("tapes");
    let drive_id = params.drive as usize;
    let entry =
        match tokio::task::spawn_blocking(move || run_drive_diagnostic(&dm, drive_id, &tapes_root))
            .await
        {
            Ok(e) => e,
            Err(join_err) => {
                DiagnosticEntry::fail(format!("drive diagnostic task panicked: {}", join_err))
            }
        };

    let lun = (params.drive as u8).saturating_add(1);
    state.diagnostic_store.record(lun, entry.clone());

    audit(&state, "drive.self_test", Some(params.drive), &entry);
    finish_self_test(&emitter, "drive", Some(params.drive), entry).await;
}

fn audit(state: &DaemonState, op: &str, drive: Option<u32>, entry: &DiagnosticEntry) {
    let Some(log) = state.audit_log.as_ref() else {
        return;
    };
    let actor = AuditActor::cli("daemon".to_string());
    let mut params = serde_json::json!({
        "passed": entry.passed,
    });
    if let Some(d) = drive {
        params["drive"] = serde_json::Value::from(d);
    }
    if !entry.passed {
        params["sense_key"] = serde_json::Value::from(entry.sense_key);
        params["asc"] = serde_json::Value::from(entry.asc);
        params["ascq"] = serde_json::Value::from(entry.ascq);
    }
    let result = if entry.passed {
        AuditResult::Ok
    } else {
        AuditResult::Error(entry.detail.clone())
    };
    log.try_append(op, actor, params, result);
}

async fn finish_self_test(
    emitter: &JobEmitter,
    target: &str,
    drive: Option<u32>,
    entry: DiagnosticEntry,
) {
    if entry.passed {
        let label = match drive {
            Some(d) => format!("drive {} self-test: PASS", d),
            None => format!("{} self-test: PASS", target),
        };
        emitter.info(label).await;
        emitter
            .emit(JobEvent::result(serde_json::json!({
                "passed": true,
                "target": target,
                "drive": drive,
                "timestamp": entry.timestamp.to_rfc3339(),
            })))
            .await;
        emitter.emit(JobEvent::done(0)).await;
    } else {
        let label = match drive {
            Some(d) => format!("drive {} self-test: FAIL — {}", d, entry.detail),
            None => format!("{} self-test: FAIL — {}", target, entry.detail),
        };
        emitter.error(label).await;
        emitter
            .emit(JobEvent::result(serde_json::json!({
                "passed": false,
                "target": target,
                "drive": drive,
                "detail": entry.detail,
                "sense_key": entry.sense_key,
                "asc": entry.asc,
                "ascq": entry.ascq,
                "timestamp": entry.timestamp.to_rfc3339(),
            })))
            .await;
        emitter.emit(JobEvent::done(1)).await;
    }
}
