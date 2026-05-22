// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-backend chunk-pool budget + upload backpressure gate.
//!
//! Pure byte accounting — no product-specific semantics. The chunk-seal
//! path on either product calls [`PoolBudget::try_reserve`] before
//! sealing a staged chunk into the pool; if the backend's slice is at
//! its hard cap (or the underlying filesystem is below
//! `disk_cache.disk_free_min_gb`), the call blocks on the internal
//! condvar until the eviction worker releases bytes — or returns
//! [`BackpressureError`] after `deadline`.
//!
//! One `PoolBudget` is constructed per `cloud.backends` entry at daemon
//! startup. The eviction worker's `release` calls wake any
//! `try_reserve` waiters. Shutdown / `Drop`-time flushes bypass the
//! gate via [`PoolBudget::force_reserve`] — bounded overshoot is
//! preferred over losing data.
//!
//! ## Where this used to live
//!
//! Originally lived at `core/ssc/src/disk_cache.rs:589-857` (tape
//! side only). Lifted into `shared-pool` so both products use the
//! same gate — `BackpressureError` is product-agnostic, both
//! products' error enums add a `Backpressured(BackpressureError)`
//! variant via `#[from]`. The tape-aware `refresh_from_disk` helper
//! that used to live alongside `PoolBudget` stays in the tape side
//! (`core/ssc/src/disk_cache.rs`) and now calls
//! [`PoolBudget::set_current_bytes`]. The block side has its own
//! parallel walker.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::warn;

/// Upload-backpressure timeout: a chunk-seal would have pushed the
/// local pool past its hard cap (or below the
/// `disk_cache.disk_free_min_gb` floor) and waiting on
/// `upload.backpressure_max_wait_seconds` did not free enough
/// headroom. Mapped at the SCSI layer to NOT READY + ASC/ASCQ
/// 0x04/0x07 ("LOGICAL UNIT NOT READY, OPERATION IN PROGRESS");
/// backup software (tar/mt, NetBackup, Veeam, Bacula) treats that
/// as transient and retries.
#[derive(Debug, Error)]
#[error(
    "upload backpressure timed out: pool {pool_used_bytes}/{pool_cap_bytes} bytes, waited {waited_secs}s"
)]
pub struct BackpressureError {
    pub pool_used_bytes: u64,
    pub pool_cap_bytes: u64,
    pub waited_secs: u64,
}

/// Per-backend hard cap on local pool occupancy, with sync
/// `Mutex`+`Condvar` semantics so the chunk-seal path can block
/// without forcing the whole write path async. One `PoolBudget` is
/// constructed per `cloud.backends` entry at daemon startup; the
/// eviction / upload worker calls `release` whenever an evicted chunk
/// frees pool bytes (and a chunk-seal that hits the cap will wake from
/// its condvar wait).
///
/// Semantics:
///   * `try_reserve(chunk_size, deadline)` blocks if reserving would
///     push us past the cap or take filesystem free space below
///     `disk_free_min_bytes`. Wakes on `release` calls; returns
///     [`BackpressureError`] after `deadline`.
///   * `release(bytes)` is called from the eviction path after a pool
///     file is unlinked. Decrements `current_bytes` and signals all
///     waiters.
///   * `force_reserve(bytes)` bypasses both gates — reserved for
///     `Cartridge::Drop` / `PageCache::flush_all` flushes where
///     surfacing `Backpressured` would mean dropping data on the floor.
///     Bounded overshoot ≤ chunk_max per concurrent unload.
///
/// Numbers are measured in bytes throughout; the config knob
/// (`disk_cache.size_gb`) is in GB but converted at construction.
pub struct PoolBudget {
    /// Backend name (matches the `cloud.backends` entry). Used as the
    /// `backend` attribute on every `<product>_pool_*` instrument
    /// emitted from this budget. Empty string for the unbounded
    /// CLI/test budget — those samples are dropped at the global
    /// no-telemetry layer anyway.
    backend: String,
    /// Hard cap in bytes for this backend's slice of the chunk pool.
    /// Daemon supplies it from either the per-entry `disk_cache_size_gb`
    /// override on the `cloud-backends.json` entry or — if no override
    /// is set — the YAML `disk_cache.size_gb` default.
    ///
    /// Atomic so the eviction worker can call [`PoolBudget::set_cap_bytes`]
    /// on every tick when the operator picked `size_gb: auto` — external
    /// disk pressure shrinks the filesystem's free space and the cap
    /// follows, all without rebuilding the budget (which would orphan
    /// in-flight `try_reserve` waiters).
    cap_bytes: AtomicU64,
    /// Reserved-for-this-backend disk-free floor; `try_reserve` also
    /// blocks if `statvfs(data_dir).free < disk_free_min_bytes`.
    disk_free_min_bytes: u64,
    /// Soft watermark (fraction of cap, 0.0–1.0). When `current_bytes`
    /// crosses this, `over_soft_watermark` returns true so callers can
    /// log/observe pressure before the hard cap.
    soft_watermark_frac: f64,
    /// Path used for `statvfs(2)` on the disk-free check. Typically
    /// `data_dir`.
    data_dir: PathBuf,
    /// Bytes currently reserved in the pool (sealed chunks the daemon
    /// believes are on disk under this backend). Mutex-protected so
    /// reservation and release can race safely; condvar is paired
    /// with this mutex for waiters.
    state: Mutex<u64>,
    cv: Condvar,
}

impl PoolBudget {
    /// Build a fresh budget. `cap_bytes == 0` is treated as "no
    /// gate" (every reserve succeeds immediately) — useful for tests
    /// and for the standalone-CLI path where no daemon is up.
    pub fn new(
        data_dir: PathBuf,
        cap_bytes: u64,
        disk_free_min_bytes: u64,
        soft_watermark_pct: u8,
    ) -> Self {
        Self::with_backend(
            String::new(),
            data_dir,
            cap_bytes,
            disk_free_min_bytes,
            soft_watermark_pct,
        )
    }

    /// Same as [`PoolBudget::new`] but tags the budget with the
    /// owning backend's name so its `<product>_pool_*` metric samples
    /// carry the right `backend` label. The daemon uses this; tests /
    /// CLI use [`PoolBudget::new`] / [`PoolBudget::unbounded`].
    pub fn with_backend(
        backend: String,
        data_dir: PathBuf,
        cap_bytes: u64,
        disk_free_min_bytes: u64,
        soft_watermark_pct: u8,
    ) -> Self {
        let pct = soft_watermark_pct.clamp(1, 100) as f64 / 100.0;
        if !backend.is_empty() {
            shared_telemetry::record::pool_cap(&backend, cap_bytes);
        }
        Self {
            backend,
            cap_bytes: AtomicU64::new(cap_bytes),
            disk_free_min_bytes,
            soft_watermark_frac: pct,
            data_dir,
            state: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    /// No-op gate (every reserve succeeds). Lives on this type so
    /// non-daemon callers (CLI tools, tests) can construct a writer
    /// without wiring real budget bookkeeping.
    pub fn unbounded(data_dir: PathBuf) -> Self {
        Self::new(data_dir, 0, 0, 80)
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes.load(Ordering::Relaxed)
    }

    /// Overwrite the hard cap. Used by the eviction worker on every
    /// tick when the operator picked `size_gb: auto`: external disk
    /// pressure shrinks the filesystem, the auto resolver derives a
    /// smaller cap, and `set_cap_bytes(new)` lets the next
    /// `try_reserve` see the tighter ceiling without rebuilding the
    /// budget. Wakes all `try_reserve` waiters so a *grown* cap
    /// immediately admits a parked reservation that would have fit.
    pub fn set_cap_bytes(&self, new_cap: u64) {
        let prev = self.cap_bytes.swap(new_cap, Ordering::Relaxed);
        if prev == new_cap {
            return;
        }
        if !self.backend.is_empty() {
            shared_telemetry::record::pool_cap(&self.backend, new_cap);
        }
        // Wake every waiter — the parked reservation may now fit
        // (cap grew) or the operator may want the gauge / dashboards
        // to re-evaluate (cap shrank).
        let _g = self.state.lock().expect("PoolBudget mutex poisoned");
        self.cv.notify_all();
    }

    pub fn current_bytes(&self) -> u64 {
        *self.state.lock().expect("PoolBudget mutex poisoned")
    }

    /// Backend name this budget was tagged with at construction.
    /// Empty for unbounded / test budgets.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Data dir used for the `statvfs(2)` disk-free probe.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn over_soft_watermark(&self) -> bool {
        let cap = self.cap_bytes();
        if cap == 0 {
            return false;
        }
        let used = self.current_bytes() as f64;
        used / cap as f64 >= self.soft_watermark_frac
    }

    /// Try to reserve `bytes` for a soon-to-seal chunk, blocking up to
    /// `deadline` while waiting for upload-completion to free
    /// headroom. Returns `Ok(())` once the reservation lands; returns
    /// [`BackpressureError`] on timeout. Idempotent vs. `release` —
    /// the seal path must call `release(bytes)` if the chunk is
    /// deduped against an existing pool file (and therefore doesn't
    /// actually consume new disk).
    //
    // `expect` on the mutex/condvar is acceptable here: a poisoned
    // budget mutex is non-recoverable (some other holder panicked
    // mid-update), and the rest of this impl panics the same way.
    #[allow(clippy::unwrap_in_result)]
    pub fn try_reserve(&self, bytes: u64, deadline: Duration) -> Result<(), BackpressureError> {
        // No-gate fast path — used by the unbounded budget tests/CLI
        // construct. Re-read the atomic each time because the eviction
        // worker may rewrite cap when running in `size_gb: auto` mode.
        let cap = self.cap_bytes();
        if cap == 0 && self.disk_free_min_bytes == 0 {
            let mut g = self.state.lock().expect("PoolBudget mutex poisoned");
            *g += bytes;
            self.report_used(*g);
            return Ok(());
        }

        let started = Instant::now();
        let mut state = self.state.lock().expect("PoolBudget mutex poisoned");
        loop {
            let cap = self.cap_bytes();
            let pool_room_ok = cap == 0 || *state + bytes <= cap;
            let disk_room_ok = self.disk_free_min_bytes == 0
                || disk_free_bytes(&self.data_dir).unwrap_or(u64::MAX) >= self.disk_free_min_bytes;
            if pool_room_ok && disk_room_ok {
                *state += bytes;
                let used = *state;
                let waited = started.elapsed();
                if waited > Duration::from_millis(0) && !self.backend.is_empty() {
                    shared_telemetry::record::pool_backpressure_wait(
                        &self.backend,
                        waited.as_secs_f64(),
                    );
                }
                self.report_used(used);
                return Ok(());
            }
            // No room — log once at warn level, then wait for either
            // a release (cv signal) or the deadline.
            warn!(
                "Upload backpressure waiting: pool {}/{} bytes, requesting {} more (disk-room {})",
                *state, cap, bytes, disk_room_ok
            );
            let elapsed = started.elapsed();
            if elapsed >= deadline {
                let waited_secs = elapsed.as_secs();
                if !self.backend.is_empty() {
                    shared_telemetry::record::pool_backpressure_wait(
                        &self.backend,
                        elapsed.as_secs_f64(),
                    );
                }
                if !self.backend.is_empty() {
                    shared_alerting::record::disk_cache_backpressure_timeout(
                        &self.backend,
                        waited_secs,
                    );
                }
                return Err(BackpressureError {
                    pool_used_bytes: *state,
                    pool_cap_bytes: cap,
                    waited_secs,
                });
            }
            let remaining = deadline - elapsed;
            let (next_state, wait_outcome) = self
                .cv
                .wait_timeout(state, remaining)
                .expect("PoolBudget mutex poisoned");
            state = next_state;
            if wait_outcome.timed_out() {
                let pool_used_bytes = *state;
                if !self.backend.is_empty() {
                    shared_telemetry::record::pool_backpressure_wait(
                        &self.backend,
                        deadline.as_secs_f64(),
                    );
                    shared_alerting::record::disk_cache_backpressure_timeout(
                        &self.backend,
                        deadline.as_secs(),
                    );
                }
                return Err(BackpressureError {
                    pool_used_bytes,
                    pool_cap_bytes: self.cap_bytes(),
                    waited_secs: deadline.as_secs(),
                });
            }
            // Otherwise: spurious wake or release-signal; loop and
            // re-check.
        }
    }

    /// Reserve `bytes` *bypassing* the cap and the disk-free floor.
    /// Reserved for `Cartridge::Drop` / `PageCache::flush_all`-time
    /// flushes where returning `Backpressured` would mean dropping
    /// data on the floor. Bounded overshoot: ≤ chunk_max per
    /// concurrent unload.
    pub fn force_reserve(&self, bytes: u64) {
        let mut state = self.state.lock().expect("PoolBudget mutex poisoned");
        *state += bytes;
        let used = *state;
        drop(state);
        self.report_used(used);
    }

    /// Free `bytes` after the eviction path unlinked a pool file (or
    /// after the seal path discovered a dedup hit and the staging
    /// reservation never materialized into a new file). Wakes all
    /// `try_reserve` waiters so the next reservation can proceed.
    pub fn release(&self, bytes: u64) {
        let mut state = self.state.lock().expect("PoolBudget mutex poisoned");
        *state = state.saturating_sub(bytes);
        let used = *state;
        self.cv.notify_all();
        drop(state);
        self.report_used(used);
    }

    /// Overwrite `current_bytes` with the supplied total. Called by
    /// the per-product startup walker once it's added up all on-disk
    /// pool bytes (sealed chunks under `<data_dir>/chunks/<backend>/`
    /// plus any `DedupScope::Local` namespaces this backend hosts).
    /// Wakes all `try_reserve` waiters so a startup recount that
    /// drops `current_bytes` immediately frees backpressure quota.
    pub fn set_current_bytes(&self, total: u64) {
        let mut state = self.state.lock().expect("PoolBudget mutex poisoned");
        *state = total;
        self.cv.notify_all();
        drop(state);
        self.report_used(total);
    }

    fn report_used(&self, used: u64) {
        if !self.backend.is_empty() {
            shared_telemetry::record::pool_used(&self.backend, used);
        }
    }
}

/// Free bytes available on the filesystem holding `path`. None on
/// failure (callers treat that as "don't gate" so a transient
/// statvfs failure can't lock the daemon up — fall back to the
/// pool-only cap).
fn disk_free_bytes(path: &Path) -> Option<u64> {
    fs2::available_space(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn pool_budget_unbounded_admits_every_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let b = PoolBudget::unbounded(tmp.path().to_path_buf());
        b.try_reserve(10 * 1024 * 1024 * 1024, Duration::from_secs(1))
            .expect("unbounded budget should always admit");
        assert_eq!(b.current_bytes(), 10 * 1024 * 1024 * 1024);
        b.release(10 * 1024 * 1024 * 1024);
        assert_eq!(b.current_bytes(), 0);
    }

    #[test]
    fn pool_budget_blocks_then_admits_after_release() {
        let tmp = tempfile::tempdir().unwrap();
        let b = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 1024, 0, 80));
        // Fill the budget.
        b.try_reserve(1024, Duration::from_secs(1)).unwrap();
        assert_eq!(b.current_bytes(), 1024);

        // Background thread will release after a short delay.
        let b_bg = b.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            b_bg.release(512);
        });

        // Foreground reserves must block until the release lands.
        let started = std::time::Instant::now();
        b.try_reserve(512, Duration::from_secs(2))
            .expect("should admit after background release");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(80),
            "should have waited ~100ms, waited {:?}",
            waited
        );
        assert_eq!(b.current_bytes(), 1024);
    }

    #[test]
    fn pool_budget_times_out_when_no_release_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let b = PoolBudget::new(tmp.path().to_path_buf(), 1024, 0, 80);
        // Fill the budget.
        b.try_reserve(1024, Duration::from_secs(1)).unwrap();
        // No background release; second reserve must time out.
        let started = std::time::Instant::now();
        let err = b
            .try_reserve(1, Duration::from_millis(150))
            .expect_err("should have timed out");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(140),
            "expected ~150ms wait, got {:?}",
            waited
        );
        assert_eq!(err.pool_used_bytes, 1024);
        assert_eq!(err.pool_cap_bytes, 1024);
    }

    #[test]
    fn pool_budget_force_reserve_bypasses_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let b = PoolBudget::new(tmp.path().to_path_buf(), 1024, 0, 80);
        // Already full.
        b.try_reserve(1024, Duration::from_secs(1)).unwrap();
        // force_reserve must succeed and push us over.
        b.force_reserve(2048);
        assert_eq!(b.current_bytes(), 3072);
    }

    #[test]
    fn pool_budget_soft_watermark_fires_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let b = PoolBudget::new(tmp.path().to_path_buf(), 1000, 0, 80);
        assert!(!b.over_soft_watermark());
        b.try_reserve(800, Duration::from_secs(1)).unwrap();
        assert!(b.over_soft_watermark());
        b.release(50);
        assert!(!b.over_soft_watermark());
    }

    #[test]
    fn set_current_bytes_overwrites_and_wakes_waiters() {
        let tmp = tempfile::tempdir().unwrap();
        let b = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 1024, 0, 80));
        b.try_reserve(1024, Duration::from_secs(1)).unwrap();
        assert_eq!(b.current_bytes(), 1024);

        // A waiter parked at the cap.
        let b_w = b.clone();
        let handle = std::thread::spawn(move || {
            b_w.try_reserve(256, Duration::from_secs(2))
                .expect("should admit after startup recount")
        });

        // Background thread overwrites current_bytes to a smaller
        // value (simulating a startup walk that discovered fewer
        // bytes than the in-memory state).
        std::thread::sleep(Duration::from_millis(50));
        b.set_current_bytes(0);
        handle.join().unwrap();
        assert_eq!(b.current_bytes(), 256);
    }

    #[test]
    fn set_cap_bytes_grows_and_wakes_waiters() {
        let tmp = tempfile::tempdir().unwrap();
        let b = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 1024, 0, 80));
        b.try_reserve(1024, Duration::from_secs(1)).unwrap();
        assert_eq!(b.current_bytes(), 1024);
        assert_eq!(b.cap_bytes(), 1024);

        // Waiter blocked at the cap.
        let b_w = b.clone();
        let handle = std::thread::spawn(move || {
            b_w.try_reserve(256, Duration::from_secs(2))
                .expect("should admit after cap grows")
        });
        std::thread::sleep(Duration::from_millis(50));

        // Grow the cap — waiter should wake and reserve.
        b.set_cap_bytes(4096);
        assert_eq!(b.cap_bytes(), 4096);
        handle.join().unwrap();
        assert_eq!(b.current_bytes(), 1024 + 256);
    }

    #[test]
    fn set_cap_bytes_shrink_takes_effect_on_next_reserve() {
        let tmp = tempfile::tempdir().unwrap();
        let b = PoolBudget::new(tmp.path().to_path_buf(), 4096, 0, 80);
        b.try_reserve(1024, Duration::from_secs(1)).unwrap();
        // Shrink below current usage; next reservation must block until
        // a release happens.
        b.set_cap_bytes(512);
        assert_eq!(b.cap_bytes(), 512);
        let err = b
            .try_reserve(1, Duration::from_millis(50))
            .expect_err("over-cap reservation must time out");
        assert_eq!(err.pool_cap_bytes, 512);
    }

    #[test]
    fn backpressure_error_format_carries_counters() {
        let err = BackpressureError {
            pool_used_bytes: 100,
            pool_cap_bytes: 80,
            waited_secs: 7,
        };
        let s = err.to_string();
        assert!(s.contains("100/80"));
        assert!(s.contains("7s"));
    }
}
