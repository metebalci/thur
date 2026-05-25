// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the async upload hand-off in
//! `VolumeWriter::write_page_unsynced` and the
//! `synchronize_bytes`-drains-pending-uploads contract on
//! `PageCache`.
//!
//! These tests run their own in-process upload "worker" — a
//! `tokio::spawn` that pulls `UploadTask`s off the writer's mpsc
//! receiver, runs `upload_chunk_inert`, and calls
//! `apply_page_upload_outcome` on the writer. The real daemon's
//! worker (`vsa/daemon/src/upload_worker.rs`) does the same thing
//! plus a `Semaphore` cap, but the contract these tests exercise
//! (sidecar `LocalOnly → Uploaded`, pending tracker mark / wake,
//! drain on SYNC) is identical.

use std::sync::Arc;

use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
use core_block::{
    DedupScope, PageCache, SyncAfter, UploadState, UploadTask, VolumeManifest, VolumeWriter,
};
use shared_object_store::{LocalBackend, ObjectStoreBackend};
use shared_upload_worker::upload_chunk_inert;
use tempfile::TempDir;
use tokio::sync::mpsc;

const PAGE: usize = DEFAULT_PAGE_SIZE_BYTES as usize;

/// Construct a 4 MiB Local-scope volume + an mpsc-wired
/// `VolumeWriter` + the receiver for the in-process worker. Wires
/// `with_upload_sender` so `write_page_unsynced` goes through the
/// async path.
async fn fixture() -> (
    TempDir,
    Arc<VolumeWriter>,
    Arc<dyn ObjectStoreBackend>,
    mpsc::Receiver<UploadTask>,
) {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_path_buf();
    let cloud_root = data_dir.join("cloud");
    std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
    let backend = LocalBackend::new(&cloud_root).await.expect("local backend");
    let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

    let name = "vol-async";
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

    let (tx, rx) = mpsc::channel(16);
    let writer = Arc::new(
        VolumeWriter::open(&data_dir, name, Arc::clone(&backend))
            .expect("open writer")
            .with_upload_sender(tx),
    );
    (tmp, writer, backend, rx)
}

/// In-process worker: drain receiver, run `upload_chunk_inert`,
/// apply outcome. Single-threaded but mirrors what the real worker
/// does per-task.
async fn drive_worker(
    mut rx: mpsc::Receiver<UploadTask>,
    writer: Arc<VolumeWriter>,
    backend: Arc<dyn ObjectStoreBackend>,
) {
    while let Some(task) = rx.recv().await {
        let outcome = upload_chunk_inert(&*backend, &task.payload)
            .await
            .expect("upload_chunk_inert");
        writer
            .apply_page_upload_outcome(&outcome)
            .await
            .expect("apply_page_upload_outcome");
    }
}

fn page_bytes(seed: u8) -> Vec<u8> {
    let mut v = vec![0u8; PAGE];
    for (i, b) in v.iter_mut().enumerate() {
        *b = seed.wrapping_add((i & 0xFF) as u8);
    }
    v
}

#[tokio::test]
async fn write_page_async_marks_localonly_then_worker_flips_uploaded() {
    let (tmp, writer, _backend, mut rx) = fixture().await;

    let bytes = page_bytes(0x42);

    // Issue the write but don't yet run the worker.
    writer.write_page(0, &bytes).await.expect("write_page");

    // Sidecar shows LocalOnly while the upload is queued.
    assert_eq!(
        writer.upload_index().read(0).expect("read sidecar"),
        UploadState::LocalOnly
    );
    assert!(
        writer.pending_uploads().snapshot().await.contains(&0),
        "pending tracker should know page 0 is in flight"
    );

    // Worker picks it up and applies the outcome.
    let task = rx.recv().await.expect("worker receives task");
    assert_eq!(task.payload.item_id, 0);
    let outcome = shared_upload_worker::upload_chunk_inert(&*_backend, &task.payload)
        .await
        .expect("inert upload");
    writer
        .apply_page_upload_outcome(&outcome)
        .await
        .expect("apply outcome");

    // Sidecar is now Uploaded; tracker is empty.
    assert_eq!(
        writer.upload_index().read(0).expect("read sidecar"),
        UploadState::Uploaded
    );
    assert!(writer.pending_uploads().snapshot().await.is_empty());

    drop(tmp);
}

#[tokio::test]
async fn synchronize_cache_drains_pending_uploads() {
    let (tmp, writer, backend, rx) = fixture().await;

    // Spawn the worker so synchronize_bytes has something to wait on.
    let writer_for_worker = Arc::clone(&writer);
    let backend_for_worker = Arc::clone(&backend);
    let worker = tokio::spawn(async move {
        drive_worker(rx, writer_for_worker, backend_for_worker).await;
    });

    let cache = PageCache::new(Arc::clone(&writer));

    // Two host writes through the cache layer.
    cache.write_bytes(0, &page_bytes(1)).await.expect("write 1");
    cache
        .write_bytes(u64::from(DEFAULT_PAGE_SIZE_BYTES), &page_bytes(2))
        .await
        .expect("write 2");

    // SYNCHRONIZE CACHE drains the cache to the pool + awaits
    // every pending upload in the synced range. After it returns,
    // the sidecar must read Uploaded for both pages.
    cache
        .synchronize_bytes(0, 2 * u64::from(DEFAULT_PAGE_SIZE_BYTES))
        .await
        .expect("sync");

    assert_eq!(
        writer.upload_index().read(0).expect("read 0"),
        UploadState::Uploaded
    );
    assert_eq!(
        writer.upload_index().read(1).expect("read 1"),
        UploadState::Uploaded
    );
    assert!(writer.pending_uploads().snapshot().await.is_empty());

    // Tear down the worker.
    drop(writer);
    drop(cache);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), worker).await;
    drop(tmp);
}

/// `disk` mode: SYNC flushes RAM → pool but does NOT wait for the
/// upload worker. The page-cache flush worker (started by the
/// host's write going through the cache) returns once the writer's
/// `write_page_unsynced` returns — which in async mode happens
/// once the upload is enqueued. The sidecar marks LocalOnly. Until
/// we tell the test-only worker to drive, the page stays
/// LocalOnly, and `synchronize_bytes` under `disk` returns Ok
/// anyway.
#[tokio::test]
async fn synchronize_cache_disk_mode_returns_without_waiting_for_upload() {
    let (tmp, writer, _backend, rx) = fixture().await;
    // Flip to `disk` BEFORE we wire the cache: SYNC will not wait
    // for the upload worker.
    writer
        .set_sync_after(SyncAfter::Disk)
        .expect("flip to disk");

    // Don't spawn a worker. The upload task we'll enqueue via
    // `write_page` sits in the channel forever; if `synchronize_bytes`
    // were waiting on it, this test would hang.
    let cache = PageCache::new(Arc::clone(&writer));
    cache.write_bytes(0, &page_bytes(9)).await.expect("write");

    // Under `disk`, SYNC returns Ok promptly even though no upload
    // worker is running. Tight timeout proves we don't drain the
    // pending tracker.
    let r = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        cache.synchronize_bytes(0, u64::from(DEFAULT_PAGE_SIZE_BYTES)),
    )
    .await
    .expect("SYNC must NOT wait for upload under disk mode");
    r.expect("sync ok");

    // Sidecar still LocalOnly — upload never ran.
    assert_eq!(
        writer.upload_index().read(0).expect("read 0"),
        UploadState::LocalOnly
    );

    drop(rx);
    drop(cache);
    drop(writer);
    drop(tmp);
}

/// `memory` mode: SYNC is a pure no-op. The RAM cache stays dirty;
/// neither pool nor upload runs as a result of SYNC. Proves it by
/// asserting that the page-index entry for the written page is
/// still absent after SYNC returns (in `cloud` mode that would be
/// `Some(hash)`).
#[tokio::test]
async fn synchronize_cache_memory_mode_is_no_op() {
    let (tmp, writer, _backend, rx) = fixture().await;
    writer
        .set_sync_after(SyncAfter::Memory)
        .expect("flip to memory");

    let cache = PageCache::new(Arc::clone(&writer));
    cache.write_bytes(0, &page_bytes(11)).await.expect("write");

    // SYNC returns immediately — no flush, no upload.
    let r = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        cache.synchronize_bytes(0, u64::from(DEFAULT_PAGE_SIZE_BYTES)),
    )
    .await
    .expect("SYNC must return immediately under memory mode");
    r.expect("sync ok");

    // Page-index slot 0 stays unallocated — the cache never
    // flushed, so `write_page_unsynced` never ran.
    assert!(
        writer.page_index().get(0).expect("page idx").is_none(),
        "memory-mode SYNC must not promote bytes out of RAM"
    );

    drop(rx);
    drop(cache);
    drop(writer);
    drop(tmp);
}

/// Live flip persists to runtime.json and survives reopen — the
/// next daemon start picks up the operator's choice.
#[tokio::test]
async fn set_sync_after_persists_across_reopen() {
    let (tmp, writer, backend, _rx) = fixture().await;
    let data_dir = tmp.path().to_path_buf();
    let name = writer.manifest().name.clone();

    assert_eq!(writer.sync_after(), SyncAfter::Storage);
    writer
        .set_sync_after(SyncAfter::Memory)
        .expect("flip to memory");
    assert_eq!(writer.sync_after(), SyncAfter::Memory);

    // Close the writer and reopen — runtime.json must carry the
    // mode forward.
    drop(writer);
    let reopened = VolumeWriter::open(&data_dir, &name, Arc::clone(&backend)).expect("reopen");
    assert_eq!(reopened.sync_after(), SyncAfter::Memory);

    drop(reopened);
    drop(tmp);
}

#[tokio::test]
async fn pending_upload_payload_round_trips_via_writer() {
    // Exercises the helper the daemon's crash-recovery scan uses to
    // reconstruct PendingUpload payloads from a freshly-opened
    // volume on boot.
    let (tmp, writer, backend, mut rx) = fixture().await;

    writer.write_page(3, &page_bytes(7)).await.expect("write");
    // Drain the queued task so the recovery payload we synthesise
    // doesn't collide with the live one.
    let _ = rx.try_recv();

    let payload = writer
        .pending_upload_payload(3)
        .expect("payload result")
        .expect("payload some");
    assert_eq!(payload.item_id, 3);
    assert_eq!(payload.dedup, DedupScope::Local);
    // Local-scope cloud keys are namespaced by the volume UUID hex,
    // not the (mutable) volume name.
    let ns = writer
        .manifest()
        .pool_namespace()
        .expect("local volume has a namespace");
    assert!(payload.object_key.contains(&ns));
    // Local pool path exists.
    assert!(payload.local_path.is_file());

    // No payload for an unallocated page.
    assert!(
        writer
            .pending_upload_payload(7)
            .expect("payload result")
            .is_none()
    );

    drop(backend);
    drop(tmp);
}
