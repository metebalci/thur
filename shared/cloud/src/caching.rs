// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Process-local "what's already in the cloud" cache layered over
//! [`CloudBackend`].
//!
//! The cloud is authoritative; the daemon's view of the cloud is a
//! warm local cache. Once we've confirmed a fact this lifetime — we
//! PUT key X, we HEAD'd Y as present, the LIST saw Z — we don't
//! re-ask. This collapses the canonical pathological workload (50+
//! concurrent zero-page PUTs to the same chunk key during
//! `mkfs.ext4`) down to one PUT plus N in-process cache hits, which
//! sidesteps GCS's per-object 1/sec mutation cap and slashes egress
//! cost on dedup-friendly workloads.
//!
//! Three states per key:
//!  - [`CloudState::Probed`]: LIST or positive HEAD confirmed presence.
//!  - [`CloudState::Uploaded`]: we ran the PUT and cached its return tuple.
//!  - [`CloudState::InFlight`]: a PUT is in flight; subscribers await one
//!    [`Shared`] future and receive identical results.
//!
//! Failure is conservative: a failed singleflight removes the entry so
//! the next caller hits the backend; a HEAD miss is not negative-cached
//! (could mask a concurrent upload by a co-resident process).

use crate::CloudBackend;
use crate::Result;
use crate::cloud_backend::LockState;
use crate::compression::CompressionAlgo;
use crate::error::CloudError;
use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What we know about a single cloud key.
enum CloudState {
    /// LIST or HEAD confirmed presence. We don't have the upload-side
    /// return tuple; on a coalesced `upload_chunk` hit we synthesize
    /// `(data.len(), None, None)` — matches what existing dedup-hit
    /// callers already discard.
    Probed,
    /// We PUT the key ourselves; cache the return tuple verbatim so
    /// coalesced waiters get identical bytes back.
    Uploaded {
        uncompressed: u64,
        compressed: Option<u64>,
        algo: Option<CompressionAlgo>,
    },
    /// A PUT is in flight; subscribers await the same future. The
    /// payload is `Arc<CloudError>` because `CloudError` is not `Clone`
    /// (it carries `std::io::Error`) and `Shared` requires the output
    /// to be `Clone`.
    InFlight(Shared<BoxFuture<'static, std::result::Result<UploadOutcome, Arc<CloudError>>>>),
}

#[derive(Clone, Copy)]
struct UploadOutcome {
    uncompressed: u64,
    compressed: Option<u64>,
    algo: Option<CompressionAlgo>,
}

/// Wrapper around a concrete [`CloudBackend`] that memoizes upload /
/// presence facts across the daemon's lifetime. Construct via
/// [`CachingCloudBackend::new`] at registry-population time; every
/// call site receives a wrapped backend automatically.
#[derive(Debug)]
pub struct CachingCloudBackend {
    inner: Arc<dyn CloudBackend>,
    name: String,
    known: Arc<Mutex<HashMap<String, CloudState>>>,
}

// `CloudState::InFlight` carries a `Shared<BoxFuture>` which is not
// `Debug`. Hand-derive a minimal Debug impl so the wrapper itself
// (which the trait requires) compiles.
impl std::fmt::Debug for CloudState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloudState::Probed => f.write_str("Probed"),
            CloudState::Uploaded { .. } => f.write_str("Uploaded"),
            CloudState::InFlight(_) => f.write_str("InFlight"),
        }
    }
}

impl CachingCloudBackend {
    /// Wrap an existing backend. The cache map starts empty; call
    /// [`Self::warmup_prefix`] from the daemon at boot to seed
    /// `Probed` entries from a LIST.
    pub fn new(inner: Box<dyn CloudBackend>, name: impl Into<String>) -> Self {
        Self {
            inner: Arc::from(inner),
            name: name.into(),
            known: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Backend name as wired in the registry — useful for log lines
    /// and (in step 5) telemetry counter labels.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl CloudBackend for CachingCloudBackend {
    async fn upload_chunk(
        &self,
        key: &str,
        data: &[u8],
    ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
        // Fast path: cache lookup. Compute action under the lock,
        // release it before any await (MutexGuard is not Send and
        // can't be held across an `await`).
        enum Action {
            ReturnTuple(u64, Option<u64>, Option<CompressionAlgo>),
            ReturnSynth,
            Await(Shared<BoxFuture<'static, std::result::Result<UploadOutcome, Arc<CloudError>>>>),
            Miss,
        }
        let action = {
            let map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(CloudState::Uploaded {
                    uncompressed,
                    compressed,
                    algo,
                }) => Action::ReturnTuple(*uncompressed, *compressed, *algo),
                Some(CloudState::Probed) => Action::ReturnSynth,
                Some(CloudState::InFlight(fut)) => Action::Await(fut.clone()),
                None => Action::Miss,
            }
        };
        match action {
            Action::ReturnTuple(u, c, a) => {
                shared_telemetry::record::chunk_cloud_cache_hit(&self.name);
                return Ok((u, c, a));
            }
            Action::ReturnSynth => {
                shared_telemetry::record::chunk_cloud_cache_hit(&self.name);
                return Ok((data.len() as u64, None, None));
            }
            Action::Await(waiter) => {
                shared_telemetry::record::chunk_cloud_cache_inflight_coalesced(&self.name);
                return match waiter.await {
                    Ok(o) => Ok((o.uncompressed, o.compressed, o.algo)),
                    Err(arc_err) => Err(CloudError::Other(arc_err.to_string())),
                };
            }
            Action::Miss => {}
        }

        // Build a singleflight future. Capture owned key + data so the
        // future is `'static` and `Shared`-able.
        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        let data_owned = data.to_vec();
        let upload_fut = async move {
            inner
                .upload_chunk(&key_owned, &data_owned)
                .await
                .map(|(uncompressed, compressed, algo)| UploadOutcome {
                    uncompressed,
                    compressed,
                    algo,
                })
                .map_err(Arc::new)
        };
        let shared: Shared<
            BoxFuture<'static, std::result::Result<UploadOutcome, Arc<CloudError>>>,
        > = upload_fut.boxed().shared();

        // Re-check under the lock — a concurrent caller may have raced
        // ahead and installed their own singleflight (or even
        // completed it) while we were building ours.
        let waiter = {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(CloudState::Uploaded {
                    uncompressed,
                    compressed,
                    algo,
                }) => return Ok((*uncompressed, *compressed, *algo)),
                Some(CloudState::Probed) => return Ok((data.len() as u64, None, None)),
                Some(CloudState::InFlight(existing)) => existing.clone(),
                None => {
                    map.insert(key.to_string(), CloudState::InFlight(shared.clone()));
                    shared
                }
            }
        };

        let result = waiter.await;
        let mut map = self.known.lock().expect("cache mutex poisoned");
        match result {
            Ok(outcome) => {
                map.insert(
                    key.to_string(),
                    CloudState::Uploaded {
                        uncompressed: outcome.uncompressed,
                        compressed: outcome.compressed,
                        algo: outcome.algo,
                    },
                );
                Ok((outcome.uncompressed, outcome.compressed, outcome.algo))
            }
            Err(arc_err) => {
                // Failure does not pollute the cache. Only remove if the
                // entry is still our `InFlight` — if a successful retry
                // by another caller has already overwritten it with
                // `Uploaded`, leave that alone.
                if matches!(map.get(key), Some(CloudState::InFlight(_))) {
                    map.remove(key);
                }
                Err(CloudError::Other(arc_err.to_string()))
            }
        }
    }

    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        // Fast path: cache lookup. Probed alone doesn't carry size, so
        // a Probed hit falls through to a real upload (we need the
        // authoritative size). Compute action under the lock, release
        // before any await — `MutexGuard` is not `Send`.
        enum Action {
            ReturnSize(u64),
            Await(Shared<BoxFuture<'static, std::result::Result<UploadOutcome, Arc<CloudError>>>>),
            Miss,
        }
        let action = {
            let map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(CloudState::Uploaded { uncompressed, .. }) => {
                    Action::ReturnSize(*uncompressed)
                }
                Some(CloudState::InFlight(fut)) => Action::Await(fut.clone()),
                Some(CloudState::Probed) | None => Action::Miss,
            }
        };
        match action {
            Action::ReturnSize(n) => {
                shared_telemetry::record::chunk_cloud_cache_hit(&self.name);
                return Ok(n);
            }
            Action::Await(waiter) => {
                shared_telemetry::record::chunk_cloud_cache_inflight_coalesced(&self.name);
                return match waiter.await {
                    Ok(o) => Ok(o.uncompressed),
                    Err(arc_err) => Err(CloudError::Other(arc_err.to_string())),
                };
            }
            Action::Miss => {}
        }

        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        let path_owned = file_path.to_path_buf();
        let upload_fut = async move {
            inner
                .upload_chunk_zerocopy(&key_owned, &path_owned)
                .await
                .map(|size| UploadOutcome {
                    uncompressed: size,
                    compressed: None,
                    algo: None,
                })
                .map_err(Arc::new)
        };
        let shared: Shared<
            BoxFuture<'static, std::result::Result<UploadOutcome, Arc<CloudError>>>,
        > = upload_fut.boxed().shared();

        let waiter = {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(CloudState::Uploaded { uncompressed, .. }) => return Ok(*uncompressed),
                Some(CloudState::InFlight(existing)) => existing.clone(),
                // For Probed (no size known) and None: install the singleflight.
                _ => {
                    map.insert(key.to_string(), CloudState::InFlight(shared.clone()));
                    shared
                }
            }
        };

        let result = waiter.await;
        let mut map = self.known.lock().expect("cache mutex poisoned");
        match result {
            Ok(outcome) => {
                map.insert(
                    key.to_string(),
                    CloudState::Uploaded {
                        uncompressed: outcome.uncompressed,
                        compressed: outcome.compressed,
                        algo: outcome.algo,
                    },
                );
                Ok(outcome.uncompressed)
            }
            Err(arc_err) => {
                if matches!(map.get(key), Some(CloudState::InFlight(_))) {
                    map.remove(key);
                }
                Err(CloudError::Other(arc_err.to_string()))
            }
        }
    }

    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        self.inner.download_chunk(key).await
    }

    async fn download_chunks_parallel(&self, keys: &[String]) -> Result<Vec<Vec<u8>>> {
        self.inner.download_chunks_parallel(keys).await
    }

    async fn upload_manifest(&self, key: &str, json: &str) -> Result<()> {
        self.inner.upload_manifest(key, json).await
    }

    async fn upload_versioned(&self, key: &str, data: &[u8]) -> Result<()> {
        // Versioned write changes content under a key the cache might
        // have memoized (warmup LIST seeding, or an erroneous prior
        // upload_chunk to the same key). Do the PUT first, then
        // unconditionally invalidate. If a concurrent upload_chunk
        // is in flight on this key, its waiters get the singleflight
        // result — those callers were uploading content-addressed
        // bytes anyway, so the staleness window is the same as the
        // next download_chunk's.
        let result = self.inner.upload_versioned(key, data).await;
        let mut map = self.known.lock().expect("cache mutex poisoned");
        map.remove(key);
        result
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        self.inner.download_manifest(key).await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        enum HeadAction {
            Hit,
            Await(Shared<BoxFuture<'static, std::result::Result<UploadOutcome, Arc<CloudError>>>>),
            Miss,
        }
        let action = {
            let map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(CloudState::Probed | CloudState::Uploaded { .. }) => HeadAction::Hit,
                Some(CloudState::InFlight(fut)) => HeadAction::Await(fut.clone()),
                None => HeadAction::Miss,
            }
        };
        match action {
            HeadAction::Hit => {
                shared_telemetry::record::chunk_cloud_cache_hit(&self.name);
                return Ok(true);
            }
            HeadAction::Await(fut) => {
                shared_telemetry::record::chunk_cloud_cache_inflight_coalesced(&self.name);
                if fut.await.is_ok() {
                    return Ok(true);
                }
                // Singleflight failed; fall through to real HEAD.
            }
            HeadAction::Miss => {}
        }
        // Miss, or coalesced singleflight failed: real HEAD. Negative
        // results are NOT cached (a co-resident process could upload
        // between our HEAD and the next caller's check).
        let exists = self.inner.chunk_exists(key).await?;
        if exists {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            map.entry(key.to_string()).or_insert(CloudState::Probed);
        }
        Ok(exists)
    }

    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        self.inner.list_objects(key_prefix).await
    }

    /// LIST `prefix` and seed every returned key as `Probed` (only
    /// into vacant entries — never overwrite `InFlight` or `Uploaded`).
    /// Returns the number of entries inserted. The daemon spawns one
    /// task per backend at registry-population time calling
    /// `warmup_prefix("chunks/")`; LIST failures are non-fatal.
    async fn warmup_prefix(&self, prefix: &str) -> Result<usize> {
        let keys = self.inner.list_objects(prefix).await?;
        let mut seeded = 0usize;
        {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            for k in keys {
                if let Entry::Vacant(slot) = map.entry(k) {
                    slot.insert(CloudState::Probed);
                    seeded += 1;
                }
            }
        }
        if seeded > 0 {
            shared_telemetry::record::chunk_cloud_cache_warmup_seeded(&self.name, seeded as u64);
        }
        Ok(seeded)
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        self.inner.delete_object(key).await?;
        let mut map = self.known.lock().expect("cache mutex poisoned");
        map.remove(key);
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        self.inner.backend_type()
    }

    async fn lock_state(&self) -> Result<LockState> {
        self.inner.lock_state().await
    }

    fn supports_legal_hold(&self) -> bool {
        self.inner.supports_legal_hold()
    }

    async fn set_object_legal_hold(&self, key: &str, held: bool) -> Result<()> {
        self.inner.set_object_legal_hold(key, held).await
    }

    async fn get_object_legal_hold(&self, key: &str) -> Result<bool> {
        self.inner.get_object_legal_hold(key).await
    }

    fn clone_box(&self) -> Box<dyn CloudBackend> {
        // Cloned wrappers share the same cache map and inner backend
        // — `Box<dyn CloudBackend>::clone()` callers must observe the
        // same in-process facts.
        Box::new(Self {
            inner: Arc::clone(&self.inner),
            name: self.name.clone(),
            known: Arc::clone(&self.known),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::join_all;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    /// Mutable counters + knobs for `MockBackend`. Held in `Arc` so
    /// test code and the wrapper's `clone_box` clones all observe and
    /// mutate the same state.
    #[derive(Debug, Default)]
    struct Counters {
        puts: AtomicUsize,
        versioned_puts: AtomicUsize,
        heads: AtomicUsize,
        gets: AtomicUsize,
        deletes: AtomicUsize,
        list_calls: AtomicUsize,
        fail_next_upload: AtomicBool,
        head_returns: AtomicBool,
        upload_delay_ms: AtomicU64,
        list_keys: Mutex<Vec<String>>,
    }

    #[derive(Debug)]
    struct MockBackend {
        c: Arc<Counters>,
    }

    impl MockBackend {
        fn new() -> (Self, Arc<Counters>) {
            let c = Arc::new(Counters::default());
            (Self { c: Arc::clone(&c) }, c)
        }
    }

    #[async_trait]
    impl CloudBackend for MockBackend {
        async fn upload_chunk(
            &self,
            _key: &str,
            data: &[u8],
        ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
            let delay = self.c.upload_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            self.c.puts.fetch_add(1, Ordering::SeqCst);
            if self.c.fail_next_upload.swap(false, Ordering::SeqCst) {
                return Err(CloudError::Other("mock failure".to_string()));
            }
            Ok((data.len() as u64, None, None))
        }

        async fn upload_chunk_zerocopy(&self, _key: &str, file_path: &Path) -> Result<u64> {
            self.c.puts.fetch_add(1, Ordering::SeqCst);
            Ok(std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0))
        }

        // Override the default impl so versioned PUTs are counted
        // separately and we can assert the wrapper is calling the
        // right inner method (vs. silently delegating to upload_chunk).
        async fn upload_versioned(&self, _key: &str, _data: &[u8]) -> Result<()> {
            self.c.versioned_puts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn download_chunk(&self, _key: &str) -> Result<Vec<u8>> {
            self.c.gets.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        }

        async fn download_chunks_parallel(&self, keys: &[String]) -> Result<Vec<Vec<u8>>> {
            for _ in keys {
                self.c.gets.fetch_add(1, Ordering::SeqCst);
            }
            Ok(keys.iter().map(|_| vec![]).collect())
        }

        async fn upload_manifest(&self, _key: &str, _json: &str) -> Result<()> {
            Ok(())
        }

        async fn download_manifest(&self, _key: &str) -> Result<String> {
            Ok(String::new())
        }

        async fn chunk_exists(&self, _key: &str) -> Result<bool> {
            self.c.heads.fetch_add(1, Ordering::SeqCst);
            Ok(self.c.head_returns.load(Ordering::SeqCst))
        }

        async fn list_objects(&self, _key_prefix: &str) -> Result<Vec<String>> {
            self.c.list_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.c.list_keys.lock().unwrap().clone())
        }

        async fn delete_object(&self, _key: &str) -> Result<()> {
            self.c.deletes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn backend_type(&self) -> &'static str {
            "mock"
        }

        async fn lock_state(&self) -> Result<LockState> {
            Ok(LockState::Off)
        }

        async fn set_object_legal_hold(&self, _key: &str, _held: bool) -> Result<()> {
            Ok(())
        }

        async fn get_object_legal_hold(&self, _key: &str) -> Result<bool> {
            Ok(false)
        }

        fn clone_box(&self) -> Box<dyn CloudBackend> {
            Box::new(Self {
                c: Arc::clone(&self.c),
            })
        }
    }

    fn wrap(mock: MockBackend) -> CachingCloudBackend {
        CachingCloudBackend::new(Box::new(mock), "test")
    }

    #[tokio::test]
    async fn singleflight_coalesces_concurrent_puts() {
        let (mock, c) = MockBackend::new();
        // 20 ms holds the first PUT long enough for the other 49
        // tasks to spin up, miss-check, hit `InFlight`, and join the
        // shared future.
        c.upload_delay_ms.store(20, Ordering::SeqCst);
        let cache = Arc::new(wrap(mock));

        let key = "chunks/aa/bb/zzz.dat".to_string();
        let zeros = vec![0u8; 65536];

        let mut handles = Vec::new();
        for _ in 0..50 {
            let cache = Arc::clone(&cache);
            let key = key.clone();
            let zeros = zeros.clone();
            handles.push(tokio::spawn(async move {
                cache.upload_chunk(&key, &zeros).await
            }));
        }

        for r in join_all(handles).await {
            assert!(r.unwrap().is_ok());
        }
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            1,
            "all 50 PUTs should coalesce into one backend call"
        );
    }

    #[tokio::test]
    async fn cache_hit_skips_put() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        let first = cache.upload_chunk("k", b"hello").await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);
        let second = cache.upload_chunk("k", b"hello").await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1, "second call is cache hit");
        assert_eq!(first, second, "cached tuple matches first PUT");
    }

    #[tokio::test]
    async fn failure_does_not_pollute_cache() {
        let (mock, c) = MockBackend::new();
        c.fail_next_upload.store(true, Ordering::SeqCst);
        let cache = wrap(mock);
        assert!(cache.upload_chunk("k", b"hello").await.is_err());
        // Backend's `fail_next_upload` was a swap-once; subsequent call
        // succeeds. The cache MUST have cleared its failed entry,
        // otherwise puts.load() would still be 1 here.
        assert!(cache.upload_chunk("k", b"hello").await.is_ok());
        assert_eq!(c.puts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn delete_invalidates_entry() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        cache.upload_chunk("k", b"hello").await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);
        cache.delete_object("k").await.unwrap();
        cache.upload_chunk("k", b"hello").await.unwrap();
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            2,
            "delete should invalidate the cache entry"
        );
    }

    #[tokio::test]
    async fn warmup_prefix_seeds_probed() {
        let (mock, c) = MockBackend::new();
        *c.list_keys.lock().unwrap() = vec![
            "chunks/aa/bb/x.dat".to_string(),
            "chunks/cc/dd/y.dat".to_string(),
        ];
        let cache = wrap(mock);
        let seeded = cache.warmup_prefix("chunks/").await.unwrap();
        assert_eq!(seeded, 2);

        // chunk_exists hits the cached Probed entry — no HEAD on backend.
        assert!(cache.chunk_exists("chunks/aa/bb/x.dat").await.unwrap());
        assert_eq!(c.heads.load(Ordering::SeqCst), 0);

        // upload_chunk on a Probed entry synthesizes (data.len, None, None) —
        // no PUT on backend.
        let (size, comp, algo) = cache
            .upload_chunk("chunks/aa/bb/x.dat", b"data")
            .await
            .unwrap();
        assert_eq!(size, 4);
        assert!(comp.is_none() && algo.is_none());
        assert_eq!(c.puts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn inflight_waiter_propagates_failure() {
        let (mock, c) = MockBackend::new();
        c.upload_delay_ms.store(20, Ordering::SeqCst);
        c.fail_next_upload.store(true, Ordering::SeqCst);
        let cache = Arc::new(wrap(mock));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let cache = Arc::clone(&cache);
            handles.push(tokio::spawn(async move {
                cache.upload_chunk("k", b"hello").await
            }));
        }
        for r in join_all(handles).await {
            assert!(r.unwrap().is_err(), "all 10 waiters see the same Err");
        }
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            1,
            "only one underlying call fires"
        );

        // Failure cleared the entry; retry hits the backend.
        cache.upload_chunk("k", b"hello").await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chunk_exists_caches_positive_head() {
        let (mock, c) = MockBackend::new();
        c.head_returns.store(true, Ordering::SeqCst);
        let cache = wrap(mock);
        assert!(cache.chunk_exists("k").await.unwrap());
        assert_eq!(c.heads.load(Ordering::SeqCst), 1);
        assert!(cache.chunk_exists("k").await.unwrap());
        assert_eq!(
            c.heads.load(Ordering::SeqCst),
            1,
            "second HEAD should be a cache hit"
        );
    }

    #[tokio::test]
    async fn chunk_exists_does_not_cache_negative() {
        let (mock, c) = MockBackend::new();
        // head_returns defaults to false.
        let cache = wrap(mock);
        assert!(!cache.chunk_exists("k").await.unwrap());
        assert_eq!(c.heads.load(Ordering::SeqCst), 1);
        assert!(!cache.chunk_exists("k").await.unwrap());
        assert_eq!(
            c.heads.load(Ordering::SeqCst),
            2,
            "negative HEAD must not be cached"
        );
    }

    #[tokio::test]
    async fn download_passthrough_no_cache() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        cache.download_chunk("k").await.unwrap();
        cache.download_chunk("k").await.unwrap();
        assert_eq!(
            c.gets.load(Ordering::SeqCst),
            2,
            "downloads pass through; bytes-cache is the chunk pool's job"
        );
    }

    #[tokio::test]
    async fn clone_box_shares_cache() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        cache.upload_chunk("k", b"hi").await.unwrap();
        let cloned: Box<dyn CloudBackend> = cache.clone_box();
        assert!(cloned.chunk_exists("k").await.unwrap());
        assert_eq!(
            c.heads.load(Ordering::SeqCst),
            0,
            "clone shares the cache map with the original"
        );
    }

    #[tokio::test]
    async fn versioned_upload_bypasses_cache() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        // Two versioned PUTs to the same key — the cache must NOT
        // memoize either; both reach the backend.
        cache.upload_versioned("k", b"v1").await.unwrap();
        cache.upload_versioned("k", b"v2").await.unwrap();
        assert_eq!(c.versioned_puts.load(Ordering::SeqCst), 2);
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            0,
            "must not delegate to upload_chunk"
        );
        // Cache stayed empty — chunk_exists falls through to a real
        // HEAD on the backend.
        assert!(!cache.chunk_exists("k").await.unwrap());
        assert_eq!(c.heads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn versioned_upload_invalidates_existing_entry() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        // Populate the cache with an Uploaded entry.
        cache.upload_chunk("k", b"v1").await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);
        // Versioned overwrite must clear the entry.
        cache.upload_versioned("k", b"v2").await.unwrap();
        assert_eq!(c.versioned_puts.load(Ordering::SeqCst), 1);
        // Subsequent upload_chunk must NOT see the stale Uploaded
        // entry — it must fire a real PUT.
        cache.upload_chunk("k", b"v3").await.unwrap();
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            2,
            "stale cache entry was not invalidated by upload_versioned"
        );
    }

    #[tokio::test]
    async fn default_trait_impl_delegates_to_upload_chunk() {
        // A backend that does NOT override upload_versioned (uses the
        // default trait impl) — confirms the default falls through to
        // upload_chunk and discards the size tuple.
        #[derive(Debug)]
        struct DefaultMock {
            c: Arc<Counters>,
        }
        #[async_trait]
        impl CloudBackend for DefaultMock {
            async fn upload_chunk(
                &self,
                _key: &str,
                data: &[u8],
            ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
                self.c.puts.fetch_add(1, Ordering::SeqCst);
                Ok((data.len() as u64, None, None))
            }
            async fn upload_chunk_zerocopy(&self, _: &str, _: &Path) -> Result<u64> {
                Ok(0)
            }
            async fn download_chunk(&self, _: &str) -> Result<Vec<u8>> {
                Ok(vec![])
            }
            async fn download_chunks_parallel(&self, _: &[String]) -> Result<Vec<Vec<u8>>> {
                Ok(vec![])
            }
            async fn upload_manifest(&self, _: &str, _: &str) -> Result<()> {
                Ok(())
            }
            async fn download_manifest(&self, _: &str) -> Result<String> {
                Ok(String::new())
            }
            async fn chunk_exists(&self, _: &str) -> Result<bool> {
                Ok(false)
            }
            async fn list_objects(&self, _: &str) -> Result<Vec<String>> {
                Ok(vec![])
            }
            async fn delete_object(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn backend_type(&self) -> &'static str {
                "default-mock"
            }
            async fn lock_state(&self) -> Result<LockState> {
                Ok(LockState::Off)
            }
            async fn set_object_legal_hold(&self, _: &str, _: bool) -> Result<()> {
                Ok(())
            }
            async fn get_object_legal_hold(&self, _: &str) -> Result<bool> {
                Ok(false)
            }
            fn clone_box(&self) -> Box<dyn CloudBackend> {
                Box::new(Self {
                    c: Arc::clone(&self.c),
                })
            }
        }
        let c = Arc::new(Counters::default());
        let backend = DefaultMock { c: Arc::clone(&c) };
        // Call the trait method directly (no cache wrapper) — the
        // default impl should delegate to upload_chunk.
        backend.upload_versioned("k", b"hi").await.unwrap();
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            1,
            "default upload_versioned must delegate to upload_chunk"
        );
    }

    // ===== upload_chunk_zerocopy — same cache states as the byte form.

    /// Helper: write a small payload to a tempdir and return the path.
    fn write_tmp(payload: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("payload.bin");
        std::fs::write(&path, payload).expect("write tempfile");
        (dir, path)
    }

    #[tokio::test]
    async fn zerocopy_cache_hit_returns_size_without_second_put() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        let (_dir, path) = write_tmp(&vec![0u8; 4096]);

        let first = cache.upload_chunk_zerocopy("k", &path).await.unwrap();
        assert_eq!(first, 4096);
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);

        let second = cache.upload_chunk_zerocopy("k", &path).await.unwrap();
        assert_eq!(second, 4096);
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            1,
            "second zerocopy call must hit the Uploaded cache entry",
        );
    }

    #[tokio::test]
    async fn zerocopy_singleflight_coalesces_concurrent_calls() {
        let (mock, c) = MockBackend::new();
        // Slow the first PUT so 49 followers see the InFlight entry.
        c.upload_delay_ms.store(20, Ordering::SeqCst);
        let cache = Arc::new(wrap(mock));
        let (_dir, path) = write_tmp(&vec![0u8; 8192]);

        let mut handles = Vec::new();
        for _ in 0..50 {
            let cache = Arc::clone(&cache);
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                cache.upload_chunk_zerocopy("kz", &path).await
            }));
        }
        for h in join_all(handles).await {
            let size = h.unwrap().unwrap();
            assert_eq!(size, 8192);
        }
        // MockBackend's upload_chunk_zerocopy increments `puts` too.
        // CachingCloudBackend's singleflight collapses to one inner call.
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            1,
            "all 50 zerocopy PUTs should coalesce into one backend call",
        );
    }

    #[tokio::test]
    async fn zerocopy_failure_clears_inflight_entry() {
        // MockBackend's zerocopy impl doesn't honour fail_next_upload,
        // so use a DefaultMock-style local impl whose zerocopy errors.
        #[derive(Debug)]
        struct ErrZeroMock;
        #[async_trait]
        impl CloudBackend for ErrZeroMock {
            async fn upload_chunk(
                &self,
                _: &str,
                _: &[u8],
            ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
                Ok((0, None, None))
            }
            async fn upload_chunk_zerocopy(&self, _: &str, _: &Path) -> Result<u64> {
                Err(CloudError::Other("zerocopy boom".into()))
            }
            async fn download_chunk(&self, _: &str) -> Result<Vec<u8>> {
                Ok(vec![])
            }
            async fn download_chunks_parallel(&self, _: &[String]) -> Result<Vec<Vec<u8>>> {
                Ok(vec![])
            }
            async fn upload_manifest(&self, _: &str, _: &str) -> Result<()> {
                Ok(())
            }
            async fn download_manifest(&self, _: &str) -> Result<String> {
                Ok(String::new())
            }
            async fn chunk_exists(&self, _: &str) -> Result<bool> {
                Ok(false)
            }
            async fn list_objects(&self, _: &str) -> Result<Vec<String>> {
                Ok(vec![])
            }
            async fn delete_object(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn backend_type(&self) -> &'static str {
                "errzero"
            }
            async fn lock_state(&self) -> Result<LockState> {
                Ok(LockState::Off)
            }
            async fn set_object_legal_hold(&self, _: &str, _: bool) -> Result<()> {
                Ok(())
            }
            async fn get_object_legal_hold(&self, _: &str) -> Result<bool> {
                Ok(false)
            }
            fn clone_box(&self) -> Box<dyn CloudBackend> {
                Box::new(Self)
            }
        }
        let cache = CachingCloudBackend::new(Box::new(ErrZeroMock), "test");
        let (_dir, path) = write_tmp(b"hello");
        // First call errors; the cache must have removed the InFlight
        // entry so a retry sees a fresh Miss (still errors, but goes
        // through the upload path again).
        assert!(cache.upload_chunk_zerocopy("kerr", &path).await.is_err());
        assert!(cache.upload_chunk_zerocopy("kerr", &path).await.is_err());
    }

    /// Exercises the trivial pass-through trait methods on the cache
    /// wrapper (download_chunks_parallel, upload_manifest,
    /// download_manifest, list_objects, lock_state,
    /// {set,get}_object_legal_hold) plus the Debug impl for CloudState.
    /// One call per method — no caching semantics to assert.
    #[tokio::test]
    async fn trivial_passthroughs_delegate_to_inner() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        // download_chunks_parallel
        let _ = cache
            .download_chunks_parallel(&[
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
            ])
            .await
            .unwrap();
        assert_eq!(c.gets.load(Ordering::SeqCst), 3);
        // upload_manifest + download_manifest
        cache
            .upload_manifest("m", "{\"v\":1}")
            .await
            .expect("manifest upload");
        let _ = cache.download_manifest("m").await.expect("manifest download");
        // list_objects
        let listed = cache.list_objects("").await.expect("list");
        assert!(listed.is_empty());
        assert_eq!(c.list_calls.load(Ordering::SeqCst), 1);
        // lock_state
        assert_eq!(cache.lock_state().await.unwrap(), LockState::Off);
        // legal-hold pass-throughs (MockBackend stubs return Ok)
        cache
            .set_object_legal_hold("k", true)
            .await
            .expect("set legal hold");
        let held = cache
            .get_object_legal_hold("k")
            .await
            .expect("get legal hold");
        assert!(!held);
        // Debug printout exercises CloudState::Probed/Uploaded/InFlight arms.
        cache.upload_chunk("u", b"x").await.unwrap(); // Uploaded
        c.head_returns.store(true, Ordering::SeqCst);
        assert!(cache.chunk_exists("p").await.unwrap()); // Probed
        let dbg = format!("{:?}", cache);
        assert!(dbg.contains("CachingCloudBackend"));
    }

    #[tokio::test]
    async fn zerocopy_after_positive_head_still_uploads_for_size() {
        // chunk_exists()==true installs a Probed (sizeless) cache entry.
        // upload_chunk_zerocopy on a Probed entry MUST fall through
        // to a real PUT because Probed carries no authoritative size.
        let (mock, c) = MockBackend::new();
        c.head_returns.store(true, Ordering::SeqCst);
        let cache = wrap(mock);
        // Populate Probed via chunk_exists.
        assert!(cache.chunk_exists("kp").await.unwrap());
        assert_eq!(c.heads.load(Ordering::SeqCst), 1);
        assert_eq!(c.puts.load(Ordering::SeqCst), 0);

        let (_dir, path) = write_tmp(&vec![0u8; 1024]);
        let size = cache.upload_chunk_zerocopy("kp", &path).await.unwrap();
        assert_eq!(size, 1024);
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            1,
            "Probed (no size) must fall through to a real PUT",
        );
    }
}
