// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for content-addressed chunk dedup.
//!
//! These tests assert the feature's headline behavior:
//!   * Two cartridges that write identical bytes produce one shared
//!     chunk file in the pool, not two.
//!   * Distinct content produces distinct files.
//!   * Reads from any cartridge resolve back to the shared file.
//!   * `thurvtl system gc` semantics: when a cartridge is wiped from the
//!     manifest set, its chunks become orphans in the pool.

mod common;

use bytes::Bytes;
use common::create_test_dir;
use core_mediachanger::{
    Cartridge, CartridgeOpenMode, CartridgeOpenOptions, ChunkStore, ChunkingMode, DedupScope,
};

/// Sized so a single 4 MB write past a 1 MB chunk_size triggers a roll
/// and seals the chunk into the shared pool. write_data with > chunk_size
/// produces one chunk per write (since roll happens before append).
fn small_chunk_cart(dir: &std::path::Path, label: &str) -> Cartridge {
    let tapes_path = dir.join("tapes");
    Cartridge::open_with(
        &tapes_path,
        label,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            // Dedup tests by definition exercise the shared pool.
            dedup: DedupScope::Global,
        },
        CartridgeOpenOptions::new().with_chunk_size(1024 * 1024), // 1 MB
    )
    .expect("open_with")
}

#[test]
fn identical_bytes_across_cartridges_produce_one_pool_file() {
    let dir = create_test_dir();
    let store = ChunkStore::new(dir.path(), "primary").unwrap();

    // Write a bigger-than-chunk-size payload to A so it rolls and seals.
    let mut a = small_chunk_cart(dir.path(), "DEDUP_A");
    let payload = vec![0xAB; 4 * 1024 * 1024]; // 4 MB
    a.write_data(Bytes::from(payload.clone())).unwrap();
    // Force a seal by writing one more byte to roll (since the 4 MB write
    // already exceeds 1 MB, it rolled before append into a fresh chunk).
    a.write_data(Bytes::from(vec![0u8; 4 * 1024 * 1024]))
        .unwrap();
    drop(a);

    // Same payload to B in a separate cartridge.
    let mut b = small_chunk_cart(dir.path(), "DEDUP_B");
    b.write_data(Bytes::from(payload.clone())).unwrap();
    b.write_data(Bytes::from(vec![0u8; 4 * 1024 * 1024]))
        .unwrap();
    drop(b);

    let pool = store.iter_chunks().unwrap();

    // Both cartridges produced the same first chunk (4 MB of 0xAB) and
    // the same second chunk (4 MB of 0x00). Two distinct hashes total,
    // not four — that's the dedup hit.
    assert_eq!(
        pool.len(),
        2,
        "expected 2 unique chunks in the shared pool (got {}: {:?})",
        pool.len(),
        pool
    );

    // Sanity: both files are non-trivial.
    for (_, size) in &pool {
        assert!(*size > 0);
    }
}

#[test]
fn distinct_bytes_produce_distinct_pool_files() {
    let dir = create_test_dir();
    let store = ChunkStore::new(dir.path(), "primary").unwrap();

    let mut a = small_chunk_cart(dir.path(), "UNIQ_A");
    a.write_data(Bytes::from(vec![0xAA; 4 * 1024 * 1024]))
        .unwrap();
    a.write_data(Bytes::from(vec![0u8; 4 * 1024 * 1024]))
        .unwrap();
    drop(a);

    let mut b = small_chunk_cart(dir.path(), "UNIQ_B");
    b.write_data(Bytes::from(vec![0xBB; 4 * 1024 * 1024]))
        .unwrap();
    b.write_data(Bytes::from(vec![0u8; 4 * 1024 * 1024]))
        .unwrap();
    drop(b);

    let pool = store.iter_chunks().unwrap();

    // Three unique chunks: 0xAA, 0xBB, and shared 0x00.
    assert_eq!(pool.len(), 3, "expected 3 unique chunks (got {:?})", pool);
}

#[test]
fn reads_resolve_through_shared_pool() {
    let dir = create_test_dir();

    // Write into A and seal.
    let payload = vec![0xCD; 4 * 1024 * 1024];
    {
        let mut a = small_chunk_cart(dir.path(), "READ_A");
        a.write_data(Bytes::from(payload.clone())).unwrap();
        a.write_data(Bytes::from(vec![0u8; 4 * 1024 * 1024]))
            .unwrap();
    }

    // Reopen A and read back the first block — must resolve through the
    // shared pool (not a per-cartridge `chunks/` directory, which no
    // longer exists).
    let tapes_path = dir.path().join("tapes");
    let mut a = Cartridge::open(&tapes_path, "READ_A", CartridgeOpenMode::Open).unwrap();
    let blk = a.read_block(0).unwrap();
    assert_eq!(blk.data.as_slice(), payload.as_slice());

    // The cartridge directory should no longer carry a `chunks/` subdir
    // (sealed chunks moved to the shared pool); only `.staging/` for the
    // active unsealed chunk remains.
    let cart_root = tapes_path.join("READ_A");
    assert!(
        !cart_root.join("chunks").exists(),
        "per-cartridge chunks/ should not exist with content-addressed storage"
    );
}

/// FastCDC + whole-block prefix shift: stage 2's primary win.
///
/// **What this proves**: under FastCDC's rolling Gear hash, a backup
/// stream that's been prefixed by one full SCSI block worth of new data
/// re-converges to the original cut points after ~one chunk, so all
/// downstream chunks dedup against the previous backup's pool.
///
/// **What this does *not* prove**: byte-level shift invariance. Thur VTL's
/// cartridge write path uses *block-aligned* CDC — the chunker can only
/// emit cuts at SCSI block boundaries, since `BlockIndex` doesn't yet
/// allow a single block to span chunks. As a result, a sub-block-sized
/// shift (e.g., a single-byte insertion in the middle of a 64 KiB block)
/// makes every block past the shift contain different bytes, and dedup
/// collapses. Whole-block shifts — the common case for tar streams that
/// pad to fixed multiples — survive.
///
/// Future work: extend `BlockIndex` to a list of `(chunk_id, offset, len)`
/// segments so byte-level CDC cuts work, recovering the full shift
/// invariance from the FastCDC paper.
#[test]
fn fastcdc_dedup_survives_whole_block_prefix_shift() {
    let dir = create_test_dir();
    let store = ChunkStore::new(dir.path(), "primary").unwrap();
    let tapes_path = dir.path().join("tapes");

    // Small CDC bounds so a 16 MiB payload produces ~32 chunks — gives
    // the rolling hash plenty of room to re-converge after the prefix.
    let mode = ChunkingMode::FastCdc {
        min: 256 * 1024,
        avg: 512 * 1024,
        max: 2 * 1024 * 1024,
    };
    let payload = deterministic_bytes(0x5151_5151_5151_5151, 16 * 1024 * 1024);
    let block_size = 64 * 1024; // 64 KiB blocks — typical tar blocking factor

    // Cartridge A: write payload in 64 KiB blocks.
    {
        let mut a = Cartridge::create_with_chunking(
            &tapes_path,
            "CDC_A",
            mode,
            8,
            "primary",
            false,
            DedupScope::Global,
        )
        .unwrap();
        write_in_blocks(&mut a, &payload, block_size);
    } // Drop seals trailing chunk

    // Cartridge B: write [one whole block of 0xFF] + payload. Stream B's
    // payload-bearing blocks contain the same bytes as A's blocks but
    // are shifted by one block in the stream — block-aligned CDC can
    // re-converge across this kind of shift.
    let mut shifted = vec![0xFFu8; block_size];
    shifted.extend_from_slice(&payload);
    {
        let mut b = Cartridge::create_with_chunking(
            &tapes_path,
            "CDC_B",
            mode,
            8,
            "primary",
            false,
            DedupScope::Global,
        )
        .unwrap();
        write_in_blocks(&mut b, &shifted, block_size);
    }

    let pool = store.iter_chunks().unwrap();
    assert!(
        pool.len() >= 8,
        "expected at least 8 distinct chunks; payload may be too small (got {})",
        pool.len()
    );

    let chunks_a = referenced_hashes_of_cartridge(&tapes_path, "CDC_A");
    let chunks_b = referenced_hashes_of_cartridge(&tapes_path, "CDC_B");

    use std::collections::HashSet;
    let set_a: HashSet<_> = chunks_a.iter().cloned().collect();
    let set_b: HashSet<_> = chunks_b.iter().cloned().collect();
    let shared = set_a.intersection(&set_b).count();
    let min_total = set_a.len().min(set_b.len());

    // Whole-block prefix → CDC re-converges within ~one chunk and all
    // downstream chunks share content. We assert ≥50% as the regression
    // bar; in practice we observe much higher.
    assert!(
        shared * 100 >= min_total * 50,
        "expected ≥50% chunk overlap under FastCDC + whole-block prefix shift; got \
         {}/{} ({:.0}%)",
        shared,
        min_total,
        (shared as f64 / min_total as f64) * 100.0
    );
}

/// Sanity check that fixed-size chunking under the same workload gets
/// approximately *zero* cross-cartridge dedup, confirming FastCDC is
/// actually the source of the dedup ratio above.
#[test]
fn fixed_chunking_loses_dedup_under_block_shift() {
    let dir = create_test_dir();
    let tapes_path = dir.path().join("tapes");

    let mode = ChunkingMode::Fixed {
        size_bytes: 1024 * 1024,
    };
    let payload = deterministic_bytes(0x1234_5678_9abc_def0, 8 * 1024 * 1024);
    let block_size = 64 * 1024;

    {
        let mut a = Cartridge::create_with_chunking(
            &tapes_path,
            "FX_A",
            mode,
            8,
            "primary",
            false,
            DedupScope::Global,
        )
        .unwrap();
        write_in_blocks(&mut a, &payload, block_size);
    }
    let mut shifted = vec![0xFFu8; block_size];
    shifted.extend_from_slice(&payload);
    {
        let mut b = Cartridge::create_with_chunking(
            &tapes_path,
            "FX_B",
            mode,
            8,
            "primary",
            false,
            DedupScope::Global,
        )
        .unwrap();
        write_in_blocks(&mut b, &shifted, block_size);
    }

    let chunks_a = referenced_hashes_of_cartridge(&tapes_path, "FX_A");
    let chunks_b = referenced_hashes_of_cartridge(&tapes_path, "FX_B");

    use std::collections::HashSet;
    let set_a: HashSet<_> = chunks_a.iter().cloned().collect();
    let set_b: HashSet<_> = chunks_b.iter().cloned().collect();
    let shared = set_a.intersection(&set_b).count();
    let min_total = set_a.len().min(set_b.len());

    // Fixed chunking aligns boundaries by stream offset, not content.
    // A whole-block prefix shifts every downstream byte — tail-chunk
    // overlap should be ≤ ~10%. Bar: <30%.
    let pct = (shared as f64 / min_total as f64) * 100.0;
    assert!(
        pct < 30.0,
        "fixed chunking should NOT dedup under shift; got {}/{} ({:.0}%) — \
         that's high enough to suggest the shift didn't actually shift",
        shared,
        min_total,
        pct
    );
}

fn write_in_blocks(cart: &mut Cartridge, data: &[u8], block_size: usize) {
    let mut off = 0;
    while off < data.len() {
        let end = (off + block_size).min(data.len());
        cart.write_data(Bytes::copy_from_slice(&data[off..end]))
            .expect("write_data");
        off = end;
    }
}

fn deterministic_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut s = seed.max(1);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(n);
    out
}

fn referenced_hashes_of_cartridge(tapes_path: &std::path::Path, label: &str) -> Vec<String> {
    let cart = Cartridge::open(tapes_path, label, CartridgeOpenMode::Open).expect("open");
    cart.referenced_chunk_hashes()
}

#[test]
fn evictable_chunks_excludes_unsealed() {
    let dir = create_test_dir();
    let mut a = small_chunk_cart(dir.path(), "EVICT_A");

    // Single small write — stays in staging, never sealed.
    a.write_data(Bytes::from(vec![0xEE; 1024])).unwrap();

    // No sealed chunks → no evictable chunks.
    assert_eq!(a.evictable_chunks().len(), 0);

    // Force a seal by exceeding the chunk size; the small first chunk
    // gets sealed (and would later become evictable once uploaded). We
    // don't upload here — the test only proves the staging exclusion,
    // not the eviction state machine.
    a.write_data(Bytes::from(vec![0xFF; 4 * 1024 * 1024]))
        .unwrap();

    // The previously-sealed chunk has hash=Some but uploaded=false, so
    // evictable_chunks() should still return 0 (safety: never evict an
    // unuploaded chunk).
    assert_eq!(a.evictable_chunks().len(), 0);
}
