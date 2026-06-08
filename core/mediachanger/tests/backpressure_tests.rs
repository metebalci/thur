// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for upload backpressure — the per-backend
//! `PoolBudget` gate at chunk-seal time.
//!
//! Production wiring (daemon-side):
//!   * Daemon constructs one `Arc<PoolBudget>` per `storage.backends`
//!     entry at startup.
//!   * `DriveManager::set_pool_budgets` plumbs them into every
//!     loaded cartridge via `Cartridge::set_pool_budget`.
//!   * `Cartridge::seal_current_chunk` calls
//!     `pool_budget.try_reserve` before `insert_from_path`.
//!   * Eviction calls `pool_budget.release` after each successful
//!     `chunk_store.remove`.
//!
//! These tests stand in for that wiring by constructing a
//! `PoolBudget` directly and attaching it to a Cartridge built via
//! the public `set_pool_budget` setter.

mod common;

use bytes::Bytes;
use common::create_test_dir;
use core_mediachanger::errors::SmcError;
use core_mediachanger::{Cartridge, CartridgeOpenMode, ChunkingMode, DedupScope, PoolBudget};
use std::sync::Arc;
use std::time::Duration;

/// Build a Cartridge with a small fixed chunk size and a wired-in
/// pool budget. 128 KiB chunks so a sequence of equal-sized writes
/// produces predictable seals.
fn cart_with_budget(
    dir: &std::path::Path,
    label: &str,
    budget: Arc<PoolBudget>,
    deadline: Duration,
) -> Cartridge {
    let tapes_path = dir.join("tapes");
    let mut cart = Cartridge::create_with_chunking(
        &tapes_path,
        label,
        ChunkingMode::Fixed {
            size_bytes: 128 * 1024,
        },
        8,
        "primary",
        false,
        DedupScope::Global,
    )
    .expect("create_with_chunking");
    cart.set_pool_budget(budget, deadline);
    cart
}

/// Helper: write three 128 KiB blocks. Block 1 fills the active
/// staging chunk (no seal). Block 2 rolls → seal of chunk 1 (reserves
/// 128 KiB) → opens chunk 2 and lands the block. Block 3 rolls again
/// → seal of chunk 2 (reserves 128 KiB more) — this is where the
/// budget gate fires when cap == 128 KiB.
fn do_three_writes(cart: &mut Cartridge) -> core_mediachanger::errors::Result<()> {
    cart.write_data(Bytes::from(vec![0xAA; 128 * 1024]))?;
    cart.write_data(Bytes::from(vec![0xBB; 128 * 1024]))?;
    cart.write_data(Bytes::from(vec![0xCC; 128 * 1024]))?;
    Ok(())
}

#[test]
fn seal_succeeds_under_cap() {
    let dir = create_test_dir();
    // 4 MiB cap is way larger than any seal in this test; reservations
    // land instantly.
    let budget = Arc::new(PoolBudget::new(
        dir.path().to_path_buf(),
        4 * 1024 * 1024,
        0,
        80,
    ));
    let mut cart = cart_with_budget(dir.path(), "BP_OK", budget, Duration::from_secs(1));
    do_three_writes(&mut cart).unwrap();
}

#[test]
fn seal_blocks_then_succeeds_after_release() {
    let dir = create_test_dir();
    // 128 KiB cap = exactly one chunk. The second seal (during write 3)
    // backpressures until something releases.
    let budget = Arc::new(PoolBudget::new(dir.path().to_path_buf(), 128 * 1024, 0, 80));
    let mut cart = cart_with_budget(
        dir.path(),
        "BP_BLOCK",
        budget.clone(),
        Duration::from_secs(2),
    );

    // Background thread will release 128 KiB after a delay, mimicking
    // an upload-completion eviction.
    let bg = budget.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        bg.release(128 * 1024, None);
    });

    // The third write rolls → seal of chunk 2 → reserves 128 KiB →
    // 128 + 128 > 128 cap → blocks until the releaser fires.
    let started = std::time::Instant::now();
    do_three_writes(&mut cart).unwrap();
    let waited = started.elapsed();
    releaser.join().unwrap();

    assert!(
        waited >= Duration::from_millis(100),
        "seal should have blocked at least ~150ms, blocked {:?}",
        waited
    );
}

#[test]
fn seal_times_out_when_no_release_arrives() {
    let dir = create_test_dir();
    // 128 KiB cap, no eviction — third write's seal must time out.
    let budget = Arc::new(PoolBudget::new(dir.path().to_path_buf(), 128 * 1024, 0, 80));
    let mut cart = cart_with_budget(dir.path(), "BP_TIMEOUT", budget, Duration::from_millis(150));

    let err = do_three_writes(&mut cart).expect_err("should time out");
    assert!(
        matches!(err, SmcError::Backpressured(_)),
        "expected Backpressured, got {:?}",
        err
    );
}

#[test]
fn drop_force_seals_past_cap_and_does_not_lose_data() {
    let dir = create_test_dir();
    // Tight cap so a regular seal would hit backpressure.
    let budget = Arc::new(PoolBudget::new(dir.path().to_path_buf(), 128 * 1024, 0, 80));
    let label = "BP_DROP";
    {
        let mut cart = cart_with_budget(
            dir.path(),
            label,
            budget.clone(),
            // Short deadline so a non-force seal would time out, not
            // hang the test.
            Duration::from_millis(50),
        );
        // Write a 128 KiB block — fills the active staging chunk
        // exactly at the cap. Drop will force-seal past the budget.
        cart.write_data(Bytes::from(vec![0xCC; 128 * 1024]))
            .unwrap();
    } // Drop runs here — must not lose the trailing chunk.

    // Reopen and confirm the data is still there.
    let tapes_path = dir.path().join("tapes");
    let mut cart = Cartridge::open(&tapes_path, label, CartridgeOpenMode::Open).unwrap();
    let block = cart.read_block(0).unwrap();
    assert_eq!(block.data.len(), 128 * 1024);
    assert!(block.data.iter().all(|&b| b == 0xCC));
}
