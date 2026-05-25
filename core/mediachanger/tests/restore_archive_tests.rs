// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cartridge archive → restore round-trip tests.
//!
//! Each test seeds a cartridge, archives it, wipes the cartridge,
//! restores from the archive, and reads every block back to confirm
//! byte-for-byte equivalence.

mod common;

use bytes::Bytes;
use common::create_test_dir;
use core_mediachanger::cartridge_archive::{ArchiveOptions, run_archive};
use core_mediachanger::library::restore_archive::{RestoreArchiveOptions, run_restore_archive};
use core_mediachanger::{
    Cartridge, CartridgeOpenMode, DedupScope, Library, LocalBackend, ObjectStoreBackend, SmcError,
};
use std::fs;
use std::path::Path;

const BACKEND: &str = "primary";

fn init_library(work: &Path) -> Library {
    let lib_root = work.join("library");
    let tapes_dir = work.join("tapes");
    fs::create_dir_all(&tapes_dir).expect("tapes_dir");
    Library::initialize(&lib_root, &tapes_dir, 8, 0, 1, 8, None, 0, 1001, 101, 1)
        .expect("library init")
}

/// Seed a cartridge bound to `BACKEND`, write data, upload to the
/// bucket. Returns the byte fixture.
async fn seed(
    tapes: &Path,
    bucket: &Path,
    label: &str,
    dedup: DedupScope,
    n_blocks: usize,
) -> Vec<Vec<u8>> {
    let backend: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(bucket).await.expect("be"));
    let mut cart = Cartridge::open_with_cloud_async(
        tapes,
        label,
        CartridgeOpenMode::Create {
            backend: BACKEND.to_string(),
            worm: false,
            dedup,
        },
        Some(backend),
    )
    .await
    .expect("create");

    const BLOCK_SIZE: usize = 4096;
    let mut written = Vec::with_capacity(n_blocks);
    for i in 0..n_blocks {
        let data: Vec<u8> = (0..BLOCK_SIZE)
            .map(|j| ((label.as_bytes()[0] as usize + i * 17 + j * 3) & 0xFF) as u8)
            .collect();
        cart.write_data(Bytes::from(data.clone())).expect("write");
        written.push(data);
    }
    cart.flush_and_seal().expect("flush");

    let pending: Vec<u64> = cart
        .get_pending_uploads()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    for id in pending {
        cart.upload_chunk_to_cloud(id).await.expect("upload chunk");
    }
    cart.backup_manifest_to_cloud()
        .await
        .expect("backup_manifest_to_cloud");
    drop(cart);
    written
}

#[tokio::test]
async fn archive_then_restore_round_trip() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let bucket = work.path().join("bucket");
    fs::create_dir_all(&bucket).expect("mkdir");
    let mut library = init_library(work.path());

    let written = seed(&tapes, &bucket, "TAPE_RT", DedupScope::Global, 3).await;

    // Archive.
    let src: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let tgt: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let _ = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_RT",
        source: src.as_ref(),
        target: tgt.as_ref(),
        target_name: BACKEND,
        label: "snap1",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("archive");

    // Wipe the live cartridge: delete the local cart dir + the
    // backend's regular-pool chunks (so a restore has to actually
    // pull every chunk from the archive prefix). The local pool
    // for this dedup scope lives at `<work>/chunks/<backend>/`.
    fs::remove_dir_all(tapes.join("TAPE_RT")).expect("rm cart");
    let local_pool = work.path().join("chunks").join(BACKEND);
    if local_pool.exists() {
        fs::remove_dir_all(&local_pool).expect("rm local pool");
    }

    // Restore.
    let restore_be: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let report = run_restore_archive(RestoreArchiveOptions {
        tapes_dir: &tapes,
        backend: restore_be.as_ref(),
        backend_name: BACKEND,
        barcode: "TAPE_RT",
        label: "snap1",
        as_barcode: None,
        allow_existing: false,
        dry_run: false,
        progress: None,
    })
    .await
    .expect("restore-archive");

    assert_eq!(report.source_barcode, "TAPE_RT");
    assert_eq!(report.local_barcode, "TAPE_RT");
    assert!(report.chunks_total > 0);
    assert_eq!(report.chunks_downloaded, report.chunks_total);
    assert!(report.bytes_downloaded > 0);
    // Seat into the library (the caller's responsibility).
    library
        .add_or_create_tape(&report.local_barcode, &report.backend)
        .expect("seat");

    // Re-open the restored cartridge and read every block.
    let read_be: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let mut cart =
        Cartridge::open_with_cloud_async(&tapes, "TAPE_RT", CartridgeOpenMode::Open, Some(read_be))
            .await
            .expect("reopen");
    cart.rewind();
    for (i, expected) in written.iter().enumerate() {
        let block = cart.read_block_async(i as u64).await.expect("read");
        assert_eq!(block.data.as_ref(), expected.as_slice(), "block {}", i);
    }
}

#[tokio::test]
async fn restore_archive_rename_via_as_barcode() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let bucket = work.path().join("bucket");
    fs::create_dir_all(&bucket).expect("mkdir");
    let _library = init_library(work.path());

    let written = seed(&tapes, &bucket, "TAPE_ORIG", DedupScope::Global, 2).await;

    let src: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let tgt: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let _ = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_ORIG",
        source: src.as_ref(),
        target: tgt.as_ref(),
        target_name: BACKEND,
        label: "snap",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("archive");

    // Source cartridge still exists locally; restore to a fresh
    // barcode so the two coexist.
    let restore_be: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let report = run_restore_archive(RestoreArchiveOptions {
        tapes_dir: &tapes,
        backend: restore_be.as_ref(),
        backend_name: BACKEND,
        barcode: "TAPE_ORIG",
        label: "snap",
        as_barcode: Some("TAPE_RESTORED"),
        allow_existing: false,
        dry_run: false,
        progress: None,
    })
    .await
    .expect("restore");

    assert_eq!(report.source_barcode, "TAPE_ORIG");
    assert_eq!(report.local_barcode, "TAPE_RESTORED");

    // Both cartridge dirs exist.
    assert!(tapes.join("TAPE_ORIG").join("manifest.json").is_file());
    assert!(tapes.join("TAPE_RESTORED").join("manifest.json").is_file());

    // Restored manifest carries the new label + a fresh UUID.
    let orig: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(tapes.join("TAPE_ORIG/manifest.json")).expect("r"),
    )
    .expect("p");
    let restored: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(tapes.join("TAPE_RESTORED/manifest.json")).expect("r"),
    )
    .expect("p");
    assert_eq!(restored["label"], "TAPE_RESTORED");
    assert_ne!(restored["uuid"], orig["uuid"]);
    // Provenance survives.
    assert_eq!(restored["archived_from_backend"], BACKEND);
    assert!(restored["archived_at"].is_string());

    // Read every block from the restored cartridge.
    let read_be: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let mut cart = Cartridge::open_with_cloud_async(
        &tapes,
        "TAPE_RESTORED",
        CartridgeOpenMode::Open,
        Some(read_be),
    )
    .await
    .expect("reopen");
    cart.rewind();
    for (i, expected) in written.iter().enumerate() {
        let block = cart.read_block_async(i as u64).await.expect("read");
        assert_eq!(block.data.as_ref(), expected.as_slice());
    }
}

#[tokio::test]
async fn restore_archive_dry_run_writes_nothing() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let bucket = work.path().join("bucket");
    fs::create_dir_all(&bucket).expect("mkdir");
    let _library = init_library(work.path());

    let _ = seed(&tapes, &bucket, "TAPE_DR", DedupScope::Global, 1).await;
    let src: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let tgt: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let _ = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_DR",
        source: src.as_ref(),
        target: tgt.as_ref(),
        target_name: BACKEND,
        label: "snap",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("archive");

    fs::remove_dir_all(tapes.join("TAPE_DR")).expect("rm cart");

    let restore_be: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let report = run_restore_archive(RestoreArchiveOptions {
        tapes_dir: &tapes,
        backend: restore_be.as_ref(),
        backend_name: BACKEND,
        barcode: "TAPE_DR",
        label: "snap",
        as_barcode: None,
        allow_existing: false,
        dry_run: true,
        progress: None,
    })
    .await
    .expect("dry-run");

    assert!(report.dry_run);
    assert!(!tapes.join("TAPE_DR").exists());
}

#[tokio::test]
async fn restore_archive_refuses_missing_archive() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let bucket = work.path().join("bucket");
    fs::create_dir_all(&bucket).expect("mkdir");
    let _library = init_library(work.path());

    let backend: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let err = run_restore_archive(RestoreArchiveOptions {
        tapes_dir: &tapes,
        backend: backend.as_ref(),
        backend_name: BACKEND,
        barcode: "TAPE_NONE",
        label: "snap",
        as_barcode: None,
        allow_existing: false,
        dry_run: false,
        progress: None,
    })
    .await
    .expect_err("must refuse missing archive");
    matches!(err, SmcError::InvalidOp(_));
}

#[tokio::test]
async fn restore_archive_allow_existing_skips() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let bucket = work.path().join("bucket");
    fs::create_dir_all(&bucket).expect("mkdir");
    let _library = init_library(work.path());

    let _ = seed(&tapes, &bucket, "TAPE_E", DedupScope::Global, 1).await;
    let src: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let tgt: Box<dyn ObjectStoreBackend> = Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let _ = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_E",
        source: src.as_ref(),
        target: tgt.as_ref(),
        target_name: BACKEND,
        label: "snap",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("archive");

    // Cartridge still locally present — restore without
    // --allow-existing refuses; with it, skips silently.
    let restore_be: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let err = run_restore_archive(RestoreArchiveOptions {
        tapes_dir: &tapes,
        backend: restore_be.as_ref(),
        backend_name: BACKEND,
        barcode: "TAPE_E",
        label: "snap",
        as_barcode: None,
        allow_existing: false,
        dry_run: false,
        progress: None,
    })
    .await
    .expect_err("must refuse existing");
    matches!(err, SmcError::InvalidOp(_));

    let restore_be2: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("be"));
    let report = run_restore_archive(RestoreArchiveOptions {
        tapes_dir: &tapes,
        backend: restore_be2.as_ref(),
        backend_name: BACKEND,
        barcode: "TAPE_E",
        label: "snap",
        as_barcode: None,
        allow_existing: true,
        dry_run: false,
        progress: None,
    })
    .await
    .expect("allow-existing skips");
    assert!(report.skipped_existing);
}
