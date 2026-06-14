// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Bounded-concurrency upload pipeline. Drives a batch of
//! [`PendingUpload`]s through [`upload_chunk_inert`] with at most
//! `max_concurrent` PUTs in flight; per-completion side-effects are a
//! caller-supplied closure so each product's flavour (legal-hold
//! re-apply, eviction-Notify, page-index flag flip) stays out of the
//! shared scaffold.

use std::future::Future;
use std::sync::Arc;

use futures::stream::{self, StreamExt};
use shared_object_store::ObjectStoreBackend;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::inert::upload_chunk_inert;
use crate::payload::{PendingUpload, UploadOutcome};

/// Pipeline `payloads` through [`upload_chunk_inert`] with at most
/// `max_concurrent` PUTs in flight. Each completion immediately frees
/// its slot so the next payload starts — the previous drain-N /
/// await-N batch shape was throughput-gated by the slowest chunk per
/// batch, leaving siblings idle behind it.
///
/// `label` is a short human-readable identifier (tape barcode, volume
/// name, "recovery" — whatever the caller wants) used purely for
/// log-line context. Not interpreted by this function.
///
/// `on_complete` runs after each successful PUT (or HEAD-skip),
/// before the outcome is yielded into the returned vector. Use it to
/// fire eviction-Notify, apply per-object legal-hold, flip the
/// per-product "uploaded" flag, etc. The hook is awaited inside the
/// `buffer_unordered` stream so a slow hook gates only its own task,
/// not its siblings.
///
/// `backend_permits`, when `Some`, is a per-backend [`Semaphore`]
/// **shared across every concurrent caller bound to that backend** —
/// each chunk acquires one permit immediately before
/// [`upload_chunk_inert`] reads the file and PUTs, releasing on
/// completion. `buffer_unordered` only bounds a single call's in-flight
/// futures; the shared semaphore is what keeps the *combined* in-flight
/// PUT count (hence the chunk-pinned RAM) at `max_concurrent` per
/// backend once independent tapes/volumes upload concurrently
/// (issue #216). `None` falls back to the per-call `buffer_unordered`
/// bound alone.
///
/// Single attempt per payload — the per-backend retry inside
/// [`shared_object_store::ObjectStoreBackend`] implementations already runs the
/// configured jittered exponential retries with classify-and-fail-fast
/// on permanent errors. Per-chunk failures are isolated: a returned
/// `Err` from `upload_chunk_inert` is logged and the outcome is
/// dropped, but the remaining in-flight payloads continue.
///
/// Lifted from `vtl/daemon/src/upload_worker.rs::run_upload_pipeline`
/// (tape-side post-upload side effects — `set_object_legal_hold` +
/// `disk_cache_evict_notify.notify_one()` — moved into the
/// `on_complete` closure the daemon passes in).
pub async fn run_upload_pipeline<F, Fut>(
    storage_backend: &dyn ObjectStoreBackend,
    label: &str,
    payloads: Vec<PendingUpload>,
    max_concurrent: usize,
    backend_permits: Option<Arc<Semaphore>>,
    on_complete: F,
) -> Vec<UploadOutcome>
where
    F: Fn(UploadOutcome) -> Fut + Sync + Send,
    Fut: Future<Output = ()> + Send,
{
    if payloads.is_empty() {
        return Vec::new();
    }

    debug!(
        "Pipelining {} items for {} (concurrency={})",
        payloads.len(),
        label,
        max_concurrent
    );

    let concurrency = max_concurrent.max(1);

    // The hook takes the outcome by value (cheap clone — single
    // `String` + scalars) so its returned future doesn't borrow from
    // the loop's `outcome` binding. Passing `&UploadOutcome` here ran
    // into the HRTB-for-closures lifetime restriction (`Fut` is one
    // type, so it can't depend on the call-site reference's lifetime);
    // by-value sidesteps the entire dance.
    let results: Vec<Option<UploadOutcome>> = stream::iter(payloads)
        .map(|payload| {
            let on_complete = &on_complete;
            let backend_permits = &backend_permits;
            async move {
                // Hold a per-backend permit (when configured) across the
                // file-read + PUT so concurrent tapes can't exceed the
                // backend's `max_concurrent` in-flight ceiling. Acquired
                // here — not before the file read — so a future merely
                // queued behind the semaphore pins no chunk in RAM.
                let _permit = match backend_permits {
                    Some(sem) => sem.acquire().await.ok(),
                    None => None,
                };
                match upload_chunk_inert(storage_backend, &payload).await {
                    Ok(outcome) => {
                        debug!(
                            "Successfully uploaded item {} for {}",
                            outcome.item_id, label
                        );
                        on_complete(outcome.clone()).await;
                        Some(outcome)
                    }
                    Err(e) => {
                        warn!(
                            "Upload failed for item {} from {} after backend retries: {}",
                            payload.item_id, label, e
                        );
                        None
                    }
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let outcomes: Vec<UploadOutcome> = results.into_iter().flatten().collect();
    for outcome in &outcomes {
        debug!(
            "Item {} upload task completed successfully",
            outcome.item_id
        );
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use futures::stream::{self, StreamExt};
    use tempfile::TempDir;

    use crate::payload::PendingUpload;
    use crate::pipeline::run_upload_pipeline;
    use crate::test_support::MockBackend;
    use shared_object_store::DedupScope;

    fn make_payload(item_id: u64, dir: &Path) -> PendingUpload {
        let local = dir.join(format!("{}.dat", item_id));
        std::fs::write(&local, format!("data-{}", item_id).as_bytes()).unwrap();
        PendingUpload {
            item_id,
            hash: format!("{:02x}", item_id).repeat(32),
            local_path: local,
            object_key: format!("chunks/{}/v.dat", item_id),
            dedup: DedupScope::Local,
            backend_name: "primary".into(),
        }
    }

    /// Property the pipelined upload worker depends on: with N
    /// futures driven through `buffer_unordered(N)`, a single slow
    /// future does not gate completion of its peers. The drain-batch
    /// `JoinSet` shape this replaced waited on the whole batch
    /// before launching the next one — a slow PUT idled every
    /// sibling worker until it settled. If `buffer_unordered` is
    /// ever swapped back to a batch drain, this test fails.
    #[tokio::test(start_paused = true)]
    async fn pipeline_does_not_gate_on_slow_task() {
        const N: usize = 8;
        const SLOW_IDX: usize = 3;
        let fast = Duration::from_millis(10);
        let slow = Duration::from_millis(500);

        let slow_done = Arc::new(AtomicUsize::new(0));
        let fast_done_before_slow = Arc::new(AtomicUsize::new(0));

        let slow_done_for_tasks = slow_done.clone();
        let fast_done_for_tasks = fast_done_before_slow.clone();

        let tasks = (0..N).map(move |i| {
            let dur = if i == SLOW_IDX { slow } else { fast };
            let slow_done = slow_done_for_tasks.clone();
            let fast_done = fast_done_for_tasks.clone();
            async move {
                tokio::time::sleep(dur).await;
                if i == SLOW_IDX {
                    slow_done.store(1, Ordering::SeqCst);
                } else if slow_done.load(Ordering::SeqCst) == 0 {
                    fast_done.fetch_add(1, Ordering::SeqCst);
                }
                i
            }
        });

        let results: Vec<usize> = stream::iter(tasks).buffer_unordered(N).collect().await;

        assert_eq!(results.len(), N, "every task must complete");
        assert_eq!(
            fast_done_before_slow.load(Ordering::SeqCst),
            N - 1,
            "all {} fast tasks must complete before the slow one — \
             pipelining is gated if any are observed completing after",
            N - 1,
        );
    }

    #[tokio::test]
    async fn empty_payloads_returns_empty_vec_without_invoking_hook() {
        let backend = MockBackend::default();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_hook = hits.clone();
        let outcomes = run_upload_pipeline(&backend, "label", vec![], 4, None, move |_| {
            let hits = hits_for_hook.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        assert!(outcomes.is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(backend.puts(), 0);
        assert_eq!(backend.heads(), 0);
    }

    #[tokio::test]
    async fn max_concurrent_zero_clamps_to_one_and_runs_all() {
        let backend = MockBackend::default();
        let tmp = TempDir::new().unwrap();
        let payloads: Vec<PendingUpload> = (0..3).map(|i| make_payload(i, tmp.path())).collect();
        let outcomes =
            run_upload_pipeline(&backend, "label", payloads, 0, None, |_| async {}).await;
        assert_eq!(outcomes.len(), 3);
        assert_eq!(backend.puts(), 3);
    }

    #[tokio::test]
    async fn per_payload_error_does_not_gate_siblings_and_hook_only_fires_on_success() {
        let backend = MockBackend::default();
        backend
            .fail_put_for_keys
            .lock()
            .unwrap()
            .insert("chunks/2/v.dat".to_string());

        let tmp = TempDir::new().unwrap();
        let payloads: Vec<PendingUpload> =
            (1..=4u64).map(|i| make_payload(i, tmp.path())).collect();

        let hook_ids = Arc::new(Mutex::new(Vec::<u64>::new()));
        let hook_ids_for_hook = hook_ids.clone();
        let outcomes = run_upload_pipeline(&backend, "label", payloads, 2, None, move |o| {
            let hook_ids = hook_ids_for_hook.clone();
            async move {
                hook_ids.lock().unwrap().push(o.item_id);
            }
        })
        .await;

        let mut returned: Vec<u64> = outcomes.iter().map(|o| o.item_id).collect();
        returned.sort();
        assert_eq!(returned, vec![1, 3, 4]);

        let mut fired = hook_ids.lock().unwrap().clone();
        fired.sort();
        assert_eq!(fired, vec![1, 3, 4]);

        // PUT was attempted for all 4 (the failing one still issued a PUT
        // — the failure is on the backend side, not pre-filter).
        assert_eq!(backend.puts(), 4);
    }

    #[tokio::test]
    async fn global_dedup_hit_skips_put_but_still_fires_hook() {
        let backend = MockBackend::default();
        // Storage-side HEAD returns true for every key → dedup hit; no PUT.
        *backend.head_returns.lock().unwrap() = true;

        let tmp = TempDir::new().unwrap();
        let mut payloads: Vec<PendingUpload> =
            (1..=3u64).map(|i| make_payload(i, tmp.path())).collect();
        // Flip every payload to Global so the HEAD probe runs.
        for p in &mut payloads {
            p.dedup = DedupScope::Global;
        }

        let hook_calls = Arc::new(AtomicUsize::new(0));
        let hook_calls_for_hook = hook_calls.clone();
        let outcomes = run_upload_pipeline(&backend, "label", payloads, 2, None, move |o| {
            let hook_calls = hook_calls_for_hook.clone();
            async move {
                assert!(o.dedup_hit, "every outcome must reflect the HEAD hit");
                assert!(o.put_bytes.is_none());
                hook_calls.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;

        assert_eq!(outcomes.len(), 3);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 3);
        assert_eq!(backend.heads(), 3);
        assert_eq!(backend.puts(), 0);
    }

    /// Issue #216: a shared per-backend semaphore caps the *combined*
    /// in-flight PUT count across concurrent pipeline calls (independent
    /// tapes uploading at once), even though each call's own
    /// `buffer_unordered` would let more overlap. Without it the calls
    /// overlap freely — the control half proves the cap is the
    /// semaphore's doing, not incidental scheduling.
    #[tokio::test]
    async fn shared_semaphore_bounds_per_backend_concurrency() {
        use tokio::sync::Semaphore;

        // One "tape": 4 chunks, each call permitted 4-wide locally.
        async fn drive(
            backend: &MockBackend,
            dir: &Path,
            base: u64,
            sem: Option<Arc<Semaphore>>,
        ) {
            let payloads: Vec<PendingUpload> =
                (0..4).map(|i| make_payload(base + i, dir)).collect();
            run_upload_pipeline(backend, "tape", payloads, 4, sem, |_| async {}).await;
        }

        // Bounded: three concurrent 4-wide calls share one Semaphore(2),
        // so at most 2 PUTs are ever in flight despite 12 being eligible.
        let dir = TempDir::new().unwrap();
        let backend = MockBackend::default();
        backend.put_delay_ms.store(20, Ordering::SeqCst);
        let sem = Arc::new(Semaphore::new(2));
        tokio::join!(
            drive(&backend, dir.path(), 0, Some(sem.clone())),
            drive(&backend, dir.path(), 10, Some(sem.clone())),
            drive(&backend, dir.path(), 20, Some(sem.clone())),
        );
        assert_eq!(backend.puts(), 12, "every chunk must still PUT");
        assert!(
            backend.max_in_flight() <= 2,
            "shared semaphore must cap combined in-flight PUTs at 2, saw {}",
            backend.max_in_flight()
        );

        // Control: no shared semaphore → the three calls overlap past 2.
        let dir2 = TempDir::new().unwrap();
        let backend2 = MockBackend::default();
        backend2.put_delay_ms.store(20, Ordering::SeqCst);
        tokio::join!(
            drive(&backend2, dir2.path(), 0, None),
            drive(&backend2, dir2.path(), 10, None),
            drive(&backend2, dir2.path(), 20, None),
        );
        assert!(
            backend2.max_in_flight() > 2,
            "without the shared semaphore the calls should overlap beyond 2, saw {}",
            backend2.max_in_flight()
        );
    }
}
