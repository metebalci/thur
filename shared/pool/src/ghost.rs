// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-backend "ghost list" — bounded ring of recently-evicted chunk
//! hashes. Measurement-only, not used for cache replacement.
//!
//! On every chunk unlink the disk-cache eviction worker calls
//! [`GhostList::insert`]. On every cache miss that falls through to a
//! backend GET, the read path calls [`GhostList::lookup`]; if the
//! chunk had been recently evicted, the returned age (`now -
//! evicted_at`) is recorded into the
//! `cache_miss_after_eviction_seconds` histogram. The histogram drives
//! operator sizing decisions on `disk_cache.size_gb` — sub-minute mass
//! means the cache is undersized by that window.
//!
//! Capacity bounds the ring; on overflow the oldest entry is dropped.
//! `capacity == 0` disables the ring entirely (every API call is a
//! no-op, returns `None`).
//!
//! Lock granularity is one mutex per ghost list. Eviction and miss
//! events are both low-rate compared to the I/O they accompany, so
//! contention is not a concern at the rates we care about.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// 32-byte BLAKE3 chunk hash — same shape used throughout the chunk pool.
pub type GhostHash = [u8; 32];

/// Per-backend bounded ring of `(hash, evicted_at_unix)` entries.
pub struct GhostList {
    backend: String,
    capacity: usize,
    inner: Mutex<GhostInner>,
}

struct GhostInner {
    ring: VecDeque<(GhostHash, u64)>,
    index: HashMap<GhostHash, u64>,
}

impl GhostList {
    pub fn new(backend: impl Into<String>, capacity: usize) -> Self {
        let init_cap = capacity.min(1024);
        Self {
            backend: backend.into(),
            capacity,
            inner: Mutex::new(GhostInner {
                ring: VecDeque::with_capacity(init_cap),
                index: HashMap::with_capacity(init_cap),
            }),
        }
    }

    pub fn backend(&self) -> &str {
        &self.backend
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Record that `hash` was evicted at `evicted_at_unix`. Drops the
    /// oldest entry from the ring if at capacity. No-op when
    /// `capacity == 0`.
    pub fn insert(&self, hash: GhostHash, evicted_at_unix: u64) {
        if self.capacity == 0 {
            return;
        }
        let mut g = self.inner.lock().expect("ghost list mutex poisoned");
        g.index.insert(hash, evicted_at_unix);
        g.ring.push_back((hash, evicted_at_unix));
        while g.ring.len() > self.capacity {
            if let Some((old_hash, old_ts)) = g.ring.pop_front() {
                // Only drop the index entry if it still points at the
                // popped (hash, ts) pair. If a later insert refreshed
                // this hash, the index now points at the newer ts; we
                // must leave it alone so the live entry survives.
                if let Some(&cur) = g.index.get(&old_hash) {
                    if cur == old_ts {
                        g.index.remove(&old_hash);
                    }
                }
            }
        }
    }

    /// Returns `Some(age_seconds)` if `hash` is in the ring, where
    /// `age = now_unix_secs - evicted_at_unix` (saturating). `None`
    /// otherwise or when `capacity == 0`.
    pub fn lookup(&self, hash: &GhostHash, now_unix_secs: u64) -> Option<u64> {
        if self.capacity == 0 {
            return None;
        }
        let g = self.inner.lock().expect("ghost list mutex poisoned");
        let evicted_at = *g.index.get(hash)?;
        Some(now_unix_secs.saturating_sub(evicted_at))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(byte: u8) -> GhostHash {
        [byte; 32]
    }

    #[test]
    fn insert_then_lookup_returns_age() {
        let gl = GhostList::new("backendA", 16);
        gl.insert(h(1), 1000);
        assert_eq!(gl.lookup(&h(1), 1042), Some(42));
    }

    #[test]
    fn lookup_without_insert_is_none() {
        let gl = GhostList::new("backendA", 16);
        assert_eq!(gl.lookup(&h(7), 5000), None);
    }

    #[test]
    fn ring_overflow_drops_oldest() {
        let gl = GhostList::new("backendA", 3);
        gl.insert(h(1), 100);
        gl.insert(h(2), 101);
        gl.insert(h(3), 102);
        gl.insert(h(4), 103);
        assert_eq!(gl.lookup(&h(1), 200), None, "oldest should be dropped");
        assert_eq!(gl.lookup(&h(2), 200), Some(99));
        assert_eq!(gl.lookup(&h(3), 200), Some(98));
        assert_eq!(gl.lookup(&h(4), 200), Some(97));
        assert_eq!(gl.len(), 3);
    }

    #[test]
    fn capacity_zero_is_noop() {
        let gl = GhostList::new("backendA", 0);
        gl.insert(h(1), 100);
        assert_eq!(gl.lookup(&h(1), 200), None);
        assert_eq!(gl.len(), 0);
    }

    #[test]
    fn reinsert_refreshes_timestamp_and_survives_overflow() {
        let gl = GhostList::new("backendA", 3);
        gl.insert(h(1), 100);
        gl.insert(h(2), 101);
        gl.insert(h(3), 102);
        // Refresh h(1) — its index entry should now point at ts=103.
        gl.insert(h(1), 103);
        // Push two more so the stale ring entry for h(1) gets popped,
        // but the index must keep the refreshed value.
        gl.insert(h(4), 104);
        gl.insert(h(5), 105);
        assert_eq!(
            gl.lookup(&h(1), 200),
            Some(97),
            "refresh should survive stale-ring-pop"
        );
    }

    #[test]
    fn age_saturates_on_clock_skew() {
        let gl = GhostList::new("backendA", 4);
        gl.insert(h(1), 1000);
        // `now` earlier than `evicted_at` (clock went backwards).
        assert_eq!(gl.lookup(&h(1), 500), Some(0));
    }

    #[test]
    fn backend_label_round_trips() {
        let gl = GhostList::new("cold-azure", 8);
        assert_eq!(gl.backend(), "cold-azure");
        assert_eq!(gl.capacity(), 8);
    }
}
