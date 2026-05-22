// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cartridge archive tests — `cartridge_archive::run_archive`.
//!
//! Each test seeds a real cartridge on a `LocalBackend`, runs
//! archive into a second `LocalBackend` (the cold target), and
//! asserts the archive's on-bucket shape + the source's
//! immutability across the operation.

mod common;

use bytes::Bytes;
use common::create_test_dir;
use core_mediachanger::cartridge_archive::{ArchiveOptions, run_archive};
use core_mediachanger::{
    Cartridge, CartridgeOpenMode, CloudBackend, DedupScope, LocalBackend, SmcError,
};
use std::fs;
use std::path::Path;

const SRC_NAME: &str = "src";
const DST_NAME: &str = "dst";

/// Seed a real cartridge bound to `SRC_NAME` on the given bucket,
/// write `n_blocks` of fixture data, upload every chunk to the
/// bucket, then drop the cartridge. Returns the byte fixture.
async fn seed(
    tapes: &Path,
    bucket: &Path,
    label: &str,
    dedup: DedupScope,
    n_blocks: usize,
) -> Vec<Vec<u8>> {
    let backend: Box<dyn CloudBackend> =
        Box::new(LocalBackend::new(bucket).await.expect("backend"));
    let mut cart = Cartridge::open_with_cloud_async(
        tapes,
        label,
        CartridgeOpenMode::Create {
            backend: SRC_NAME.to_string(),
            worm: false,
            dedup,
        },
        Some(backend),
    )
    .await
    .expect("create cartridge");

    const BLOCK_SIZE: usize = 4096;
    let mut written = Vec::with_capacity(n_blocks);
    for i in 0..n_blocks {
        let data: Vec<u8> = (0..BLOCK_SIZE)
            .map(|j| ((label.as_bytes()[0] as usize + i * 13 + j * 5) & 0xFF) as u8)
            .collect();
        cart.write_data(Bytes::from(data.clone()))
            .expect("write_data");
        written.push(data);
    }
    cart.flush_and_seal().expect("flush_and_seal");

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

fn walkdir_count_files(p: &Path) -> usize {
    if !p.exists() {
        return 0;
    }
    let mut count = 0;
    for entry in fs::read_dir(p).expect("readdir").flatten() {
        let ft = entry.file_type().expect("ft");
        if ft.is_dir() {
            count += walkdir_count_files(&entry.path());
        } else if ft.is_file() {
            count += 1;
        }
    }
    count
}

fn read_json(p: &Path) -> serde_json::Value {
    let s = fs::read_to_string(p).expect("read manifest");
    serde_json::from_str(&s).expect("parse manifest")
}

#[tokio::test]
async fn archive_global_dedup_round_trip() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("mkdir");
    fs::create_dir_all(&dst_bucket).expect("mkdir");

    let _written = seed(&tapes, &src_bucket, "TAPE_A1", DedupScope::Global, 3).await;

    // Snapshot source state pre-archive so we can assert non-mutation.
    let src_manifest_before = fs::read(tapes.join("TAPE_A1").join("manifest.json")).expect("read");
    let src_chunks_idx_before = fs::read(tapes.join("TAPE_A1").join("chunks.idx")).expect("read");
    let src_bucket_files_before = walkdir_count_files(&src_bucket);

    let source: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
    let target: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));
    let report = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_A1",
        source: source.as_ref(),
        target: target.as_ref(),
        target_name: DST_NAME,
        label: "archive-2026-05-13",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("archive");

    // Report shape.
    assert_eq!(report.barcode, "TAPE_A1");
    assert_eq!(report.from_backend, SRC_NAME);
    assert_eq!(report.to_backend, DST_NAME);
    assert_eq!(report.label, "archive-2026-05-13");
    assert!(report.chunks_total > 0);
    assert_eq!(report.chunks_uploaded, report.chunks_total);
    assert!(report.bytes_uploaded > 0);
    assert!(report.index_files_uploaded >= 2); // chunks.idx + at least one blocks-p<N>.idx

    // Local pool path is preferred — every chunk should come from
    // the local pool here since we just sealed.
    assert_eq!(report.chunks_from_local_pool, report.chunks_total);
    assert_eq!(report.chunks_from_source_cloud, 0);

    // Source cartridge is unmodified.
    let src_manifest_after = fs::read(tapes.join("TAPE_A1").join("manifest.json")).expect("read");
    let src_chunks_idx_after = fs::read(tapes.join("TAPE_A1").join("chunks.idx")).expect("read");
    assert_eq!(src_manifest_before, src_manifest_after);
    assert_eq!(src_chunks_idx_before, src_chunks_idx_after);
    assert_eq!(walkdir_count_files(&src_bucket), src_bucket_files_before);

    // Archive layout on target.
    let archive_root = dst_bucket
        .join("archives")
        .join("TAPE_A1")
        .join("archive-2026-05-13");
    assert!(archive_root.join("manifest.json").is_file());
    assert!(archive_root.join("chunks.idx").is_file());
    assert!(archive_root.join("blocks-p0.idx").is_file());
    assert!(archive_root.join("chunks").is_dir());
    let archived_chunk_count = walkdir_count_files(&archive_root.join("chunks"));
    assert_eq!(archived_chunk_count, report.chunks_total as usize);

    // Manifest stamps provenance.
    let m = read_json(&archive_root.join("manifest.json"));
    assert_eq!(m["archived_from_backend"], SRC_NAME);
    assert!(
        m["archived_at"].as_str().unwrap_or("").starts_with("20"),
        "archived_at: {:?}",
        m["archived_at"],
    );
    // Original fields survive.
    assert_eq!(m["label"], "TAPE_A1");
    assert_eq!(m["backend"], SRC_NAME);
}

#[tokio::test]
async fn archive_dry_run_writes_nothing() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("mkdir");
    fs::create_dir_all(&dst_bucket).expect("mkdir");

    let _written = seed(&tapes, &src_bucket, "TAPE_DR", DedupScope::Global, 2).await;
    assert_eq!(walkdir_count_files(&dst_bucket), 0);

    let source: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
    let target: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));
    let report = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_DR",
        source: source.as_ref(),
        target: target.as_ref(),
        target_name: DST_NAME,
        label: "dry",
        dry_run: true,
        progress: None,
    })
    .await
    .expect("dry-run");

    assert!(report.dry_run);
    assert!(report.chunks_total > 0);
    assert_eq!(report.chunks_uploaded, 0);
    assert_eq!(report.bytes_uploaded, 0);
    assert_eq!(report.index_files_uploaded, 0);
    assert_eq!(walkdir_count_files(&dst_bucket), 0);
}

#[tokio::test]
async fn archive_refuses_duplicate_label() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("mkdir");
    fs::create_dir_all(&dst_bucket).expect("mkdir");

    let _written = seed(&tapes, &src_bucket, "TAPE_DUP", DedupScope::Global, 1).await;

    let source: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
    let target: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));

    // First archive succeeds.
    let _ = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_DUP",
        source: source.as_ref(),
        target: target.as_ref(),
        target_name: DST_NAME,
        label: "snap1",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("first archive");

    // Same label refused.
    let source2: Box<dyn CloudBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
    let target2: Box<dyn CloudBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));
    let err = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_DUP",
        source: source2.as_ref(),
        target: target2.as_ref(),
        target_name: DST_NAME,
        label: "snap1",
        dry_run: false,
        progress: None,
    })
    .await
    .expect_err("must refuse duplicate label");
    matches!(err, SmcError::InvalidOp(_));

    // Distinct label succeeds — both archives coexist.
    let source3: Box<dyn CloudBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
    let target3: Box<dyn CloudBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));
    let _ = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_DUP",
        source: source3.as_ref(),
        target: target3.as_ref(),
        target_name: DST_NAME,
        label: "snap2",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("second archive under fresh label");

    let archives_root = dst_bucket.join("archives").join("TAPE_DUP");
    assert!(archives_root.join("snap1").join("manifest.json").is_file());
    assert!(archives_root.join("snap2").join("manifest.json").is_file());
}

#[tokio::test]
async fn archive_refuses_invalid_label() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("mkdir");
    fs::create_dir_all(&dst_bucket).expect("mkdir");
    let _ = seed(&tapes, &src_bucket, "TAPE_LBL", DedupScope::Global, 1).await;

    let source: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
    let target: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));

    for bad in &["", "has space", "with/slash", &"x".repeat(65)] {
        let s: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
        let _ = source;
        let _ = target;
        let t: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));
        let err = run_archive(ArchiveOptions {
            tapes_dir: &tapes,
            barcode: "TAPE_LBL",
            source: s.as_ref(),
            target: t.as_ref(),
            target_name: DST_NAME,
            label: bad,
            dry_run: false,
            progress: None,
        })
        .await
        .expect_err("must refuse bad label");
        matches!(err, SmcError::InvalidOp(_));
        // No partial archive on the bucket.
        assert!(
            !dst_bucket
                .join("archives")
                .join("TAPE_LBL")
                .join(*bad)
                .exists(),
            "bad label {:?} created an archive dir",
            bad,
        );
    }
}

#[tokio::test]
async fn archive_local_dedup_chunks_land_under_archive_prefix() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("mkdir");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("mkdir");
    fs::create_dir_all(&dst_bucket).expect("mkdir");

    let _ = seed(&tapes, &src_bucket, "TAPE_L", DedupScope::Local, 2).await;

    let source: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&src_bucket).await.expect("be"));
    let target: Box<dyn CloudBackend> = Box::new(LocalBackend::new(&dst_bucket).await.expect("be"));
    let report = run_archive(ArchiveOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_L",
        source: source.as_ref(),
        target: target.as_ref(),
        target_name: DST_NAME,
        label: "l1",
        dry_run: false,
        progress: None,
    })
    .await
    .expect("archive local-dedup");

    // Chunks land under the archive prefix on the target — not the
    // target's regular `chunks/` pool. The target bucket has no
    // pool entries (this was a fresh bucket).
    assert!(
        !dst_bucket.join("chunks").exists() || walkdir_count_files(&dst_bucket.join("chunks")) == 0
    );
    let archive_chunks = dst_bucket
        .join("archives")
        .join("TAPE_L")
        .join("l1")
        .join("chunks");
    assert!(archive_chunks.is_dir());
    assert_eq!(
        walkdir_count_files(&archive_chunks),
        report.chunks_total as usize,
    );
}
