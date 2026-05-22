// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `cartridge.archive` job — snapshot a cartridge to a different
//! cloud backend as a frozen, self-contained blob.
//!
//! Drives [`core_mediachanger::cartridge_archive::run_archive`].
//!
//! Body params: `{ "barcode": "...", "target_backend": "...",
//!                 "label": "...",
//!                 "dry_run": false }`.
//!
//! Refuse-gates: target backend named in cloud-backends.json,
//! source ≠ target (the archive prefix would collide with the live
//! cartridge's manifest backups under shared backend), cartridge not
//! loaded in a drive, target's `retention_mode` matches WORM state
//! if the cartridge is WORM.
//!
//! Audit: `cartridge.archived` with provenance + counts.

use std::sync::Arc;

use core_mediachanger::cartridge_archive::{ArchiveOptions, run_archive};
use core_mediachanger::legal_hold::find_drive_for_loaded_cartridge;
use core_mediachanger::{AuditActor, AuditResult};
use serde::Deserialize;
use serde_json::json;
use shared_admin_server::{JobEmitter, JobEvent};

use crate::state::DaemonState;

#[derive(Debug, Deserialize)]
pub struct ArchiveParams {
    pub barcode: String,
    pub target_backend: String,
    pub label: String,
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn run(emitter: JobEmitter, body: serde_json::Value, state: Arc<DaemonState>) {
    let params: ArchiveParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };

    let actor = AuditActor::cli("daemon".to_string());
    let op = "cartridge.archived";

    if let Err(reason) = preflight(&params, &state).await {
        audit_failure(&state, op, actor.clone(), &params, &reason);
        emitter.emit(JobEvent::done_with_error(2, reason)).await;
        return;
    }

    // Read source backend + worm from manifest.
    let manifest_path = state
        .data_dir
        .join("tapes")
        .join(&params.barcode)
        .join("manifest.json");
    let manifest_str = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            let reason = format!("read manifest for '{}': {}", params.barcode, e);
            audit_failure(&state, op, actor, &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };
    let (source_backend, is_worm) = match parse_backend_and_worm(&manifest_str) {
        Ok(v) => v,
        Err(reason) => {
            audit_failure(&state, op, actor, &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };

    if source_backend == params.target_backend {
        let reason = "source and target backend must differ — \
                      archive prefix would collide with live manifest backups"
            .to_string();
        audit_failure(&state, op, actor, &params, &reason);
        emitter.emit(JobEvent::done_with_error(2, reason)).await;
        return;
    }

    if is_worm {
        let mode = state
            .cloud_config
            .retention_mode_named(&params.target_backend);
        if !mode.requires_lock() {
            let reason = format!(
                "WORM cartridge cannot archive to backend '{}' (retention_mode={}); \
                 target must be governance or compliance",
                params.target_backend,
                mode.label()
            );
            audit_failure(&state, op, actor, &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    }

    // Construct backend handles. Source for chunk-fetch fallback,
    // target for the archive PUTs.
    let source = match state
        .cloud_config
        .create_backend_named(&source_backend)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            let reason = format!("construct source backend '{}': {}", source_backend, e);
            audit_failure(&state, op, actor, &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };
    let target = match state
        .cloud_config
        .create_backend_named(&params.target_backend)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            let reason = format!(
                "construct target backend '{}': {}",
                params.target_backend, e
            );
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
            "archive {} ({}) -> {}: archives/{}/{}/",
            params.barcode, source_backend, params.target_backend, params.barcode, params.label
        ))
        .await;

    let outcome = run_archive(ArchiveOptions {
        tapes_dir: &tapes_dir,
        barcode: &params.barcode,
        source: source.as_ref(),
        target: target.as_ref(),
        target_name: &params.target_backend,
        label: &params.label,
        dry_run: params.dry_run,
        progress: Some(&progress_cb),
    })
    .await;

    drop(progress_cb);
    let _ = forwarder.await;

    match outcome {
        Ok(report) => {
            let summary = json!({
                "barcode": report.barcode,
                "from_backend": report.from_backend,
                "to_backend": report.to_backend,
                "label": report.label,
                "archived_at": report.archived_at,
                "chunks_total": report.chunks_total,
                "chunks_uploaded": report.chunks_uploaded,
                "chunks_from_local_pool": report.chunks_from_local_pool,
                "chunks_from_source_cloud": report.chunks_from_source_cloud,
                "bytes_uploaded": report.bytes_uploaded,
                "index_files_uploaded": report.index_files_uploaded,
                "dry_run": report.dry_run,
            });
            if !report.dry_run
                && let Some(log) = state.audit_log.as_ref()
            {
                log.try_append(op, actor, summary.clone(), AuditResult::Ok);
            }
            emitter
                .info(format!(
                    "archive complete: {} chunks ({} bytes), {} index files",
                    report.chunks_uploaded, report.bytes_uploaded, report.index_files_uploaded
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

async fn preflight(params: &ArchiveParams, state: &DaemonState) -> Result<(), String> {
    if params.target_backend.is_empty() {
        return Err("target_backend must be non-empty".to_string());
    }
    if params.label.is_empty() {
        return Err("label must be non-empty".to_string());
    }
    let names = state.cloud_config.backend_names();
    if !names.iter().any(|n| n == &params.target_backend) {
        return Err(format!(
            "target backend '{}' not defined under `cloud.backends:` in YAML conffile (known: {})",
            params.target_backend,
            names.join(", ")
        ));
    }
    match find_drive_for_loaded_cartridge(&state.data_dir, &params.barcode) {
        Ok(Some(drive_id)) => Err(format!(
            "cartridge '{}' is loaded on drive {} — unload it first",
            params.barcode, drive_id
        )),
        Ok(None) => Ok(()),
        Err(e) => Err(format!("inventory check: {}", e)),
    }
}

fn parse_backend_and_worm(manifest_json: &str) -> Result<(String, bool), String> {
    let v: serde_json::Value =
        serde_json::from_str(manifest_json).map_err(|e| format!("parse manifest: {}", e))?;
    let backend = v["backend"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "manifest has no `backend` field".to_string())?
        .to_string();
    let worm = v["worm"].as_bool().unwrap_or(false);
    Ok((backend, worm))
}

fn audit_failure(
    state: &DaemonState,
    op: &str,
    actor: AuditActor,
    params: &ArchiveParams,
    reason: &str,
) {
    if let Some(log) = state.audit_log.as_ref() {
        log.try_append(
            op,
            actor,
            json!({
                "barcode": params.barcode,
                "target_backend": params.target_backend,
                "label": params.label,
                "dry_run": params.dry_run,
            }),
            AuditResult::Error(reason.to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_params_minimal() {
        let p: ArchiveParams = serde_json::from_value(serde_json::json!({
            "barcode": "T1",
            "target_backend": "s3b",
            "label": "snap1",
        }))
        .expect("minimal body");
        assert_eq!(p.barcode, "T1");
        assert_eq!(p.target_backend, "s3b");
        assert_eq!(p.label, "snap1");
        assert!(!p.dry_run);
    }

    #[test]
    fn archive_params_parses_dry_run() {
        let p: ArchiveParams = serde_json::from_value(serde_json::json!({
            "barcode": "T1",
            "target_backend": "s3b",
            "label": "snap1",
            "dry_run": true,
        }))
        .expect("explicit body");
        assert!(p.dry_run);
    }

    #[test]
    fn archive_params_requires_label() {
        assert!(
            serde_json::from_value::<ArchiveParams>(
                serde_json::json!({"barcode": "T1", "target_backend": "s3b"})
            )
            .is_err()
        );
    }

    #[test]
    fn archive_params_requires_barcode() {
        assert!(
            serde_json::from_value::<ArchiveParams>(
                serde_json::json!({"target_backend": "s3b", "label": "snap1"})
            )
            .is_err()
        );
    }

    #[test]
    fn parse_backend_and_worm_reads_fields() {
        let (backend, worm) =
            parse_backend_and_worm(r#"{"backend":"s3b","worm":true}"#).expect("parse");
        assert_eq!(backend, "s3b");
        assert!(worm);
    }

    #[test]
    fn parse_backend_and_worm_worm_defaults_false() {
        let (backend, worm) = parse_backend_and_worm(r#"{"backend":"local"}"#).expect("parse");
        assert_eq!(backend, "local");
        assert!(!worm);
    }

    #[test]
    fn parse_backend_and_worm_rejects_missing_backend() {
        assert!(parse_backend_and_worm(r#"{"worm":true}"#).is_err());
    }

    #[test]
    fn parse_backend_and_worm_rejects_malformed_json() {
        assert!(parse_backend_and_worm("not json at all").is_err());
    }
}
