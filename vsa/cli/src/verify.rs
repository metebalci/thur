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
