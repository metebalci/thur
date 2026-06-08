// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! End-to-end batch restore test for `library::restore::run_restore`.
//!
//! The single-cartridge cold-bucket round-trip is already covered by
//! `index_backup_tests::cold_bucket_dr_via_index_pages`. This test
//! exercises what's new on top of that primitive: discovery from the
//! storage bucket alone, the batch driver that fans out per-cartridge,
//! the filter, and the on-disk state seeded by the restore pass.

mod common;

use bytes::Bytes;
use common::create_test_dir;
use core_mediachanger::library::restore::run_restore;
use core_mediachanger::{
    Cartridge, CartridgeOpenMode, DedupScope, LocalBackend, ObjectStoreBackend,
};
use std::fs;

/// Create a cartridge bound to the test LocalBackend, write `n_blocks`
/// of deterministic data, force seal + manifest backup to storage, drop
/// the cartridge. Returns the data so callers can verify reads later.
async fn seed_cartridge(
    tapes: &std::path::Path,
    backend: Box<dyn ObjectStoreBackend>,
    label: &str,
    n_blocks: usize,
) -> Vec<Vec<u8>> {
    let mut cart = Cartridge::open_with_storage_async(
        tapes,
        label,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: DedupScope::Global,
        },
        Some(backend),
    )
    .await
    .expect("create cartridge");

    const BLOCK_SIZE: usize = 4096;
    let mut written = Vec::with_capacity(n_blocks);
    for i in 0..n_blocks {
        let data: Vec<u8> = (0..BLOCK_SIZE)
            .map(|j| ((label.as_bytes()[0] as usize + i * 31 + j * 7) & 0xFF) as u8)
            .collect();
        cart.write_data(Bytes::from(data.clone()))
            .expect("write_data");
        written.push(data);
    }
    cart.flush_and_seal().expect("flush_and_seal");
    cart.backup_manifest_to_storage()
        .await
        .expect("backup_manifest_to_storage");
    written
}

#[tokio::test]
async fn run_restore_batch_round_trip_three_cartridges() {
    let work = create_test_dir();
    let backend_dir = work.path().join("backend");
    fs::create_dir_all(&backend_dir).unwrap();

    // Source side: three cartridges land in the storage bucket. Each
    // gets its own backend handle because Cartridge::open takes
    // ownership of the box.
    let source_tapes = work.path().join("source_tapes");
    fs::create_dir_all(&source_tapes).unwrap();
    let mut originals = Vec::new();
    for label in &["TAPE_A", "TAPE_B", "TAPE_C"] {
        let backend: Box<dyn ObjectStoreBackend> =
            Box::new(LocalBackend::new(&backend_dir).await.unwrap());
        let bytes = seed_cartridge(&source_tapes, backend, label, 4).await;
        originals.push((label.to_string(), bytes));
    }

    // Wipe the source tapes dir so the restore truly has nothing
    // local to lean on — the only state remaining is in the storage
    // bucket at `backend_dir`.
    fs::remove_dir_all(&source_tapes).unwrap();

    // Target side: a fresh data dir. Discovery should find all three
    // cartridges and the restore should reconstruct each one.
    let target_tapes = work.path().join("target_tapes");
    fs::create_dir_all(&target_tapes).unwrap();
    let restore_backend: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&backend_dir).await.unwrap());

    let report = run_restore(
        &target_tapes,
        restore_backend.as_ref(),
        "mirror",
        &[],
        false,
        false,
    )
    .await
    .unwrap();

    assert_eq!(report.discovered.len(), 3, "{:#?}", report);
    let successes = report.successes();
    assert_eq!(
        successes,
        vec!["TAPE_A", "TAPE_B", "TAPE_C"],
        "report: {:#?}",
        report
    );
    assert!(report.failures().is_empty(), "{:#?}", report);

    // Per-cartridge on-disk state seeded by the restore: each
    // directory must exist with a manifest and the two index files.
    for (label, _) in &originals {
        let cart_root = target_tapes.join(label);
        assert!(
            cart_root.join("manifest.json").exists(),
            "manifest missing for {}",
            label
        );
        assert!(
            cart_root.join("chunks.idx").exists(),
            "chunks.idx missing for {}",
            label
        );
        assert!(
            cart_root.join("blocks-p0.idx").exists(),
            "blocks-p0.idx missing for {}",
            label
        );
    }

    // Reopen each cartridge and read back every block — proves the
    // restored metadata + the chunk-pool refs in storage are sufficient
    // for the cartridge to serve reads.
    for (label, original) in &originals {
        let backend: Box<dyn ObjectStoreBackend> =
            Box::new(LocalBackend::new(&backend_dir).await.unwrap());
        let mut cart = Cartridge::open_with_storage_async(
            &target_tapes,
            label,
            CartridgeOpenMode::Open,
            Some(backend),
        )
        .await
        .unwrap();
        cart.rewind();
        for (i, expected) in original.iter().enumerate() {
            let block = cart
                .read_block_async(i as u64)
                .await
                .unwrap_or_else(|e| panic!("read block {} of {} failed: {:?}", i, label, e));
            match block.kind {
                core_mediachanger::BlockKind::Data => assert_eq!(
                    block.data.as_ref(),
                    expected.as_slice(),
                    "block {} of {} diverged after batch restore",
                    i,
                    label
                ),
                other => panic!("block {} of {} unexpected kind {:?}", i, label, other),
            }
        }
    }
}

#[tokio::test]
async fn run_restore_filter_only_attempts_selected() {
    let work = create_test_dir();
    let backend_dir = work.path().join("backend");
    fs::create_dir_all(&backend_dir).unwrap();

    let source_tapes = work.path().join("source_tapes");
    fs::create_dir_all(&source_tapes).unwrap();
    for label in &["TAPE_KEEP", "TAPE_DROP"] {
        let backend: Box<dyn ObjectStoreBackend> =
            Box::new(LocalBackend::new(&backend_dir).await.unwrap());
        seed_cartridge(&source_tapes, backend, label, 2).await;
    }
    fs::remove_dir_all(&source_tapes).unwrap();

    let target_tapes = work.path().join("target_tapes");
    fs::create_dir_all(&target_tapes).unwrap();
    let restore_backend: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&backend_dir).await.unwrap());

    let report = run_restore(
        &target_tapes,
        restore_backend.as_ref(),
        "mirror",
        &["TAPE_KEEP".to_string()],
        false,
        false,
    )
    .await
    .unwrap();

    assert_eq!(report.successes(), vec!["TAPE_KEEP"], "{:#?}", report);
    assert_eq!(report.filtered_out, vec!["TAPE_DROP".to_string()]);
    assert!(
        target_tapes.join("TAPE_KEEP").exists(),
        "kept cartridge dir missing"
    );
    assert!(
        !target_tapes.join("TAPE_DROP").exists(),
        "filtered-out cartridge must not be restored"
    );
}
