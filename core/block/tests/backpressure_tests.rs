// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for upload backpressure — the per-backend
//! `PoolBudget` gate at page-seal time. Block-side parallel of
//! `core/smc/tests/backpressure_tests.rs`.
//!
//! Production wiring (daemon-side):
//!   * Daemon constructs one `Arc<PoolBudget>` per `cloud.backends`
//!     entry at startup and seeds it via
//!     `core_block::refresh_pool_budget_from_volumes`.
//!   * `discover_and_register` plumbs each budget into every
//!     `VolumeWriter` via `with_pool_budget(budget, deadline)`.
//!   * `VolumeWriter::write_page_unsynced` calls
//!     `pool_budget.try_reserve` before `pool.insert_bytes`.
//!   * The eviction worker calls `pool_budget.release` after each
//!     successful `pool.remove`.
//!
//! These tests stand in for that wiring by constructing a
//! `PoolBudget` directly and attaching it via `with_pool_budget`.

use std::sync::Arc;
use std::time::Duration;

use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
use core_block::{DedupScope, UploaderError, VolumeManifest, VolumeWriter};
use shared_object_store::{LocalBackend, ObjectStoreBackend};
use shared_pool::PoolBudget;
use tempfile::TempDir;

/// Pages are 64 KiB by default. Three pages = 192 KiB; a 128 KiB cap
/// admits the first 2 pages but blocks/times-out on the third.
const PAGE: usize = DEFAULT_PAGE_SIZE_BYTES as usize;

/// Bring up a 4 MiB Local-scope volume backed by a `LocalBackend`,
/// wire the supplied budget + deadline into the writer, and return
/// (TempDir, Arc<VolumeWriter>). Single backend → all pages share
/// the same `PoolBudget`.
async fn fixture(budget: Arc<PoolBudget>, deadline: Duration) -> (TempDir, Arc<VolumeWriter>) {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_path_buf();
    let cloud_root = data_dir.join("cloud");
    std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
    let backend = LocalBackend::new(&cloud_root).await.expect("local backend");
    let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

    let name = "vol-bp";
    VolumeManifest::new(
        name.to_string(),
        4 * (1u64 << 20),
        DEFAULT_SECTOR_BYTES,
        DEFAULT_PAGE_SIZE_BYTES,
        "primary".into(),
        DedupScope::Local,
        false,
        0,
    )
    .expect("manifest new")
    .create(&data_dir)
    .expect("manifest create");

    let writer = VolumeWriter::open(&data_dir, name, backend)
        .expect("open writer")
        .with_pool_budget(budget, deadline);
    (tmp, Arc::new(writer))
}

/// Distinct payloads so each page seals as a fresh chunk (no local
/// dedup hits that would release the reservation early).
fn page_bytes(seed: u8) -> Vec<u8> {
    let mut v = vec![0u8; PAGE];
    for (i, b) in v.iter_mut().enumerate() {
        *b = seed.wrapping_add((i & 0xFF) as u8);
    }
    v
}

#[tokio::test]
async fn seal_succeeds_under_cap() {
    // 4 MiB cap is way larger than any seal in this test; reservations
    // land instantly.
    let budget = Arc::new(PoolBudget::new(
        std::path::PathBuf::from("."),
        4 * 1024 * 1024,
        0,
        80,
    ));
    let (_tmp, writer) = fixture(budget.clone(), Duration::from_secs(1)).await;
    writer.write_page(0, &page_bytes(0xAA)).await.unwrap();
    writer.write_page(1, &page_bytes(0xBB)).await.unwrap();
    writer.write_page(2, &page_bytes(0xCC)).await.unwrap();
    // Three distinct pages → budget reflects three sealed chunks.
    assert!(budget.current_bytes() >= (3 * PAGE) as u64);
}

#[tokio::test]
async fn seal_blocks_then_succeeds_after_release() {
    // Cap = exactly two pages. The third page-seal would push us over;
    // background release of one page's worth of bytes should let it
    // through after a short delay.
    let cap = (2 * PAGE) as u64;
    let budget = Arc::new(PoolBudget::new(std::path::PathBuf::from("."), cap, 0, 80));
    let (_tmp, writer) = fixture(budget.clone(), Duration::from_secs(2)).await;

    writer.write_page(0, &page_bytes(0xAA)).await.unwrap();
    writer.write_page(1, &page_bytes(0xBB)).await.unwrap();
    assert_eq!(budget.current_bytes(), (2 * PAGE) as u64);

    // Background thread releases one page's worth of bytes after a
    // short delay — mimics the eviction worker reclaiming an LRU
    // chunk under sustained pressure.
    let bg = budget.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        bg.release(PAGE as u64, None);
    });

    let started = std::time::Instant::now();
    writer.write_page(2, &page_bytes(0xCC)).await.unwrap();
    let waited = started.elapsed();
    releaser.join().unwrap();

    assert!(
        waited >= Duration::from_millis(100),
        "third seal should have blocked at least ~150ms, blocked {:?}",
        waited
    );
}

#[tokio::test]
async fn seal_times_out_when_no_release_arrives() {
    // Cap = two pages, no release path. The third seal must time out
    // and surface UploaderError::Backpressured.
    let cap = (2 * PAGE) as u64;
    let budget = Arc::new(PoolBudget::new(std::path::PathBuf::from("."), cap, 0, 80));
    let (_tmp, writer) = fixture(budget, Duration::from_millis(200)).await;

    writer.write_page(0, &page_bytes(0xAA)).await.unwrap();
    writer.write_page(1, &page_bytes(0xBB)).await.unwrap();
    let err = writer
        .write_page(2, &page_bytes(0xCC))
        .await
        .expect_err("third seal should time out");
    assert!(
        matches!(err, UploaderError::Backpressured(_)),
        "expected Backpressured, got {:?}",
        err
    );
}

#[tokio::test]
async fn dedup_hit_releases_reservation() {
    // Same bytes written twice → second insert hits local dedup → the
    // reservation made for that second seal must be released so a
    // *third* distinct page can still fit under a 2-page cap.
    let cap = (2 * PAGE) as u64;
    let budget = Arc::new(PoolBudget::new(std::path::PathBuf::from("."), cap, 0, 80));
    let (_tmp, writer) = fixture(budget.clone(), Duration::from_millis(500)).await;

    writer.write_page(0, &page_bytes(0xAA)).await.unwrap();
    // Same bytes, different page id → local dedup hit on insert. The
    // reservation must be released; current_bytes stays at one page.
    writer.write_page(1, &page_bytes(0xAA)).await.unwrap();
    assert_eq!(
        budget.current_bytes(),
        PAGE as u64,
        "dedup hit must release the reservation"
    );

    // One more distinct page lands under cap — would fail if the
    // dedup-hit reservation hadn't been released.
    writer.write_page(2, &page_bytes(0xBB)).await.unwrap();
    assert_eq!(budget.current_bytes(), (2 * PAGE) as u64);
}
