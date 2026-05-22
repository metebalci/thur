// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for tape partitioning (LTFS support).
//!
//! These tests cover the cartridge-side partition model: staging a layout
//! via `set_pending_partition_layout`, applying it with FORMAT MEDIUM
//! (`apply_format_medium`), per-partition write/read isolation,
//! cross-partition LOCATE, and the ALLOW OVERWRITE barrier semantics.

mod common;

use bytes::Bytes;
use common::*;
use core_mediachanger::{Cartridge, CartridgeOpenMode, PendingPartitionLayout};

fn ltfs_layout(p0_mib: u64, p1_mib: u64) -> PendingPartitionLayout {
    PendingPartitionLayout {
        fdp: false,
        sdp: false,
        idp: true,
        additional_partitions: 1,
        psum: 2, // MiB
        partition_sizes: vec![p0_mib, p1_mib],
    }
}

#[test]
fn unpartitioned_tape_has_one_partition() {
    let dir = create_test_dir();
    let cart = create_test_cartridge(&dir, "PART_DEFAULT");
    assert_eq!(cart.partition_count(), 1);
    assert_eq!(cart.active_partition(), 0);
}

#[test]
fn format_medium_creates_two_partitions() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "PART_FORMAT");

    // Stage a two-partition layout (P0 = 1 GiB, P1 = rest of tape) and apply
    // it with FORMAT MEDIUM(0x01) — exactly what mkltfs issues.
    cart.set_pending_partition_layout(ltfs_layout(1024, 0xFFFF))
        .unwrap();
    cart.apply_format_medium(0x01).unwrap();

    assert_eq!(cart.partition_count(), 2);
    assert_eq!(cart.active_partition(), 0);
    assert_eq!(cart.position(), 0);
}

#[test]
fn format_medium_default_partition_reverts_to_one() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "PART_REVERT");
    cart.set_pending_partition_layout(ltfs_layout(1024, 0xFFFF))
        .unwrap();
    cart.apply_format_medium(0x01).unwrap();
    assert_eq!(cart.partition_count(), 2);
    // FORMAT MEDIUM with format=0x02 reverts to a single partition.
    cart.apply_format_medium(0x02).unwrap();
    assert_eq!(cart.partition_count(), 1);
}

#[test]
fn writes_to_one_partition_do_not_appear_in_the_other() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "PART_ISOLATE");
    cart.set_pending_partition_layout(ltfs_layout(1024, 0xFFFF))
        .unwrap();
    cart.apply_format_medium(0x01).unwrap();

    // Write three blocks to P0.
    cart.write_data(Bytes::from_static(b"index-block-1"))
        .unwrap();
    cart.write_data(Bytes::from_static(b"index-block-2"))
        .unwrap();
    cart.write_data(Bytes::from_static(b"index-block-3"))
        .unwrap();
    assert_eq!(cart.next_lba(), 3);

    // Switch to P1 — should be empty.
    cart.locate_partition(1, 0).unwrap();
    assert_eq!(cart.active_partition(), 1);
    assert_eq!(cart.next_lba(), 0);
    assert!(cart.at_eod());

    // Write four blocks to P1.
    for i in 0..4 {
        cart.write_data(Bytes::copy_from_slice(format!("data-{i}").as_bytes()))
            .unwrap();
    }
    assert_eq!(cart.next_lba(), 4);

    // Back to P0: still has its three blocks intact.
    cart.locate_partition(0, 0).unwrap();
    assert_eq!(cart.next_lba(), 3);
    let blk = cart.read_next().unwrap();
    assert_eq!(&blk.data[..], b"index-block-1");

    // And P1 still has its four.
    cart.locate_partition(1, 0).unwrap();
    let blk = cart.read_next().unwrap();
    assert_eq!(&blk.data[..], b"data-0");
}

#[test]
fn locate_partition_out_of_range_errors() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "PART_OOR");
    // Single-partition tape: locate_partition(1, 0) must fail.
    assert!(cart.locate_partition(1, 0).is_err());
    assert_eq!(cart.active_partition(), 0); // unchanged on failure? no — we set first.
    // Note: set_pending + format gives us 2 partitions, so partition 2 is OOR.
    cart.set_pending_partition_layout(ltfs_layout(1024, 0xFFFF))
        .unwrap();
    cart.apply_format_medium(0x01).unwrap();
    assert!(cart.locate_partition(2, 0).is_err());
}

#[test]
fn partition_state_persists_across_open() {
    let dir = create_test_dir();
    let label = "PART_PERSIST";
    {
        let mut cart = create_test_cartridge(&dir, label);
        cart.set_pending_partition_layout(ltfs_layout(1024, 0xFFFF))
            .unwrap();
        cart.apply_format_medium(0x01).unwrap();
        cart.locate_partition(1, 0).unwrap();
        cart.write_data(Bytes::from_static(b"p1-payload")).unwrap();
        cart.locate_partition(0, 0).unwrap();
        cart.write_data(Bytes::from_static(b"p0-payload")).unwrap();
    }
    let tapes_path = dir.path().join("tapes");
    let mut cart = Cartridge::open(&tapes_path, label, CartridgeOpenMode::Open).unwrap();
    assert_eq!(cart.partition_count(), 2);
    assert_eq!(cart.next_lba(), 1); // P0 has 1 block

    cart.locate_partition(1, 0).unwrap();
    let blk = cart.read_next().unwrap();
    assert_eq!(&blk.data[..], b"p1-payload");

    cart.locate_partition(0, 0).unwrap();
    let blk = cart.read_next().unwrap();
    assert_eq!(&blk.data[..], b"p0-payload");
}

#[test]
fn allow_overwrite_api_accepts_and_clears() {
    // ALLOW OVERWRITE in real LTO only *permits* a write-in-the-middle —
    // the trailing data on a real tape is still lost. Thur VTL already
    // permits those writes unconditionally, so this test just confirms
    // the API surface (set, set-on-other-partition, clear) round-trips
    // without error and rejects out-of-range partitions.
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "PART_OW");
    cart.set_pending_partition_layout(ltfs_layout(1024, 0xFFFF))
        .unwrap();
    cart.apply_format_medium(0x01).unwrap();
    cart.set_allow_overwrite(0, 7).unwrap();
    cart.set_allow_overwrite(1, 42).unwrap();
    cart.set_allow_overwrite(0, 0).unwrap(); // clear
    assert!(cart.set_allow_overwrite(2, 0).is_err()); // partition out of range
}

#[test]
fn write_in_middle_truncates_trailing_data() {
    // The "writes erase from here on" rule is what mkltfs relies on
    // when it writes a fresh volume label after FORMAT MEDIUM. Make
    // sure it survives the partition refactor on a single-partition tape.
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "PART_TRUNCATE");
    for i in 0..5u8 {
        cart.write_data(Bytes::copy_from_slice(&[i; 16])).unwrap();
    }
    assert_eq!(cart.next_lba(), 5);

    cart.locate(2).unwrap();
    cart.write_data(Bytes::from_static(b"replacement")).unwrap();
    assert_eq!(cart.next_lba(), 3);
    cart.locate(2).unwrap();
    let blk = cart.read_next().unwrap();
    assert_eq!(&blk.data[..], b"replacement");
}

#[test]
fn erase_clears_all_partitions_and_resets_to_p0() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "PART_ERASE");
    cart.set_pending_partition_layout(ltfs_layout(1024, 0xFFFF))
        .unwrap();
    cart.apply_format_medium(0x01).unwrap();

    cart.write_data(Bytes::from_static(b"p0")).unwrap();
    cart.locate_partition(1, 0).unwrap();
    cart.write_data(Bytes::from_static(b"p1")).unwrap();

    cart.erase().unwrap();
    assert_eq!(cart.active_partition(), 0);
    assert_eq!(cart.partition_count(), 2); // erase keeps the layout
    assert_eq!(cart.position(), 0);
    assert_eq!(cart.next_lba(), 0);
    cart.locate_partition(1, 0).unwrap();
    assert_eq!(cart.next_lba(), 0);
}
