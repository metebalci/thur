// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl system tiering plan` — daemon-routed, read-only preview of
//! the migrations the configured tiering policies would trigger.
//!
//! The daemon owns the on-disk inventory during operation, so routing
//! the walk through the admin socket gives a synchronized view and
//! lets the daemon do the per-cartridge legal-hold cloud reads. The
//! CLI streams the job events, decodes the `TieringPlanReport` from the
//! terminal Result line, and renders human or JSON output.
//!
//! Exit codes:
//!   0 — plan produced (including "no moves")
//!   2 — transport error / no structured payload

use anyhow::{Context, Result};
use core_mediachanger::{TieringPlanReport, TieringRunReport};
use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;

pub async fn cmd_tiering_plan(json: bool) -> Result<u8> {
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = serde_json::json!({});

    let mut report: Option<TieringPlanReport> = None;
    let exit = client
        .run_job("system.tiering.plan", &body, |ev| match ev {
            // Log lines to stderr so json mode keeps stdout clean.
            JobEvent::Log { message, .. } => eprintln!("{}", message),
            JobEvent::Result { data } => match serde_json::from_value::<TieringPlanReport>(data) {
                Ok(r) => report = Some(r),
                Err(e) => eprintln!("warning: failed to decode tiering report: {}", e),
            },
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .with_context(|| "tiering plan job stream")?;

    let report = match report {
        Some(r) => r,
        None => return Ok(u8::try_from(exit.max(0)).unwrap_or(2)),
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize tiering report")?
        );
    } else {
        print_human(&report);
    }

    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

fn print_human(r: &TieringPlanReport) {
    println!(
        "Tiering plan: {} policy(ies), {} cartridge(s) scanned",
        r.policies, r.cartridges_scanned
    );
    println!();

    if r.moves.is_empty() {
        println!("Moves: none");
    } else {
        let total_bytes: u64 = r.moves.iter().map(|m| m.bytes).sum();
        println!("Moves ({}, {} bytes total)", r.moves.len(), total_bytes);
        for m in &r.moves {
            println!(
                "  {}  {} -> {}  ({} chunks, {} bytes)",
                m.barcode, m.from_backend, m.to_backend, m.chunk_count, m.bytes
            );
        }
    }

    if !r.excluded_legal_hold.is_empty() {
        println!();
        println!(
            "Excluded ({} under legal hold): {}",
            r.excluded_legal_hold.len(),
            r.excluded_legal_hold.join(", ")
        );
    }

    if !r.skipped.is_empty() {
        println!();
        println!("Skipped ({})", r.skipped.len());
        for s in &r.skipped {
            println!("  {}: {}", s.barcode, s.reason);
        }
    }
}

pub async fn cmd_tiering_run(json: bool) -> Result<u8> {
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = serde_json::json!({});

    let mut report: Option<TieringRunReport> = None;
    let exit = client
        .run_job("system.tiering.run", &body, |ev| match ev {
            JobEvent::Log { message, .. } => eprintln!("{}", message),
            JobEvent::Result { data } => match serde_json::from_value::<TieringRunReport>(data) {
                Ok(r) => report = Some(r),
                Err(e) => eprintln!("warning: failed to decode tiering run report: {}", e),
            },
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .with_context(|| "tiering run job stream")?;

    let report = match report {
        Some(r) => r,
        None => return Ok(u8::try_from(exit.max(0)).unwrap_or(2)),
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize tiering run report")?
        );
    } else {
        print_run_human(&report);
    }

    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

pub async fn cmd_tiering_status(json: bool) -> Result<u8> {
    // Status is a compact view over the same read-only plan the
    // operator previews; it reports counts, not the full move list.
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = serde_json::json!({});

    let mut report: Option<TieringPlanReport> = None;
    let exit = client
        .run_job("system.tiering.plan", &body, |ev| match ev {
            JobEvent::Log { .. } => {}
            JobEvent::Result { data } => match serde_json::from_value::<TieringPlanReport>(data) {
                Ok(r) => report = Some(r),
                Err(e) => eprintln!("warning: failed to decode tiering report: {}", e),
            },
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .with_context(|| "tiering status job stream")?;

    let report = match report {
        Some(r) => r,
        None => return Ok(u8::try_from(exit.max(0)).unwrap_or(2)),
    };

    if json {
        let summary = serde_json::json!({
            "policies": report.policies,
            "cartridges_scanned": report.cartridges_scanned,
            "pending_moves": report.moves.len(),
            "under_legal_hold": report.excluded_legal_hold.len(),
            "unevaluable": report.skipped.len(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).context("serialize tiering status")?
        );
    } else if report.policies == 0 {
        println!("Tiering: no policies configured");
    } else {
        println!(
            "Tiering: {} policy(ies), {} cartridge(s) scanned",
            report.policies, report.cartridges_scanned
        );
        println!("  pending moves:     {}", report.moves.len());
        println!("  under legal hold:  {}", report.excluded_legal_hold.len());
        println!("  unevaluable:       {}", report.skipped.len());
    }

    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

fn print_run_human(r: &TieringRunReport) {
    println!(
        "Tiering run-now: {} policy(ies), {} cartridge(s) scanned",
        r.policies, r.cartridges_scanned
    );
    println!();

    if r.migrated.is_empty() {
        println!("Migrated: none");
    } else {
        let total: u64 = r.migrated.iter().map(|m| m.bytes_copied).sum();
        println!("Migrated ({}, {} bytes copied)", r.migrated.len(), total);
        for m in &r.migrated {
            println!(
                "  {}  {} -> {}  ({} chunks, {} bytes)",
                m.barcode, m.from_backend, m.to_backend, m.chunks_copied, m.bytes_copied
            );
        }
    }

    if !r.failed.is_empty() {
        println!();
        println!("Failed ({})", r.failed.len());
        for f in &r.failed {
            println!(
                "  {}  {} -> {}: {}",
                f.barcode, f.from_backend, f.to_backend, f.reason
            );
        }
    }

    if !r.excluded_legal_hold.is_empty() {
        println!();
        println!(
            "Excluded ({} under legal hold): {}",
            r.excluded_legal_hold.len(),
            r.excluded_legal_hold.join(", ")
        );
    }

    if !r.skipped.is_empty() {
        println!();
        println!("Skipped ({})", r.skipped.len());
        for s in &r.skipped {
            println!("  {}: {}", s.barcode, s.reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::{FailedMove, MigratedReport, PlannedMoveReport, SkippedCartridge};

    #[test]
    fn print_human_handles_empty_plan() {
        print_human(&TieringPlanReport::default());
    }

    #[test]
    fn print_human_renders_moves_excluded_and_skipped() {
        let report = TieringPlanReport {
            policies: 2,
            cartridges_scanned: 5,
            moves: vec![PlannedMoveReport {
                barcode: "ARCH001".into(),
                from_backend: "hot".into(),
                to_backend: "cold".into(),
                chunk_count: 12,
                bytes: 4096,
            }],
            excluded_legal_hold: vec!["HOLD01".into()],
            skipped: vec![SkippedCartridge {
                barcode: "BAD01".into(),
                reason: "manifest parse failed".into(),
            }],
        };
        print_human(&report);
        // Round-trips through JSON (the cross-process contract).
        let v = serde_json::to_value(&report).unwrap();
        let back: TieringPlanReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn print_run_human_handles_empty_and_populated() {
        print_run_human(&TieringRunReport::default());

        let report = TieringRunReport {
            policies: 1,
            cartridges_scanned: 3,
            migrated: vec![MigratedReport {
                barcode: "ARCH001".into(),
                from_backend: "hot".into(),
                to_backend: "cold".into(),
                chunks_copied: 4,
                bytes_copied: 8192,
            }],
            failed: vec![FailedMove {
                barcode: "ARCH002".into(),
                from_backend: "hot".into(),
                to_backend: "cold".into(),
                reason: "loaded on drive 0".into(),
            }],
            excluded_legal_hold: vec!["HOLD01".into()],
            skipped: vec![SkippedCartridge {
                barcode: "BAD01".into(),
                reason: "manifest parse failed".into(),
            }],
        };
        print_run_human(&report);
        let v = serde_json::to_value(&report).unwrap();
        let back: TieringRunReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);
    }
}
