// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! End-to-end DR test for delta-page index backup.
//!
//! Exercises: write a cartridge, run `backup_manifest_to_storage`,
//! wipe the cartridge directory entirely (simulating cold-bucket
//! disaster recovery on a fresh host), reopen the cartridge from the
//! storage-only state, and verify that every previously-written block
//! reads back identically. Without index pages landing in storage, this
//! test would fail at step 4 because `chunks.idx` and `blocks-p0.idx`
//! would be empty after wipe.

use bytes::Bytes;
use core_mediachanger::{Cartridge, CartridgeOpenMode, DedupScope, LocalBackend};
use std::fs;

#[tokio::test]
async fn cold_bucket_dr_via_index_pages() {
    let work = tempfile::tempdir().unwrap();
    let tapes = work.path().join("tapes");
    let backend_dir = work.path().join("backend");
    fs::create_dir_all(&tapes).unwrap();
    let backend: Box<dyn core_mediachanger::ObjectStoreBackend> =
        Box::new(LocalBackend::new(&backend_dir).await.unwrap());

    let label = "DR0001";
    // 1. Create + write some blocks (block bytes deterministic so we
    // can verify after restore).
    let mut cart = Cartridge::open_with_storage_async(
        &tapes,
        label,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: DedupScope::Global,
        },
        Some(backend.clone()),
    )
    .await
    .unwrap();

    const BLOCK_SIZE: usize = 64 * 1024;
    const N_BLOCKS: usize = 32;
    let mut written: Vec<Vec<u8>> = Vec::with_capacity(N_BLOCKS);
    for i in 0..N_BLOCKS {
        let data: Vec<u8> = (0..BLOCK_SIZE)
            .map(|j| ((i * 7919 + j * 31) & 0xFF) as u8)
            .collect();
        cart.write_data(Bytes::from(data.clone())).unwrap();
        written.push(data);
    }
    // Force an fsync of indexes + roll the active staging chunk into
    // the pool by dropping; backup writes need the bytes durable.
    cart.flush_and_seal().unwrap();

    // 2. Backup — ships index pages + manifest sentinel.
    cart.backup_manifest_to_storage().await.unwrap();

    // Sanity: backend now contains chunk pool entries + manifest +
    // at least one chunks page + one blocks-p0 page.
    let backend_for_listing = backend.clone();
    let manifest_keys = backend_for_listing
        .list_objects(&format!("manifests/{}/", label))
        .await
        .unwrap();
    let chunks_pages: Vec<_> = manifest_keys
        .iter()
        .filter(|k| k.contains(&format!("manifests/{}/chunks/page-", label)))
        .collect();
    let blocks_pages: Vec<_> = manifest_keys
        .iter()
        .filter(|k| k.contains(&format!("manifests/{}/blocks-p0/page-", label)))
        .collect();
    assert!(
        !chunks_pages.is_empty(),
        "expected at least one chunks/page-* in storage"
    );
    assert!(
        !blocks_pages.is_empty(),
        "expected at least one blocks-p0/page-* in storage"
    );
    let sentinel = manifest_keys
        .iter()
        .find(|k| k.ends_with("manifest-latest.json"))
        .expect("sentinel must exist");
    let sentinel_json = backend.download_manifest(sentinel).await.unwrap();
    assert!(
        sentinel_json.contains("\"index_epoch\""),
        "sentinel must record index_epoch"
    );

    // 3. Drop the cartridge handle and wipe its on-disk state
    // entirely — simulates a fresh host with only the storage bucket.
    drop(cart);
    let cart_root = tapes.join(label);
    fs::remove_dir_all(&cart_root).unwrap();
    assert!(!cart_root.exists());

    // 4. Reopen — must auto-restore manifest and stitch index pages
    // back into chunks.idx + blocks-p0.idx.
    let mut cart2 = Cartridge::open_with_storage_async(
        &tapes,
        label,
        CartridgeOpenMode::Open,
        Some(backend.clone()),
    )
    .await
    .unwrap();

    // 5. Read every block back; verify byte-identical.
    cart2.rewind();
    for (i, expected) in written.iter().enumerate() {
        let block = cart2
            .read_block_async(i as u64)
            .await
            .unwrap_or_else(|e| panic!("read block {} failed: {:?}", i, e));
        match block.kind {
            core_mediachanger::BlockKind::Data => {
                assert_eq!(
                    block.data.as_slice(),
                    expected.as_slice(),
                    "block {} contents diverged after cold-bucket restore",
                    i
                );
            }
            other => panic!("block {} unexpected kind {:?}", i, other),
        }
    }
}
