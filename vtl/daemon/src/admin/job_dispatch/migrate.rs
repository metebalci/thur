// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `cartridge.migrate` job — move or rebind a cartridge between
//! storage backends. Drives [`core_mediachanger::cartridge_migrate::run_migrate`].
//!
//! Body params: `{ "barcode": "...", "target_backend": "...",
//!                 "mode": "move" | "rebind",
//!                 "verify": true,
//!                 "dry_run": false }`.
//!
//! Refuse-gates layered on top of the primitive:
//!   - target backend named in `storage-backends.json`
//!   - source ≠ target (also caught by the primitive; checked here
//!     for a clearer operator message)
//!   - cartridge not loaded in any drive
//!   - WORM cartridges require the target's `retention_mode` to be
//!     governance or compliance
//!   - migrate from a `local` backend (no storage presence to copy
//!     from) is refused for `--mode=move`; `rebind` is still valid
//!     for moving the cartridge's "binding" between local roots
//!
//! Audit: `cartridge.migrated` (move) or `cartridge.rebound` (rebind)
//! with operation parameters + outcome. Both Ok and Err paths.

use std::sync::Arc;

use core_mediachanger::cartridge_migrate::{
    MigrateMode, MigrateOptions, MigrateReport, run_migrate,
};
use core_mediachanger::legal_hold::find_drive_for_loaded_cartridge;
use core_mediachanger::{AuditActor, AuditResult};
use serde::Deserialize;
use serde_json::json;
use shared_admin_server::{JobEmitter, JobEvent};

use crate::iscsi::drive_manager::DriveManager;
use crate::state::DaemonState;

/// RAII migration claim (issue #212). Marks the barcode in-migration
/// in the `DriveManager` so the SCSI/admin load path refuses to mount
/// it for the migrate's whole lifetime, and clears the mark on drop
/// (every exit path — Ok, Err, early return, panic). `acquire` returns
/// `None` if another migration already holds the barcode.
struct MigrationGuard {
    drive_manager: Arc<DriveManager>,
    barcode: String,
}

impl MigrationGuard {
    fn acquire(drive_manager: Arc<DriveManager>, barcode: &str) -> Option<Self> {
        if drive_manager.try_begin_migration(barcode) {
            Some(Self {
                drive_manager,
                barcode: barcode.to_string(),
            })
        } else {
            None
        }
    }
}

impl Drop for MigrationGuard {
    fn drop(&mut self) {
        self.drive_manager.end_migration(&self.barcode);
    }
}

#[derive(Debug, Deserialize)]
pub struct MigrateParams {
    pub barcode: String,
    pub target_backend: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_true")]
    pub verify: bool,
    #[serde(default)]
    pub dry_run: bool,
}

fn default_mode() -> String {
    "move".to_string()
}
fn default_true() -> bool {
    true
}

pub async fn run(emitter: JobEmitter, body: serde_json::Value, state: Arc<DaemonState>) {
    let params: MigrateParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };

    let actor = AuditActor::cli("daemon".to_string());
    let op = match params.mode.as_str() {
        "move" => "cartridge.migrated",
        "rebind" => "cartridge.rebound",
        other => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("unknown mode '{}' (expected 'move' or 'rebind')", other),
                ))
                .await;
            return;
        }
    };

    // Pre-flight gates. Each emits a Done(2) with an explanatory
    // string and writes a failure audit entry.
    if let Err(reason) = preflight(&params, &state).await {
        audit_failure(&state, op, actor.clone(), &params, &reason);
        emitter.emit(JobEvent::done_with_error(2, reason)).await;
        return;
    }

    // Hold the cartridge against a concurrent host load for the whole
    // migrate (issue #212): the load path refuses a migrating barcode,
    // so a backup job can't MOVE MEDIUM it into a drive and append
    // chunks while the copy-then-flip-then-delete sequence runs. Held
    // until `_mig_guard` drops at the end of this function.
    let _mig_guard = match MigrationGuard::acquire(state.drive_manager.clone(), &params.barcode) {
        Some(g) => g,
        None => {
            let reason = format!("cartridge '{}' is already being migrated", params.barcode);
            audit_failure(&state, op, actor.clone(), &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };
    // Re-check not-loaded now the claim is set — closes the window
    // where a load landed between the preflight check and the claim.
    if let Ok(Some(drive_id)) = find_drive_for_loaded_cartridge(&state.data_dir, &params.barcode) {
        let reason = format!(
            "cartridge '{}' was loaded on drive {} during migrate setup — unload it first",
            params.barcode, drive_id
        );
        audit_failure(&state, op, actor.clone(), &params, &reason);
        emitter.emit(JobEvent::done_with_error(2, reason)).await;
        return;
    }

    // Source backend + WORM gate — discovered from the manifest.
    let manifest_path = state
        .data_dir
        .join("tapes")
        .join(&params.barcode)
        .join("manifest.json");
    let manifest_str = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            let reason = format!("read manifest for '{}': {}", params.barcode, e);
            audit_failure(&state, op, actor.clone(), &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };
    let (source_backend, is_worm) = match parse_backend_and_worm(&manifest_str) {
        Ok(v) => v,
        Err(reason) => {
            audit_failure(&state, op, actor.clone(), &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };

    if source_backend == params.target_backend {
        let reason = "source and target backend must differ".to_string();
        audit_failure(&state, op, actor.clone(), &params, &reason);
        emitter.emit(JobEvent::done_with_error(2, reason)).await;
        return;
    }

    // WORM cartridges require the target to have governance/compliance.
    if is_worm {
        let target_mode = state
            .storage_config
            .retention_mode_named(&params.target_backend);
        if !target_mode.requires_lock() {
            let reason = format!(
                "WORM cartridge cannot migrate to backend '{}' (retention_mode={}); \
                 target must be governance or compliance",
                params.target_backend,
                target_mode.label()
            );
            audit_failure(&state, op, actor.clone(), &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    }

    // Build backend handles.
    let source = match state
        .storage_config
        .create_backend_named(&source_backend)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            let reason = format!("construct source backend '{}': {}", source_backend, e);
            audit_failure(&state, op, actor.clone(), &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };
    let target = match state
        .storage_config
        .create_backend_named(&params.target_backend)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            let reason = format!(
                "construct target backend '{}': {}",
                params.target_backend, e
            );
            audit_failure(&state, op, actor.clone(), &params, &reason);
            emitter.emit(JobEvent::done_with_error(2, reason)).await;
            return;
        }
    };

    // Sync → async progress bridge. The primitive's progress callback
    // is sync `Fn(&str)`; we forward each line through an unbounded
    // channel into the emitter's async `info(...)`. The forwarder
    // task exits when the channel's only sender (the closure below,
    // dropped together with `progress_cb`) goes out of scope.
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

    let mode = match params.mode.as_str() {
        "move" => MigrateMode::Move,
        "rebind" => MigrateMode::Rebind {
            verify: params.verify,
        },
        // Already validated above.
        _ => unreachable!(),
    };
    let tapes_dir = state.data_dir.join("tapes");

    emitter
        .info(format!(
            "migrate {} ({}) {} -> {}",
            params.barcode, params.mode, source_backend, params.target_backend
        ))
        .await;

    let outcome = run_migrate(MigrateOptions {
        tapes_dir: &tapes_dir,
        barcode: &params.barcode,
        source: source.as_ref(),
        source_name: &source_backend,
        target: target.as_ref(),
        target_name: &params.target_backend,
        mode,
        dry_run: params.dry_run,
        progress: Some(&progress_cb),
        // Keep both per-backend budgets exact across the move: the
        // source releases the bytes that physically leave its pool, the
        // target reserves the ones that land. Absent (None) only if a
        // backend has no live budget wired.
        source_budget: state.pool_budgets.get(&source_backend).cloned(),
        target_budget: state.pool_budgets.get(&params.target_backend).cloned(),
    })
    .await;

    // Dropping the closure drops the channel's only sender; the
    // forwarder then sees `rx.recv()` return `None` and exits.
    drop(progress_cb);
    let _ = forwarder.await;

    match outcome {
        Ok(report) => {
            let summary = json!({
                "barcode": report.barcode,
                "mode": report.mode,
                "from_backend": report.from_backend,
                "to_backend": report.to_backend,
                "chunks_total": report.chunks_total,
                "chunks_copied": report.chunks_copied,
                "chunks_verified": report.chunks_verified,
                "bytes_copied": report.bytes_copied,
                "manifest_objects_copied": report.manifest_objects_copied,
                "source_objects_deleted": report.source_objects_deleted,
                "local_files_moved": report.local_files_moved,
                "source_delete_warnings": report.source_delete_warnings,
                "dry_run": report.dry_run,
            });
            if !report.dry_run
                && let Some(log) = state.audit_log.as_ref()
            {
                log.try_append(op, actor, summary.clone(), AuditResult::Ok);
            }
            for w in &report.source_delete_warnings {
                emitter.warn(format!("source delete warning: {}", w)).await;
            }
            // The job-stream warnings above are seen once; fire a
            // standing OrphanedObjects alert so the leaked source-side
            // objects (orphaned until a future GC sweep) stay visible to
            // operators. They live on the source (`from_backend`).
            if !report.source_delete_warnings.is_empty() {
                shared_alerting::record::orphaned_objects(
                    &report.from_backend,
                    &format!("migrate {}", report.barcode),
                    &report.source_delete_warnings,
                );
            }
            emitter
                .info(format!(
                    "migrate complete: {} chunks ({} bytes), {} manifest objects",
                    report.chunks_copied, report.bytes_copied, report.manifest_objects_copied,
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

/// Gates + backend build + `run_migrate` for one cartridge, shared by
/// the tiering run-now path ([`super::tiering`]). Mirrors the gates the
/// `cartridge.migrate` job applies inline — not-loaded, source≠target,
/// WORM→retention — then drives the same `run_migrate` primitive (whose
/// legal-hold refusal is the hard backstop). Returns the report on
/// success or a human reason on failure; the caller owns auditing and
/// terminal job events (the audit op differs: `cartridge.tiered`).
///
/// The manual `cartridge.migrate` `run` above keeps its own inline copy
/// of these gates for its richer per-field error messages and dry-run
/// reporting; both funnel through `run_migrate`.
pub(crate) async fn migrate_one(
    state: &DaemonState,
    emitter: &JobEmitter,
    barcode: &str,
    target_backend: &str,
    mode: MigrateMode,
) -> std::result::Result<MigrateReport, String> {
    match find_drive_for_loaded_cartridge(&state.data_dir, barcode) {
        Ok(Some(drive_id)) => {
            return Err(format!(
                "cartridge '{barcode}' is loaded on drive {drive_id} — unload it first"
            ));
        }
        Ok(None) => {}
        Err(e) => return Err(format!("inventory check: {e}")),
    }

    // Hold the cartridge against a concurrent host load for the whole
    // move (issue #212). Held until `_mig_guard` drops at return.
    let _mig_guard = MigrationGuard::acquire(state.drive_manager.clone(), barcode)
        .ok_or_else(|| format!("cartridge '{barcode}' is already being migrated"))?;
    // Re-check not-loaded with the claim set (closes the preflight→claim window).
    if let Ok(Some(drive_id)) = find_drive_for_loaded_cartridge(&state.data_dir, barcode) {
        return Err(format!(
            "cartridge '{barcode}' was loaded on drive {drive_id} during migrate setup — unload it first"
        ));
    }

    let manifest_path = state
        .data_dir
        .join("tapes")
        .join(barcode)
        .join("manifest.json");
    let manifest_str = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read manifest for '{barcode}': {e}"))?;
    let (source_backend, is_worm) = parse_backend_and_worm(&manifest_str)?;

    if source_backend == target_backend {
        return Err("source and target backend must differ".to_string());
    }
    if is_worm {
        let target_mode = state.storage_config.retention_mode_named(target_backend);
        if !target_mode.requires_lock() {
            return Err(format!(
                "WORM cartridge cannot migrate to backend '{}' (retention_mode={}); \
                 target must be governance or compliance",
                target_backend,
                target_mode.label()
            ));
        }
    }

    let source = state
        .storage_config
        .create_backend_named(&source_backend)
        .await
        .map_err(|e| format!("construct source backend '{source_backend}': {e}"))?;
    let target = state
        .storage_config
        .create_backend_named(target_backend)
        .await
        .map_err(|e| format!("construct target backend '{target_backend}': {e}"))?;

    // Sync→async progress bridge (same shape as the manual job).
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

    let outcome = run_migrate(MigrateOptions {
        tapes_dir: &tapes_dir,
        barcode,
        source: source.as_ref(),
        source_name: &source_backend,
        target: target.as_ref(),
        target_name: target_backend,
        mode,
        dry_run: false,
        progress: Some(&progress_cb),
        source_budget: state.pool_budgets.get(&source_backend).cloned(),
        target_budget: state.pool_budgets.get(target_backend).cloned(),
    })
    .await;

    drop(progress_cb);
    let _ = forwarder.await;
    outcome.map_err(|e| e.to_string())
}

async fn preflight(params: &MigrateParams, state: &DaemonState) -> Result<(), String> {
    if params.target_backend.is_empty() {
        return Err("target_backend must be non-empty".to_string());
    }
    // Target backend must be defined under `storage.backends:`.
    let names = state.storage_config.backend_names();
    if !names.iter().any(|n| n == &params.target_backend) {
        return Err(format!(
            "target backend '{}' not defined under `storage.backends:` in YAML conffile (known: {})",
            params.target_backend,
            names.join(", ")
        ));
    }
    // Cartridge must not be loaded in a drive.
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
    params: &MigrateParams,
    reason: &str,
) {
    if let Some(log) = state.audit_log.as_ref() {
        log.try_append(
            op,
            actor,
            json!({
                "barcode": params.barcode,
                "target_backend": params.target_backend,
                "mode": params.mode,
                "verify": params.verify,
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
    fn migrate_params_minimal_uses_defaults() {
        let p: MigrateParams =
            serde_json::from_value(serde_json::json!({"barcode": "T1", "target_backend": "s3b"}))
                .expect("minimal body");
        assert_eq!(p.barcode, "T1");
        assert_eq!(p.target_backend, "s3b");
        assert_eq!(p.mode, "move");
        assert!(p.verify);
        assert!(!p.dry_run);
    }

    #[test]
    fn migrate_params_parses_explicit_fields() {
        let p: MigrateParams = serde_json::from_value(serde_json::json!({
            "barcode": "T1",
            "target_backend": "s3b",
            "mode": "rebind",
            "verify": false,
            "dry_run": true,
        }))
        .expect("explicit body");
        assert_eq!(p.mode, "rebind");
        assert!(!p.verify);
        assert!(p.dry_run);
    }

    #[test]
    fn migrate_params_requires_barcode() {
        assert!(
            serde_json::from_value::<MigrateParams>(serde_json::json!({"target_backend": "s3b"}))
                .is_err()
        );
    }

    #[test]
    fn migrate_params_requires_target_backend() {
        assert!(
            serde_json::from_value::<MigrateParams>(serde_json::json!({"barcode": "T1"})).is_err()
        );
    }

    #[test]
    fn default_mode_is_move_and_default_true() {
        assert_eq!(default_mode(), "move");
        assert!(default_true());
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
        let (backend, worm) = parse_backend_and_worm(r#"{"backend":"s3b"}"#).expect("parse");
        assert_eq!(backend, "s3b");
        assert!(!worm);
    }

    #[test]
    fn parse_backend_and_worm_rejects_missing_backend() {
        assert!(parse_backend_and_worm(r#"{"worm":false}"#).is_err());
    }

    #[test]
    fn parse_backend_and_worm_rejects_empty_backend() {
        assert!(parse_backend_and_worm(r#"{"backend":""}"#).is_err());
    }

    #[test]
    fn parse_backend_and_worm_rejects_malformed_json() {
        assert!(parse_backend_and_worm("{not json").is_err());
    }
}
