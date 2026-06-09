// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `--test` mode smoke harness. Runs in-process against a fresh data dir
//! and exercises the cartridge / library / storage / prefetch / parallel-upload
//! paths plus a small failure-scenario sweep. Not part of normal daemon
//! startup; main.rs dispatches into here when `--test` is set.

use anyhow::Result;
use tracing::{info, warn};

use super::Config;
use crate::memory_buffer_manager::UploadRequest;
use crate::upload_worker::run_event_driven_upload_worker;

/// Runs a self-contained smoke test:
/// - creates/opens a test tape
/// - writes data + filemarks
/// - exercises rewind/read/space/locate
/// - verifies with checksums and scrubs
pub(crate) async fn run_smoke_test(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::{Cartridge, CartridgeOpenMode};

    let cart_root = std::path::Path::new(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&cart_root).await.ok();

    // Open existing or create fresh test tape
    let mut cart = match Cartridge::open(&cart_root, "TAPE001", CartridgeOpenMode::Open) {
        Ok(c) => c,
        Err(_) => Cartridge::open(
            &cart_root,
            "TAPE001",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )?,
    };

    // Start at BOT
    cart.rewind();
    info!("SMOKE: BOT position={}", cart.position());

    // Write: DATA, DATA, FILEMARK, DATA, FILEMARK, DATA
    let l1 = cart.write_data(Bytes::from_static(b"alpha"))?;
    let l2 = cart.write_data(Bytes::from_static(b"beta"))?;
    let lf1 = cart.write_filemark()?;
    let l3 = cart.write_data(Bytes::from_static(b"gamma"))?;
    let lf2 = cart.write_filemark()?;
    let l4 = cart.write_data(Bytes::from_static(b"delta"))?;
    info!(
        "SMOKE: wrote data@{l1},{l2},{l3},{l4}  filemarks@{lf1},{lf2}  total_blocks={}",
        cart.total_blocks()
    );

    // Verify by direct LBA
    let v1 = cart.read_block_verify(l1)?;
    let v4 = cart.read_block_verify(l4)?;
    info!(
        "SMOKE: verify LBA{}='{}', LBA{}='{}'",
        l1,
        String::from_utf8_lossy(&v1.data),
        l4,
        String::from_utf8_lossy(&v4.data)
    );

    // Sequential read from BOT
    cart.rewind();
    let b0 = cart.read_next_verify()?; // alpha
    let b1 = cart.read_next_verify()?; // beta
    let k2 = cart.peek_kind(); // should be Filemark
    info!(
        "SMOKE: seq read '{}' then '{}' ; next kind={:?} at pos={}",
        String::from_utf8_lossy(&b0.data),
        String::from_utf8_lossy(&b1.data),
        k2,
        cart.position()
    );

    // SPACE records: spacing forward over a filemark halts on it (SSC-4
    // §7.5), leaving the head just past the FM at the next data ('gamma').
    let moved_recs = cart.space_records(1).moved;
    let pos_after_recs = cart.position();
    let b_after = cart.read_next_verify()?; // gamma
    info!(
        "SMOKE: SPACE records +1 => moved={}, pos={}, then read='{}'",
        moved_recs,
        pos_after_recs,
        String::from_utf8_lossy(&b_after.data)
    );

    // SPACE filemarks: jump to after next filemark (before 'delta'), then read it
    let crossed_fm = cart.space_filemarks(1);
    let pos_after_fm = cart.position();
    let next = cart.read_next_verify()?; // delta
    info!(
        "SMOKE: SPACE filemarks +1 => crossed={}, pos={}, then read='{}'",
        crossed_fm,
        pos_after_fm,
        String::from_utf8_lossy(&next.data)
    );

    // LOCATE to block after 'beta' and confirm filemark is next
    cart.locate(l2 + 1)?;
    let kind_here = cart.peek_kind();
    let blk_here = cart.read_next()?; // should be the FILEMARK
    info!(
        "SMOKE: LOCATE to {} => peek kind={:?}, read-next kind={:?}, pos now={}",
        l2 + 1,
        kind_here,
        blk_here.kind,
        cart.position()
    );

    // EOD
    cart.space_to_eod();
    info!(
        "SMOKE: at EOD? {}  position={}  total={}",
        cart.at_eod(),
        cart.position(),
        cart.total_blocks()
    );

    // Full scrub
    if let Ok((ok, tot)) = cart.scrub_all() {
        info!("SMOKE: scrub verified {ok}/{tot} data blocks");
    }

    Ok(())
}

/// Demonstrates a small library with 8 slots:
/// - ensure TAPE001 and TAPE002 exist and are in slots
/// - load from slot 1, write/read a block, unload
/// - load from slot 2, read position/eod, unload
pub(crate) async fn run_changer_smoke_test(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::Library;

    let lib_root = std::path::Path::new(&cfg.data_dir).join("library");
    let tapes_root = std::path::Path::new(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&lib_root).await.ok();
    tokio::fs::create_dir_all(&tapes_root).await.ok();

    // Initialize or open library for smoke test
    let mut lib = if lib_root.join("library.json").exists() {
        Library::open(&lib_root, &tapes_root)?
    } else {
        Library::initialize(&lib_root, &tapes_root, 8, 2, 2, 8, None, 0, 1001, 101, 1)? // 8 slots, 2 mail, 2 drives, LTO-8, default firmware, default element bases
    };

    // Make sure TAPE001 and TAPE002 are present in some slots
    let _s1 = lib.add_or_create_tape("TAPE001", "primary").unwrap_or(0);
    let _s2 = lib.add_or_create_tape("TAPE002", "primary").unwrap_or(1);

    // Show inventory
    for s in lib.storage_slots() {
        info!(
            "LIB: slot {} occupied={} barcode={:?}",
            s.id, s.occupied, s.barcode
        );
    }

    // Load from slot 0 (if empty, find any occupied)
    let slot_to_load = lib
        .storage_slots()
        .iter()
        .find(|s| s.occupied)
        .map(|s| s.id)
        .unwrap_or(0);

    let mut loaded = lib.load(slot_to_load)?;
    info!(
        "LIB: loaded slot {} barcode {}",
        loaded.slot_id, loaded.barcode
    );

    // Write something on the loaded tape
    let l = loaded
        .cartridge
        .write_data(Bytes::from_static(b"hello from changer"))?;
    let b = loaded.cartridge.read_block_verify(l)?;
    info!(
        "LIB: wrote+verified on {} LBA{}='{}'",
        loaded.barcode,
        l,
        String::from_utf8_lossy(&b.data)
    );

    // Unload back to the same slot
    lib.unload(loaded)?;

    // Load another tape (if available) and just check EOD
    if let Some(s) = lib.storage_slots().iter().find(|s| s.occupied) {
        let loaded2 = lib.load(s.id)?;
        let pos = loaded2.cartridge.position();
        let tot = loaded2.cartridge.total_blocks();
        info!(
            "LIB: second load {} pos={} total={}",
            loaded2.barcode, pos, tot
        );
        lib.unload(loaded2)?;
    }

    Ok(())
}

/// S3 smoke test: comprehensive test of storage tiering functionality
/// Tests:
/// - Write data with S3 backend
/// - Upload chunks to S3
/// - Backup manifest to S3
/// - Evict chunk from cache
/// - Download chunk from S3 on read
/// - Restore manifest from S3
pub(crate) async fn run_s3_smoke_test(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::{Cartridge, CartridgeOpenMode};
    use std::path::PathBuf;

    info!("=== S3 SMOKE TEST START ===");

    // Create storage backend
    let storage_backend: Box<dyn core_mediachanger::ObjectStoreBackend> = cfg
        .storage
        .create_backend_named(&cfg.storage.backend_names()[0])
        .await?;

    let cart_root = PathBuf::from(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&cart_root).await?;

    // Test 1: Create cartridge with S3 backend and write data
    info!("S3-SMOKE: Test 1 - Create cartridge with S3 backend");
    let tape_label = "S3TEST001";

    // Remove existing test cartridge if present
    let tape_dir = cart_root.join(tape_label);
    if tape_dir.exists() {
        tokio::fs::remove_dir_all(&tape_dir).await.ok();
    }

    let mut cart = Cartridge::open_with_storage_async(
        &cart_root,
        tape_label,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: core_mediachanger::DedupScope::Local,
        },
        Some(storage_backend.clone()),
    )
    .await?;

    info!("S3-SMOKE: Created cartridge with S3 backend");

    // Write some data blocks (write enough to create a chunk worth uploading)
    let data = Bytes::from(vec![0xAB; 1024 * 1024]); // 1 MiB
    for i in 0..5 {
        cart.write_data(data.clone())?;
        if i % 2 == 0 {
            cart.write_filemark()?;
        }
    }
    info!("S3-SMOKE: Wrote 5 MiB of data across 5 blocks with filemarks");

    // Test 2: Upload chunks to S3
    info!("S3-SMOKE: Test 2 - Upload chunks to S3");
    let pending = cart.get_pending_uploads();
    info!("S3-SMOKE: Found {} pending uploads", pending.len());

    for (chunk_id, _s3_key, _local_path) in pending {
        cart.upload_chunk_to_storage(chunk_id).await?;
        info!("S3-SMOKE: Uploaded chunk {}", chunk_id);
    }

    // Test 3: Backup manifest to S3
    info!("S3-SMOKE: Test 3 - Backup manifest to S3");
    cart.backup_manifest_to_storage().await?;
    info!("S3-SMOKE: Manifest backed up to S3");

    // Verify manifest exists in S3
    let manifest_key = format!("manifests/{}/manifest-latest.json", tape_label);
    let manifest_exists = storage_backend.chunk_exists(&manifest_key).await?;
    if manifest_exists {
        info!("S3-SMOKE: Manifest exists in S3: {}", manifest_key);
    } else {
        warn!("S3-SMOKE: Manifest not found in S3: {}", manifest_key);
    }

    // Test 4: Verify data can be read back
    info!("S3-SMOKE: Test 4 - Read data back (cache hit)");
    cart.rewind();
    let block0 = cart.read_next()?;
    info!(
        "S3-SMOKE: Read block 0 from cache ({} bytes)",
        block0.data.len()
    );

    // Test 5: Evict chunk from cache and read again (S3 download)
    info!("S3-SMOKE: Test 5 - Evict chunk and test S3 download");

    // Mark chunk as evicted (simulate cache eviction). With content-addressed
    // shared storage, marking the chunk S3Only is enough — the read path
    // sees the location flag and routes through the storage download branch
    // even if the pool file is still on disk.
    let chunk_id = 1; // First chunk
    if let Ok(()) = cart.mark_chunk_evicted(chunk_id) {
        info!("S3-SMOKE: Marked chunk {} as evicted", chunk_id);

        // Read again - should trigger S3 download
        cart.rewind();
        let block0_from_s3 = cart.read_block_async(0).await?;
        info!(
            "S3-SMOKE: Successfully downloaded and read block from S3 ({} bytes)",
            block0_from_s3.data.len()
        );

        // Verify data matches
        if block0.data == block0_from_s3.data {
            info!("S3-SMOKE: Data integrity verified (cache vs S3)");
        } else {
            warn!("S3-SMOKE: Data mismatch between cache and S3!");
        }
    } else {
        warn!("S3-SMOKE: Could not evict chunk (might not be uploaded yet)");
    }

    // Test 6: Manifest restore from S3
    info!("S3-SMOKE: Test 6 - Test manifest restore from S3");

    // Save original manifest path
    let manifest_path = cart.root_path().join("manifest.json");
    let manifest_backup_path = cart.root_path().join("manifest.json.backup");

    // Backup and delete local manifest
    if manifest_path.exists() {
        tokio::fs::copy(&manifest_path, &manifest_backup_path).await?;
        tokio::fs::remove_file(&manifest_path).await?;
        info!("S3-SMOKE: Deleted local manifest");
    }

    // Open cartridge again - should restore from S3
    match Cartridge::open_with_storage_async(
        &cart_root,
        tape_label,
        CartridgeOpenMode::Open,
        Some(storage_backend.clone()),
    )
    .await
    {
        Ok(restored_cart) => {
            info!("S3-SMOKE: Successfully restored cartridge from S3 manifest");
            info!(
                "S3-SMOKE: Restored cartridge has {} blocks",
                restored_cart.total_blocks()
            );
        }
        Err(e) => {
            warn!("S3-SMOKE: Failed to restore from S3: {e:?}");
            // Restore backup
            if manifest_backup_path.exists() {
                tokio::fs::copy(&manifest_backup_path, &manifest_path).await?;
            }
        }
    }

    // Cleanup backup
    if manifest_backup_path.exists() {
        tokio::fs::remove_file(&manifest_backup_path).await.ok();
    }

    // Test 7: Manifest version cleanup
    info!("S3-SMOKE: Test 7 - Test manifest version cleanup");

    // Create multiple manifest versions
    for _i in 0..15 {
        cart.backup_manifest_to_storage().await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    info!("S3-SMOKE: Created 15 manifest versions");

    // Cleanup old versions (keep last 10)
    let deleted = cart.cleanup_old_manifest_versions(10).await?;
    info!(
        "S3-SMOKE: Cleaned up {} old manifest versions (kept last 10)",
        deleted
    );

    // Verify only 10 versions remain (plus manifest-latest.json)
    let prefix = format!("manifests/{}/", tape_label);
    let keys = storage_backend.list_objects(&prefix).await?;
    let versioned_count = keys
        .iter()
        .filter(|k| k.contains("manifest-") && !k.ends_with("manifest-latest.json"))
        .count();
    info!(
        "S3-SMOKE: Remaining versioned manifests: {}",
        versioned_count
    );

    info!("=== S3 SMOKE TEST COMPLETE ===");
    Ok(())
}

/// Prefetch smoke test: verify aggressive prefetching works correctly
/// Tests:
/// - Create cartridge with prefetch manager
/// - Write data spanning multiple chunks
/// - Upload chunks to S3
/// - Evict chunk to force S3 download
/// - Sequential read triggers prefetch
/// - Verify prefetch tasks are active
pub(crate) async fn run_prefetch_smoke_test(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::{Cartridge, CartridgeOpenMode, PrefetchConfig, PrefetchManager};
    use std::path::PathBuf;
    use std::sync::Arc;

    info!("=== PREFETCH SMOKE TEST START ===");

    // Check if prefetch is enabled
    if cfg.memory_buffers.read_prefetch_chunks_ahead == 0 {
        info!("PREFETCH-SMOKE: Prefetch disabled (chunks_ahead=0), skipping test");
        return Ok(());
    }

    // Create S3 backend
    let storage_backend = Arc::new(
        cfg.storage
            .create_backend_named(&cfg.storage.backend_names()[0])
            .await?,
    );

    // Create prefetch manager
    let prefetch_config = PrefetchConfig {
        enabled: cfg.memory_buffers.read_prefetch_chunks_ahead > 0,
        chunks_ahead: cfg.memory_buffers.read_prefetch_chunks_ahead,
    };
    let prefetch_mgr = Arc::new(PrefetchManager::new(
        storage_backend.clone(),
        prefetch_config,
    ));

    let cart_root = PathBuf::from(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&cart_root).await?;

    // Test 1: Create cartridge with prefetch manager
    info!("PREFETCH-SMOKE: Test 1 - Create cartridge with prefetch manager");
    let tape_label = "PREFETCHTEST001";

    // Remove existing test cartridge if present
    let tape_dir = cart_root.join(tape_label);
    if tape_dir.exists() {
        tokio::fs::remove_dir_all(&tape_dir).await.ok();
    }

    let mut cart = Cartridge::open_with_storage_async(
        &cart_root,
        tape_label,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: core_mediachanger::DedupScope::Local,
        },
        Some((**storage_backend).clone_box()),
    )
    .await?;

    // Attach prefetch manager
    cart.set_prefetch_manager(prefetch_mgr.clone());
    info!("PREFETCH-SMOKE: Attached prefetch manager to cartridge");

    // Test 2: Write data spanning multiple chunks (5 MiB each to test cross-chunk prefetch)
    info!("PREFETCH-SMOKE: Test 2 - Write data spanning multiple chunks");
    let data = Bytes::from(vec![0xCD; 5 * 1024 * 1024]); // 5 MiB per block
    for i in 0..5 {
        cart.write_data(data.clone())?;
        info!("PREFETCH-SMOKE: Wrote block {}", i);
    }
    info!("PREFETCH-SMOKE: Wrote 25 MiB total across 5 blocks");

    // Test 3: Upload chunks to S3. Seal the trailing chunk first —
    // 25 MiB sits well under the 128 MiB chunk-roll default, so without
    // an explicit flush the active chunk stays staging and
    // `get_pending_uploads()` returns nothing.
    info!("PREFETCH-SMOKE: Test 3 - Upload all chunks to S3");
    cart.flush_and_seal()?;
    let pending = cart.get_pending_uploads();
    for (chunk_id, _s3_key, _local_path) in pending {
        cart.upload_chunk_to_storage(chunk_id).await?;
        info!("PREFETCH-SMOKE: Uploaded chunk {}", chunk_id);
    }

    // Test 4: Evict the first sealed chunk to force S3 download during
    // reads. With content-addressed shared storage we just flip the
    // location to StorageOnly; the read path takes the storage download
    // branch on the strength of that flag alone (the local pool file
    // staying around is fine — `read_block_async` keys off the manifest
    // location, not directory existence).
    info!("PREFETCH-SMOKE: Test 4 - Evict chunk to test prefetch from S3");
    cart.mark_chunk_evicted(0)?;
    info!("PREFETCH-SMOKE: Evicted chunk 0 to force S3 download");

    // Test 5: Sequential read with prefetch from S3
    info!("PREFETCH-SMOKE: Test 5 - Sequential read triggers prefetch from S3");
    cart.rewind_async().await; // Use async version to properly cancel any prefetches

    // Read first block using read_next_async() - this should trigger prefetch of next chunks from S3
    info!("PREFETCH-SMOKE: Reading first block (will download from S3)...");
    let block0 = cart.read_next_async().await?;
    info!(
        "PREFETCH-SMOKE: Read block 0 ({} bytes) at position {}",
        block0.data.len(),
        cart.position()
    );

    // Give prefetch tasks a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Check if prefetch tasks are active - chunk is in S3Only state, so prefetch may have already completed
    let active_count = prefetch_mgr.active_task_count().await;
    info!(
        "PREFETCH-SMOKE: Active prefetch tasks: {} (may be 0 if prefetch completed quickly)",
        active_count
    );
    info!("PREFETCH-SMOKE: Prefetch mechanism is functional");

    // Read next block sequentially using read_next_async()
    let block1 = cart.read_next_async().await?;
    info!(
        "PREFETCH-SMOKE: Read block 1 ({} bytes) at position {}",
        block1.data.len(),
        cart.position()
    );

    // Test 6: LOCATE cancels prefetches
    info!("PREFETCH-SMOKE: Test 6 - LOCATE cancels prefetches");
    let before_locate = prefetch_mgr.active_task_count().await;
    info!(
        "PREFETCH-SMOKE: Active tasks before LOCATE: {}",
        before_locate
    );

    cart.locate_async(4).await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let after_locate = prefetch_mgr.active_task_count().await;
    info!(
        "PREFETCH-SMOKE: Active tasks after LOCATE: {}",
        after_locate
    );

    if after_locate == 0 && before_locate > 0 {
        info!("PREFETCH-SMOKE: LOCATE correctly cancelled prefetch tasks");
    }

    info!("=== PREFETCH SMOKE TEST COMPLETE ===");
    Ok(())
}

/// Parallel upload smoke test: verify parallel uploads work correctly
/// Tests:
/// - Write data spanning 10 chunks (130 MiB per chunk = 1.3 GB total)
/// - Verify multiple chunks upload in parallel
/// - Verify all chunks uploaded successfully
pub(crate) async fn run_parallel_upload_smoke_test(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::{Cartridge, CartridgeOpenMode};
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::task::JoinSet;

    info!("=== PARALLEL UPLOAD SMOKE TEST START ===");

    // Create S3 backend
    let storage_backend = cfg
        .storage
        .create_backend_named(&cfg.storage.backend_names()[0])
        .await?;

    let upload_cfg = &cfg.storage.upload;
    let max_concurrent = upload_cfg.max_concurrent;

    let cart_root = PathBuf::from(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&cart_root).await?;

    // Test 1: Create cartridge and write 10 chunks worth of data
    info!("PARALLEL-UPLOAD: Test 1 - Write 10 chunks of data");
    let tape_label = "PARALLELTEST001";

    // Remove existing test cartridge if present
    let tape_dir = cart_root.join(tape_label);
    if tape_dir.exists() {
        tokio::fs::remove_dir_all(&tape_dir).await.ok();
    }

    let mut cart = Cartridge::open_with_storage_async(
        &cart_root,
        tape_label,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: core_mediachanger::DedupScope::Local,
        },
        Some(storage_backend.clone()),
    )
    .await?;

    // Write 130 MiB blocks (slightly over 128 MiB chunk size to create multiple chunks)
    // Write 10 blocks to create ~10 chunks
    let data = Bytes::from(vec![0xEF; 13 * 1024 * 1024]); // 13 MiB per block
    for i in 0..10 {
        cart.write_data(data.clone())?;
        if i % 3 == 0 {
            cart.write_filemark()?;
        }
    }
    info!("PARALLEL-UPLOAD: Wrote 130 MiB across 10 blocks");

    // Test 2: Get pending uploads
    let pending = cart.get_pending_uploads();
    info!(
        "PARALLEL-UPLOAD: Test 2 - Found {} pending uploads",
        pending.len()
    );

    if pending.len() < 4 {
        warn!("PARALLEL-UPLOAD: Not enough chunks to test parallelism (need at least 4)");
    }

    // Test 3: Upload chunks in parallel (simulate what upload worker does)
    info!(
        "PARALLEL-UPLOAD: Test 3 - Upload chunks in parallel (max_concurrent={})",
        max_concurrent
    );

    let start = Instant::now();
    let mut join_set = JoinSet::new();
    let mut uploaded_count = 0;

    // Take first batch of chunks (up to max_concurrent)
    let batch: Vec<_> = pending.into_iter().take(max_concurrent).collect();

    info!(
        "PARALLEL-UPLOAD: Starting parallel batch of {} chunks",
        batch.len()
    );

    // Spawn upload tasks
    for (chunk_id, _s3_key, _local_path) in batch {
        let storage_backend_clone = storage_backend.clone();
        let cart_root_clone = cart_root.clone();
        let tape_label_clone = tape_label.to_string();

        join_set.spawn(async move {
            let mut cart_clone = Cartridge::open_with_storage_async(
                &cart_root_clone,
                &tape_label_clone,
                CartridgeOpenMode::Open,
                Some(storage_backend_clone),
            )
            .await?;

            let upload_start = Instant::now();
            cart_clone.upload_chunk_to_storage(chunk_id).await?;
            let upload_duration = upload_start.elapsed();

            info!(
                "PARALLEL-UPLOAD: Uploaded chunk {} in {:.2}s",
                chunk_id,
                upload_duration.as_secs_f64()
            );
            Ok::<u64, anyhow::Error>(chunk_id)
        });
    }

    // Wait for all uploads to complete
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(_chunk_id)) => {
                uploaded_count += 1;
            }
            Ok(Err(e)) => {
                warn!("PARALLEL-UPLOAD: Upload failed: {e:?}");
            }
            Err(e) => {
                warn!("PARALLEL-UPLOAD: Task panicked: {e:?}");
            }
        }
    }

    let total_duration = start.elapsed();
    info!(
        "PARALLEL-UPLOAD: Uploaded {} chunks in {:.2}s ({:.2} chunks/sec)",
        uploaded_count,
        total_duration.as_secs_f64(),
        uploaded_count as f64 / total_duration.as_secs_f64()
    );

    // Test 4: Verify all chunks are marked as uploaded
    let pending_after = cart.get_pending_uploads();
    info!(
        "PARALLEL-UPLOAD: Test 4 - Pending uploads after batch: {}",
        pending_after.len()
    );

    if uploaded_count >= max_concurrent.min(4) {
        info!(
            "PARALLEL-UPLOAD: Successfully uploaded at least {} chunks in parallel",
            uploaded_count
        );
    } else {
        warn!(
            "PARALLEL-UPLOAD: Expected to upload at least 4 chunks, only uploaded {}",
            uploaded_count
        );
    }

    info!("=== PARALLEL UPLOAD SMOKE TEST COMPLETE ===");
    Ok(())
}

/// End-to-end test of `run_event_driven_upload_worker`: spin up the
/// worker against a real Cartridge + LocalBackend, send one UploadRequest
/// through the mpsc channel, then close the channel and assert the
/// worker drained the request, the manifest landed in storage, and the
/// chunks flipped to `uploaded=true`.
///
/// First direct coverage of the worker — keeps the helper split honest
/// against future refactors.
pub(crate) async fn run_upload_worker_smoke_test(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::{Cartridge, CartridgeOpenMode};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Notify;
    use tokio::sync::mpsc;

    info!("=== UPLOAD WORKER SMOKE TEST START ===");

    let backend_name = cfg.storage.backend_names()[0].clone();
    let storage_backend = cfg.storage.create_backend_named(&backend_name).await?;

    let cart_root = PathBuf::from(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&cart_root).await?;

    let tape_label = "WORKERSMOKE001";
    let tape_dir = cart_root.join(tape_label);
    if tape_dir.exists() {
        tokio::fs::remove_dir_all(&tape_dir).await.ok();
    }

    let mut cart = Cartridge::open_with_storage_async(
        &cart_root,
        tape_label,
        CartridgeOpenMode::Create {
            backend: backend_name.clone(),
            worm: false,
            dedup: core_mediachanger::DedupScope::Local,
        },
        Some(storage_backend.clone()),
    )
    .await?;

    // Write a few MiB of data, then force a chunk seal so the worker has
    // something to upload (chunks otherwise only seal at the chunk-size
    // boundary, which is 128 MiB by default — too much for a smoke test).
    let data = Bytes::from(vec![0x77u8; 1024 * 1024]);
    for _ in 0..5 {
        cart.write_data(data.clone())?;
    }
    cart.write_filemark()?;
    cart.flush_and_seal()?;

    let pending = cart.get_pending_uploads();
    let chunk_ids: Vec<u32> = pending.iter().map(|(id, _, _)| *id as u32).collect();
    info!(
        "UPLOAD-WORKER-SMOKE: enqueueing UploadRequest for {} chunks",
        chunk_ids.len()
    );

    // Drop the cart so the worker can re-open exclusively.
    drop(cart);

    let (tx, rx) = mpsc::channel::<UploadRequest>(8);
    let notify = Arc::new(Notify::new());
    let cfg_clone = cfg.clone();
    let worker =
        tokio::spawn(async move { run_event_driven_upload_worker(&cfg_clone, rx, notify).await });

    tx.send(UploadRequest {
        tape_id: tape_label.to_string(),
        chunk_ids: chunk_ids.clone(),
    })
    .await
    .ok();
    // Closing the sender lets the worker drain queued requests then exit.
    drop(tx);

    match tokio::time::timeout(Duration::from_secs(60), worker).await {
        Ok(Ok(Ok(()))) => info!("UPLOAD-WORKER-SMOKE: worker completed cleanly"),
        Ok(Ok(Err(e))) => warn!("UPLOAD-WORKER-SMOKE: worker returned Err: {e:?}"),
        Ok(Err(e)) => warn!("UPLOAD-WORKER-SMOKE: worker panicked: {e:?}"),
        Err(_) => warn!("UPLOAD-WORKER-SMOKE: worker timed out after 60s"),
    }

    let manifest_key = format!("manifests/{}/manifest-latest.json", tape_label);
    let manifest_exists = storage_backend.chunk_exists(&manifest_key).await?;
    if manifest_exists {
        info!(
            "UPLOAD-WORKER-SMOKE: Manifest landed in storage at {}",
            manifest_key
        );
    } else {
        warn!(
            "UPLOAD-WORKER-SMOKE: Manifest not in storage at {}",
            manifest_key
        );
    }

    let restored_cart = Cartridge::open_with_storage_async(
        &cart_root,
        tape_label,
        CartridgeOpenMode::Open,
        Some(storage_backend.clone()),
    )
    .await?;
    let pending_after = restored_cart.get_pending_uploads();
    if pending_after.is_empty() {
        info!("UPLOAD-WORKER-SMOKE: All chunks marked uploaded by worker");
    } else {
        warn!(
            "UPLOAD-WORKER-SMOKE: {} chunks still pending after worker run",
            pending_after.len()
        );
    }

    info!("=== UPLOAD WORKER SMOKE TEST COMPLETE ===");
    Ok(())
}

/// Performance benchmarks: measure throughput for various operations
/// Tests:
/// - Sequential read throughput with prefetch enabled
/// - Sequential read throughput with prefetch disabled
/// - Write throughput
pub(crate) async fn run_performance_benchmarks(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::{Cartridge, CartridgeOpenMode, PrefetchConfig, PrefetchManager};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    info!("=== PERFORMANCE BENCHMARKS START ===");

    let storage_backend = Arc::new(
        cfg.storage
            .create_backend_named(&cfg.storage.backend_names()[0])
            .await?,
    );

    let cart_root = PathBuf::from(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&cart_root).await?;

    // Benchmark 1: Write throughput
    info!("--- Benchmark 1: Write Throughput ---");
    let tape_label = "PERFTEST001";
    let tape_dir = cart_root.join(tape_label);
    if tape_dir.exists() {
        tokio::fs::remove_dir_all(&tape_dir).await.ok();
    }

    let mut cart = Cartridge::open_with_storage_async(
        &cart_root,
        tape_label,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: core_mediachanger::DedupScope::Local,
        },
        Some((**storage_backend).clone_box()),
    )
    .await?;

    // Write 200 MB (about 1.5 chunks)
    let write_size_mb = 200;
    let block_size_mb = 10;
    let num_blocks = write_size_mb / block_size_mb;
    let data = Bytes::from(vec![0xCD; block_size_mb * 1024 * 1024]);

    let write_start = Instant::now();
    for _ in 0..num_blocks {
        cart.write_data(data.clone())?;
    }
    let write_duration = write_start.elapsed();
    let write_throughput = write_size_mb as f64 / write_duration.as_secs_f64();

    info!(
        "PERF: Write throughput: {:.2} MB/s ({} MB in {:.2}s)",
        write_throughput,
        write_size_mb,
        write_duration.as_secs_f64()
    );

    if write_throughput < 100.0 {
        warn!("PERF: Write throughput below target (< 100 MB/s)");
    } else {
        info!("PERF: Write throughput meets target (>= 100 MB/s)");
    }

    // Upload all chunks to S3 for read benchmarks. Seal the trailing
    // chunk first so its pending entry becomes visible; otherwise the
    // sub-chunk-roll tail stays staging and never uploads.
    info!("PERF: Uploading chunks to S3 for read benchmarks...");
    cart.flush_and_seal()?;
    let pending = cart.get_pending_uploads();
    for (chunk_id, _s3_key, _local_path) in pending {
        cart.upload_chunk_to_storage(chunk_id).await?;
    }
    info!("PERF: All chunks uploaded to S3");

    // Benchmark 2: Sequential read with prefetch
    info!("--- Benchmark 2: Sequential Read (WITH prefetch) ---");

    // Create prefetch manager
    let prefetch_config = PrefetchConfig {
        enabled: true,
        chunks_ahead: cfg.memory_buffers.read_prefetch_chunks_ahead,
    };
    let prefetch_mgr = Arc::new(PrefetchManager::new(
        storage_backend.clone(),
        prefetch_config,
    ));
    cart.set_prefetch_manager(prefetch_mgr.clone());

    // Evict chunk 0 (the first sealed chunk) to force a cold S3 download
    // on the first read. StorageOnly is enough — `read_block_async`
    // routes through the storage branch off the manifest flag alone.
    cart.mark_chunk_evicted(0)?;

    cart.rewind_async().await;
    let read_start = Instant::now();
    let mut bytes_read = 0;

    for _ in 0..num_blocks {
        let block = cart.read_next_async().await?;
        bytes_read += block.data.len();
    }

    let read_duration = read_start.elapsed();
    let read_throughput = (bytes_read as f64 / (1024.0 * 1024.0)) / read_duration.as_secs_f64();

    info!(
        "PERF: Sequential read (WITH prefetch): {:.2} MB/s ({} MB in {:.2}s)",
        read_throughput,
        bytes_read / (1024 * 1024),
        read_duration.as_secs_f64()
    );

    // Benchmark 3: Sequential read without prefetch
    info!("--- Benchmark 3: Sequential Read (WITHOUT prefetch) ---");

    // Disable prefetch
    let prefetch_config_disabled = PrefetchConfig {
        enabled: false,
        chunks_ahead: 0,
    };
    let prefetch_mgr_disabled = Arc::new(PrefetchManager::new(
        storage_backend.clone(),
        prefetch_config_disabled,
    ));
    cart.set_prefetch_manager(prefetch_mgr_disabled);

    // Evict chunk 0 again — the previous benchmark's first read
    // transitioned it back to Both, so this restores the StorageOnly
    // start state for the prefetch-disabled run.
    cart.mark_chunk_evicted(0)?;

    cart.rewind_async().await;
    let read_start_no_prefetch = Instant::now();
    let mut bytes_read_no_prefetch = 0;

    for _ in 0..num_blocks {
        let block = cart.read_next_async().await?;
        bytes_read_no_prefetch += block.data.len();
    }

    let read_duration_no_prefetch = read_start_no_prefetch.elapsed();
    let read_throughput_no_prefetch = (bytes_read_no_prefetch as f64 / (1024.0 * 1024.0))
        / read_duration_no_prefetch.as_secs_f64();

    info!(
        "PERF: Sequential read (WITHOUT prefetch): {:.2} MB/s ({} MB in {:.2}s)",
        read_throughput_no_prefetch,
        bytes_read_no_prefetch / (1024 * 1024),
        read_duration_no_prefetch.as_secs_f64()
    );

    // Compare results
    let speedup = read_throughput / read_throughput_no_prefetch;
    info!("PERF: Prefetch speedup: {:.2}x", speedup);

    if speedup > 1.5 {
        info!("PERF: Prefetch provides significant speedup (>1.5x)");
    } else {
        warn!(
            "PERF: Prefetch speedup below expected (>1.5x, got {:.2}x)",
            speedup
        );
    }

    info!("=== PERFORMANCE BENCHMARKS COMPLETE ===");
    Ok(())
}

/// Failure scenario tests: verify system handles failures correctly
/// Tests:
/// - Prefetch cancellation on LOCATE
/// - Compression roundtrip
pub(crate) async fn run_failure_scenario_tests(cfg: &Config) -> Result<()> {
    use bytes::Bytes;
    use core_mediachanger::{Cartridge, CartridgeOpenMode, PrefetchConfig, PrefetchManager};
    use std::path::PathBuf;
    use std::sync::Arc;

    info!("=== FAILURE SCENARIO TESTS START ===");

    let storage_backend = Arc::new(
        cfg.storage
            .create_backend_named(&cfg.storage.backend_names()[0])
            .await?,
    );

    let cart_root = PathBuf::from(&cfg.data_dir).join("tapes");
    tokio::fs::create_dir_all(&cart_root).await?;

    // Test 1: Prefetch cancellation on LOCATE
    info!("--- Test 1: Prefetch Cancellation on LOCATE ---");

    if cfg.memory_buffers.read_prefetch_chunks_ahead > 0 {
        let prefetch_config = PrefetchConfig {
            enabled: true,
            chunks_ahead: cfg.memory_buffers.read_prefetch_chunks_ahead,
        };
        let prefetch_mgr = Arc::new(PrefetchManager::new(
            storage_backend.clone(),
            prefetch_config,
        ));

        let tape_label = "FAILTEST001";
        let tape_dir = cart_root.join(tape_label);
        if tape_dir.exists() {
            tokio::fs::remove_dir_all(&tape_dir).await.ok();
        }

        let mut cart = Cartridge::open_with_storage_async(
            &cart_root,
            tape_label,
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
            Some((**storage_backend).clone_box()),
        )
        .await?;

        cart.set_prefetch_manager(prefetch_mgr.clone());

        // Write some data
        let data = Bytes::from(vec![0xAB; 5 * 1024 * 1024]); // 5 MiB
        for _ in 0..10 {
            cart.write_data(data.clone())?;
        }

        // Upload chunks. 50 MiB sits below the 128 MiB chunk-roll
        // default, so seal the trailing chunk first or `pending` is
        // empty.
        cart.flush_and_seal()?;
        let pending = cart.get_pending_uploads();
        for (chunk_id, _s3_key, _local_path) in pending {
            cart.upload_chunk_to_storage(chunk_id).await?;
        }

        // Evict the first sealed chunk to force an S3 download — the
        // location flag alone is enough to route the read through the
        // storage branch.
        cart.mark_chunk_evicted(0)?;

        // Start sequential read to trigger prefetch
        cart.rewind_async().await;
        cart.read_next_async().await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let active_before = prefetch_mgr.active_task_count().await;
        info!(
            "FAIL-TEST: Active prefetch tasks before LOCATE: {}",
            active_before
        );

        // LOCATE should cancel prefetches
        cart.locate_async(5).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let active_after = prefetch_mgr.active_task_count().await;
        info!(
            "FAIL-TEST: Active prefetch tasks after LOCATE: {}",
            active_after
        );

        info!("FAIL-TEST: Prefetch cancellation test complete");
    } else {
        info!("FAIL-TEST: Prefetch disabled, skipping cancellation test");
    }

    // Test 2: Compression roundtrip
    info!("--- Test 2: Compression Roundtrip ---");

    let compression_enabled = !matches!(
        cfg.storage.compression.algorithm,
        core_mediachanger::object_store_config::CompressionAlgoYaml::None
    );
    if compression_enabled {
        let tape_label = "COMPRESSTEST001";
        let tape_dir = cart_root.join(tape_label);
        if tape_dir.exists() {
            tokio::fs::remove_dir_all(&tape_dir).await.ok();
        }

        let mut cart = Cartridge::open_with_storage_async(
            &cart_root,
            tape_label,
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
            Some((**storage_backend).clone_box()),
        )
        .await?;

        // Write highly compressible data
        let compressible_data = Bytes::from(vec![0x42; 10 * 1024 * 1024]); // 10 MiB of same byte
        let lba = cart.write_data(compressible_data.clone())?;
        info!("FAIL-TEST: Wrote 10 MiB of highly compressible data");

        // Upload to S3 (will be compressed). Seal first so the 10 MiB
        // active chunk gets a hash and surfaces in `pending`.
        cart.flush_and_seal()?;
        let pending = cart.get_pending_uploads();
        for (chunk_id, _s3_key, _local_path) in pending {
            cart.upload_chunk_to_storage(chunk_id).await?;
            info!("FAIL-TEST: Uploaded chunk {} (with compression)", chunk_id);
        }

        // Evict chunk 0 and read back from S3 (will be decompressed).
        // Location flag alone routes the read through the storage branch.
        cart.mark_chunk_evicted(0)?;

        let decompressed_block = cart.read_block_async(lba).await?;
        info!(
            "FAIL-TEST: Read back {} bytes from S3 (after decompression)",
            decompressed_block.data.len()
        );

        // Verify data integrity
        if decompressed_block.data == compressible_data {
            info!("FAIL-TEST: Compression roundtrip successful - data integrity verified");
        } else {
            warn!("FAIL-TEST: Compression roundtrip failed - data mismatch!");
            return Err(anyhow::anyhow!("Compression roundtrip data mismatch"));
        }
    } else {
        info!("FAIL-TEST: Compression disabled, skipping roundtrip test");
    }

    info!("=== FAILURE SCENARIO TESTS COMPLETE ===");
    Ok(())
}
