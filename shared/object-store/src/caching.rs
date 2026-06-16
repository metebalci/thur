// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Process-local "what's already in the storage" cache layered over
//! [`ObjectStoreBackend`].
//!
//! The storage is authoritative; the daemon's view of the storage is a
//! warm local cache. Once we've confirmed a fact this lifetime — we
//! PUT key X, we HEAD'd Y as present, the LIST saw Z — we don't
//! re-ask. This collapses the canonical pathological workload (50+
//! concurrent zero-page PUTs to the same chunk key during
//! `mkfs.ext4`) down to one PUT plus N in-process cache hits, which
//! sidesteps GCS's per-object 1/sec mutation cap and slashes egress
//! cost on dedup-friendly workloads.
//!
//! Three states per key:
//!  - [`StorageState::Probed`]: LIST or positive HEAD confirmed presence.
//!  - [`StorageState::Uploaded`]: we ran the PUT and cached its return tuple.
//!  - [`StorageState::InFlight`]: a PUT is in flight; subscribers await one
//!    [`Shared`] future and receive identical results.
//!
//! Failure is conservative: a failed singleflight removes the entry so
//! the next caller hits the backend; a HEAD miss is not negative-cached
//! (could mask a concurrent upload by a co-resident process).

use crate::ObjectStoreBackend;
use crate::Result;
use crate::compression::CompressionAlgo;
use crate::error::ObjectStoreError;
use crate::object_store_backend::LockState;
use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use lru::LruCache;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Upper bound on memoized presence/upload facts. ~150–200 B per entry,
/// so ~40–50 MB worst case — enough to coalesce any realistic hot working
/// set while keeping daemon RSS bounded regardless of pool size. LRU
/// eviction of a terminal (Probed / Uploaded) fact is harmless: the next
/// caller re-HEADs or re-PUTs (content-addressed, idempotent). See
/// issue #192.
const DEFAULT_KNOWN_CAPACITY: usize = 262_144;

/// What we know about a single storage key.
enum StorageState {
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
    /// payload is `Arc<ObjectStoreError>` because `ObjectStoreError` is not `Clone`
    /// (it carries `std::io::Error`) and `Shared` requires the output
    /// to be `Clone`.
    InFlight(Shared<BoxFuture<'static, std::result::Result<UploadOutcome, Arc<ObjectStoreError>>>>),
}

#[derive(Clone, Copy)]
struct UploadOutcome {
    uncompressed: u64,
    compressed: Option<u64>,
    algo: Option<CompressionAlgo>,
}

/// Drop guard that removes a stranded `InFlight` entry if the installer
/// task is cancelled before completing its terminal transition. Without
/// it, a cancelled installer (an aborted job, a dropped future in a
/// `select!`/`timeout`) leaves the `InFlight` forever; a later caller
/// polls its `Shared` future to an `Err`, which `Shared` memoizes, and
/// every subsequent upload of that key returns the same error instantly
/// with no backend call — a permanently-poisoned key until restart
/// (issue #264). The installer disarms it once it has run the terminal
/// `Uploaded`/remove transition itself.
struct InFlightGuard {
    known: Arc<Mutex<LruCache<String, StorageState>>>,
    key: String,
    armed: bool,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut map = self.known.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(map.get(&self.key), Some(StorageState::InFlight(_))) {
            map.pop(&self.key);
        }
    }
}

/// Wrapper around a concrete [`ObjectStoreBackend`] that memoizes upload /
/// presence facts across the daemon's lifetime. Construct via
/// [`CachingObjectStoreBackend::new`] at registry-population time; every
/// call site receives a wrapped backend automatically.
#[derive(Debug)]
pub struct CachingObjectStoreBackend {
    inner: Arc<dyn ObjectStoreBackend>,
    name: String,
    /// Bounded LRU of presence/upload facts (issue #192). An `InFlight`
    /// entry may in principle be evicted under cap pressure while its PUT
    /// is still running; that only costs a rare duplicate (idempotent,
    /// content-addressed) PUT — every completion install/remove below is
    /// already gated on the entry still being `InFlight`, so a missing
    /// entry is a safe no-op.
    known: Arc<Mutex<LruCache<String, StorageState>>>,
    /// Monotonic invalidation epoch, bumped by every op that removes a
    /// cache fact (`delete_object`, `upload_versioned`). A HEAD or PUT
    /// snapshots the epoch before it touches the backend and only
    /// installs its result if the epoch is unchanged at completion. This
    /// closes the TOCTOU where a stale completion would resurrect a
    /// concurrently-deleted object in the cache (issue #134) — which
    /// would later let a content-addressed re-write skip its PUT, losing
    /// data against the authoritative backend.
    epoch: Arc<AtomicU64>,
}

// `StorageState::InFlight` carries a `Shared<BoxFuture>` which is not
// `Debug`. Hand-derive a minimal Debug impl so the wrapper itself
// (which the trait requires) compiles.
impl std::fmt::Debug for StorageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageState::Probed => f.write_str("Probed"),
            StorageState::Uploaded { .. } => f.write_str("Uploaded"),
            StorageState::InFlight(_) => f.write_str("InFlight"),
        }
    }
}

impl CachingObjectStoreBackend {
    /// Wrap an existing backend. The cache map starts empty; call
    /// [`Self::warmup_prefix`] from the daemon at boot to seed
    /// `Probed` entries from a LIST.
    pub fn new(inner: Box<dyn ObjectStoreBackend>, name: impl Into<String>) -> Self {
        Self::with_capacity(inner, name, DEFAULT_KNOWN_CAPACITY)
    }

    /// As [`Self::new`] but with an explicit fact-cache capacity. Used by
    /// tests to drive LRU eviction with a tiny bound.
    fn with_capacity(
        inner: Box<dyn ObjectStoreBackend>,
        name: impl Into<String>,
        capacity: usize,
    ) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: Arc::from(inner),
            name: name.into(),
            known: Arc::new(Mutex::new(LruCache::new(cap))),
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Backend name as wired in the registry — useful for log lines
    /// and (in step 5) telemetry counter labels.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl ObjectStoreBackend for CachingObjectStoreBackend {
    async fn upload_chunk(
        &self,
        key: &str,
        data: Vec<u8>,
    ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
        // Fast path: cache lookup. Compute action under the lock,
        // release it before any await (MutexGuard is not Send and
        // can't be held across an `await`).
        enum Action {
            ReturnTuple(u64, Option<u64>, Option<CompressionAlgo>),
            ReturnSynth,
            Await(
                Shared<
                    BoxFuture<'static, std::result::Result<UploadOutcome, Arc<ObjectStoreError>>>,
                >,
            ),
            Miss,
        }
        let action = {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(StorageState::Uploaded {
                    uncompressed,
                    compressed,
                    algo,
                }) => Action::ReturnTuple(*uncompressed, *compressed, *algo),
                Some(StorageState::Probed) => Action::ReturnSynth,
                Some(StorageState::InFlight(fut)) => Action::Await(fut.clone()),
                None => Action::Miss,
            }
        };
        match action {
            Action::ReturnTuple(u, c, a) => {
                shared_telemetry::record::chunk_storage_cache_hit(&self.name);
                return Ok((u, c, a));
            }
            Action::ReturnSynth => {
                shared_telemetry::record::chunk_storage_cache_hit(&self.name);
                return Ok((data.len() as u64, None, None));
            }
            Action::Await(waiter) => {
                shared_telemetry::record::chunk_storage_cache_inflight_coalesced(&self.name);
                return match waiter.await {
                    Ok(o) => Ok((o.uncompressed, o.compressed, o.algo)),
                    Err(arc_err) => Err(ObjectStoreError::Other(arc_err.to_string())),
                };
            }
            Action::Miss => {}
        }

        // Before the (up to 128 MiB) payload copy, re-check the map: in
        // the concurrent-cold-write storm this wrapper exists to optimize
        // (e.g. 50 parallel zero-page mkfs writes), every caller's first
        // check misses, but only one installs — the rest would each copy
        // the whole payload just to lose the install race and drop it.
        // Coalesce onto a now-present entry here so only the eventual
        // installer materializes the buffer (issue #265).
        {
            enum PreCheck {
                Hit(u64, Option<u64>, Option<CompressionAlgo>),
                Synth,
                Await(
                    Shared<
                        BoxFuture<'static, std::result::Result<UploadOutcome, Arc<ObjectStoreError>>>,
                    >,
                ),
                Miss,
            }
            let pre = {
                let mut map = self.known.lock().expect("cache mutex poisoned");
                match map.get(key) {
                    Some(StorageState::Uploaded {
                        uncompressed,
                        compressed,
                        algo,
                    }) => PreCheck::Hit(*uncompressed, *compressed, *algo),
                    Some(StorageState::Probed) => PreCheck::Synth,
                    Some(StorageState::InFlight(fut)) => PreCheck::Await(fut.clone()),
                    None => PreCheck::Miss,
                }
            };
            match pre {
                PreCheck::Hit(u, c, a) => {
                    shared_telemetry::record::chunk_storage_cache_hit(&self.name);
                    return Ok((u, c, a));
                }
                PreCheck::Synth => {
                    shared_telemetry::record::chunk_storage_cache_hit(&self.name);
                    return Ok((data.len() as u64, None, None));
                }
                PreCheck::Await(waiter) => {
                    shared_telemetry::record::chunk_storage_cache_inflight_coalesced(&self.name);
                    return match waiter.await {
                        Ok(o) => Ok((o.uncompressed, o.compressed, o.algo)),
                        Err(arc_err) => Err(ObjectStoreError::Other(arc_err.to_string())),
                    };
                }
                PreCheck::Miss => {}
            }
        }

        // Build a singleflight future. Capture owned key + data so the
        // future is `'static` and `Shared`-able. `data` is already owned
        // (issue #236) so it moves in; stash its length first for the
        // Probed-state synth path below, which still needs it after the move.
        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        let data_len = data.len() as u64;
        let data_owned = data;
        let name = self.name.clone();
        let upload_fut = async move {
            // Record one storage PUT per actual backend call (the
            // singleflight runs the inner upload once; joiners await the
            // shared future and don't double-count) — issue #204.
            let started = std::time::Instant::now();
            let res = inner.upload_chunk(&key_owned, data_owned).await;
            let secs = started.elapsed().as_secs_f64();
            match &res {
                Ok((unc, comp, _)) => shared_telemetry::record::storage_request(
                    &name,
                    "put",
                    "ok",
                    comp.unwrap_or(*unc),
                    secs,
                ),
                Err(_) => {
                    shared_telemetry::record::storage_request(&name, "put", "error", 0, secs)
                }
            }
            res.map(|(uncompressed, compressed, algo)| UploadOutcome {
                uncompressed,
                compressed,
                algo,
            })
            .map_err(Arc::new)
        };
        let shared: Shared<
            BoxFuture<'static, std::result::Result<UploadOutcome, Arc<ObjectStoreError>>>,
        > = upload_fut.boxed().shared();

        // Re-check under the lock — a concurrent caller may have raced
        // ahead and installed their own singleflight (or even
        // completed it) while we were building ours. The creator captures
        // the invalidation epoch at install time; only the creator
        // installs the terminal `Uploaded`, gated on that epoch (joiners
        // just return the shared result), so a delete that races our PUT
        // can't be overwritten by a stale completion.
        let (waiter, install_epoch) = {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(StorageState::Uploaded {
                    uncompressed,
                    compressed,
                    algo,
                }) => return Ok((*uncompressed, *compressed, *algo)),
                Some(StorageState::Probed) => return Ok((data_len, None, None)),
                Some(StorageState::InFlight(existing)) => (existing.clone(), None),
                None => {
                    let epoch = self.epoch.load(Ordering::SeqCst);
                    map.put(key.to_string(), StorageState::InFlight(shared.clone()));
                    (shared, Some(epoch))
                }
            }
        };

        // Arm a cleanup guard for the installer (install_epoch is Some):
        // if this task is cancelled during the await below, the guard's
        // Drop removes our stranded InFlight so it can't poison the key
        // (issue #264). Joiners (install_epoch None) get no guard.
        let mut cleanup = install_epoch.map(|_| InFlightGuard {
            known: Arc::clone(&self.known),
            key: key.to_string(),
            armed: true,
        });

        let result = waiter.await;

        // The await is the only cancellation point; it completed, so the
        // terminal transition below runs synchronously — disarm the guard.
        if let Some(g) = cleanup.as_mut() {
            g.armed = false;
        }

        // Joiners don't install — the creator's gated install is the single
        // authority for the terminal cache fact.
        let Some(install_epoch) = install_epoch else {
            return match result {
                Ok(outcome) => Ok((outcome.uncompressed, outcome.compressed, outcome.algo)),
                Err(arc_err) => Err(ObjectStoreError::Other(arc_err.to_string())),
            };
        };

        let mut map = self.known.lock().expect("cache mutex poisoned");
        match result {
            Ok(outcome) => {
                // Only memoize if no invalidating op (delete / versioned
                // write) raced our PUT. Otherwise drop our InFlight so the
                // next caller re-checks the authoritative backend.
                if self.epoch.load(Ordering::SeqCst) == install_epoch {
                    map.put(
                        key.to_string(),
                        StorageState::Uploaded {
                            uncompressed: outcome.uncompressed,
                            compressed: outcome.compressed,
                            algo: outcome.algo,
                        },
                    );
                } else if matches!(map.get(key), Some(StorageState::InFlight(_))) {
                    map.pop(key);
                }
                Ok((outcome.uncompressed, outcome.compressed, outcome.algo))
            }
            Err(arc_err) => {
                // Failure does not pollute the cache. Only remove if the
                // entry is still our `InFlight` — if a successful retry
                // by another caller has already overwritten it with
                // `Uploaded`, leave that alone.
                if matches!(map.get(key), Some(StorageState::InFlight(_))) {
                    map.pop(key);
                }
                Err(ObjectStoreError::Other(arc_err.to_string()))
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
            Await(
                Shared<
                    BoxFuture<'static, std::result::Result<UploadOutcome, Arc<ObjectStoreError>>>,
                >,
            ),
            Miss,
        }
        let action = {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(StorageState::Uploaded { uncompressed, .. }) => {
                    Action::ReturnSize(*uncompressed)
                }
                Some(StorageState::InFlight(fut)) => Action::Await(fut.clone()),
                Some(StorageState::Probed) | None => Action::Miss,
            }
        };
        match action {
            Action::ReturnSize(n) => {
                shared_telemetry::record::chunk_storage_cache_hit(&self.name);
                return Ok(n);
            }
            Action::Await(waiter) => {
                shared_telemetry::record::chunk_storage_cache_inflight_coalesced(&self.name);
                return match waiter.await {
                    Ok(o) => Ok(o.uncompressed),
                    Err(arc_err) => Err(ObjectStoreError::Other(arc_err.to_string())),
                };
            }
            Action::Miss => {}
        }

        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        let path_owned = file_path.to_path_buf();
        let name = self.name.clone();
        let upload_fut = async move {
            let started = std::time::Instant::now();
            let res = inner.upload_chunk_zerocopy(&key_owned, &path_owned).await;
            let secs = started.elapsed().as_secs_f64();
            match &res {
                Ok(size) => {
                    shared_telemetry::record::storage_request(&name, "put", "ok", *size, secs)
                }
                Err(_) => {
                    shared_telemetry::record::storage_request(&name, "put", "error", 0, secs)
                }
            }
            res.map(|size| UploadOutcome {
                uncompressed: size,
                compressed: None,
                algo: None,
            })
            .map_err(Arc::new)
        };
        let shared: Shared<
            BoxFuture<'static, std::result::Result<UploadOutcome, Arc<ObjectStoreError>>>,
        > = upload_fut.boxed().shared();

        let (waiter, install_epoch) = {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(StorageState::Uploaded { uncompressed, .. }) => return Ok(*uncompressed),
                Some(StorageState::InFlight(existing)) => (existing.clone(), None),
                // For Probed (no size known) and None: install the singleflight.
                _ => {
                    let epoch = self.epoch.load(Ordering::SeqCst);
                    map.put(key.to_string(), StorageState::InFlight(shared.clone()));
                    (shared, Some(epoch))
                }
            }
        };

        let result = waiter.await;

        let Some(install_epoch) = install_epoch else {
            return match result {
                Ok(outcome) => Ok(outcome.uncompressed),
                Err(arc_err) => Err(ObjectStoreError::Other(arc_err.to_string())),
            };
        };

        let mut map = self.known.lock().expect("cache mutex poisoned");
        match result {
            Ok(outcome) => {
                if self.epoch.load(Ordering::SeqCst) == install_epoch {
                    map.put(
                        key.to_string(),
                        StorageState::Uploaded {
                            uncompressed: outcome.uncompressed,
                            compressed: outcome.compressed,
                            algo: outcome.algo,
                        },
                    );
                } else if matches!(map.get(key), Some(StorageState::InFlight(_))) {
                    map.pop(key);
                }
                Ok(outcome.uncompressed)
            }
            Err(arc_err) => {
                if matches!(map.get(key), Some(StorageState::InFlight(_))) {
                    map.pop(key);
                }
                Err(ObjectStoreError::Other(arc_err.to_string()))
            }
        }
    }

    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        // Every download is a real backend GET (no read caching) — record
        // it (issue #204).
        let started = std::time::Instant::now();
        let res = self.inner.download_chunk(key).await;
        let secs = started.elapsed().as_secs_f64();
        match &res {
            Ok(d) => {
                shared_telemetry::record::storage_request(&self.name, "get", "ok", d.len() as u64, secs)
            }
            Err(_) => shared_telemetry::record::storage_request(&self.name, "get", "error", 0, secs),
        }
        res
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
        // Bump the epoch under the lock so an upload_chunk completion that
        // snapshotted before this write can't reinstall a stale fact.
        self.epoch.fetch_add(1, Ordering::SeqCst);
        map.pop(key);
        result
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        self.inner.download_manifest(key).await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        enum HeadAction {
            Hit,
            Await(
                Shared<
                    BoxFuture<'static, std::result::Result<UploadOutcome, Arc<ObjectStoreError>>>,
                >,
            ),
            Miss,
        }
        let action = {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            match map.get(key) {
                Some(StorageState::Probed | StorageState::Uploaded { .. }) => HeadAction::Hit,
                Some(StorageState::InFlight(fut)) => HeadAction::Await(fut.clone()),
                None => HeadAction::Miss,
            }
        };
        match action {
            HeadAction::Hit => {
                shared_telemetry::record::chunk_storage_cache_hit(&self.name);
                return Ok(true);
            }
            HeadAction::Await(fut) => {
                shared_telemetry::record::chunk_storage_cache_inflight_coalesced(&self.name);
                if fut.await.is_ok() {
                    return Ok(true);
                }
                // Singleflight failed; fall through to real HEAD.
            }
            HeadAction::Miss => {}
        }
        // Miss, or coalesced singleflight failed: real HEAD. Negative
        // results are NOT cached (a co-resident process could upload
        // between our HEAD and the next caller's check). Snapshot the
        // invalidation epoch before the HEAD; if a delete races our probe
        // we must not cache the (now stale) positive result, or a later
        // content-addressed write would skip its PUT and lose data.
        let probe_epoch = self.epoch.load(Ordering::SeqCst);
        // This path is a real backend HEAD (cache hits returned above) —
        // record it (issue #204).
        let started = std::time::Instant::now();
        let head = self.inner.chunk_exists(key).await;
        let secs = started.elapsed().as_secs_f64();
        let outcome = if head.is_ok() { "ok" } else { "error" };
        shared_telemetry::record::storage_request(&self.name, "head", outcome, 0, secs);
        let exists = head?;
        if exists {
            let mut map = self.known.lock().expect("cache mutex poisoned");
            // Don't overwrite an existing InFlight / Uploaded fact; only
            // seed Probed into a vacant slot. `peek` avoids promoting on
            // the no-op branch.
            if self.epoch.load(Ordering::SeqCst) == probe_epoch && map.peek(key).is_none() {
                map.put(key.to_string(), StorageState::Probed);
            }
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
                // Seed only vacant slots — never overwrite InFlight /
                // Uploaded. The LRU bound means a bucket larger than the
                // cap keeps only the most recently seeded keys; the rest
                // are HEAD'd on demand (issue #192).
                if map.peek(&k).is_none() {
                    map.put(k, StorageState::Probed);
                    seeded += 1;
                }
            }
        }
        if seeded > 0 {
            shared_telemetry::record::chunk_storage_cache_warmup_seeded(&self.name, seeded as u64);
        }
        Ok(seeded)
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        let started = std::time::Instant::now();
        let res = self.inner.delete_object(key).await;
        let secs = started.elapsed().as_secs_f64();
        let outcome = if res.is_ok() { "ok" } else { "error" };
        shared_telemetry::record::storage_request(&self.name, "delete", outcome, 0, secs);
        res?;
        let mut map = self.known.lock().expect("cache mutex poisoned");
        // Bump the epoch under the lock so any HEAD/PUT that snapshotted
        // before this delete can't reinstall the now-deleted object as
        // present (issue #134). Whether our remove or a racing completion
        // wins the lock, the epoch mismatch makes the stale install a
        // no-op, leaving the cache to re-probe the backend.
        self.epoch.fetch_add(1, Ordering::SeqCst);
        map.pop(key);
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

    fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
        // Cloned wrappers share the same cache map and inner backend
        // — `Box<dyn ObjectStoreBackend>::clone()` callers must observe the
        // same in-process facts.
        Box::new(Self {
            inner: Arc::clone(&self.inner),
            name: self.name.clone(),
            known: Arc::clone(&self.known),
            epoch: Arc::clone(&self.epoch),
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
        head_delay_ms: AtomicU64,
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
    impl ObjectStoreBackend for MockBackend {
        async fn upload_chunk(
            &self,
            _key: &str,
            data: Vec<u8>,
        ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
            let delay = self.c.upload_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            self.c.puts.fetch_add(1, Ordering::SeqCst);
            if self.c.fail_next_upload.swap(false, Ordering::SeqCst) {
                return Err(ObjectStoreError::Other("mock failure".to_string()));
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
            let delay = self.c.head_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
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

        fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
            Box::new(Self {
                c: Arc::clone(&self.c),
            })
        }
    }

    fn wrap(mock: MockBackend) -> CachingObjectStoreBackend {
        CachingObjectStoreBackend::new(Box::new(mock), "test")
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
                cache.upload_chunk(&key, zeros.to_vec()).await
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
        let first = cache.upload_chunk("k", b"hello".to_vec()).await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);
        let second = cache.upload_chunk("k", b"hello".to_vec()).await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1, "second call is cache hit");
        assert_eq!(first, second, "cached tuple matches first PUT");
    }

    #[tokio::test]
    async fn known_map_is_bounded_and_evicts_lru() {
        // Issue #192: the fact cache must not grow without bound. With a
        // 2-entry cap, inserting a third key evicts the least-recently-used
        // one; re-touching the evicted key misses and re-PUTs, while the
        // still-cached key stays a hit.
        let (mock, c) = MockBackend::new();
        let cache = CachingObjectStoreBackend::with_capacity(Box::new(mock), "test", 2);
        cache.upload_chunk("k1", b"a".to_vec()).await.unwrap();
        cache.upload_chunk("k2", b"b".to_vec()).await.unwrap();
        cache.upload_chunk("k3", b"c".to_vec()).await.unwrap(); // evicts k1 (LRU)
        assert_eq!(c.puts.load(Ordering::SeqCst), 3);

        // k3 is still cached — a re-upload coalesces to a hit, no new PUT.
        cache.upload_chunk("k3", b"c".to_vec()).await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 3, "k3 still cached");

        // k1 was evicted — its re-upload misses and PUTs again.
        cache.upload_chunk("k1", b"a".to_vec()).await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 4, "evicted k1 re-uploads");
    }

    #[tokio::test]
    async fn failure_does_not_pollute_cache() {
        let (mock, c) = MockBackend::new();
        c.fail_next_upload.store(true, Ordering::SeqCst);
        let cache = wrap(mock);
        assert!(cache.upload_chunk("k", b"hello".to_vec()).await.is_err());
        // Backend's `fail_next_upload` was a swap-once; subsequent call
        // succeeds. The cache MUST have cleared its failed entry,
        // otherwise puts.load() would still be 1 here.
        assert!(cache.upload_chunk("k", b"hello".to_vec()).await.is_ok());
        assert_eq!(c.puts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn delete_invalidates_entry() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        cache.upload_chunk("k", b"hello".to_vec()).await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);
        cache.delete_object("k").await.unwrap();
        cache.upload_chunk("k", b"hello".to_vec()).await.unwrap();
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
            .upload_chunk("chunks/aa/bb/x.dat", b"data".to_vec())
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
                cache.upload_chunk("k", b"hello".to_vec()).await
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
        cache.upload_chunk("k", b"hello".to_vec()).await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 2);
    }

    /// Issue #264: a cancelled installer (timeout/abort mid-upload) must
    /// not strand an InFlight whose memoized failure permanently poisons
    /// the key — subsequent uploads must reach the backend, not return a
    /// frozen error.
    #[tokio::test]
    async fn cancelled_installer_does_not_poison_key() {
        let (mock, c) = MockBackend::new();
        c.upload_delay_ms.store(200, Ordering::SeqCst);
        c.fail_next_upload.store(true, Ordering::SeqCst);
        let cache = wrap(mock);

        // Call 1: cancel the installer mid-upload (50 ms < 200 ms delay),
        // before its backend PUT increments `puts` or fails.
        let r =
            tokio::time::timeout(Duration::from_millis(50), cache.upload_chunk("k", b"data".to_vec())).await;
        assert!(r.is_err(), "first call should time out (installer cancelled)");

        // Call 2: the key is not poisoned — the cleaned-up InFlight forces
        // a fresh backend PUT, which sleeps and then fails (fail_next).
        let r2 = cache.upload_chunk("k", b"data".to_vec()).await;
        assert!(r2.is_err(), "call 2 reaches the backend and fails");

        // Call 3: still not poisoned — another fresh PUT, now succeeding.
        let r3 = cache.upload_chunk("k", b"data".to_vec()).await;
        assert!(
            r3.is_ok(),
            "call 3 must reach the backend (not a memoized error)"
        );

        // A poisoned key would have served calls 2/3 from the stranded
        // InFlight without touching the backend (puts would stay 0/1).
        assert!(
            c.puts.load(Ordering::SeqCst) >= 2,
            "post-cancel uploads must reach the backend, got puts={}",
            c.puts.load(Ordering::SeqCst)
        );
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
    async fn delete_racing_positive_head_is_not_cached() {
        // Reproduces issue #134: a HEAD reaches the backend while the
        // object still exists; a concurrent delete removes it; the stale
        // positive HEAD must NOT install a Probed entry (which would later
        // let a content-addressed write skip its PUT and lose data).
        let (mock, c) = MockBackend::new();
        c.head_returns.store(true, Ordering::SeqCst);
        c.head_delay_ms.store(40, Ordering::SeqCst);
        let cache = Arc::new(wrap(mock));

        let probe = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.chunk_exists("k").await })
        };
        // Let the HEAD start (and block in its 40 ms sleep), then delete.
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.delete_object("k").await.unwrap();

        assert!(probe.await.unwrap().unwrap(), "HEAD observed the object");
        assert_eq!(c.heads.load(Ordering::SeqCst), 1);

        // The stale positive must not have been cached: a follow-up
        // chunk_exists fires a fresh HEAD rather than returning a cached
        // Probed.
        c.head_returns.store(false, Ordering::SeqCst);
        c.head_delay_ms.store(0, Ordering::SeqCst);
        assert!(!cache.chunk_exists("k").await.unwrap());
        assert_eq!(
            c.heads.load(Ordering::SeqCst),
            2,
            "delete-racing HEAD must not have cached a Probed entry"
        );
    }

    #[tokio::test]
    async fn delete_racing_upload_completion_is_not_cached() {
        // Reproduces the upload-side half of #134: a PUT is in flight when
        // a delete removes the key; the PUT completion must NOT install a
        // stale Uploaded entry, or the next upload of the same content
        // would be skipped.
        let (mock, c) = MockBackend::new();
        c.upload_delay_ms.store(40, Ordering::SeqCst);
        let cache = Arc::new(wrap(mock));

        let put = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.upload_chunk("k", b"hello".to_vec()).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        cache.delete_object("k").await.unwrap();

        put.await.unwrap().unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);

        // The racing delete must have prevented the Uploaded install, so a
        // follow-up upload of the same content fires a real PUT.
        c.upload_delay_ms.store(0, Ordering::SeqCst);
        cache.upload_chunk("k", b"hello".to_vec()).await.unwrap();
        assert_eq!(
            c.puts.load(Ordering::SeqCst),
            2,
            "delete-racing PUT completion must not have cached Uploaded"
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
        cache.upload_chunk("k", b"hi".to_vec()).await.unwrap();
        let cloned: Box<dyn ObjectStoreBackend> = cache.clone_box();
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
        cache.upload_chunk("k", b"v1".to_vec()).await.unwrap();
        assert_eq!(c.puts.load(Ordering::SeqCst), 1);
        // Versioned overwrite must clear the entry.
        cache.upload_versioned("k", b"v2").await.unwrap();
        assert_eq!(c.versioned_puts.load(Ordering::SeqCst), 1);
        // Subsequent upload_chunk must NOT see the stale Uploaded
        // entry — it must fire a real PUT.
        cache.upload_chunk("k", b"v3".to_vec()).await.unwrap();
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
        impl ObjectStoreBackend for DefaultMock {
            async fn upload_chunk(
                &self,
                _key: &str,
                data: Vec<u8>,
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
            fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
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
        // CachingObjectStoreBackend's singleflight collapses to one inner call.
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
        impl ObjectStoreBackend for ErrZeroMock {
            async fn upload_chunk(
                &self,
                _: &str,
                _: Vec<u8>,
            ) -> Result<(u64, Option<u64>, Option<CompressionAlgo>)> {
                Ok((0, None, None))
            }
            async fn upload_chunk_zerocopy(&self, _: &str, _: &Path) -> Result<u64> {
                Err(ObjectStoreError::Other("zerocopy boom".into()))
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
            fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
                Box::new(Self)
            }
        }
        let cache = CachingObjectStoreBackend::new(Box::new(ErrZeroMock), "test");
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
    /// {set,get}_object_legal_hold) plus the Debug impl for StorageState.
    /// One call per method — no caching semantics to assert.
    #[tokio::test]
    async fn trivial_passthroughs_delegate_to_inner() {
        let (mock, c) = MockBackend::new();
        let cache = wrap(mock);
        // download_chunks_parallel
        let _ = cache
            .download_chunks_parallel(&["a".to_string(), "b".to_string(), "c".to_string()])
            .await
            .unwrap();
        assert_eq!(c.gets.load(Ordering::SeqCst), 3);
        // upload_manifest + download_manifest
        cache
            .upload_manifest("m", "{\"v\":1}")
            .await
            .expect("manifest upload");
        let _ = cache
            .download_manifest("m")
            .await
            .expect("manifest download");
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
        // Debug printout exercises StorageState::Probed/Uploaded/InFlight arms.
        cache.upload_chunk("u", b"x".to_vec()).await.unwrap(); // Uploaded
        c.head_returns.store(true, Ordering::SeqCst);
        assert!(cache.chunk_exists("p").await.unwrap()); // Probed
        let dbg = format!("{:?}", cache);
        assert!(dbg.contains("CachingObjectStoreBackend"));
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
