// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Generic per-event rate-limiter for audit emissions.
//!
//! Bounds flood emissions on host-driven failure paths (CHAP login
//! failures, MOVE MEDIUM refusals) where a misconfigured initiator
//! can otherwise spam the audit chain. The first event in a window
//! is emitted normally; subsequent events with the same key within
//! the window are silently counted; after the window expires (or at
//! daemon shutdown), one rollup entry is emitted carrying the
//! suppressed count.
//!
//! Opt-in: applied per-site at audit emission time. Lifecycle /
//! one-shot events (`cartridge.create`, `daemon.start`) bypass it
//! entirely — they carry no flood risk and the chain narrative needs
//! every one.
//!
//! Failure mode is **fail-open**: if the in-memory mutex is
//! poisoned, [`decide`](AuditRateLimiter::decide) returns
//! [`Decision::Emit`] so events are never silently dropped. A
//! poisoned audit-rate-limiter biases toward chain noise, not
//! chain blindness.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::audit::AuditActor;

#[derive(Debug)]
pub struct AuditRateLimiter {
    window: Duration,
    inner: Mutex<HashMap<String, ActiveWindow>>,
}

#[derive(Debug)]
struct ActiveWindow {
    first_seen: Instant,
    suppressed: u64,
    op: String,
    actor: AuditActor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Caller emits the event normally.
    Emit,
    /// Caller skips the event; the limiter has counted it for the
    /// rollup.
    Suppress,
}

#[derive(Debug, Clone)]
pub struct Rollup {
    pub op: String,
    pub actor: AuditActor,
    pub key: String,
    pub suppressed_count: u64,
    pub window_seconds: u64,
}

impl AuditRateLimiter {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub const fn window(&self) -> Duration {
        self.window
    }

    /// Caller passes a stable key (e.g. `<op>:<peer>:<reason>`) plus
    /// the op and actor that would normally be passed to
    /// `AuditLog::append`. The first call for a key returns
    /// [`Decision::Emit`]; further calls within the window return
    /// [`Decision::Suppress`] until [`flush_expired`] drains the
    /// window.
    ///
    /// The second element is a *displaced* rollup: when a fresh event
    /// arrives after the window expired but before the flush task drained
    /// it, the prior window's suppressed count would otherwise be reset
    /// to zero and lost. Under a steady flood — exactly when the rollup
    /// matters most — the next event almost always beats the 10 s flush
    /// tick, so that loss is the common case, not flush-task starvation
    /// (issue #202). The caller MUST emit this rollup (via
    /// [`AuditChannel::append_rollup`](crate::AuditChannel::append_rollup))
    /// in addition to acting on the [`Decision`].
    pub fn decide(&self, key: String, op: &str, actor: &AuditActor) -> (Decision, Option<Rollup>) {
        let Ok(mut map) = self.inner.lock() else {
            return (Decision::Emit, None);
        };
        let now = Instant::now();
        match map.get_mut(&key) {
            None => {
                map.insert(
                    key,
                    ActiveWindow {
                        first_seen: now,
                        suppressed: 0,
                        op: op.to_string(),
                        actor: actor.clone(),
                    },
                );
                (Decision::Emit, None)
            }
            Some(w) if now.duration_since(w.first_seen) >= self.window => {
                // Window expired but the flush task hasn't run yet.
                // Capture the prior window's suppressed count as a
                // displaced rollup so it isn't lost when we re-arm the
                // window for this event (issue #202).
                let displaced = (w.suppressed > 0).then(|| Rollup {
                    op: w.op.clone(),
                    actor: w.actor.clone(),
                    key: key.clone(),
                    suppressed_count: w.suppressed,
                    window_seconds: self.window.as_secs(),
                });
                w.first_seen = now;
                w.suppressed = 0;
                w.op = op.to_string();
                w.actor = actor.clone();
                (Decision::Emit, displaced)
            }
            Some(w) => {
                w.suppressed = w.suppressed.saturating_add(1);
                (Decision::Suppress, None)
            }
        }
    }

    /// Drain every window whose age has reached the limiter's
    /// configured window. Returns one [`Rollup`] per drained window
    /// that actually suppressed at least one event; windows that
    /// only saw a single (already-emitted) event are dropped silently.
    pub fn flush_expired(&self) -> Vec<Rollup> {
        let now = Instant::now();
        self.flush_by(|w| now.duration_since(w.first_seen) >= self.window)
    }

    /// Drain every window regardless of age. Used at daemon shutdown
    /// so in-flight suppression counts make it into the chain before
    /// the process exits.
    pub fn flush_all(&self) -> Vec<Rollup> {
        self.flush_by(|_| true)
    }

    fn flush_by<F: Fn(&ActiveWindow) -> bool>(&self, predicate: F) -> Vec<Rollup> {
        let Ok(mut map) = self.inner.lock() else {
            return Vec::new();
        };
        let window_seconds = self.window.as_secs();
        let keys: Vec<String> = map
            .iter()
            .filter(|(_, w)| predicate(w))
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some(w) = map.remove(&k)
                && w.suppressed > 0
            {
                out.push(Rollup {
                    op: w.op,
                    actor: w.actor,
                    key: k,
                    suppressed_count: w.suppressed,
                    window_seconds,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> AuditActor {
        AuditActor::iscsi(Some("iqn.test".to_string()), "1.2.3.4:1234".to_string())
    }

    #[test]
    fn first_event_emits() {
        let rl = AuditRateLimiter::new(Duration::from_secs(60));
        assert_eq!(
            rl.decide("k".into(), "iscsi.chap.failure", &actor()).0,
            Decision::Emit
        );
    }

    #[test]
    fn second_event_suppresses() {
        let rl = AuditRateLimiter::new(Duration::from_secs(60));
        let _ = rl.decide("k".into(), "iscsi.chap.failure", &actor());
        assert_eq!(
            rl.decide("k".into(), "iscsi.chap.failure", &actor()).0,
            Decision::Suppress
        );
    }

    #[test]
    fn different_keys_are_independent() {
        let rl = AuditRateLimiter::new(Duration::from_secs(60));
        assert_eq!(rl.decide("a".into(), "op", &actor()).0, Decision::Emit);
        assert_eq!(rl.decide("b".into(), "op", &actor()).0, Decision::Emit);
    }

    #[test]
    fn expired_window_with_suppressions_returns_displaced_rollup() {
        // Issue #202: a fresh event arriving after window expiry but
        // before flush must carry the prior window's suppressed count out
        // as a displaced rollup, not silently zero it.
        let rl = AuditRateLimiter::new(Duration::from_millis(20));
        let _ = rl.decide("k".into(), "op", &actor()); // emit, opens window
        for _ in 0..4 {
            let _ = rl.decide("k".into(), "op", &actor()); // suppressed x4
        }
        std::thread::sleep(Duration::from_millis(40));
        let (decision, rollup) = rl.decide("k".into(), "op", &actor());
        assert_eq!(decision, Decision::Emit);
        let rollup = rollup.expect("displaced rollup must be returned");
        assert_eq!(rollup.suppressed_count, 4);
        assert_eq!(rollup.key, "k");
        // The flusher must not re-emit it (count was moved out).
        std::thread::sleep(Duration::from_millis(40));
        assert!(rl.flush_expired().is_empty());
    }

    #[test]
    fn flush_emits_rollup_after_suppressions() {
        let rl = AuditRateLimiter::new(Duration::from_millis(50));
        let _ = rl.decide("k".into(), "op", &actor());
        for _ in 0..5 {
            let _ = rl.decide("k".into(), "op", &actor());
        }
        std::thread::sleep(Duration::from_millis(80));
        let rollups = rl.flush_expired();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].suppressed_count, 5);
        assert_eq!(rollups[0].op, "op");
        assert_eq!(rollups[0].window_seconds, 0); // 50ms rounds down
    }

    #[test]
    fn flush_skips_windows_with_no_suppressions() {
        let rl = AuditRateLimiter::new(Duration::from_millis(50));
        let _ = rl.decide("k".into(), "op", &actor());
        std::thread::sleep(Duration::from_millis(80));
        let rollups = rl.flush_expired();
        assert_eq!(
            rollups.len(),
            0,
            "no suppressions occurred — no rollup expected"
        );
    }

    #[test]
    fn flush_all_drains_unexpired_windows() {
        let rl = AuditRateLimiter::new(Duration::from_secs(60));
        let _ = rl.decide("k".into(), "op", &actor());
        let _ = rl.decide("k".into(), "op", &actor());
        let rollups = rl.flush_all();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].suppressed_count, 1);
    }

    #[test]
    fn next_emit_after_expired_window_starts_fresh() {
        let rl = AuditRateLimiter::new(Duration::from_millis(20));
        let _ = rl.decide("k".into(), "op", &actor());
        let _ = rl.decide("k".into(), "op", &actor());
        std::thread::sleep(Duration::from_millis(40));
        // Flush task hasn't run yet — but `decide` past the window
        // boundary should emit again, not silently suppress.
        assert_eq!(rl.decide("k".into(), "op", &actor()).0, Decision::Emit);
    }

    #[test]
    fn flush_expired_leaves_active_windows_alone() {
        let rl = AuditRateLimiter::new(Duration::from_millis(50));
        // expired window — has suppressions
        let _ = rl.decide("old".into(), "op", &actor());
        let _ = rl.decide("old".into(), "op", &actor());
        std::thread::sleep(Duration::from_millis(70));
        // fresh window — also has a suppression but is not expired yet
        let _ = rl.decide("new".into(), "op", &actor());
        let _ = rl.decide("new".into(), "op", &actor());

        let rollups = rl.flush_expired();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].key, "old");

        // The fresh window survives the flush; flush_all drains it.
        let remaining = rl.flush_all();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].key, "new");
    }
}
