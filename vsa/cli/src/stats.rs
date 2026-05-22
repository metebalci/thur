// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvsa system stats` — daemon-routed dedup analytics.
//!
//! Mirror of the daemon's `StatsReport` shape (see
//! `vsa/daemon/src/admin/job_dispatch/stats.rs`). The CLI deserializes
//! the structured Result line out of the job stream and pretty-prints
//! it. Block-side parallel of the tape CLI's `system stats`; the
//! report has no `location` column because `pages.idx` records no
//! local/cloud tag.

use anyhow::{Context, Result};
use serde::Deserialize;

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;

#[derive(Debug, Clone, Deserialize)]
struct VolumeStats {
    volume: String,
    backend: String,
    scope: String,
    allocated_pages: u64,
    logical_bytes: u64,
    #[allow(dead_code)]
    volume_unique_bytes: u64,
    exclusive_bytes: u64,
    shared_bytes: u64,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct BackendStats {
    backend: String,
    volumes_global: u64,
    volumes_local: u64,
    allocated_pages: u64,
    logical_bytes: u64,
    unique_pool_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct StatsReport {
    backends: Vec<BackendStats>,
    volumes: Vec<VolumeStats>,
    skipped: Vec<SkippedVolume>,
}

#[derive(Debug, Clone, Deserialize)]
struct SkippedVolume {
    volume: String,
    reason: String,
}

pub async fn cmd_stats(json: bool) -> Result<u8> {
    let client = AdminClient::auto_discover(&shared_naming::DISK);
    // Capture the raw JSON for `--json` rendering and the typed shape
    // for human output. The local mirror doesn't derive Serialize on
    // purpose — the daemon owns the canonical shape.
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
        println!("No volumes found.");
        if !r.skipped.is_empty() {
            print_skipped(r);
        }
        return;
    }

    for b in &r.backends {
        println!("=== Backend: {} ===", b.backend);
        println!(
            "Volumes:           {} global, {} local",
            b.volumes_global, b.volumes_local,
        );
        println!("Allocated pages:   {}", b.allocated_pages);
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
        println!();

        let vs: Vec<&VolumeStats> = r
            .volumes
            .iter()
            .filter(|v| v.backend == b.backend)
            .collect();
        if !vs.is_empty() {
            println!("Per-volume contribution:");
            println!(
                "  {:<20} {:<7} {:>8} {:>12} {:>12} {:>12}",
                "Volume", "Scope", "Pages", "Logical", "Exclusive", "Shared",
            );
            for v in vs {
                println!(
                    "  {:<20} {:<7} {:>8} {:>12} {:>12} {:>12}",
                    v.volume,
                    v.scope,
                    v.allocated_pages,
                    fmt_bytes(v.logical_bytes),
                    fmt_bytes(v.exclusive_bytes),
                    fmt_bytes(v.shared_bytes),
                );
            }
            println!();
        }
    }

    print_skipped(r);

    println!("Note: byte figures cover chunks resident in the local pool;");
    println!("chunks evicted to cloud-only count toward page totals but are");
    println!("not sized here. This walker covers seal-time pool dedup.");
}

fn print_skipped(r: &StatsReport) {
    if r.skipped.is_empty() {
        return;
    }
    println!("Skipped volumes (could not measure):");
    for s in &r.skipped {
        println!("  {} — {}", s.volume, s.reason);
    }
    println!();
}
