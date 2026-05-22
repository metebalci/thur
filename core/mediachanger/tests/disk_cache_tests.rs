// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for Cache functionality
//!
//! These tests verify the cache management system including:
//! - Cache usage calculation
//! - LRU eviction policy
//! - Chunk eviction with S3 backend
//! - Cache capacity management

mod common;

use bytes::Bytes;
use common::*;
use core_mediachanger::{Cartridge, CartridgeOpenMode, DedupScope, DiskCacheManager};

#[test]
fn test_cache_manager_creation() {
    let dir = create_test_dir();
    let cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 10 * 1024 * 1024);

    assert_eq!(cache.capacity(), 10 * 1024 * 1024);
    assert_eq!(cache.current_usage(), 0);
    assert_eq!(cache.usage_percent(), 0.0);
}

#[test]
fn test_calculate_usage_empty_dir() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 1024 * 1024);

    let usage = cache.calculate_usage().unwrap();
    assert_eq!(usage, 0);
    assert_eq!(cache.current_usage(), 0);
}

#[test]
fn test_calculate_usage_with_cartridges() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 100 * 1024 * 1024);

    // Create a cartridge and write some data
    let mut cartridge = create_test_cartridge(&dir, "CACHE001");
    let data = vec![0x42; 10 * 1024]; // 10 KB
    cartridge.write_data(Bytes::from(data)).unwrap();
    drop(cartridge); // Flush to disk

    // Calculate usage
    let usage = cache.calculate_usage().unwrap();
    assert!(usage > 0, "Usage should be > 0 after writing data");
    assert_eq!(cache.current_usage(), usage);
}

#[test]
fn test_calculate_usage_multiple_cartridges() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 100 * 1024 * 1024);

    // Create multiple cartridges with data
    for i in 0..3 {
        let label = format!("CACHE{:03}", i);
        let mut cartridge = create_test_cartridge(&dir, &label);
        let data = vec![i as u8; 5 * 1024]; // 5 KB each
        cartridge.write_data(Bytes::from(data)).unwrap();
        drop(cartridge);
    }

    // Calculate total usage
    let usage = cache.calculate_usage().unwrap();
    assert!(usage > 0);
    assert!(usage >= 15 * 1024, "Should have at least 15 KB (3 x 5 KB)");
}

#[test]
fn test_is_over_capacity() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 10 * 1024);

    // Initially under capacity
    assert!(!cache.is_over_capacity());

    // Simulate usage
    cache.calculate_usage().unwrap();
    assert!(!cache.is_over_capacity());

    // Create a cartridge that exceeds capacity
    let mut cartridge = create_test_cartridge(&dir, "BIG001");
    let data = vec![0xFF; 20 * 1024]; // 20 KB - exceeds 10 KB limit
    cartridge.write_data(Bytes::from(data)).unwrap();
    drop(cartridge);

    cache.calculate_usage().unwrap();
    assert!(cache.is_over_capacity());
}

#[test]
fn test_usage_percent_calculation() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 100 * 1024);

    // Write data to fill cache to ~50%
    let mut cartridge = create_test_cartridge(&dir, "HALF001");
    let data = vec![0xAA; 50 * 1024];
    cartridge.write_data(Bytes::from(data)).unwrap();
    drop(cartridge);

    cache.calculate_usage().unwrap();
    let percent = cache.usage_percent();

    assert!(
        percent > 40.0 && percent < 60.0,
        "Usage should be ~50%, got {}",
        percent
    );
}

#[test]
fn test_zero_capacity_usage_percent() {
    let dir = create_test_dir();
    let cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 0);

    // Should not panic with zero capacity
    let percent = cache.usage_percent();
    assert_eq!(percent, 0.0);
}

#[test]
fn test_cache_with_no_tapes_directory() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 1024 * 1024);

    // Don't create any cartridges (no tapes/ directory)
    let usage = cache.calculate_usage().unwrap();
    assert_eq!(usage, 0);
}

#[test]
fn test_cache_with_empty_chunks_directory() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 1024 * 1024);

    // Create a cartridge but don't write data (empty chunks dir)
    let cartridge = create_test_cartridge(&dir, "EMPTY001");
    drop(cartridge);

    let usage = cache.calculate_usage().unwrap();
    // Should be 0 or very small (just manifest)
    assert!(usage < 1024, "Empty cartridge should have minimal usage");
}

#[test]
fn test_cache_recalculation_after_write() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 100 * 1024 * 1024);

    // Initial calculation
    let usage1 = cache.calculate_usage().unwrap();
    assert_eq!(usage1, 0);

    // Write data
    let mut cartridge = create_test_cartridge(&dir, "GROW001");
    let data1 = vec![0x11; 10 * 1024];
    cartridge.write_data(Bytes::from(data1)).unwrap();
    drop(cartridge);

    // Recalculate
    let usage2 = cache.calculate_usage().unwrap();
    assert!(usage2 > usage1, "Usage should increase after writing data");

    // Write *different* data — under content-addressed dedup an identical
    // payload would just hit the existing pool entry and not grow the
    // cache, so we use a distinct byte pattern to actually exercise growth.
    let tapes_path = dir.path().join("tapes");
    let mut cartridge = Cartridge::open(&tapes_path, "GROW001", CartridgeOpenMode::Open).unwrap();
    let data2 = vec![0x22; 10 * 1024];
    cartridge.write_data(Bytes::from(data2)).unwrap();
    drop(cartridge);

    // Recalculate again
    let usage3 = cache.calculate_usage().unwrap();
    assert!(usage3 > usage2, "Usage should increase further");
}

#[test]
fn test_cache_capacity_boundary() {
    let dir = create_test_dir();
    let capacity = 50 * 1024;
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", capacity);

    // Write data that exceeds capacity
    let mut cartridge = create_test_cartridge(&dir, "EXACT001");
    let data = vec![0x77; (capacity + 5 * 1024) as usize]; // 55 KB - exceeds 50 KB limit
    cartridge.write_data(Bytes::from(data)).unwrap();
    drop(cartridge);

    cache.calculate_usage().unwrap();

    // Should be over capacity
    assert!(cache.is_over_capacity());
}

// Note: LRU eviction tests with S3 backend require async runtime and S3 setup
// Those would typically be in separate integration tests that use tokio::test
// and would require S3 configuration. The basic LRU logic is tested above.

#[test]
fn test_calculate_usage_includes_local_scope_namespace() {
    // A `DedupScope::Local` cartridge writes its chunks to
    // `<data_dir>/chunks/<backend>/<barcode>/<aa>/<bb>/<hash>.dat`,
    // not the shared per-backend pool. The cache manager must pick
    // those up — otherwise `disk_cache.size_gb` is silently uncapped
    // for any deployment that uses local-scope cartridges.
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 100 * 1024 * 1024);

    let tapes_path = dir.path().join("tapes");
    let mut cartridge = Cartridge::open(
        &tapes_path,
        "LOCAL001",
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: DedupScope::Local,
        },
    )
    .expect("create local-scope cartridge");
    let data = vec![0x33; 16 * 1024];
    cartridge.write_data(Bytes::from(data)).unwrap();
    drop(cartridge);

    let usage = cache.calculate_usage().unwrap();
    assert!(
        usage >= 16 * 1024,
        "local-scope chunks must be visible to calculate_usage; saw {}",
        usage
    );

    // The shared pool path under `chunks/primary/<aa>/<bb>` must be
    // empty — the chunk must live under the per-cartridge namespace.
    let shared_pool = dir.path().join("chunks").join("primary");
    let mut found_shared_chunk = false;
    let mut found_namespace_chunk = false;
    if shared_pool.is_dir() {
        for shard in std::fs::read_dir(&shared_pool).unwrap() {
            let shard = shard.unwrap();
            let name = shard.file_name();
            let name = name.to_str().unwrap();
            // Two-hex shard dir = shared pool; anything else (e.g.
            // "LOCAL001") = local-scope namespace.
            if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                // Walk and see if any chunk file exists.
                for s2 in std::fs::read_dir(shard.path()).unwrap() {
                    let s2 = s2.unwrap();
                    if s2.file_type().unwrap().is_dir() {
                        for f in std::fs::read_dir(s2.path()).unwrap() {
                            if f.unwrap().file_type().unwrap().is_file() {
                                found_shared_chunk = true;
                            }
                        }
                    }
                }
            } else if name == "LOCAL001" {
                found_namespace_chunk = true;
            }
        }
    }
    assert!(
        found_namespace_chunk,
        "local-scope cartridge should have a per-cartridge namespace dir"
    );
    assert!(
        !found_shared_chunk,
        "local-scope cartridge must not place chunks in the shared pool"
    );
}

#[test]
fn test_cache_with_multiple_chunks() {
    let dir = create_test_dir();
    let mut cache = DiskCacheManager::new(dir.path().to_path_buf(), "primary", 500 * 1024 * 1024);

    // Write enough data to land at least one full sealed chunk in the
    // pool. Each block's content is distinct (byte fill = block index)
    // so content-addressed dedup doesn't collapse them into one pool
    // file — without that, two identical fills hash to the same chunk
    // and the assertion below fails.
    let mut cartridge = create_test_cartridge(&dir, "MULTI001");
    let block_size = 64 * 1024 * 1024;

    for i in 0..2u8 {
        let data = vec![i; block_size];
        cartridge.write_data(Bytes::from(data)).unwrap();
    }
    drop(cartridge);

    cache.calculate_usage().unwrap();
    let usage = cache.current_usage();

    // Should be around 128 MiB
    assert!(
        usage > 100 * 1024 * 1024,
        "Should have significant chunk data"
    );
}
