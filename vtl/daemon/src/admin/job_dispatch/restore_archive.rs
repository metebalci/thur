// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `library.restore_archive` job — pull a frozen archive (produced
//! by `cartridge.archive`) back into a live cartridge.
//!
//! Drives [`core_mediachanger::library::restore_archive::run_restore_archive`].
//!
//! Body params: `{ "backend": "...", "barcode": "...",
//!                 "label": "...",
//!                 "as_barcode": null | "...",
//!                 "allow_existing": false,
//!                 "dry_run": false }`.
//!
//! Refuse-gates: backend named in cloud-backends.json, library
//! initialized (the primitive's `add_or_create_tape` call needs a
//! living `Library` mutex which only exists after `library init`),
//! free storage slot available.
//!
//! Audit: `library.restore_archive`.

use std::sync::Arc;

use core_mediachanger::library::restore_archive::{RestoreArchiveOptions, run_restore_archive};
use core_mediachanger::{AuditActor, AuditResult};
use serde::Deserialize;
use serde_json::json;
use shared_admin_server::{JobEmitter, JobEvent};

use crate::state::DaemonState;

#[derive(Debug, Deserialize)]
pub struct RestoreArchiveParams {
    pub backend: String,
    pub barcode: String,
    pub label: String,
    #[serde(default)]
    pub as_barcode: Option<String>,
    #[serde(default)]
    pub allow_existing: bool,
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn run(emitter: JobEmitter, body: serde_json::Value, state: Arc<DaemonState>) {
    let params: RestoreArchiveParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };

    let actor = AuditActor::cli("daemon".to_string());
    let op = "library.restore_archive";

    if let Err(reason) = preflight(&params, &state) {
        audit_failure(&state, op, actor.clone(), &params, &reason);
        emitter.emit(JobEvent::done_with_error(2, reason)).await;
        return;
    }

    // Build backend handle.
    let backend = match state
        .cloud_config
        .create_backend_named(&params.backend)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            let reason = format!("construct backend '{}': {}", params.backend, e);
            audit_failure(&state, op, actor, &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let forwarder = {
        let em = emitter.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                em.info(msg).await;
            }
        })
    };
    let progress_cb = move |msg: &str| {
        let _ = tx.send(msg.to_string());
    };

    let tapes_dir = state.data_dir.join("tapes");
    emitter
        .info(format!(
            "restore-archive backend={} archives/{}/{}/ -> {}",
            params.backend,
            params.barcode,
            params.label,
            params.as_barcode.as_deref().unwrap_or(&params.barcode),
        ))
        .await;

    // Run the primitive without holding the library mutex (mutex
    // guards aren't Send across awaits). Seat at the end as a
    // separate, short-held lock.
    let outcome = run_restore_archive(RestoreArchiveOptions {
        tapes_dir: &tapes_dir,
        backend: backend.as_ref(),
        backend_name: &params.backend,
        barcode: &params.barcode,
        label: &params.label,
        as_barcode: params.as_barcode.as_deref(),
        allow_existing: params.allow_existing,
        dry_run: params.dry_run,
        progress: Some(&progress_cb),
    })
    .await;

    drop(progress_cb);
    let _ = forwarder.await;

    match outcome {
        Ok(report) => {
            // Seat the restored cartridge into the library. Skipped
            // for dry-run and for the `allow_existing` skip case.
            // Lock is held only for the seat call — never across the
            // subsequent JobEmitter awaits (MutexGuard isn't Send).
            let seat_result: Result<Option<u32>, String> =
                if !report.dry_run && !report.skipped_existing {
                    match state.library.lock() {
                        Ok(mut lib) => {
                            match lib.add_or_create_tape(&report.local_barcode, &report.backend) {
                                Ok(slot) => Ok(Some(slot)),
                                Err(e) => Err(format!("seat into library: {}", e)),
                            }
                        }
                        Err(_) => Err("library mutex poisoned".to_string()),
                    }
                } else {
                    Ok(None)
                };
            let seated_slot = match seat_result {
                Ok(s) => s,
                Err(reason) => {
                    audit_failure(&state, op, actor, &params, &reason);
                    emitter.emit(JobEvent::done_with_error(1, reason)).await;
                    return;
                }
            };

            let summary = json!({
                "source_barcode": report.source_barcode,
                "local_barcode": report.local_barcode,
                "backend": report.backend,
                "label": report.label,
                "chunks_total": report.chunks_total,
                "chunks_downloaded": report.chunks_downloaded,
                "bytes_downloaded": report.bytes_downloaded,
                "index_files_downloaded": report.index_files_downloaded,
                "seated_in_slot": seated_slot,
                "skipped_existing": report.skipped_existing,
                "dry_run": report.dry_run,
            });
            if !report.dry_run
                && !report.skipped_existing
                && let Some(log) = state.audit_log.as_ref()
            {
                log.try_append(op, actor, summary.clone(), AuditResult::Ok);
            }
            emitter
                .info(format!(
                    "restore-archive complete: {} chunks ({} bytes), slot {:?}",
                    report.chunks_downloaded, report.bytes_downloaded, seated_slot
                ))
                .await;
            emitter.emit(JobEvent::result(summary)).await;
            emitter.emit(JobEvent::done(0)).await;
        }
        Err(e) => {
            let reason = e.to_string();
            audit_failure(&state, op, actor, &params, &reason);
            emitter.emit(JobEvent::done_with_error(1, reason)).await;
        }
    }
}

fn preflight(params: &RestoreArchiveParams, state: &DaemonState) -> Result<(), String> {
    if params.backend.is_empty() {
        return Err("backend must be non-empty".to_string());
    }
    if params.barcode.is_empty() {
        return Err("barcode must be non-empty".to_string());
    }
    if params.label.is_empty() {
        return Err("label must be non-empty".to_string());
    }
    let names = state.cloud_config.backend_names();
    if !names.iter().any(|n| n == &params.backend) {
        return Err(format!(
            "backend '{}' not defined under `cloud.backends:` in YAML conffile (known: {})",
            params.backend,
            names.join(", ")
        ));
    }
    Ok(())
}

fn audit_failure(
    state: &DaemonState,
    op: &str,
    actor: AuditActor,
    params: &RestoreArchiveParams,
    reason: &str,
) {
    if let Some(log) = state.audit_log.as_ref() {
        log.try_append(
            op,
            actor,
            json!({
                "backend": params.backend,
                "barcode": params.barcode,
                "label": params.label,
                "as_barcode": params.as_barcode,
                "allow_existing": params.allow_existing,
                "dry_run": params.dry_run,
            }),
            AuditResult::Error(reason.to_string()),
        );
    }
}
