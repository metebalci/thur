// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `--test` mode in-process smoke harness for thurvsad.
//!
//! Mirrors `vtl-daemon`'s `--test` smoke: spins up the SBC stack
//! against a private tempdir (own volume, own local storage backend),
//! dispatches a fixed series of SCSI commands through the live
//! `SbcScsiDispatcher`, and exits non-zero if anything misbehaves.
//! Operator's `<data_dir>` and `storage-backends.json` are not touched.
//!
//! Coverage today:
//! - volume bring-up + REPORT LUNS + INQUIRY identity
//! - WRITE 16 / READ 16 round trip, single + multi-page
//! - SYNCHRONIZE CACHE 16 fence semantics
//! - unallocated-page read returns zeros (sparse provisioning)

use std::sync::Arc;

use anyhow::{Result, anyhow};
use tempfile::TempDir;
use tracing::{info, warn};

use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
use scsi_sbc::{SbcScsiDispatcher, VolumeLookup};
use scsi_spc::scsi::ScsiRequest;
use shared_object_store::{LocalBackend, ObjectStoreBackend};

use crate::registry::VolumeRegistry;

const SMOKE_VOLUME_SIZE: u64 = 4 * (1u64 << 20); // 4 MiB
const SECTOR: usize = DEFAULT_SECTOR_BYTES as usize;
const PAGE: usize = DEFAULT_PAGE_SIZE_BYTES as usize;
const SECTORS_PER_PAGE: u32 = (PAGE / SECTOR) as u32;

/// Run every smoke test in sequence. Returns Err if any sub-test
/// failed so the daemon exits non-zero. Logs a per-test pass / fail
/// line so a runner script can grep results without parsing exit
/// codes per phase.
pub(crate) async fn run_all() -> Result<()> {
    info!("thurvsa --test mode: running in-process smoke harness");

    let mut all_passed = true;

    info!(">>> Running volume bring-up smoke test...");
    if let Err(e) = run_bringup_smoke_test().await {
        warn!("Volume bring-up smoke test failed: {e:?}");
        all_passed = false;
    } else {
        info!("Volume bring-up smoke test passed");
    }

    info!(">>> Running data-path round-trip smoke test...");
    if let Err(e) = run_data_path_smoke_test().await {
        warn!("Data-path smoke test failed: {e:?}");
        all_passed = false;
    } else {
        info!("Data-path smoke test passed");
    }

    info!(">>> Running SYNCHRONIZE CACHE smoke test...");
    if let Err(e) = run_sync_cache_smoke_test().await {
        warn!("SYNCHRONIZE CACHE smoke test failed: {e:?}");
        all_passed = false;
    } else {
        info!("SYNCHRONIZE CACHE smoke test passed");
    }

    info!(">>> Running sparse-page read smoke test...");
    if let Err(e) = run_sparse_read_smoke_test().await {
        warn!("Sparse-page read smoke test failed: {e:?}");
        all_passed = false;
    } else {
        info!("Sparse-page read smoke test passed");
    }

    info!("{}", "=".repeat(60));
    if all_passed {
        info!("All smoke tests passed!");
        Ok(())
    } else {
        Err(anyhow!("Some smoke tests failed"))
    }
}

/// Bring up an isolated single-volume registry in a tempdir, backed
/// by a LocalBackend. Returns the dispatcher + TempDir guard so the
/// caller can keep the on-disk state alive for the duration of the
/// test.
async fn build_smoke_dispatcher() -> Result<(SbcScsiDispatcher, TempDir)> {
    let tmp = TempDir::new()?;
    let data_dir = tmp.path().to_path_buf();
    let storage_root = data_dir.join("storage");
    std::fs::create_dir_all(&storage_root)?;

    let backend = LocalBackend::new(&storage_root)
        .await
        .map_err(|e| anyhow!("LocalBackend init failed: {e}"))?;
    let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

    VolumeManifest::new(
        "SMOKE-VOL-1".to_string(),
        SMOKE_VOLUME_SIZE,
        DEFAULT_SECTOR_BYTES,
        DEFAULT_PAGE_SIZE_BYTES,
        "primary".to_string(),
        DedupScope::Local,
        false,
        0,
    )
    .map_err(|e| anyhow!("VolumeManifest::new failed: {e}"))?
    .create(&data_dir)
    .map_err(|e| anyhow!("VolumeManifest::create failed: {e}"))?;

    let writer = Arc::new(
        VolumeWriter::open(&data_dir, "SMOKE-VOL-1", backend)
            .map_err(|e| anyhow!("VolumeWriter::open failed: {e}"))?,
    );
    let cache = PageCache::new(writer);

    let registry = VolumeRegistry::new();
    registry.register(0, cache);

    let dispatcher = SbcScsiDispatcher::new(
        Arc::new(registry) as Arc<dyn VolumeLookup>,
        scsi_sbc::ISCSI_DISK_TARGET_IQN.to_string(),
    );
    Ok((dispatcher, tmp))
}

fn write16_cdb(lba: u64, blocks: u32) -> Vec<u8> {
    let mut cdb = vec![0u8; 16];
    cdb[0] = 0x8A;
    cdb[2..10].copy_from_slice(&lba.to_be_bytes());
    cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
    cdb
}

fn read16_cdb(lba: u64, blocks: u32) -> Vec<u8> {
    let mut cdb = vec![0u8; 16];
    cdb[0] = 0x88;
    cdb[2..10].copy_from_slice(&lba.to_be_bytes());
    cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
    cdb
}

fn sync16_cdb(lba: u64, blocks: u32) -> Vec<u8> {
    let mut cdb = vec![0u8; 16];
    cdb[0] = 0x91;
    cdb[2..10].copy_from_slice(&lba.to_be_bytes());
    cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
    cdb
}

fn req<'a>(lun: u64, cdb: &'a [u8], data_out: &[u8], data_in_max: usize) -> ScsiRequest<'a> {
    ScsiRequest {
        lun,
        cdb,
        data_out: data_out.to_vec(),
        data_in_max,
        tsih: 0,
        initiator_iqn: None,
        initiator_isid: [0u8; 6],
        cid: 0,
        peer: "smoke",
        session_partition: None,
        session_volumes: None,
    }
}

/// Volume bring-up: INQUIRY against LUN 0 returns vendor 'MB' and
/// REPORT LUNS lists the smoke volume.
async fn run_bringup_smoke_test() -> Result<()> {
    let (dispatcher, _tmp) = build_smoke_dispatcher().await?;

    // INQUIRY (0x12), allocation length 96 bytes (standard data 36 +
    // padding — daemon answers with the standard 36-byte block).
    let inq = [0x12u8, 0, 0, 0x00, 0x60, 0];
    let resp = dispatcher.dispatch(req(0, &inq, &[], 4096)).await;
    if resp.sense.is_some() {
        return Err(anyhow!("INQUIRY returned sense: {:?}", resp.sense));
    }
    if &resp.data_in[8..16] != b"MB      " {
        return Err(anyhow!(
            "INQUIRY vendor mismatch: got {:?}",
            std::str::from_utf8(&resp.data_in[8..16])
        ));
    }
    info!("SMOKE: INQUIRY vendor='MB' ok");

    // REPORT LUNS (0xA0), allocation length 256 bytes.
    let mut rl = vec![0u8; 12];
    rl[0] = 0xA0;
    rl[6..10].copy_from_slice(&256u32.to_be_bytes());
    let resp = dispatcher.dispatch(req(0, &rl, &[], 256)).await;
    if resp.sense.is_some() {
        return Err(anyhow!("REPORT LUNS returned sense: {:?}", resp.sense));
    }
    // Bytes 0..4 hold the LUN list length in bytes; one LUN occupies 8 bytes.
    let list_len = u32::from_be_bytes([
        resp.data_in[0],
        resp.data_in[1],
        resp.data_in[2],
        resp.data_in[3],
    ]) as usize;
    if list_len != 8 {
        return Err(anyhow!("REPORT LUNS: expected list_len=8, got {list_len}"));
    }
    info!("SMOKE: REPORT LUNS reports 1 LUN");
    Ok(())
}

/// WRITE 16 + READ 16 round trip across 2 pages. Verifies the
/// data-path lifts the host write into PageCache, flushes to the
/// VolumeWriter, and the read path returns identical bytes.
async fn run_data_path_smoke_test() -> Result<()> {
    let (dispatcher, _tmp) = build_smoke_dispatcher().await?;

    // Build a two-page payload with two distinct fill bytes so any
    // page-boundary misalignment shows up on read-back.
    let mut payload = vec![0x10u8; PAGE];
    payload.extend(std::iter::repeat_n(0x20u8, PAGE));

    let cdb = write16_cdb(0, 2 * SECTORS_PER_PAGE);
    let resp = dispatcher.dispatch(req(0, &cdb, &payload, 0)).await;
    if resp.sense.is_some() {
        return Err(anyhow!("WRITE 16 returned sense: {:?}", resp.sense));
    }
    info!("SMOKE: WRITE 16 of 2 pages ok");

    let cdb = read16_cdb(0, 2 * SECTORS_PER_PAGE);
    let resp = dispatcher.dispatch(req(0, &cdb, &[], 2 * PAGE)).await;
    if resp.sense.is_some() {
        return Err(anyhow!("READ 16 returned sense: {:?}", resp.sense));
    }
    if resp.data_in != payload {
        return Err(anyhow!(
            "READ 16 data mismatch: returned {} bytes, head={:?}",
            resp.data_in.len(),
            &resp.data_in[..16.min(resp.data_in.len())]
        ));
    }
    info!(
        "SMOKE: READ 16 round trip verified ({} bytes)",
        payload.len()
    );
    Ok(())
}

/// SYNCHRONIZE CACHE 16 fences a prior WRITE. Verifies it returns
/// GOOD with no sense and a subsequent READ still returns the
/// written bytes (so the fence didn't drop data).
async fn run_sync_cache_smoke_test() -> Result<()> {
    let (dispatcher, _tmp) = build_smoke_dispatcher().await?;

    let payload = vec![0x55u8; PAGE];
    let cdb = write16_cdb(0, SECTORS_PER_PAGE);
    let resp = dispatcher.dispatch(req(0, &cdb, &payload, 0)).await;
    if resp.sense.is_some() {
        return Err(anyhow!("WRITE 16 returned sense: {:?}", resp.sense));
    }

    let cdb = sync16_cdb(0, SECTORS_PER_PAGE);
    let resp = dispatcher.dispatch(req(0, &cdb, &[], 0)).await;
    if resp.sense.is_some() {
        return Err(anyhow!(
            "SYNCHRONIZE CACHE 16 returned sense: {:?}",
            resp.sense
        ));
    }
    info!("SMOKE: SYNCHRONIZE CACHE 16 ok");

    let cdb = read16_cdb(0, SECTORS_PER_PAGE);
    let resp = dispatcher.dispatch(req(0, &cdb, &[], PAGE)).await;
    if resp.sense.is_some() {
        return Err(anyhow!(
            "READ 16 after SYNC returned sense: {:?}",
            resp.sense
        ));
    }
    if resp.data_in != payload {
        return Err(anyhow!("READ 16 after SYNC data mismatch"));
    }
    info!("SMOKE: data persists after SYNCHRONIZE CACHE");
    Ok(())
}

/// Sparse-provisioning sanity: reading an LBA range that's never
/// been written must return zeros without an error. This is the
/// thin-provisioning contract the cache + writer have to honor.
async fn run_sparse_read_smoke_test() -> Result<()> {
    let (dispatcher, _tmp) = build_smoke_dispatcher().await?;

    let cdb = read16_cdb(0, SECTORS_PER_PAGE);
    let resp = dispatcher.dispatch(req(0, &cdb, &[], PAGE)).await;
    if resp.sense.is_some() {
        return Err(anyhow!(
            "READ 16 of unallocated page returned sense: {:?}",
            resp.sense
        ));
    }
    if resp.data_in.len() != PAGE {
        return Err(anyhow!(
            "unexpected READ length: got {}, want {}",
            resp.data_in.len(),
            PAGE
        ));
    }
    if !resp.data_in.iter().all(|&b| b == 0) {
        return Err(anyhow!("unallocated page must read as zeros"));
    }
    info!("SMOKE: sparse-page read returns zeros");
    Ok(())
}
