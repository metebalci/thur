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
    cloud_only: u64,
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

fn fmt_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let f = n as f64;
    if f >= TIB {
        format!("{:.2} TiB", f / TIB)
    } else if f >= GIB {
        format!("{:.2} GiB", f / GIB)
    } else if f >= MIB {
        format!("{:.2} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.2} KiB", f / KIB)
    } else {
        format!("{} B", n)
    }
}

fn fmt_ratio(logical: u64, unique: u64) -> String {
    if unique == 0 {
        "—".into()
    } else {
        format!("{:.2}x", logical as f64 / unique as f64)
    }
}

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
            "Location:          LocalOnly={}, Both={}, CloudOnly={} (evicted from disk)",
            b.location.local_only, b.location.both, b.location.cloud_only,
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

    println!("Note: cloud HEAD-skip rate (upload-time dedup) is exposed");
    println!("via the daemon's /metrics endpoint as");
    println!("  thurvtl_chunk_cloud_head_hits_total /");
    println!("  thurvtl_chunk_cloud_head_probes_total");
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
