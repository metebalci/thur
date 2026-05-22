// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvsa system verify` — daemon-routed volume consistency check.
//!
//! The daemon owns the on-disk state during operation; routing the
//! walk through the admin socket means verify always sees a
//! synchronized view. The CLI streams the daemon's events,
//! deserializes the structured `VolumeVerifyReport` out of the
//! terminal Result line, and renders human / json output.
//!
//! Exit codes:
//!   0 — clean
//!   1 — inconsistencies found (errors > 0)
//!   2 — fatal scan failure / transport error

use anyhow::{Context, Result};
use core_block::verify::VolumeVerifyReport;

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;

pub async fn cmd_verify(
    skip_cloud: bool,
    verbose: bool,
    json: bool,
    volumes: Vec<String>,
) -> Result<u8> {
    let client = AdminClient::auto_discover(&shared_naming::DISK);
    let body = serde_json::json!({
        "skip_cloud": skip_cloud,
        "volumes": volumes,
    });

    let mut report: Option<VolumeVerifyReport> = None;
    let exit = client
        .run_job("system.verify", &body, |ev| match ev {
            // Log lines always go to stderr so a redirected stdout
            // carries only the json report (when `--json`).
            JobEvent::Log { message, .. } => eprintln!("{}", message),
            JobEvent::Result { data } => match serde_json::from_value::<VolumeVerifyReport>(data) {
                Ok(r) => report = Some(r),
                Err(e) => eprintln!("warning: failed to decode verify report: {}", e),
            },
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .context("verify job stream")?;

    let report = match report {
        Some(r) => r,
        None => return Ok(u8::try_from(exit.max(0)).unwrap_or(2)),
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize verify report")?
        );
    } else {
        print_human(&report, verbose);
    }

    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

fn print_human(r: &VolumeVerifyReport, verbose: bool) {
    println!("Volumes ({} scanned)", r.volumes.len());
    for v in &r.volumes {
        let mark = if !v.errors.is_empty() {
            "ERR "
        } else if !v.warnings.is_empty() {
            "WARN"
        } else {
            "OK  "
        };
        println!(
            "  [{}] {}  backend={}  scope={}  pages={}",
            mark,
            v.volume,
            v.backend.as_deref().unwrap_or("?"),
            v.scope.as_deref().unwrap_or("?"),
            v.allocated_pages,
        );
        if !v.pages_idx_ok {
            println!("      pages.idx: INTEGRITY CHECK FAILED");
        }
        if v.local_chunks_missing > 0 {
            println!("      missing from local pool: {}", v.local_chunks_missing);
        }
        if let Some(missing) = v.cloud_chunks_missing
            && missing > 0
        {
            println!("      missing from cloud: {}", missing);
        }
        if verbose {
            for e in &v.errors {
                println!("      ERROR: {}", e);
            }
            for w in &v.warnings {
                println!("      WARN:  {}", w);
            }
        } else {
            for e in v.errors.iter().take(3) {
                println!("      ERROR: {}", e);
            }
            if v.errors.len() > 3 {
                println!(
                    "      ... {} more errors (use --verbose)",
                    v.errors.len() - 3
                );
            }
        }
    }
    println!();

    println!("Chunk pools ({} backend(s))", r.pool.len());
    for p in &r.pool {
        println!(
            "  backend={}  shared_chunks={}  shared_orphans={} ({} bytes)  namespaces={}",
            p.backend,
            p.shared_chunks,
            p.shared_orphans,
            p.shared_orphan_bytes,
            p.namespaces.len(),
        );
        if let Some(cp) = &p.cloud {
            println!(
                "      cloud: chunk_objects={} chunk_orphans={}",
                cp.chunk_objects, cp.chunk_orphans
            );
        }
        for hint in &p.gc_hints {
            println!("      GC hint: {}", hint);
        }
        if verbose {
            for ns in &p.namespaces {
                println!(
                    "      namespace {}: chunks={} orphans={} ({} bytes)",
                    ns.namespace, ns.chunks, ns.orphans, ns.orphan_bytes
                );
            }
            for orphan_dir in &p.orphan_namespace_dirs {
                println!("      orphan namespace dir: {}", orphan_dir);
            }
        }
        for e in &p.errors {
            println!("      ERROR: {}", e);
        }
        for w in &p.warnings {
            println!("      WARN:  {}", w);
        }
    }
    println!();

    println!(
        "Result: {} error(s), {} warning(s), {} GC hint(s)",
        r.error_count(),
        r.warning_count(),
        r.gc_hint_count(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::verify::{CloudReport, NamespaceReport, PoolReport, VolumeReport};

    /// A clean, empty report renders the header + result lines without
    /// touching any error/warning branch.
    #[test]
    fn empty_report_prints() {
        let r = VolumeVerifyReport::default();
        assert_eq!(r.error_count(), 0);
        assert_eq!(r.warning_count(), 0);
        assert_eq!(r.gc_hint_count(), 0);
        print_human(&r, false);
        print_human(&r, true);
    }

    /// A report exercising every print branch: volume with errors +
    /// warnings, failed pages.idx, missing local + cloud chunks, a pool
    /// with namespaces, cloud counters, GC hints, and orphan dirs.
    #[test]
    fn populated_report_prints_all_branches() {
        let mut vol = VolumeReport {
            volume: "vol-a".into(),
            backend: Some("primary".into()),
            scope: Some("local".into()),
            pages_idx_ok: false,
            allocated_pages: 12,
            local_chunks_missing: 3,
            cloud_chunks_missing: Some(2),
            ..Default::default()
        };
        // More than three errors so the "... N more" branch fires.
        for i in 0..5 {
            vol.errors.push(format!("error number {i}"));
        }
        vol.warnings.push("a warning".into());

        let pool = PoolReport {
            backend: "primary".into(),
            shared_chunks: 100,
            shared_orphans: 4,
            shared_orphan_bytes: 4096,
            namespaces: vec![NamespaceReport {
                namespace: "ns1".into(),
                chunks: 50,
                orphans: 2,
                orphan_bytes: 2048,
            }],
            orphan_namespace_dirs: vec!["stale-ns".into()],
            gc_hints: vec!["run gc to reclaim 4096 bytes".into()],
            cloud: Some(CloudReport {
                chunk_objects: 200,
                chunk_orphans: 5,
            }),
            errors: vec!["pool error".into()],
            warnings: vec!["pool warning".into()],
        };

        let report = VolumeVerifyReport {
            volumes: vec![vol],
            pool: vec![pool],
        };

        assert_eq!(report.error_count(), 6); // 5 vol + 1 pool
        assert_eq!(report.warning_count(), 2);
        assert_eq!(report.gc_hint_count(), 1);

        // Non-verbose truncates the error list; verbose dumps everything.
        print_human(&report, false);
        print_human(&report, true);
    }

    /// The CLI's `VolumeVerifyReport` arrives over the job stream as
    /// JSON — confirm the daemon's serialized shape deserializes.
    #[test]
    fn report_round_trips_through_json() {
        let report = VolumeVerifyReport {
            volumes: vec![VolumeReport {
                volume: "v".into(),
                pages_idx_ok: true,
                ..Default::default()
            }],
            pool: vec![],
        };
        let json = serde_json::to_value(&report).expect("serialize");
        let back: VolumeVerifyReport = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.volumes.len(), 1);
        assert_eq!(back.volumes[0].volume, "v");
    }
}
