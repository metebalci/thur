// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cartridge migration tests — `cartridge_migrate::run_migrate`.
//!
//! Each test sets up two `LocalBackend`s in separate temp dirs (the
//! source and target buckets), seeds a cartridge against the source,
//! runs migrate, and asserts the on-disk + on-bucket state matches
//! the contract.

mod common;

use bytes::Bytes;
use common::create_test_dir;
use core_mediachanger::cartridge_migrate::{MigrateMode, MigrateOptions, run_migrate};
use core_mediachanger::{
    Cartridge, CartridgeOpenMode, DedupScope, LocalBackend, ObjectStoreBackend, PoolBudget,
    SmcError,
};
use std::fs;
use std::path::Path;
use std::sync::Arc;

const TEST_BACKEND_SRC: &str = "src";
const TEST_BACKEND_DST: &str = "dst";

/// Build a real cartridge bound to `backend_name`, write data,
/// upload chunks to the bucket, run a manifest backup, drop the
/// cartridge. Returns the byte fixture so callers can verify reads
/// later if they want to round-trip.
async fn seed_cartridge(
    tapes: &Path,
    bucket: &Path,
    backend_name: &str,
    label: &str,
    dedup: DedupScope,
    n_blocks: usize,
) -> Vec<Vec<u8>> {
    let backend: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(bucket).await.expect("test setup"));
    let mut cart = Cartridge::open_with_storage_async(
        tapes,
        label,
        CartridgeOpenMode::Create {
            backend: backend_name.to_string(),
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
            .map(|j| ((label.as_bytes()[0] as usize + i * 31 + j * 7) & 0xFF) as u8)
            .collect();
        cart.write_data(Bytes::from(data.clone()))
            .expect("write_data");
        written.push(data);
    }
    cart.flush_and_seal().expect("flush_and_seal");

    // Upload every sealed chunk so the source bucket carries them.
    let pending: Vec<u64> = cart
        .get_pending_uploads()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    for id in pending {
        cart.upload_chunk_to_storage(id)
            .await
            .expect("upload chunk");
    }

    cart.backup_manifest_to_storage()
        .await
        .expect("backup_manifest_to_storage");
    drop(cart);
    written
}

fn read_manifest_backend(cart_root: &Path) -> String {
    let s = fs::read_to_string(cart_root.join("manifest.json")).expect("test setup");
    let v: serde_json::Value = serde_json::from_str(&s).expect("test setup");
    v["backend"].as_str().expect("test setup").to_string()
}

fn count_pool_files(data_dir: &Path, backend: &str, namespace: Option<&str>) -> usize {
    let mut root = data_dir.join("chunks").join(backend);
    if let Some(ns) = namespace {
        root = root.join(ns);
    }
    if !root.exists() {
        return 0;
    }
    let mut count = 0;
    for s1 in fs::read_dir(&root).expect("test setup").flatten() {
        if !s1.file_type().expect("test setup").is_dir() {
            continue;
        }
        for s2 in fs::read_dir(s1.path()).expect("test setup").flatten() {
            if !s2.file_type().expect("test setup").is_dir() {
                continue;
            }
            for f in fs::read_dir(s2.path()).expect("test setup").flatten() {
                if f.file_type().expect("test setup").is_file() {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Sum the on-disk byte size of every chunk file under a backend's
/// pool slice (optionally a namespace). Used to seed a `PoolBudget` to
/// the cartridge's real pool occupancy so the migrate budget-transfer
/// assertions have a ground-truth total.
fn sum_pool_bytes(data_dir: &Path, backend: &str, namespace: Option<&str>) -> u64 {
    let mut root = data_dir.join("chunks").join(backend);
    if let Some(ns) = namespace {
        root = root.join(ns);
    }
    if !root.exists() {
        return 0;
    }
    let mut total = 0u64;
    for s1 in fs::read_dir(&root).expect("test setup").flatten() {
        if !s1.file_type().expect("test setup").is_dir() {
            continue;
        }
        for s2 in fs::read_dir(s1.path()).expect("test setup").flatten() {
            if !s2.file_type().expect("test setup").is_dir() {
                continue;
            }
            for f in fs::read_dir(s2.path()).expect("test setup").flatten() {
                if f.file_type().expect("test setup").is_file() {
                    total += f.metadata().expect("test setup").len();
                }
            }
        }
    }
    total
}

fn count_bucket_chunks(bucket: &Path) -> usize {
    let chunks_dir = bucket.join("chunks");
    if !chunks_dir.exists() {
        return 0;
    }
    walkdir_count_files(&chunks_dir)
}

fn count_bucket_manifests(bucket: &Path, barcode: &str) -> usize {
    let mp = bucket.join("manifests").join(barcode);
    if !mp.exists() {
        return 0;
    }
    walkdir_count_files(&mp)
}

fn walkdir_count_files(p: &Path) -> usize {
    let mut count = 0;
    for entry in fs::read_dir(p).expect("test setup").flatten() {
        let ft = entry.file_type().expect("test setup");
        if ft.is_dir() {
            count += walkdir_count_files(&entry.path());
        } else if ft.is_file() {
            count += 1;
        }
    }
    count
}

#[tokio::test]
async fn migrate_move_global_dedup_round_trip() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    let written = seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE001",
        DedupScope::Global,
        4,
    )
    .await;

    // Pre-migration state.
    let pool_src_before = count_pool_files(work.path(), TEST_BACKEND_SRC, None);
    assert!(pool_src_before > 0, "src pool must have chunks pre-migrate");
    assert_eq!(count_pool_files(work.path(), TEST_BACKEND_DST, None), 0);
    let bucket_src_chunks_before = count_bucket_chunks(&src_bucket);
    let bucket_src_manifests_before = count_bucket_manifests(&src_bucket, "TAPE001");
    assert!(bucket_src_chunks_before > 0);
    assert!(bucket_src_manifests_before > 0);
    assert_eq!(count_bucket_chunks(&dst_bucket), 0);
    assert_eq!(count_bucket_manifests(&dst_bucket, "TAPE001"), 0);
    assert_eq!(
        read_manifest_backend(&tapes.join("TAPE001")),
        TEST_BACKEND_SRC
    );

    // Migrate.
    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let report = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE001",
        source: source.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Move,
        dry_run: false,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect("migrate move");

    // Report shape.
    assert_eq!(report.from_backend, TEST_BACKEND_SRC);
    assert_eq!(report.to_backend, TEST_BACKEND_DST);
    assert_eq!(report.mode, "move");
    assert!(report.chunks_total > 0);
    assert_eq!(report.chunks_copied, report.chunks_total);
    assert!(report.bytes_copied > 0);
    assert!(report.manifest_objects_copied > 0);
    assert_eq!(report.local_files_moved as usize, pool_src_before);

    // Manifest now points at the new backend.
    assert_eq!(
        read_manifest_backend(&tapes.join("TAPE001")),
        TEST_BACKEND_DST
    );

    // Local pool moved.
    assert_eq!(count_pool_files(work.path(), TEST_BACKEND_SRC, None), 0);
    assert_eq!(
        count_pool_files(work.path(), TEST_BACKEND_DST, None),
        pool_src_before,
    );

    // Target bucket has chunks + manifest backups.
    assert!(count_bucket_chunks(&dst_bucket) > 0);
    assert!(count_bucket_manifests(&dst_bucket, "TAPE001") > 0);

    // Under Global dedup, source-side CHUNK deletion is skipped (the
    // chunks may be referenced by sibling cartridges on the source
    // backend; GC sweep cleans up). Manifest backups ARE deleted.
    assert!(count_bucket_chunks(&src_bucket) > 0);
    assert_eq!(count_bucket_manifests(&src_bucket, "TAPE001"), 0);

    // Re-open against the new backend; reads still match the fixture.
    let target_again: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let mut cart = Cartridge::open_with_storage_async(
        &tapes,
        "TAPE001",
        CartridgeOpenMode::Open,
        Some(target_again),
    )
    .await
    .expect("re-open after migrate");
    cart.rewind();
    for (i, expected) in written.iter().enumerate() {
        let block = cart.read_block_async(i as u64).await.expect("read block");
        match block.kind {
            core_mediachanger::BlockKind::Data => {
                assert_eq!(block.data.as_ref(), expected.as_slice())
            }
            other => panic!("block {} unexpected kind {:?}", i, other),
        }
    }
}

#[tokio::test]
async fn migrate_move_local_dedup_deletes_source_chunks() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    let _written = seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE_L",
        DedupScope::Local,
        3,
    )
    .await;

    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let report = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_L",
        source: source.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Move,
        dry_run: false,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect("migrate local-dedup move");

    assert_eq!(report.to_backend, TEST_BACKEND_DST);

    // Local-dedup source chunks must be deleted (per-cartridge namespace,
    // so no sibling references possible).
    assert_eq!(count_bucket_chunks(&src_bucket), 0);
    assert_eq!(count_bucket_manifests(&src_bucket, "TAPE_L"), 0);
    assert!(count_bucket_chunks(&dst_bucket) > 0);

    // Local pool moved under the per-cartridge namespace.
    assert_eq!(
        count_pool_files(work.path(), TEST_BACKEND_SRC, Some("TAPE_L")),
        0
    );
    assert!(count_pool_files(work.path(), TEST_BACKEND_DST, Some("TAPE_L")) > 0);
}

#[tokio::test]
async fn migrate_dry_run_writes_nothing() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    let _written = seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE_DR",
        DedupScope::Global,
        2,
    )
    .await;
    let pool_src_before = count_pool_files(work.path(), TEST_BACKEND_SRC, None);
    let bucket_src_chunks_before = count_bucket_chunks(&src_bucket);
    let bucket_src_manifests_before = count_bucket_manifests(&src_bucket, "TAPE_DR");

    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let report = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_DR",
        source: source.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Move,
        dry_run: true,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect("migrate dry-run");

    assert!(report.dry_run);
    assert!(report.chunks_total > 0);
    assert_eq!(report.chunks_copied, 0);
    assert_eq!(report.bytes_copied, 0);
    assert_eq!(report.local_files_moved, 0);

    // Nothing changed.
    assert_eq!(
        read_manifest_backend(&tapes.join("TAPE_DR")),
        TEST_BACKEND_SRC
    );
    assert_eq!(
        count_pool_files(work.path(), TEST_BACKEND_SRC, None),
        pool_src_before
    );
    assert_eq!(count_pool_files(work.path(), TEST_BACKEND_DST, None), 0);
    assert_eq!(count_bucket_chunks(&src_bucket), bucket_src_chunks_before);
    assert_eq!(
        count_bucket_manifests(&src_bucket, "TAPE_DR"),
        bucket_src_manifests_before
    );
    assert_eq!(count_bucket_chunks(&dst_bucket), 0);
}

#[tokio::test]
async fn migrate_rebind_verify_happy_path() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    let written = seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE_RB",
        DedupScope::Global,
        3,
    )
    .await;

    // Pre-stage everything on the target bucket (simulates the
    // operator's out-of-band bucket replication having finished).
    copy_tree(&src_bucket, &dst_bucket);

    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let report = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_RB",
        source: source.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Rebind { verify: true },
        dry_run: false,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect("rebind");

    assert_eq!(report.mode, "rebind");
    assert_eq!(report.chunks_copied, 0); // rebind never copies data
    assert_eq!(report.bytes_copied, 0);
    assert!(report.chunks_verified > 0);

    // Manifest backend flipped; local pool moved.
    assert_eq!(
        read_manifest_backend(&tapes.join("TAPE_RB")),
        TEST_BACKEND_DST
    );
    assert_eq!(count_pool_files(work.path(), TEST_BACKEND_SRC, None), 0);
    assert!(count_pool_files(work.path(), TEST_BACKEND_DST, None) > 0);

    // Source bucket is untouched (rebind never deletes).
    assert!(count_bucket_chunks(&src_bucket) > 0);
    assert!(count_bucket_manifests(&src_bucket, "TAPE_RB") > 0);

    // Reads off the new backend match the fixture.
    let target_again: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let mut cart = Cartridge::open_with_storage_async(
        &tapes,
        "TAPE_RB",
        CartridgeOpenMode::Open,
        Some(target_again),
    )
    .await
    .expect("reopen on target");
    cart.rewind();
    for (i, expected) in written.iter().enumerate() {
        let block = cart.read_block_async(i as u64).await.expect("test setup");
        assert_eq!(block.data.as_ref(), expected.as_slice());
    }
}

#[tokio::test]
async fn migrate_rebind_refuses_when_target_missing_chunks() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    let _written = seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE_RBM",
        DedupScope::Global,
        2,
    )
    .await;

    // Target bucket is empty — the operator forgot to run replication.
    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let err = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_RBM",
        source: source.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Rebind { verify: true },
        dry_run: false,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect_err("must refuse — target is empty");

    match err {
        SmcError::RebindTargetMissing { keys } => assert!(!keys.is_empty()),
        other => panic!("expected RebindTargetMissing, got {:?}", other),
    }

    // Manifest is unchanged — rebind aborted before any mutation.
    assert_eq!(
        read_manifest_backend(&tapes.join("TAPE_RBM")),
        TEST_BACKEND_SRC
    );
}

#[tokio::test]
async fn migrate_rebind_no_verify_proceeds_without_check() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    let _written = seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE_RBNV",
        DedupScope::Global,
        2,
    )
    .await;

    // Target is empty; we tell migrate to trust us anyway.
    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let report = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_RBNV",
        source: source.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Rebind { verify: false },
        dry_run: false,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect("no-verify rebind must succeed");

    assert_eq!(report.mode, "rebind-noverify");
    assert_eq!(report.chunks_verified, 0);
    assert_eq!(
        read_manifest_backend(&tapes.join("TAPE_RBNV")),
        TEST_BACKEND_DST
    );
}

#[tokio::test]
async fn migrate_refuses_same_source_and_target() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let bucket = work.path().join("bucket");
    fs::create_dir_all(&bucket).expect("test setup");

    let _ = seed_cartridge(
        &tapes,
        &bucket,
        TEST_BACKEND_SRC,
        "TAPE_SAME",
        DedupScope::Global,
        1,
    )
    .await;

    let a: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("test setup"));
    let b: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&bucket).await.expect("test setup"));
    let err = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_SAME",
        source: a.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: b.as_ref(),
        target_name: TEST_BACKEND_SRC,
        mode: MigrateMode::Move,
        dry_run: false,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect_err("source==target must refuse");
    matches!(err, SmcError::InvalidOp(_));
}

#[tokio::test]
async fn migrate_refuses_when_manifest_backend_does_not_match() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    fs::create_dir_all(&tapes).expect("test setup");
    let src_bucket = work.path().join("src_bucket");
    let dst_bucket = work.path().join("dst_bucket");
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    let _ = seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE_BMM",
        DedupScope::Global,
        1,
    )
    .await;

    // Tell migrate the source is "other" — disagrees with the
    // manifest's recorded backend.
    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    let err = run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE_BMM",
        source: source.as_ref(),
        source_name: "other",
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Move,
        dry_run: false,
        progress: None,
        source_budget: None,
        target_budget: None,
    })
    .await
    .expect_err("backend mismatch must refuse");
    matches!(err, SmcError::InvalidOp(_));
}

/// Budget split-direction: a Move migrate must release every moved
/// chunk's bytes from the SOURCE backend's `PoolBudget` and reserve the
/// same bytes against the TARGET's, so both `current_bytes()` stay equal
/// to their on-disk pool slices for the per-backend eviction workers.
#[tokio::test]
async fn migrate_move_transfers_pool_budget_source_to_target() {
    let work = create_test_dir();
    let tapes = work.path().join("tapes");
    let src_bucket = work.path().join("buckets").join(TEST_BACKEND_SRC);
    let dst_bucket = work.path().join("buckets").join(TEST_BACKEND_DST);
    fs::create_dir_all(&src_bucket).expect("test setup");
    fs::create_dir_all(&dst_bucket).expect("test setup");

    seed_cartridge(
        &tapes,
        &src_bucket,
        TEST_BACKEND_SRC,
        "TAPE001",
        DedupScope::Global,
        4,
    )
    .await;

    // Ground-truth source pool occupancy → seed the source budget to
    // match (Global dedup → namespace None). Target budget starts empty.
    let src_pool_bytes = sum_pool_bytes(work.path(), TEST_BACKEND_SRC, None);
    assert!(src_pool_bytes > 0, "src pool must hold chunk bytes");
    let source_budget = Arc::new(PoolBudget::new(work.path().to_path_buf(), 0, 0, 80));
    source_budget.force_reserve(src_pool_bytes, None);
    let target_budget = Arc::new(PoolBudget::new(work.path().to_path_buf(), 0, 0, 80));
    assert_eq!(source_budget.current_bytes(), src_pool_bytes);
    assert_eq!(target_budget.current_bytes(), 0);

    let source: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&src_bucket).await.expect("test setup"));
    let target: Box<dyn ObjectStoreBackend> =
        Box::new(LocalBackend::new(&dst_bucket).await.expect("test setup"));
    run_migrate(MigrateOptions {
        tapes_dir: &tapes,
        barcode: "TAPE001",
        source: source.as_ref(),
        source_name: TEST_BACKEND_SRC,
        target: target.as_ref(),
        target_name: TEST_BACKEND_DST,
        mode: MigrateMode::Move,
        dry_run: false,
        progress: None,
        source_budget: Some(source_budget.clone()),
        target_budget: Some(target_budget.clone()),
    })
    .await
    .expect("migrate move");

    // Every moved byte left the source budget and landed in the
    // target's — and each tracks its on-disk pool slice exactly.
    assert_eq!(
        source_budget.current_bytes(),
        0,
        "source budget must drop by the moved bytes"
    );
    assert_eq!(
        target_budget.current_bytes(),
        src_pool_bytes,
        "target budget must rise by the moved bytes"
    );
    assert_eq!(
        sum_pool_bytes(work.path(), TEST_BACKEND_DST, None),
        src_pool_bytes,
        "target on-disk pool now holds the moved bytes"
    );
    assert_eq!(
        target_budget.current_bytes(),
        sum_pool_bytes(work.path(), TEST_BACKEND_DST, None),
        "target budget == target on-disk pool bytes"
    );
}

/// Recursively copy `src` into `dst`. Used to pre-stage the target
/// bucket for rebind-mode tests, simulating provider-side bucket
/// replication having completed.
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("test setup");
    for entry in fs::read_dir(src).expect("test setup").flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type().expect("test setup");
        if ft.is_dir() {
            copy_tree(&from, &to);
        } else if ft.is_file() {
            fs::copy(&from, &to).expect("test setup");
        }
    }
}
