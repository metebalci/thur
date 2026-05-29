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
use core_mediachanger::TieringPlanReport;
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

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::{PlannedMoveReport, SkippedCartridge};

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
}
