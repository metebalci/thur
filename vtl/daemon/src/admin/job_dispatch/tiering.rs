// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.tiering.plan` job — evaluate the configured tiering
//! policies against the live cartridge inventory and report the
//! migrations they would trigger. Read-only: no data moves, no audit
//! entry, no mutation.
//!
//! Flow:
//!   1. Walk `<data_dir>/tapes/*` on the blocking pool, reading each
//!      manifest (`backend`, `worm`, `lto_generation`) and summing the
//!      sealed bytes / chunk count from `chunks.idx`.
//!   2. Run the pure decision engine
//!      ([`core_mediachanger::plan_moves`]) with `legal_held = false`
//!      to get the provisional move set — the cartridges a policy
//!      would relocate.
//!   3. For *only* those candidates, read cloud-native legal-hold
//!      state (one HEAD per cartridge, bounded concurrency). Held
//!      cartridges are excluded (hard rule); read failures are
//!      surfaced as skips; candidates on a `local` backend are treated
//!      as never-held (local storage has no hold concept). This keeps
//!      the storage round-trips proportional to the candidate set, not
//!      the whole library.
//!
//! Execution (`run-now`) and the legal-hold refusal gate on the
//! migrate primitive itself land in a later phase; this job is the
//! operator's read-only decision surface.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use core_mediachanger::ObjectStoreBackend;
use core_mediachanger::cartridge_migrate::MigrateMode;
use core_mediachanger::chunk_index::ChunkIndexFile;
use core_mediachanger::{
    AuditActor, AuditResult, CartridgeFacts, FailedMove, MigratedReport, PlannedMoveReport,
    SkippedCartridge, TieringPlanReport, TieringRunReport, plan_moves, read_cartridge_held,
};
use futures::stream::{self, StreamExt};
use serde_json::json;
use shared_admin_server::{JobEmitter, JobEvent};

use super::migrate::migrate_one;
use crate::state::DaemonState;

/// Bounded concurrency for the per-cartridge legal-hold HEADs. Matches
/// `shared_verify_core::STORAGE_VERIFY_CONCURRENCY`.
const HOLD_CHECK_CONCURRENCY: usize = 16;

/// Per-cartridge facts read off local disk (no storage I/O). The
/// legal-hold bit is filled in later, only for move candidates.
struct DiskFacts {
    barcode: String,
    backend: String,
    worm: bool,
    lto_generation: u8,
    chunk_count: u64,
    bytes: u64,
}

/// `system.tiering.plan` — read-only preview of the migrations the
/// policies would trigger.
pub async fn run(emitter: JobEmitter, _body: serde_json::Value, state: Arc<DaemonState>) {
    match compute_plan(&emitter, &state).await {
        Ok(report) => {
            emitter
                .info(format!(
                    "tiering plan: {} move(s), {} excluded (legal hold), {} skipped",
                    report.moves.len(),
                    report.excluded_legal_hold.len(),
                    report.skipped.len()
                ))
                .await;
            emit_report(&emitter, report).await;
        }
        Err(e) => emitter.emit(JobEvent::done_with_error(2, e)).await,
    }
}

/// `system.tiering.run` — execute the plan. Each proposed move is
/// applied in turn through the shared [`migrate_one`] path (gates +
/// `run_migrate`); a failed move is recorded and the run continues.
/// Every attempt — success or failure — writes a `cartridge.tiered`
/// audit row. Exit code is non-zero if any move failed.
pub async fn run_apply(emitter: JobEmitter, _body: serde_json::Value, state: Arc<DaemonState>) {
    let plan = match compute_plan(&emitter, &state).await {
        Ok(p) => p,
        Err(e) => {
            emitter.emit(JobEvent::done_with_error(2, e)).await;
            return;
        }
    };

    if plan.moves.is_empty() {
        emitter.info("tiering run-now: no moves to apply").await;
        emit_run_report(
            &emitter,
            run_report_from_plan(&plan, Vec::new(), Vec::new()),
            0,
        )
        .await;
        return;
    }

    emitter
        .info(format!(
            "tiering run-now: applying {} move(s)",
            plan.moves.len()
        ))
        .await;

    let actor = AuditActor::cli("daemon".to_string());
    let mut migrated: Vec<MigratedReport> = Vec::new();
    let mut failed: Vec<FailedMove> = Vec::new();

    for mv in &plan.moves {
        emitter
            .info(format!(
                "migrating {} {} -> {}",
                mv.barcode, mv.from_backend, mv.to_backend
            ))
            .await;
        match migrate_one(
            &state,
            &emitter,
            &mv.barcode,
            &mv.to_backend,
            MigrateMode::Move,
        )
        .await
        {
            Ok(report) => {
                if let Some(log) = state.audit_log.as_ref() {
                    log.try_append(
                        "cartridge.tiered",
                        actor.clone(),
                        json!({
                            "barcode": mv.barcode,
                            "from": mv.from_backend,
                            "to": mv.to_backend,
                            "chunk_count": report.chunks_copied,
                            "bytes": report.bytes_copied,
                        }),
                        AuditResult::Ok,
                    );
                }
                migrated.push(MigratedReport {
                    barcode: mv.barcode.clone(),
                    from_backend: mv.from_backend.clone(),
                    to_backend: mv.to_backend.clone(),
                    chunks_copied: report.chunks_copied,
                    bytes_copied: report.bytes_copied,
                });
            }
            Err(reason) => {
                if let Some(log) = state.audit_log.as_ref() {
                    log.try_append(
                        "cartridge.tiered",
                        actor.clone(),
                        json!({
                            "barcode": mv.barcode,
                            "from": mv.from_backend,
                            "to": mv.to_backend,
                        }),
                        AuditResult::Error(reason.clone()),
                    );
                }
                emitter
                    .warn(format!("migrate {} failed: {}", mv.barcode, reason))
                    .await;
                failed.push(FailedMove {
                    barcode: mv.barcode.clone(),
                    from_backend: mv.from_backend.clone(),
                    to_backend: mv.to_backend.clone(),
                    reason,
                });
            }
        }
    }

    let exit = if failed.is_empty() { 0 } else { 1 };
    emitter
        .info(format!(
            "tiering run-now complete: {} migrated, {} failed",
            migrated.len(),
            failed.len()
        ))
        .await;
    emit_run_report(
        &emitter,
        run_report_from_plan(&plan, migrated, failed),
        exit,
    )
    .await;
}

/// Build a [`TieringRunReport`], carrying the plan's exclusions/skips
/// for context alongside the attempt outcomes.
fn run_report_from_plan(
    plan: &TieringPlanReport,
    migrated: Vec<MigratedReport>,
    failed: Vec<FailedMove>,
) -> TieringRunReport {
    TieringRunReport {
        policies: plan.policies,
        cartridges_scanned: plan.cartridges_scanned,
        migrated,
        failed,
        excluded_legal_hold: plan.excluded_legal_hold.clone(),
        skipped: plan.skipped.clone(),
    }
}

/// Shared plan computation for `plan` and `run-now`: scan the
/// inventory, evaluate the policies, then read cloud-native legal hold
/// for the move candidates. Returns the full plan, or an error string
/// if the blocking disk scan panicked.
async fn compute_plan(
    emitter: &JobEmitter,
    state: &Arc<DaemonState>,
) -> std::result::Result<TieringPlanReport, String> {
    let policies = state.tiering.policies.clone();
    if policies.is_empty() {
        emitter.info("no tiering policies configured").await;
        return Ok(TieringPlanReport::default());
    }

    emitter
        .info(format!(
            "scanning cartridges against {} tiering policy(ies)",
            policies.len()
        ))
        .await;

    // 1. Disk scan on the blocking pool.
    let data_dir = state.data_dir.clone();
    let (disk, mut skipped) = tokio::task::spawn_blocking(move || scan_disk_facts(&data_dir))
        .await
        .map_err(|e| format!("scan panicked: {e}"))?;
    let cartridges_scanned = disk.len() + skipped.len();

    // 2. Provisional plan, ignoring legal hold. The engine's
    //    legal-held exclusion is equivalent to dropping held
    //    cartridges from this candidate set afterwards (a held
    //    cartridge it skips produces no move either way), so we only
    //    pay the storage round-trip for cartridges a policy would move.
    let provisional_facts: Vec<CartridgeFacts> = disk
        .iter()
        .map(|d| CartridgeFacts {
            barcode: d.barcode.clone(),
            lto_generation: d.lto_generation,
            worm: d.worm,
            current_backend: d.backend.clone(),
            legal_held: false,
        })
        .collect();
    let provisional = plan_moves(&provisional_facts, &policies);

    // Byte/chunk estimate lookup, keyed by barcode.
    let by_barcode: HashMap<&str, &DiskFacts> =
        disk.iter().map(|d| (d.barcode.as_str(), d)).collect();

    // 3. Build a backend handle per distinct source backend among the
    //    candidates (reused across that backend's cartridges).
    let needed: BTreeSet<String> = provisional
        .iter()
        .map(|m| m.source_backend.clone())
        .collect();
    let mut handles: HashMap<String, Arc<dyn ObjectStoreBackend>> = HashMap::new();
    for name in needed {
        match state.storage_config.create_backend_named(&name).await {
            Ok(b) => {
                handles.insert(name, Arc::from(b));
            }
            Err(e) => {
                emitter
                    .warn(format!(
                        "backend '{name}' unavailable for legal-hold check: {e}"
                    ))
                    .await;
                // Leave absent; its candidates become skips below.
            }
        }
    }

    // Read legal hold for each candidate, bounded concurrency.
    let hold_results: Vec<(
        core_mediachanger::PlannedMove,
        Option<core_mediachanger::Result<bool>>,
    )> = stream::iter(provisional.into_iter().map(|mv| {
        let handle = handles.get(&mv.source_backend).cloned();
        async move {
            match handle {
                // A local backend cannot carry a cloud-native hold, so
                // short-circuit rather than issuing a read that would
                // error with NotSupported and wrongly skip the move.
                Some(h) if h.backend_type() == "local" => (mv, Some(Ok(false))),
                Some(h) => {
                    let held = read_cartridge_held(h, mv.barcode.clone()).await;
                    (mv, Some(held))
                }
                None => (mv, None),
            }
        }
    }))
    .buffer_unordered(HOLD_CHECK_CONCURRENCY)
    .collect()
    .await;

    let mut moves: Vec<PlannedMoveReport> = Vec::new();
    let mut excluded_legal_hold: Vec<String> = Vec::new();
    for (mv, held) in hold_results {
        match held {
            Some(Ok(false)) => {
                let (chunk_count, bytes) = by_barcode
                    .get(mv.barcode.as_str())
                    .map(|d| (d.chunk_count, d.bytes))
                    .unwrap_or((0, 0));
                moves.push(PlannedMoveReport {
                    barcode: mv.barcode,
                    from_backend: mv.source_backend,
                    to_backend: mv.target_backend,
                    chunk_count,
                    bytes,
                });
            }
            Some(Ok(true)) => excluded_legal_hold.push(mv.barcode),
            Some(Err(e)) => skipped.push(SkippedCartridge {
                barcode: mv.barcode,
                reason: format!("legal-hold read failed: {e}"),
            }),
            None => skipped.push(SkippedCartridge {
                barcode: mv.barcode,
                reason: format!(
                    "backend '{}' unavailable for legal-hold check",
                    mv.source_backend
                ),
            }),
        }
    }

    // Stable ordering for deterministic output.
    moves.sort_by(|a, b| a.barcode.cmp(&b.barcode));
    excluded_legal_hold.sort();
    skipped.sort_by(|a, b| a.barcode.cmp(&b.barcode));

    Ok(TieringPlanReport {
        policies: policies.len(),
        cartridges_scanned,
        moves,
        excluded_legal_hold,
        skipped,
    })
}

async fn emit_report(emitter: &JobEmitter, report: TieringPlanReport) {
    match serde_json::to_value(&report) {
        Ok(v) => {
            emitter.emit(JobEvent::result(v)).await;
            emitter.emit(JobEvent::done(0)).await;
        }
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("serialize tiering report: {e}"),
                ))
                .await;
        }
    }
}

async fn emit_run_report(emitter: &JobEmitter, report: TieringRunReport, exit: i32) {
    match serde_json::to_value(&report) {
        Ok(v) => {
            emitter.emit(JobEvent::result(v)).await;
            emitter.emit(JobEvent::done(exit)).await;
        }
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("serialize tiering run report: {e}"),
                ))
                .await;
        }
    }
}

/// Walk `<data_dir>/tapes/*`, reading each cartridge's manifest and
/// `chunks.idx`. Cartridges with an unreadable manifest or no `backend`
/// field are returned in the skip list; a missing/corrupt `chunks.idx`
/// is non-fatal (bytes/chunks default to 0 so empty cartridges remain
/// plannable).
fn scan_disk_facts(data_dir: &Path) -> (Vec<DiskFacts>, Vec<SkippedCartridge>) {
    let mut facts = Vec::new();
    let mut skipped = Vec::new();
    let tapes_dir = data_dir.join("tapes");
    let entries = match std::fs::read_dir(&tapes_dir) {
        Ok(e) => e,
        Err(_) => return (facts, skipped),
    };

    for entry in entries.flatten() {
        let tape_path = entry.path();
        let manifest_path = tape_path.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let barcode = match tape_path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let json = match std::fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(e) => {
                skipped.push(SkippedCartridge {
                    barcode,
                    reason: format!("manifest read failed: {e}"),
                });
                continue;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(e) => {
                skipped.push(SkippedCartridge {
                    barcode,
                    reason: format!("manifest parse failed: {e}"),
                });
                continue;
            }
        };
        let backend = match v.get("backend").and_then(|s| s.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                skipped.push(SkippedCartridge {
                    barcode,
                    reason: "manifest missing or empty `backend` field".into(),
                });
                continue;
            }
        };
        let worm = v.get("worm").and_then(|w| w.as_bool()).unwrap_or(false);
        let lto_generation = v
            .get("lto_generation")
            .and_then(|g| g.as_u64())
            .unwrap_or(0) as u8;

        let (chunk_count, bytes) = count_sealed_chunks(&tape_path);

        facts.push(DiskFacts {
            barcode,
            backend,
            worm,
            lto_generation,
            chunk_count,
            bytes,
        });
    }

    (facts, skipped)
}

/// Sum sealed chunk count + bytes from a cartridge's `chunks.idx`. A
/// missing or unreadable index yields (0, 0) — the cartridge is still
/// plannable (e.g. a freshly created empty cartridge).
fn count_sealed_chunks(tape_path: &Path) -> (u64, u64) {
    if !ChunkIndexFile::path_for(tape_path).is_file() {
        return (0, 0);
    }
    let cif = match ChunkIndexFile::open_or_create(tape_path) {
        Ok(f) => f,
        Err(_) => return (0, 0),
    };
    let mut count = 0u64;
    let mut bytes = 0u64;
    for item in cif.iter() {
        let rec = match item {
            Ok((_id, rec)) => rec,
            // Stop counting on a decode error; report best-effort.
            Err(_) => break,
        };
        if rec.hash.is_some() {
            count += 1;
            bytes = bytes.saturating_add(rec.size);
        }
    }
    (count, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::chunk_index::{ChunkIndexFile, LocationTag};

    fn write_manifest(dir: &Path, backend: &str, worm: bool, lto: u8) {
        std::fs::create_dir_all(dir).unwrap();
        let json = serde_json::json!({
            "label": dir.file_name().unwrap().to_str().unwrap(),
            "backend": backend,
            "worm": worm,
            "lto_generation": lto,
        });
        std::fs::write(dir.join("manifest.json"), json.to_string()).unwrap();
    }

    #[test]
    fn scan_reads_manifest_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let tapes = tmp.path().join("tapes");
        write_manifest(&tapes.join("ARCH001"), "hot", true, 8);
        write_manifest(&tapes.join("PROD002"), "cold", false, 7);

        let (facts, skipped) = scan_disk_facts(tmp.path());
        assert!(skipped.is_empty());
        assert_eq!(facts.len(), 2);
        let arch = facts.iter().find(|f| f.barcode == "ARCH001").unwrap();
        assert_eq!(arch.backend, "hot");
        assert!(arch.worm);
        assert_eq!(arch.lto_generation, 8);
    }

    #[test]
    fn scan_skips_unreadable_and_backendless_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let tapes = tmp.path().join("tapes");

        // Missing `backend`.
        let nobk = tapes.join("NOBK01");
        std::fs::create_dir_all(&nobk).unwrap();
        std::fs::write(nobk.join("manifest.json"), r#"{"worm":false}"#).unwrap();

        // Malformed JSON.
        let bad = tapes.join("BAD001");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("manifest.json"), "{not json").unwrap();

        let (facts, skipped) = scan_disk_facts(tmp.path());
        assert!(facts.is_empty());
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn scan_missing_tapes_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (facts, skipped) = scan_disk_facts(tmp.path());
        assert!(facts.is_empty());
        assert!(skipped.is_empty());
    }

    #[test]
    fn count_sealed_chunks_sums_only_sealed() {
        let tmp = tempfile::tempdir().unwrap();
        let cart = tmp.path().join("T1");
        std::fs::create_dir_all(&cart).unwrap();
        let cif = ChunkIndexFile::open_or_create(&cart).unwrap();
        // Two sealed (hash set) + one unsealed (no hash).
        cif.append(&core_mediachanger::chunk_index::ChunkRec {
            hash: Some("a".repeat(64)),
            size: 1000,
            location: LocationTag::Both,
            uploaded: true,
            compression: None,
        })
        .unwrap();
        cif.append(&core_mediachanger::chunk_index::ChunkRec {
            hash: Some("b".repeat(64)),
            size: 2000,
            location: LocationTag::LocalOnly,
            uploaded: false,
            compression: None,
        })
        .unwrap();
        cif.append(&core_mediachanger::chunk_index::ChunkRec {
            hash: None,
            size: 0,
            location: LocationTag::LocalOnly,
            uploaded: false,
            compression: None,
        })
        .unwrap();
        drop(cif);

        let (count, bytes) = count_sealed_chunks(&cart);
        assert_eq!(count, 2);
        assert_eq!(bytes, 3000);
    }

    #[test]
    fn count_sealed_chunks_missing_idx_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let cart = tmp.path().join("EMPTY");
        std::fs::create_dir_all(&cart).unwrap();
        assert_eq!(count_sealed_chunks(&cart), (0, 0));
    }
}
