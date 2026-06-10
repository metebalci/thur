// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Orphan-chunk sweeps: one at boot, then periodic (issue #107).
//!
//! Each sweep walks every cartridge under `<data_dir>/tapes/`, finds
//! entries in `chunks.idx` that are sealed (`hash.is_some()`) but not
//! uploaded (`!uploaded`), and re-queues them for upload via the
//! existing [`UploadRequest`] mpsc. The upload worker treats a
//! recovery request the same as a live `on_cartridge_unloaded` flush
//! — its `pending_upload_payload` filter already skips
//! already-uploaded and unsealed ids, and the worker processes
//! requests sequentially against a fresh cartridge open, so a sweep
//! request that races a live dispatch for the same chunk filters out
//! instead of double-PUTting. The request is naturally idempotent.
//!
//! Why this exists. The upload pipeline is event-driven and
//! fire-and-forget: a chunk seal publishes onto a broadcast bus
//! consumed by `MemoryBufferManager`, which queues an
//! `UploadRequest` and forgets the chunk at dispatch — PUT outcomes
//! never feed back into the manager. Two things therefore strand a
//! sealed chunk at `uploaded=false` with no event left to re-drive
//! it: a daemon killed mid-PUT (the seal event was consumed and is
//! not replayed on restart), and a PUT that fails after the
//! backend's retry budget (issue #107). The boot sweep catches the
//! first; the periodic sweep catches the second, so a transient
//! backend outage heals within [`PERIODIC_SWEEP_INTERVAL`] instead
//! of persisting until the next daemon restart. Stranded chunks are
//! safe meanwhile — eviction skips `uploaded=false` — but the
//! storage copy is the DR source of truth and must converge.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use core_mediachanger::chunk_index::ChunkIndexFile;
use core_mediachanger::{AuditActor, AuditChannel, AuditResult};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::memory_buffer_manager::UploadRequest;

struct CartridgeOrphans {
    tape_id: String,
    chunk_ids: Vec<u32>,
}

/// How often the periodic sweep re-walks `chunks.idx` after boot.
/// Ten minutes bounds the healing latency after a transient backend
/// outage; the scan itself is index-file preads, cheap enough that
/// no opt-out knob is warranted.
const PERIODIC_SWEEP_INTERVAL: Duration = Duration::from_secs(600);

/// What initiated a sweep — controls audit verbosity. The boot scan
/// always writes its start/completed audit pair (one per daemon
/// start, as documented in SPEC.md § Audit Log); a periodic sweep
/// writes a completed row only when it actually found and re-queued
/// something, so the quiet steady state doesn't add ~144 no-op rows
/// a day to the audit chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SweepTrigger {
    Boot,
    Periodic,
}

impl SweepTrigger {
    fn as_str(self) -> &'static str {
        match self {
            SweepTrigger::Boot => "boot",
            SweepTrigger::Periodic => "periodic",
        }
    }
}

fn should_audit(trigger: SweepTrigger, orphans_found: usize) -> bool {
    trigger == SweepTrigger::Boot || orphans_found > 0
}

/// Run the boot sweep immediately, then keep sweeping every
/// [`PERIODIC_SWEEP_INTERVAL`] until the upload worker goes away
/// (daemon shutdown). Spawned once from `main`.
pub async fn run_orphan_sweeps(
    data_dir: PathBuf,
    upload_tx: mpsc::Sender<UploadRequest>,
    audit: Option<AuditChannel>,
) {
    scan_and_enqueue_orphans(
        data_dir.clone(),
        upload_tx.clone(),
        audit.clone(),
        SweepTrigger::Boot,
    )
    .await;
    loop {
        tokio::time::sleep(PERIODIC_SWEEP_INTERVAL).await;
        if upload_tx.is_closed() {
            debug!("Orphan upload sweep: upload worker gone - stopping periodic sweeps");
            return;
        }
        scan_and_enqueue_orphans(
            data_dir.clone(),
            upload_tx.clone(),
            audit.clone(),
            SweepTrigger::Periodic,
        )
        .await;
    }
}

/// Scan `<data_dir>/tapes/` for cartridges with sealed-but-not-uploaded
/// chunks and dispatch one `UploadRequest` per cartridge to the upload
/// worker. Survives transient errors per cartridge; emits audit events
/// (gated by `trigger` — see [`SweepTrigger`]) and a duration
/// histogram sample.
pub async fn scan_and_enqueue_orphans(
    data_dir: PathBuf,
    upload_tx: mpsc::Sender<UploadRequest>,
    audit: Option<AuditChannel>,
    trigger: SweepTrigger,
) {
    let started = Instant::now();
    let tapes_root = data_dir.join("tapes");

    let read_dir = match std::fs::read_dir(&tapes_root) {
        Ok(d) => d,
        Err(e) => {
            debug!(
                "Orphan upload scan: tapes root {} not readable ({e}) - nothing to recover",
                tapes_root.display()
            );
            return;
        }
    };

    let mut all_orphans: Vec<CartridgeOrphans> = Vec::new();
    let mut cartridges_scanned: usize = 0;
    for entry in read_dir {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(tape_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        cartridges_scanned += 1;
        match scan_cartridge(&path, &tape_id) {
            Ok(Some(orphans)) => all_orphans.push(orphans),
            Ok(None) => {}
            Err(e) => warn!("Orphan upload scan: cartridge '{tape_id}' read failed: {e}"),
        }
    }

    let orphans_found: usize = all_orphans.iter().map(|c| c.chunk_ids.len()).sum();
    let audit_gated = audit
        .as_ref()
        .filter(|_| should_audit(trigger, orphans_found));

    if let Some(a) = audit_gated {
        a.try_append(
            "storage.orphan_scan_started",
            AuditActor::system(),
            json!({
                "cartridges_scanned": cartridges_scanned,
                "trigger": trigger.as_str(),
            }),
            AuditResult::Ok,
        );
    }

    if orphans_found == 0 {
        if trigger == SweepTrigger::Boot {
            info!(
                "Orphan upload scan: {} cartridges scanned, no orphans found",
                cartridges_scanned
            );
        } else {
            debug!(
                "Orphan upload sweep: {} cartridges scanned, no orphans found",
                cartridges_scanned
            );
        }
        finalize(started, audit_gated, trigger, orphans_found, 0);
        return;
    }

    info!(
        "Orphan upload scan: {} orphan chunks across {} of {} cartridges - re-queuing",
        orphans_found,
        all_orphans.len(),
        cartridges_scanned,
    );

    let mut orphans_requeued: usize = 0;
    for c in all_orphans {
        let count = c.chunk_ids.len();
        let request = UploadRequest {
            tape_id: c.tape_id.clone(),
            chunk_ids: c.chunk_ids,
        };
        match upload_tx.send(request).await {
            Ok(()) => {
                orphans_requeued += count;
                debug!(
                    "Orphan upload scan: queued {count} chunk(s) for cartridge {}",
                    c.tape_id
                );
            }
            Err(e) => {
                warn!(
                    "Orphan upload scan: send to upload worker failed for {}: {e}",
                    c.tape_id
                );
            }
        }
    }

    info!(
        "Orphan upload scan: completed in {:.2}s - {} chunk(s) queued for re-upload",
        started.elapsed().as_secs_f64(),
        orphans_requeued,
    );

    finalize(
        started,
        audit_gated,
        trigger,
        orphans_found,
        orphans_requeued,
    );
}

fn finalize(
    started: Instant,
    audit: Option<&AuditChannel>,
    trigger: SweepTrigger,
    orphans_found: usize,
    orphans_requeued: usize,
) {
    let elapsed = started.elapsed().as_secs_f64();
    shared_telemetry::record::orphan_scan_completed(orphans_found as u64, elapsed);
    if let Some(a) = audit {
        a.try_append(
            "storage.orphan_scan_completed",
            AuditActor::system(),
            json!({
                "orphans_found": orphans_found,
                "orphans_requeued": orphans_requeued,
                "duration_seconds": elapsed,
                "trigger": trigger.as_str(),
            }),
            AuditResult::Ok,
        );
    }
}

fn scan_cartridge(cartridge_root: &Path, tape_id: &str) -> Result<Option<CartridgeOrphans>> {
    let idx_path = ChunkIndexFile::path_for(cartridge_root);
    if !idx_path.exists() {
        debug!("Orphan upload scan: '{tape_id}' has no chunks.idx - skipping");
        return Ok(None);
    }
    let idx = ChunkIndexFile::open_or_create(cartridge_root)?;
    let mut orphans: Vec<u32> = Vec::new();
    for entry in idx.iter() {
        let (id, rec) = entry?;
        if rec.hash.is_some() && !rec.uploaded {
            // The upload worker's `UploadRequest.chunk_ids` is `Vec<u32>`;
            // chunk ids past u32::MAX cannot ride that channel. In
            // practice chunks.idx never grows past a few million per
            // cartridge, so a u32 cap is safe to enforce here.
            if let Ok(id_u32) = u32::try_from(id) {
                orphans.push(id_u32);
            } else {
                warn!(
                    "Orphan upload scan: cartridge '{tape_id}' chunk id {id} exceeds u32 - skipping",
                );
            }
        }
    }
    if orphans.is_empty() {
        Ok(None)
    } else {
        // chunks.idx::iter() yields in insertion order; preserve it so
        // the upload worker processes oldest-first (same convention
        // `MemoryBufferManager::trigger_upload_batch` uses).
        Ok(Some(CartridgeOrphans {
            tape_id: tape_id.to_string(),
            chunk_ids: orphans,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_mediachanger::chunk_index::{ChunkRec, LocationTag};
    use tempfile::TempDir;

    fn write_chunk(idx: &ChunkIndexFile, hash: Option<&str>, uploaded: bool) -> u64 {
        let rec = ChunkRec {
            size: 64,
            hash: hash.map(String::from),
            location: if uploaded {
                LocationTag::Both
            } else {
                LocationTag::LocalOnly
            },
            uploaded,
            compression: None,
        };
        idx.append(&rec).unwrap()
    }

    fn fake_hash(n: u8) -> String {
        // 32-byte BLAKE3 = 64 hex chars.
        let mut s = String::with_capacity(64);
        for _ in 0..64 {
            s.push(char::from_digit((n & 0x0f) as u32, 16).unwrap());
        }
        s
    }

    #[test]
    fn scan_picks_only_sealed_unuploaded() {
        let tmp = TempDir::new().unwrap();
        let cart = tmp.path().join("tape1");
        std::fs::create_dir_all(&cart).unwrap();

        let idx = ChunkIndexFile::open_or_create(&cart).unwrap();
        let _staging = write_chunk(&idx, None, false); // unsealed
        let orphan = write_chunk(&idx, Some(&fake_hash(0xa)), false); // orphan
        let _uploaded = write_chunk(&idx, Some(&fake_hash(0xb)), true); // done

        let result = scan_cartridge(&cart, "tape1").unwrap().unwrap();
        assert_eq!(result.tape_id, "tape1");
        assert_eq!(result.chunk_ids, vec![orphan as u32]);
    }

    #[test]
    fn scan_returns_none_for_empty_or_all_uploaded() {
        let tmp = TempDir::new().unwrap();
        let cart = tmp.path().join("tape2");
        std::fs::create_dir_all(&cart).unwrap();

        let idx = ChunkIndexFile::open_or_create(&cart).unwrap();
        let _ = write_chunk(&idx, Some(&fake_hash(0x1)), true);
        let _ = write_chunk(&idx, Some(&fake_hash(0x2)), true);

        assert!(scan_cartridge(&cart, "tape2").unwrap().is_none());
    }

    #[test]
    fn scan_returns_none_when_no_chunks_idx() {
        let tmp = TempDir::new().unwrap();
        let cart = tmp.path().join("empty");
        std::fs::create_dir_all(&cart).unwrap();
        assert!(scan_cartridge(&cart, "empty").unwrap().is_none());
    }

    #[tokio::test]
    async fn scan_and_enqueue_dispatches_one_request_per_cartridge() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let tapes_root = data_dir.join("tapes");
        std::fs::create_dir_all(&tapes_root).unwrap();

        // Cartridge A: 2 orphans, 1 uploaded
        let a = tapes_root.join("tapeA");
        std::fs::create_dir_all(&a).unwrap();
        {
            let idx = ChunkIndexFile::open_or_create(&a).unwrap();
            write_chunk(&idx, Some(&fake_hash(0x1)), false);
            write_chunk(&idx, Some(&fake_hash(0x2)), false);
            write_chunk(&idx, Some(&fake_hash(0x3)), true);
        }
        // Cartridge B: all uploaded — no request expected
        let b = tapes_root.join("tapeB");
        std::fs::create_dir_all(&b).unwrap();
        {
            let idx = ChunkIndexFile::open_or_create(&b).unwrap();
            write_chunk(&idx, Some(&fake_hash(0x4)), true);
        }

        let (tx, mut rx) = mpsc::channel::<UploadRequest>(16);
        scan_and_enqueue_orphans(data_dir, tx, None, SweepTrigger::Boot).await;

        let req = rx.recv().await.expect("one request from tapeA");
        assert_eq!(req.tape_id, "tapeA");
        assert_eq!(req.chunk_ids.len(), 2);
        // No more requests.
        assert!(rx.try_recv().is_err());
    }

    /// A periodic sweep must dispatch exactly like the boot scan —
    /// the trigger only changes audit verbosity, never coverage.
    #[tokio::test]
    async fn periodic_sweep_dispatches_same_as_boot() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let cart = data_dir.join("tapes").join("tapeC");
        std::fs::create_dir_all(&cart).unwrap();
        {
            let idx = ChunkIndexFile::open_or_create(&cart).unwrap();
            write_chunk(&idx, Some(&fake_hash(0x7)), false);
        }

        let (tx, mut rx) = mpsc::channel::<UploadRequest>(16);
        scan_and_enqueue_orphans(data_dir, tx, None, SweepTrigger::Periodic).await;

        let req = rx
            .recv()
            .await
            .expect("periodic sweep re-queues the orphan");
        assert_eq!(req.tape_id, "tapeC");
        assert_eq!(req.chunk_ids.len(), 1);
    }

    /// Audit gating: boot always audits; a periodic sweep audits only
    /// when it found something — the quiet steady state must not add
    /// no-op rows to the audit chain.
    #[test]
    fn audit_gating_by_trigger_and_findings() {
        assert!(should_audit(SweepTrigger::Boot, 0));
        assert!(should_audit(SweepTrigger::Boot, 3));
        assert!(!should_audit(SweepTrigger::Periodic, 0));
        assert!(should_audit(SweepTrigger::Periodic, 3));
    }
}
