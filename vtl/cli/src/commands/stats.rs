// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl system stats` — daemon-routed dedup analytics.
//!
//! Mirror of the daemon's `StatsReport` shape (see
//! `vtl/daemon/src/admin/job_dispatch/stats.rs`). The CLI just
//! deserializes the structured Result line out of the job stream and
//! pretty-prints it.

use anyhow::{Context, Result};
use serde::Deserialize;

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;

#[derive(Debug, Default, Clone, Deserialize)]
struct LocationCounts {
    local_only: u64,
    both: u64,
    storage_only: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct CartridgeStats {
    barcode: String,
    backend: String,
    scope: String,
    sealed_chunks: u64,
    logical_bytes: u64,
    #[allow(dead_code)]
    cart_unique_bytes: u64,
    exclusive_bytes: u64,
    shared_bytes: u64,
    #[allow(dead_code)]
    location: LocationCounts,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct BackendStats {
    backend: String,
    cartridges_global: u64,
    cartridges_local: u64,
    sealed_chunks: u64,
    logical_bytes: u64,
    unique_pool_bytes: u64,
    location: LocationCounts,
}

#[derive(Debug, Clone, Deserialize)]
struct StatsReport {
    backends: Vec<BackendStats>,
    cartridges: Vec<CartridgeStats>,
    skipped: Vec<SkippedCartridge>,
}

#[derive(Debug, Clone, Deserialize)]
struct SkippedCartridge {
    barcode: String,
    reason: String,
}

pub async fn cmd_stats(json: bool) -> Result<u8> {
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    // Capture the raw JSON for `--json` rendering and the typed
    // shape for human output. Cheaper than re-serializing the
    // strongly-typed `StatsReport` (the local mirror doesn't derive
    // Serialize on purpose — daemon owns the canonical shape).
    let mut raw: Option<serde_json::Value> = None;
    let mut typed: Option<StatsReport> = None;

    let exit = client
        .run_job("system.stats", &serde_json::json!({}), |ev| match ev {
            JobEvent::Log { message, .. } => {
                eprintln!("{}", message);
            }
            JobEvent::Result { data } => {
                if let Ok(r) = serde_json::from_value::<StatsReport>(data.clone()) {
                    typed = Some(r);
                }
                raw = Some(data);
            }
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .context("stats job stream")?;

    if json {
        if let Some(v) = raw {
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    } else if let Some(report) = typed {
        print_human(&report);
    }

    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

use shared_cli_system::fmt::{fmt_bytes, fmt_ratio};

fn print_human(r: &StatsReport) {
    if r.backends.is_empty() {
        println!("No cartridges found under tapes/.");
        if !r.skipped.is_empty() {
            print_skipped(r);
        }
        return;
    }

    for b in &r.backends {
        println!("=== Backend: {} ===", b.backend);
        println!(
            "Cartridges:        {} global, {} local",
            b.cartridges_global, b.cartridges_local,
        );
        println!("Sealed chunks:     {}", b.sealed_chunks);
        println!("Logical bytes:     {}", fmt_bytes(b.logical_bytes));
        println!("Unique pool bytes: {}", fmt_bytes(b.unique_pool_bytes));
        let saved = b.logical_bytes.saturating_sub(b.unique_pool_bytes);
        let saved_pct = if b.logical_bytes == 0 {
            0.0
        } else {
            (saved as f64 * 100.0) / (b.logical_bytes as f64)
        };
        println!(
            "Dedup ratio:       {}  (saved {} / {:.1}%)",
            fmt_ratio(b.logical_bytes, b.unique_pool_bytes),
            fmt_bytes(saved),
            saved_pct,
        );
        println!(
            "Location:          LocalOnly={}, Both={}, StorageOnly={} (evicted from disk)",
            b.location.local_only, b.location.both, b.location.storage_only,
        );
        println!();

        let cs: Vec<&CartridgeStats> = r
            .cartridges
            .iter()
            .filter(|c| c.backend == b.backend)
            .collect();
        if !cs.is_empty() {
            println!("Per-cartridge contribution:");
            println!(
                "  {:<16} {:<7} {:>8} {:>12} {:>12} {:>12}",
                "Barcode", "Scope", "Chunks", "Logical", "Exclusive", "Shared",
            );
            for c in cs {
                println!(
                    "  {:<16} {:<7} {:>8} {:>12} {:>12} {:>12}",
                    c.barcode,
                    c.scope,
                    c.sealed_chunks,
                    fmt_bytes(c.logical_bytes),
                    fmt_bytes(c.exclusive_bytes),
                    fmt_bytes(c.shared_bytes),
                );
            }
            println!();
        }
    }

    print_skipped(r);

    println!("Note: storage HEAD-skip rate (upload-time dedup) is exposed");
    println!("via the daemon's /metrics endpoint as");
    println!("  thurvtl_chunk_storage_head_hits_total /");
    println!("  thurvtl_chunk_storage_head_probes_total");
    println!("This walker covers seal-time pool dedup only.");
}

fn print_skipped(r: &StatsReport) {
    if r.skipped.is_empty() {
        return;
    }
    println!("Skipped cartridges (could not measure):");
    for s in &r.skipped {
        println!("  {} — {}", s.barcode, s.reason);
    }
    println!();
}

#[derive(Deserialize)]
struct SystemResetStatsResp {
    drives: usize,
    cartridges: usize,
    errors: Vec<String>,
}

/// `thurvtl system reset-stats` — zero every drive's lifetime stats and
/// every cartridge's mount + byte counters. Plain daemon-routed POST
/// (not a job — the sweep is bounded by the inventory size).
pub async fn cmd_reset_stats() -> Result<()> {
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let resp: SystemResetStatsResp = client
        .post_json("/api/v1/system/reset-stats", &serde_json::json!({}))
        .await?;
    println!(
        "OK: reset stats for {} drive(s) and {} cartridge(s)",
        resp.drives, resp.cartridges
    );
    for e in &resp.errors {
        eprintln!("  warning: {}", e);
    }
    if !resp.errors.is_empty() {
        anyhow::bail!("{} cartridge(s) failed to reset", resp.errors.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // fmt_bytes / fmt_ratio moved to `shared_cli_system::fmt`; their
    // unit tests live there now.

    #[test]
    fn print_human_empty_report_takes_no_cartridges_branch() {
        let report: StatsReport = serde_json::from_value(serde_json::json!({
            "backends": [],
            "cartridges": [],
            "skipped": [],
        }))
        .expect("parse empty report");
        print_human(&report);
    }

    #[test]
    fn print_human_empty_with_skipped() {
        let report: StatsReport = serde_json::from_value(serde_json::json!({
            "backends": [],
            "cartridges": [],
            "skipped": [{"barcode": "T9", "reason": "manifest unreadable"}],
        }))
        .expect("parse report with skipped");
        print_human(&report);
    }

    #[test]
    fn print_human_renders_backend_and_cartridges() {
        let report: StatsReport = serde_json::from_value(serde_json::json!({
            "backends": [{
                "backend": "s3b",
                "cartridges_global": 2,
                "cartridges_local": 1,
                "sealed_chunks": 100,
                "logical_bytes": 4000000,
                "unique_pool_bytes": 1000000,
                "location": {"local_only": 1, "both": 2, "storage_only": 3},
            }],
            "cartridges": [{
                "barcode": "TAPE001",
                "backend": "s3b",
                "scope": "global",
                "sealed_chunks": 50,
                "logical_bytes": 2000000,
                "cart_unique_bytes": 800000,
                "exclusive_bytes": 500000,
                "shared_bytes": 300000,
                "location": {"local_only": 0, "both": 0, "storage_only": 0},
            }],
            "skipped": [],
        }))
        .expect("parse full report");
        print_human(&report);
    }

    #[test]
    fn location_counts_default_is_zero() {
        let c = LocationCounts::default();
        assert_eq!(c.local_only, 0);
        assert_eq!(c.both, 0);
        assert_eq!(c.storage_only, 0);
    }
}
