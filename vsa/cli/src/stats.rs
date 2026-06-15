// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvsa system stats` — daemon-routed dedup analytics.
//!
//! Mirror of the daemon's `StatsReport` shape (see
//! `vsa/daemon/src/admin/job_dispatch/stats.rs`). The CLI deserializes
//! the structured Result line out of the job stream and pretty-prints
//! it. Block-side parallel of the tape CLI's `system stats`; the
//! report has no `location` column because `pages.idx` records no
//! local/storage tag.

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
    let mut decode_err: Option<String> = None;

    let exit = client
        .run_job("system.stats", &serde_json::json!({}), |ev| match ev {
            JobEvent::Log { message, .. } => {
                eprintln!("{}", message);
            }
            JobEvent::Result { data } => {
                match serde_json::from_value::<StatsReport>(data.clone()) {
                    Ok(r) => typed = Some(r),
                    Err(e) => decode_err = Some(e.to_string()),
                }
                raw = Some(data);
            }
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .context("stats job stream")?;

    if json {
        match raw {
            Some(v) => println!("{}", serde_json::to_string_pretty(&v)?),
            None => {
                eprintln!("error: daemon returned no stats report");
                return Ok(2);
            }
        }
    } else if let Some(report) = typed {
        print_human(&report);
    } else {
        // No decodable report — surface the error to stderr and return a
        // non-zero exit instead of printing nothing and exiting 0, which
        // masked any daemon-side StatsReport shape drift as a "successful"
        // empty run (issue #291). Mirrors verify.rs.
        match decode_err {
            Some(e) => eprintln!("error: failed to decode stats report: {e}"),
            None => eprintln!("error: daemon returned no stats report"),
        }
        return Ok(2);
    }

    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

use shared_cli_system::fmt::{fmt_bytes, fmt_ratio};

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
    println!("chunks evicted to storage-only count toward page totals but are");
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

#[cfg(test)]
mod tests {
    use super::*;

    // fmt_bytes / fmt_ratio moved to `shared_cli_system::fmt`; their
    // unit tests live there now.

    /// A populated report deserializes from the daemon's JSON shape and
    /// renders without panicking through every print branch.
    #[test]
    fn stats_report_round_trips_and_prints() {
        let json = serde_json::json!({
            "backends": [{
                "backend": "primary",
                "volumes_global": 1,
                "volumes_local": 2,
                "allocated_pages": 30,
                "logical_bytes": 3_000_000u64,
                "unique_pool_bytes": 1_000_000u64,
            }],
            "volumes": [{
                "volume": "vol-a",
                "backend": "primary",
                "scope": "local",
                "allocated_pages": 10,
                "logical_bytes": 1_000_000u64,
                "volume_unique_bytes": 800_000u64,
                "exclusive_bytes": 600_000u64,
                "shared_bytes": 200_000u64,
            }],
            "skipped": [{ "volume": "vol-bad", "reason": "manifest unreadable" }],
        });
        let report: StatsReport =
            serde_json::from_value(json).expect("daemon stats shape deserializes");
        assert_eq!(report.backends.len(), 1);
        assert_eq!(report.volumes.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        // Exercises the populated-backend + per-volume + skipped branches.
        print_human(&report);
    }

    /// An empty report exercises the "No volumes found" early-return path.
    #[test]
    fn empty_report_prints_no_volumes() {
        let report = StatsReport {
            backends: vec![],
            volumes: vec![],
            skipped: vec![],
        };
        print_human(&report);
        // Empty backends + a skip entry: the "No volumes found" branch
        // still surfaces the skipped list.
        let report = StatsReport {
            backends: vec![],
            volumes: vec![],
            skipped: vec![SkippedVolume {
                volume: "v".into(),
                reason: "io error".into(),
            }],
        };
        print_human(&report);
    }

    #[test]
    fn default_backend_stats_zeroes_out() {
        let b = BackendStats::default();
        assert_eq!(b.backend, "");
        assert_eq!(b.allocated_pages, 0);
        assert_eq!(b.logical_bytes, 0);
        assert_eq!(b.unique_pool_bytes, 0);
    }
}
