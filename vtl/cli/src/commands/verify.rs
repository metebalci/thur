// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl system verify` — daemon-routed library consistency
//! check.
//!
//! The daemon owns the on-disk state during operation; routing the
//! walk through the admin socket means verify always sees a
//! synchronized view (no `--allow-running` escape hatch needed) and
//! the audit trail picks up every check natively. The CLI streams
//! the daemon's events, deserializes the structured `VerifyReport`
//! out of the terminal Result line, and renders the same human/json
//! output it always did.
//!
//! Exit codes mirror the previous CLI:
//!   0 — clean
//!   1 — inconsistencies found (errors > 0)
//!   2 — fatal scan failure / transport error

use anyhow::{Context, Result};
use core_mediachanger::verify::VerifyReport;

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;

pub async fn cmd_verify(
    skip_storage: bool,
    verbose: bool,
    json: bool,
    barcodes: Vec<String>,
) -> Result<u8> {
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = serde_json::json!({
        "skip_storage": skip_storage,
        "barcodes": barcodes,
    });

    let mut report: Option<VerifyReport> = None;
    let exit = client
        .run_job("system.verify", &body, |ev| match ev {
            // All log lines go to stderr so that in json mode a
            // redirected stdout carries only the final structured
            // report.
            JobEvent::Log { message, .. } => eprintln!("{}", message),
            JobEvent::Result { data } => match serde_json::from_value::<VerifyReport>(data) {
                Ok(r) => report = Some(r),
                Err(e) => eprintln!("warning: failed to decode verify report: {}", e),
            },
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .with_context(|| "verify job stream")?;

    let report = match report {
        Some(r) => r,
        None => {
            // No structured payload — bail with the daemon's exit code
            // (already logged via stderr above).
            return Ok(u8::try_from(exit.max(0)).unwrap_or(2));
        }
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

fn print_human(r: &VerifyReport, verbose: bool) {
    println!("Library");
    if r.library.library_json_present {
        println!(
            "  library.json: {} slots, {} mail-slots, {} drives",
            r.library.num_storage_slots, r.library.num_mail_slots, r.library.num_drives
        );
    } else {
        println!("  library.json: MISSING");
    }
    println!(
        "  inventory.json: {}",
        if r.library.inventory_json_present {
            "present"
        } else {
            "MISSING"
        }
    );
    if !r.library.missing_cartridges.is_empty() {
        println!(
            "  missing cartridges (in inventory but not on disk): {}",
            r.library.missing_cartridges.join(", ")
        );
    }
    if !r.library.orphan_cartridges.is_empty() {
        println!(
            "  orphan cartridges (on disk but not in inventory): {}",
            r.library.orphan_cartridges.join(", ")
        );
    }
    for e in &r.library.errors {
        println!("  ERROR: {}", e);
    }
    for w in &r.library.warnings {
        println!("  WARN:  {}", w);
    }
    println!();

    println!("Cartridges ({} scanned)", r.cartridges.len());
    for c in &r.cartridges {
        let mark = if !c.errors.is_empty() {
            "ERR "
        } else if !c.warnings.is_empty() {
            "WARN"
        } else {
            "OK  "
        };
        println!(
            "  [{}] {}  backend={}  dedup={}  records={}  hashed={}",
            mark,
            c.dir,
            c.backend.as_deref().unwrap_or("?"),
            c.dedup.as_deref().unwrap_or("?"),
            c.chunks_idx_records,
            c.chunks_with_hash,
        );
        if c.local_chunks_missing > 0 {
            println!("      missing from local pool: {}", c.local_chunks_missing);
        }
        if c.local_chunks_size_mismatch > 0 {
            println!(
                "      size mismatch (chunks.idx vs pool): {}",
                c.local_chunks_size_mismatch
            );
        }
        if let Some(missing) = c.storage_chunks_missing
            && missing > 0
        {
            println!("      missing from storage: {}", missing);
        }
        if let Some(missing_pages) = c.storage_index_pages_missing
            && missing_pages > 0
        {
            println!("      storage index pages missing: {}", missing_pages);
        }
        if let Some(false) = c.storage_sentinel_present {
            println!("      storage sentinel manifest-latest.json: MISSING");
        }
        let oob: u64 = c.partitions.iter().map(|p| p.chunk_id_oob).sum();
        let off: u64 = c.partitions.iter().map(|p| p.offset_oob).sum();
        if oob > 0 {
            println!(
                "      block records with chunk_id past chunks.idx tail: {}",
                oob
            );
        }
        if off > 0 {
            println!(
                "      block records with offset+len past chunk size: {}",
                off
            );
        }
        if verbose {
            for p in &c.partitions {
                println!(
                    "      P{}: records={} data={} filemarks={}",
                    p.partition, p.records, p.data_blocks, p.filemarks
                );
            }
            for e in &c.errors {
                println!("      ERROR: {}", e);
            }
            for w in &c.warnings {
                println!("      WARN:  {}", w);
            }
        } else {
            for e in c.errors.iter().take(3) {
                println!("      ERROR: {}", e);
            }
            if c.errors.len() > 3 {
                println!(
                    "      ... {} more errors (use --verbose)",
                    c.errors.len() - 3
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
        if let Some(cp) = &p.storage {
            println!(
                "      storage: chunk_objects={} chunk_orphans={} index_page_orphans={}",
                cp.chunk_objects, cp.chunk_orphans, cp.index_page_orphans
            );
        }
        for hint in &p.gc_hints {
            println!("      GC hint: {}", hint);
        }
        if verbose {
            for ns in &p.namespaces {
                println!(
                    "      namespace {}: chunks={} orphans={} ({} bytes)",
                    ns.barcode, ns.chunks, ns.orphans, ns.orphan_bytes
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

    #[test]
    fn print_human_handles_empty_report() {
        let report = VerifyReport::default();
        // No cartridges, no pools, library.json absent — exercises
        // the MISSING branches.
        print_human(&report, false);
        assert_eq!(report.error_count(), 0);
    }

    #[test]
    fn print_human_renders_present_library_and_cartridges() {
        // Build a populated report via JSON so the test doesn't have
        // to enumerate every field of the (large) report structs.
        let report: VerifyReport = serde_json::from_value(serde_json::json!({
            "library": {
                "library_json_present": true,
                "inventory_json_present": true,
                "num_storage_slots": 40,
                "num_mail_slots": 5,
                "num_drives": 3,
                "missing_cartridges": ["GHOST1"],
                "orphan_cartridges": ["ORPHAN1"],
                "errors": ["a library error"],
                "warnings": ["a library warning"],
            },
            "cartridges": [{
                "dir": "TAPE001",
                "label": "TAPE001",
                "backend": "s3b",
                "dedup": "global",
                "manifest_ok": true,
                "chunks_idx_present": true,
                "chunks_idx_records": 10,
                "chunks_with_hash": 10,
                "partitions": [],
                "local_chunks_missing": 0,
                "local_chunks_size_mismatch": 0,
                "storage_chunks_missing": null,
                "storage_index_pages_missing": null,
                "storage_sentinel_present": null,
                "errors": ["e1", "e2", "e3", "e4"],
                "warnings": [],
            }],
            "pool": [],
        }))
        .expect("populated verify report");
        // verbose=false caps printed errors at 3.
        print_human(&report, false);
        // verbose=true prints partitions + all errors.
        print_human(&report, true);
        assert_eq!(report.error_count(), 5);
    }

    #[test]
    fn verify_report_round_trips_through_json() {
        let report = VerifyReport::default();
        let json = serde_json::to_value(&report).expect("serialize");
        let back: VerifyReport = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.cartridges.len(), 0);
    }
}
