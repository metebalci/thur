// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for Cartridge functionality
//!
//! These tests verify the core tape cartridge operations including:
//! - Basic I/O (write, read)
//! - Filemarks
//! - Positioning (rewind, locate, space)
//! - Manifest persistence
//! - Chunk management

mod common;

use bytes::Bytes;
use common::*;
use core_mediachanger::{BlockKind, Cartridge, CartridgeOpenMode, ChunkStore, LocalBackend};

#[test]
fn test_cartridge_creation() {
    let dir = create_test_dir();
    let cartridge = create_test_cartridge(&dir, "CREATE001");

    assert_eq!(cartridge.next_lba(), 0);
    assert_eq!(cartridge.label(), "CREATE001");
}

#[test]
fn test_write_and_read_single_block() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "RW001");

    // Write a block
    let test_data = vec![0x42; 1024];
    cartridge
        .write_data(Bytes::from(test_data.clone()))
        .unwrap();

    assert_eq!(cartridge.next_lba(), 1);

    // Rewind and read back
    cartridge.rewind();
    assert_eq!(cartridge.position(), 0);

    let block = cartridge.read_next().unwrap();
    assert_eq!(block.kind, BlockKind::Data);
    assert_eq!(block.data, test_data);
    assert_eq!(cartridge.position(), 1);
}

#[test]
fn test_host_byte_counters_track_writes_and_reads() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "RWCNT1");
    assert_eq!(cartridge.host_bytes_written(), 0);
    assert_eq!(cartridge.host_bytes_read(), 0);

    let data_a = vec![0x11; 1024];
    let data_b = vec![0x22; 512];
    cartridge.write_data(Bytes::from(data_a)).unwrap();
    cartridge.write_filemark().unwrap();
    cartridge.write_data(Bytes::from(data_b)).unwrap();
    // `host_bytes_written` counts plaintext data bytes; the
    // zero-byte filemark between them does not move it.
    assert_eq!(cartridge.host_bytes_written(), 1536);

    cartridge.rewind();
    cartridge.read_next().unwrap(); // data_a
    cartridge.read_next().unwrap(); // filemark — serves 0 plaintext bytes
    cartridge.read_next().unwrap(); // data_b
    // `host_bytes_read` is the read-side mirror; the filemark read
    // returns early and never counts.
    assert_eq!(cartridge.host_bytes_read(), 1536);
}

#[test]
fn test_write_multiple_blocks() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "MULTI001");

    // Write 10 blocks with different patterns
    let written_data = write_test_data(&mut cartridge, 10, 512);

    assert_eq!(cartridge.next_lba(), 10);

    // Rewind and read all blocks back
    cartridge.rewind();

    for (i, expected_data) in written_data.iter().enumerate() {
        let block = cartridge.read_next().unwrap();
        assert_eq!(block.kind, BlockKind::Data);
        assert_eq!(block.data, *expected_data, "Block {} mismatch", i);
    }

    assert_eq!(cartridge.next_lba(), 10);
}

#[test]
fn test_filemarks() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "FM001");

    // Write data, filemark, more data
    let data1 = vec![0x11; 256];
    let data2 = vec![0x22; 256];

    cartridge.write_data(Bytes::from(data1.clone())).unwrap();
    cartridge.write_filemark().unwrap();
    cartridge.write_data(Bytes::from(data2.clone())).unwrap();

    // Rewind and verify
    cartridge.rewind();

    // Read first block
    let block1 = cartridge.read_next().unwrap();
    assert_eq!(block1.kind, BlockKind::Data);
    assert_eq!(block1.data, data1);

    // Read filemark
    let fm = cartridge.read_next().unwrap();
    assert_eq!(fm.kind, BlockKind::Filemark);
    assert!(fm.data.is_empty());

    // Read second block
    let block2 = cartridge.read_next().unwrap();
    assert_eq!(block2.kind, BlockKind::Data);
    assert_eq!(block2.data, data2);
}

#[test]
fn test_rewind() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "REWIND001");

    // Write some data
    write_test_data(&mut cartridge, 5, 512);
    assert_eq!(cartridge.next_lba(), 5);

    // Rewind
    cartridge.rewind();
    assert_eq!(cartridge.position(), 0);

    // Should be able to read from beginning
    let block = cartridge.read_next().unwrap();
    assert_eq!(block.kind, BlockKind::Data);
}

#[test]
fn test_locate() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "LOCATE001");

    // Write blocks at LBA 0, 1, 2, 3, 4
    let written = write_test_data(&mut cartridge, 5, 256);

    // Locate to LBA 2
    cartridge.locate(2).unwrap();
    assert_eq!(cartridge.position(), 2);

    // Read should return block at LBA 2
    let block = cartridge.read_next().unwrap();
    assert_eq!(block.data, written[2]);
    assert_eq!(cartridge.position(), 3);

    // Locate to LBA 0
    cartridge.locate(0).unwrap();
    let block = cartridge.read_next().unwrap();
    assert_eq!(block.data, written[0]);
}

#[test]
fn test_space_records() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "SPACE001");

    // Write 10 blocks
    write_test_data(&mut cartridge, 10, 128);

    // Rewind to BOT
    cartridge.rewind();
    assert_eq!(cartridge.position(), 0);

    // Space forward 3 records
    cartridge.space_records(3);
    assert_eq!(cartridge.position(), 3);

    // Space forward 2 more
    cartridge.space_records(2);
    assert_eq!(cartridge.position(), 5);
}

#[test]
fn test_space_filemarks() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "SPACEFM001");

    // Write 3 files separated by filemarks
    write_test_files(&mut cartridge, 2, 3, 128);

    // Rewind
    cartridge.rewind();

    // Space forward 1 filemark (should position after first FM)
    cartridge.space_filemarks(1);

    // Next read should be the first block of file 2
    let block = cartridge.read_next().unwrap();
    assert_eq!(block.kind, BlockKind::Data);

    // Space forward 1 more filemark
    cartridge.space_filemarks(1);

    // Next read should be the first block of file 3
    let block = cartridge.read_next().unwrap();
    assert_eq!(block.kind, BlockKind::Data);
}

#[test]
fn test_manifest_persistence() {
    let dir = create_test_dir();
    let label = "PERSIST001";

    // Create cartridge and write data
    {
        let mut cartridge = create_test_cartridge(&dir, label);
        write_test_data(&mut cartridge, 5, 512);
    } // Cartridge dropped, manifest should persist

    // Reopen cartridge
    let tapes_path = dir.path().join("tapes");
    let mut cartridge = Cartridge::open(&tapes_path, label, CartridgeOpenMode::Open).unwrap();

    // Should have the data we wrote
    assert_eq!(cartridge.next_lba(), 5);

    cartridge.rewind();
    let block = cartridge.read_next().unwrap();
    assert_eq!(block.kind, BlockKind::Data);
    assert_eq!(block.data.len(), 512);
}

#[test]
fn test_chunk_rollover() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "CHUNK001");

    // Write enough data to fill multiple chunks (chunk size is 128 MiB)
    // Write 1000 blocks of 256 KB each = 256 MB total (should span 2 chunks)
    for i in 0..1000 {
        let data = vec![(i % 256) as u8; 256 * 1024];
        cartridge.write_data(Bytes::from(data)).unwrap();
    }

    assert_eq!(cartridge.next_lba(), 1000);

    // Rewind and verify we can read everything back
    cartridge.rewind();

    for i in 0..1000 {
        let block = cartridge.read_next().unwrap();
        assert_eq!(block.kind, BlockKind::Data);
        assert_eq!(block.data.len(), 256 * 1024);
        assert_eq!(block.data[0], (i % 256) as u8, "Block {} mismatch", i);
    }
}

#[test]
fn test_space_to_eod() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "EOD001");

    // Write some data
    write_test_data(&mut cartridge, 10, 256);
    let eod_lba = cartridge.next_lba();

    // Rewind
    cartridge.rewind();
    assert_eq!(cartridge.position(), 0);

    // Space to end of data
    cartridge.space_to_eod();
    assert_eq!(cartridge.position(), eod_lba);
}

#[test]
fn test_read_beyond_eod() {
    let dir = create_test_dir();
    let mut cartridge = create_test_cartridge(&dir, "BEOD001");

    // Write 3 blocks
    write_test_data(&mut cartridge, 3, 128);

    // Rewind
    cartridge.rewind();

    // Read the 3 blocks
    for _ in 0..3 {
        cartridge.read_next().unwrap();
    }

    // Try to read beyond EOD - should return error
    let result = cartridge.read_next();
    assert!(result.is_err());
}

#[test]
fn test_create_test_helper_binds_to_primary_backend() {
    // The shared test helper creates cartridges bound to the
    // "primary" backend; downstream tests that don't care about
    // backend routing rely on this.
    let dir = create_test_dir();
    let cartridge = create_test_cartridge(&dir, "PRIM001");
    assert_eq!(cartridge.backend(), "primary");
}

#[test]
fn test_create_with_named_backend_persists_in_manifest() {
    use core_mediachanger::ChunkingMode;

    let dir = create_test_dir();
    let tapes = dir.path().join("tapes");
    {
        // Create with explicit backend name.
        let cart = Cartridge::create_with_chunking(
            &tapes,
            "MULTI001",
            ChunkingMode::fastcdc_default(),
            8,
            "primary",
            false, // not WORM
            core_mediachanger::DedupScope::Local,
        )
        .expect("create with named backend");
        assert_eq!(cart.backend(), "primary");
    }
    // Re-open: the backend name survives a manifest round-trip.
    let cart = Cartridge::open(&tapes, "MULTI001", CartridgeOpenMode::Open)
        .expect("re-open named-backend cartridge");
    assert_eq!(cart.backend(), "primary");
}

#[test]
fn test_worm_cartridge_persists_flag_and_refuses_mid_tape_write() {
    use core_mediachanger::ChunkingMode;

    let dir = create_test_dir();
    let tapes = dir.path().join("tapes");
    {
        let mut cart = Cartridge::create_with_chunking(
            &tapes,
            "WORM001",
            ChunkingMode::fastcdc_default(),
            8,
            "primary",
            true, // WORM
            core_mediachanger::DedupScope::Local,
        )
        .expect("create WORM cartridge");
        assert!(cart.worm());
        // Append at EOD: allowed.
        cart.write_data(Bytes::from(vec![0xAA; 256])).unwrap();
        cart.write_data(Bytes::from(vec![0xBB; 256])).unwrap();
        // Rewind moves head off EOD; next write must be refused.
        cart.rewind();
        let err = cart
            .write_data(Bytes::from(vec![0xCC; 256]))
            .expect_err("WORM must refuse mid-tape write");
        assert!(matches!(
            err,
            core_mediachanger::errors::SmcError::WormViolation
        ));
    }
    // Re-open: WORM flag survives the manifest round-trip and still
    // enforces append-only.
    let cart = Cartridge::open(&tapes, "WORM001", CartridgeOpenMode::Open)
        .expect("re-open WORM cartridge");
    assert!(cart.worm());
}

#[test]
fn test_worm_cartridge_refuses_erase_and_format_and_allow_overwrite() {
    use core_mediachanger::ChunkingMode;

    let dir = create_test_dir();
    let tapes = dir.path().join("tapes");
    let mut cart = Cartridge::create_with_chunking(
        &tapes,
        "WORM002",
        ChunkingMode::fastcdc_default(),
        8,
        "primary",
        true,
        core_mediachanger::DedupScope::Local,
    )
    .expect("create WORM cartridge");

    let err = cart.erase().expect_err("WORM must refuse erase");
    assert!(matches!(
        err,
        core_mediachanger::errors::SmcError::WormViolation
    ));

    let err = cart
        .apply_format_medium(0x00)
        .expect_err("WORM must refuse FORMAT MEDIUM");
    assert!(matches!(
        err,
        core_mediachanger::errors::SmcError::WormViolation
    ));

    let err = cart
        .set_allow_overwrite(0, 0)
        .expect_err("WORM must refuse ALLOW OVERWRITE");
    assert!(matches!(
        err,
        core_mediachanger::errors::SmcError::WormViolation
    ));
}

/// Regression for the cold-start "wiped local pool" scenario
/// (`test-backup-cloud.sh`, 2026-05-03): the manifest still claims
/// `Both` for every chunk, but the pool file under
/// `<data_dir>/chunks/<backend>/<aa>/<bb>/<hash>.dat` is missing. The
/// async read path must refetch from cloud on miss instead of
/// surfacing the OS NotFound to the SCSI layer.
#[tokio::test]
async fn read_block_async_refetches_when_pool_file_missing() {
    let dir = create_test_dir();
    let tapes_path = dir.path().join("tapes");
    let bucket_path = dir.path().join("bucket");

    // Create cartridge + write data + seal.
    let test_data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    {
        let mut cart = Cartridge::open(
            &tapes_path,
            "REFETCH001",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )
        .expect("create cartridge");
        cart.write_data(Bytes::from(test_data.clone())).unwrap();
        // drop seals trailing chunk via flush_and_seal
    }

    // Reopen with a cloud backend and push the chunk to "the cloud".
    let backend: Box<dyn core_mediachanger::CloudBackend> = Box::new(
        LocalBackend::new(&bucket_path)
            .await
            .expect("create LocalBackend"),
    );
    let mut cart = Cartridge::open_with_cloud_async(
        &tapes_path,
        "REFETCH001",
        CartridgeOpenMode::Open,
        Some(backend),
    )
    .await
    .expect("reopen with cloud backend");
    let pending = cart.get_pending_uploads();
    assert!(!pending.is_empty(), "expected at least one chunk to upload");
    for (chunk_id, _hash, _path) in &pending {
        cart.upload_chunk_to_cloud(*chunk_id)
            .await
            .expect("upload chunk to cloud");
    }
    let pool_path_for_chunk = pending[0].2.clone();
    drop(cart);

    // Wipe the local pool — simulates the cold-start scenario the
    // integration test reproduces with `rm -rf <data_dir>/chunks/`.
    assert!(
        pool_path_for_chunk.is_file(),
        "pool file should exist before wipe"
    );
    // Chunk store root is the parent of `tapes/` (see derive_chunk_store).
    let pool_root = dir.path().join("chunks").join("primary");
    std::fs::remove_dir_all(&pool_root).expect("wipe local pool");
    assert!(
        !pool_path_for_chunk.is_file(),
        "pool file should be gone after wipe"
    );

    // Re-establish the chunk store layout (cartridge open expects it).
    let _store = ChunkStore::new(dir.path(), "primary").expect("recreate chunk store dir");

    // Reopen with the same cloud backend and read — must transparently
    // refetch the chunk from the bucket and re-cache it locally.
    let backend2: Box<dyn core_mediachanger::CloudBackend> = Box::new(
        LocalBackend::new(&bucket_path)
            .await
            .expect("reopen LocalBackend"),
    );
    let mut cart = Cartridge::open_with_cloud_async(
        &tapes_path,
        "REFETCH001",
        CartridgeOpenMode::Open,
        Some(backend2),
    )
    .await
    .expect("reopen after wipe");
    cart.rewind();
    let block = cart.read_block_async(0).await.expect("read block");
    assert_eq!(block.kind, BlockKind::Data);
    assert_eq!(block.data.as_ref(), test_data.as_slice());
    assert!(
        pool_path_for_chunk.is_file(),
        "pool file should be restored after refetch"
    );
    // The cache miss pulled the chunk down from cloud, and the read
    // then served the 4096-byte plaintext block to the host.
    assert!(
        cart.backend_bytes_read() >= test_data.len() as u64,
        "backend_bytes_read should cover the refetched chunk"
    );
    assert_eq!(cart.host_bytes_read(), test_data.len() as u64);
}

/// Without a configured cloud backend, a wiped pool should surface a
/// clear error rather than the raw OS NotFound. Read-only or
/// air-gapped local-backend deployments need to know they cannot
/// recover a chunk that's missing from the local pool.
#[tokio::test]
async fn read_block_async_errors_when_pool_missing_and_no_backend() {
    let dir = create_test_dir();
    let tapes_path = dir.path().join("tapes");

    let test_data: Vec<u8> = vec![0xAB; 1024];
    {
        let mut cart = Cartridge::open(
            &tapes_path,
            "NOFETCH001",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )
        .unwrap();
        cart.write_data(Bytes::from(test_data.clone())).unwrap();
    }

    // Wipe the pool.
    // Chunk store root is the parent of `tapes/` (see derive_chunk_store).
    let pool_root = dir.path().join("chunks").join("primary");
    std::fs::remove_dir_all(&pool_root).expect("wipe pool");
    let _store = ChunkStore::new(dir.path(), "primary").expect("recreate chunk store dir");

    // Reopen with no backend.
    let mut cart = Cartridge::open(&tapes_path, "NOFETCH001", CartridgeOpenMode::Open).unwrap();
    cart.rewind();
    let err = cart
        .read_block_async(0)
        .await
        .expect_err("read must fail when chunk missing and no backend");
    let msg = format!("{err}");
    assert!(
        msg.contains("missing from local pool") || msg.contains("no cloud backend"),
        "expected clear missing-pool error, got: {msg}"
    );
}

#[test]
fn test_early_warning_fires_once_then_eom_refuses_writes() {
    use bytes::Bytes;
    use core_mediachanger::ChunkingMode;
    use core_mediachanger::errors::SmcError;

    // Tiny capacity (1 GB = 1 GiB on this code path) so EW threshold
    // is reachable without writing for an hour.
    let dir = create_test_dir();
    let tapes = dir.path().join("tapes");
    use core_mediachanger::CartridgeOpenMode;
    let _ = ChunkingMode::Fixed { size_bytes: 0 }; // silence import warning
    let mut cart = Cartridge::open_with(
        &tapes,
        "EW001L8",
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: core_mediachanger::DedupScope::Local,
        },
        core_mediachanger::CartridgeOpenOptions::new()
            .with_capacity_gb(1) // 1 GiB effective
            .with_chunk_size(64 * 1024 * 1024),
    )
    .expect("create EW cartridge with explicit capacity");

    let total = cart
        .effective_capacity_bytes()
        .expect("capacity should be bounded");
    let threshold = cart
        .early_warning_threshold_bytes()
        .expect("threshold should exist");
    assert!(threshold < total);
    assert!(!cart.early_warning_reported());

    // Write 1 MiB blocks until we cross 95%. Each call returns Ok
    // until the latch fires once.
    let block = Bytes::from(vec![0xAAu8; 1024 * 1024]);
    let mut ew_seen = 0usize;
    let mut wrote_until_eom = false;
    for _ in 0..(total / block.len() as u64 + 8) {
        match cart.write_data(block.clone()) {
            Ok(_) => {}
            Err(SmcError::EarlyWarning) => {
                ew_seen += 1;
                assert!(cart.early_warning_reported());
                assert!(cart.used_capacity_bytes() >= threshold);
            }
            Err(SmcError::EndOfMedium) => {
                wrote_until_eom = true;
                break;
            }
            Err(e) => panic!("unexpected error during EW run: {:?}", e),
        }
    }
    assert_eq!(
        ew_seen, 1,
        "early warning must fire exactly once per pass to BOM"
    );
    assert!(wrote_until_eom, "must hit EndOfMedium past 100%");

    // After EOM, every write is refused.
    let err = cart
        .write_data(block.clone())
        .expect_err("post-EOM writes must be refused");
    assert!(matches!(err, SmcError::EndOfMedium));

    // Rewind clears the latch; the next 95%-crossing fires EW again.
    cart.rewind();
    assert!(!cart.early_warning_reported());
}

#[test]
fn test_set_capacity_proportion_persists_and_shrinks_effective() {
    use core_mediachanger::CartridgeOpenMode;
    use core_mediachanger::ChunkingMode;

    let dir = create_test_dir();
    let tapes = dir.path().join("tapes");
    let mut cart = Cartridge::open_with(
        &tapes,
        "SC001L8",
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: core_mediachanger::DedupScope::Local,
        },
        core_mediachanger::CartridgeOpenOptions::new()
            .with_capacity_gb(10)
            .with_chunk_size(64 * 1024 * 1024),
    )
    .expect("create capacity cartridge");
    let _ = ChunkingMode::fastcdc_default(); // silence import on no-CDC tape

    let full = cart.effective_capacity_bytes().unwrap();
    // Half capacity.
    cart.set_capacity_proportion(u16::MAX / 2)
        .expect("set capacity to ~50%");
    let half = cart.effective_capacity_bytes().unwrap();
    assert!(
        half < full && half > full / 3,
        "expected ~50% capacity; got {}/{}",
        half,
        full
    );
    assert_eq!(cart.capacity_proportion(), u16::MAX / 2);

    // 0 is treated as full per SSC-5.
    cart.set_capacity_proportion(0).expect("0 → full native");
    assert_eq!(cart.capacity_proportion(), u16::MAX);
    assert_eq!(cart.effective_capacity_bytes().unwrap(), full);

    // Persistence: re-open and the proportion persists.
    cart.set_capacity_proportion(1024).expect("set tiny");
    drop(cart);
    let cart = Cartridge::open(&tapes, "SC001L8", CartridgeOpenMode::Open)
        .expect("re-open after SET CAPACITY");
    assert_eq!(cart.capacity_proportion(), 1024);
    let tiny = cart.effective_capacity_bytes().unwrap();
    assert!(tiny < full / 30, "tiny proportion should shrink capacity");
}

#[test]
fn test_standard_manifest_omits_media_type_field() {
    // Standard cartridges should not write a media_type entry into
    // manifest.json — the `skip_serializing_if` keeps the on-disk shape
    // backward-compatible with older deployments.
    use core_mediachanger::ChunkingMode;

    let dir = create_test_dir();
    let tapes = dir.path().join("tapes");
    let _cart = Cartridge::create_with_chunking(
        &tapes,
        "STD001L8",
        ChunkingMode::fastcdc_default(),
        8,
        "primary",
        false,
        core_mediachanger::DedupScope::Local,
    )
    .expect("create standard cartridge");
    let manifest_path = tapes.join("STD001L8").join("manifest.json");
    let body = std::fs::read_to_string(&manifest_path).expect("read manifest");
    assert!(
        !body.contains("media_type"),
        "standard manifest should omit media_type; got:\n{}",
        body
    );
}
