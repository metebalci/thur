// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-class dedup keyed on the alert's `dedup_key` field.
//!
//! Wraps [`shared_audit::AuditRateLimiter`] — same windowing /
//! fail-open semantics — but exposes a thin Alert-flavored
//! `decide(class, dedup_key)` that lets the dispatcher key on
//! `(class, dedup_key)` without re-implementing the window.

use std::time::Duration;

use shared_audit::audit_ratelimit::{AuditRateLimiter, Decision};

use crate::alert::Alert;

/// Window-based dedup for alerts. One window per `(class, dedup_key)`
/// pair.
pub struct AlertRateLimiter {
    inner: AuditRateLimiter,
}

impl AlertRateLimiter {
    pub fn new(window: Duration) -> Self {
        Self {
            inner: AuditRateLimiter::new(window),
        }
    }

    pub fn window(&self) -> Duration {
        self.inner.window()
    }

    /// Returns true when this alert should be emitted, false when it
    /// should be suppressed (an earlier emit inside the same window
    /// already went out).
    pub fn allow(&self, alert: &Alert) -> bool {
        // Drain expired windows so fire-once dedup keys (a backend that
        // failed and was replaced, a one-shot audit error) don't
        // accumulate in the inner map forever. Unlike the audit side
        // there's no flush task here, and the rollups aren't consumed —
        // alerts are fire-and-forget — so we drop them. Alerts are rare
        // (dedup'd + class-gated) so the full-map walk is cheap.
        let _ = self.inner.flush_expired();
        let key = format!("{}:{}", alert.class.as_str(), alert.dedup_key);
        // The `op` + `actor` fields aren't surfaced anywhere on the
        // alerting path; pass placeholder values so AuditRateLimiter's
        // bookkeeping is happy. The Rollup it can emit isn't consumed
        // here — alerts are fire-and-forget, no follow-up rollup.
        let actor = shared_audit::AuditActor::daemon();
        // `.0` is the decision; the displaced rollup (`.1`) the audit
        // path consumes is irrelevant here — alerts are fire-and-forget.
        matches!(self.inner.decide(key, "alert", &actor).0, Decision::Emit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertClass, Severity};

    fn make_alert(class: AlertClass, dedup_key: &str) -> Alert {
        Alert::new(
            class,
            Severity::Warn,
            String::from("msg"),
            serde_json::Map::new(),
            String::from(dedup_key),
        )
    }

    #[test]
    fn first_alert_passes_second_suppressed() {
        let rl = AlertRateLimiter::new(Duration::from_secs(60));
        let a = make_alert(AlertClass::AuditFailure, "warn");
        assert!(rl.allow(&a));
        let b = make_alert(AlertClass::AuditFailure, "warn");
        assert!(!rl.allow(&b));
    }

    #[test]
    fn different_keys_each_pass() {
        let rl = AlertRateLimiter::new(Duration::from_secs(60));
        let a = make_alert(AlertClass::BackendReachability, "primary:failure");
        let b = make_alert(AlertClass::BackendReachability, "archive:failure");
        assert!(rl.allow(&a));
        assert!(rl.allow(&b));
    }

    #[test]
    fn different_classes_each_pass() {
        let rl = AlertRateLimiter::new(Duration::from_secs(60));
        let a = make_alert(AlertClass::AuditFailure, "warn");
        let b = make_alert(AlertClass::DiskCacheBackpressure, "warn");
        assert!(rl.allow(&a));
        assert!(rl.allow(&b));
    }
}
