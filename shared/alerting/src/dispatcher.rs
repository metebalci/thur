// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Owns the rate limiter, sinks, CHAP per-user counters, and the
//! storage-check last-status map (for backend-reachability transitions).
//!
//! Why one struct and not three. v1 has one global handle per
//! daemon; splitting state into multiple statics is more surface
//! without buying isolation we don't need. The per-class on/off
//! gate lives here too so producers always emit unconditionally and
//! the dispatcher decides whether to ship.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shared_naming::ProductIdentity;
use shared_telemetry::Telemetry;

use crate::alert::{Alert, AlertClass, Severity};
use crate::config::{AlertingConfig, EventsConfig, SinkConfig};
use crate::ratelimit::AlertRateLimiter;
use crate::sinks::AlertSink;
use crate::sinks::email::EmailSink;
use crate::sinks::webhook::WebhookSink;

#[derive(Debug, thiserror::Error)]
pub enum DispatcherError {
    #[error("alerting.sinks must not be empty when alerting.enabled=true")]
    NoSinks,
    #[error("sink '{name}': {error}")]
    SinkBuild { name: String, error: String },
    #[error("duplicate sink name: '{0}'")]
    DuplicateSink(String),
}

pub struct AlertingDispatcher {
    product: &'static ProductIdentity,
    version: &'static str,
    events: EventsConfig,
    chap_failures_threshold: u32,
    rate_limiter: AlertRateLimiter,
    sinks: Vec<Arc<dyn AlertSink>>,
    /// Per-user CHAP-failure counters, reset every window.
    chap_state: Mutex<ChapState>,
    /// Last-known-status per backend so we only fire on transitions.
    backend_status: Mutex<HashMap<String, BackendStatus>>,
    telemetry: Telemetry,
}

#[derive(Debug)]
struct ChapState {
    window: Duration,
    started_at: Instant,
    /// `user -> failures inside the current window`.
    counts: HashMap<String, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendStatus {
    Healthy,
    Failing,
}

impl AlertingDispatcher {
    /// Build the dispatcher from validated config. Each sink is
    /// constructed synchronously here so a misconfiguration (bad
    /// SMTP host, malformed webhook URL, missing env var for an
    /// interpolated header) fails the daemon at boot — not the
    /// first time an alert tries to fire at 3 am.
    pub fn build(
        cfg: &AlertingConfig,
        product: &'static ProductIdentity,
        version: &'static str,
        telemetry: Telemetry,
    ) -> Result<Self, DispatcherError> {
        if cfg.sinks.is_empty() {
            return Err(DispatcherError::NoSinks);
        }

        let mut names = std::collections::HashSet::new();
        let mut sinks: Vec<Arc<dyn AlertSink>> = Vec::with_capacity(cfg.sinks.len());
        for spec in &cfg.sinks {
            if !names.insert(spec.name.clone()) {
                return Err(DispatcherError::DuplicateSink(spec.name.clone()));
            }
            let sink: Arc<dyn AlertSink> = match &spec.config {
                SinkConfig::Email(e) => Arc::new(
                    EmailSink::build(spec.name.clone(), e, product.name).map_err(|err| {
                        DispatcherError::SinkBuild {
                            name: spec.name.clone(),
                            error: err.to_string(),
                        }
                    })?,
                ),
                SinkConfig::Webhook(w) => {
                    Arc::new(WebhookSink::build(spec.name.clone(), w).map_err(|err| {
                        DispatcherError::SinkBuild {
                            name: spec.name.clone(),
                            error: err.to_string(),
                        }
                    })?)
                }
            };
            sinks.push(sink);
        }

        let window = Duration::from_secs(cfg.dedup_window_seconds.max(1));
        let rate_limiter = AlertRateLimiter::new(window);
        let chap_state = Mutex::new(ChapState {
            window,
            started_at: Instant::now(),
            counts: HashMap::new(),
        });

        Ok(Self {
            product,
            version,
            events: cfg.events.clone(),
            chap_failures_threshold: cfg.chap_failures_threshold,
            rate_limiter,
            sinks,
            chap_state,
            backend_status: Mutex::new(HashMap::new()),
            telemetry,
        })
    }

    pub fn product(&self) -> &'static ProductIdentity {
        self.product
    }

    pub fn sink_names(&self) -> Vec<String> {
        self.sinks.iter().map(|s| s.name().to_string()).collect()
    }

    pub fn dedup_window_seconds(&self) -> u64 {
        // Read back what we built the rate-limiter with; matches the
        // YAML value rounded to whole seconds.
        let window = self.rate_limiter.window();
        window.as_secs()
    }

    fn class_enabled(&self, class: AlertClass) -> bool {
        match class {
            AlertClass::BackendReachability => self.events.backend_reachability,
            AlertClass::AuditFailure => self.events.audit_failure,
            AlertClass::DiskCacheBackpressure => self.events.disk_cache_backpressure,
            AlertClass::ChapFailures => self.events.chap_failures,
            AlertClass::OrphanedObjects => self.events.orphaned_objects,
        }
    }

    /// Producer-facing emit. Non-blocking: spawns a fan-out task and
    /// returns immediately so the caller never waits on SMTP /
    /// reqwest. Rate-limiter check is synchronous so we don't even
    /// allocate the task for a suppressed alert.
    pub fn try_emit(&self, alert: Alert) {
        if !self.class_enabled(alert.class) {
            return;
        }

        // Backend-reachability is special-cased: we only emit on
        // status transitions (healthy->failing or failing->healthy).
        // Drop repeats inside the same status.
        let mut pending_backend_commit: Option<(String, BackendStatus)> = None;
        if alert.class == AlertClass::BackendReachability {
            let backend = alert
                .fields
                .get("backend")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let outcome = alert
                .fields
                .get("outcome")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let next = match outcome {
                "recovery" => BackendStatus::Healthy,
                _ => BackendStatus::Failing,
            };
            // Read the prior status without committing the new one yet
            // (issue #203).
            let prev = {
                let map = match self.backend_status.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                map.get(backend).copied()
            };
            // Non-firing transitions — same-status repeats and the
            // first-seen-healthy baseline (a "recovered" alert only makes
            // sense after an observed failure) — don't page, but we DO
            // record the status: it tracks observed backends for display
            // and seeds the transition gate. Committing here is safe
            // precisely because the gate already returned false.
            if !backend_reachability_should_fire(prev, next) {
                let mut map = match self.backend_status.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                map.insert(backend.to_string(), next);
                return;
            }
            // A *firing* transition: defer the commit until after the
            // rate-limiter so a suppressed transition leaves `prev`
            // intact. Committing it here (the old behaviour) let a brief
            // flap permanently silence an ongoing outage — the
            // healthy->failing transition passed the gate but was
            // suppressed by an open dedup window, after which the
            // already-flipped status made every later failing report a
            // same-status repeat the gate dropped forever.
            pending_backend_commit = Some((backend.to_string(), next));
        }

        if !self.rate_limiter.allow(&alert) {
            self.record_outcome(&alert, "all", "suppressed");
            return;
        }

        // The alert is firing — now commit the backend status so a
        // suppressed transition above never poisons the transition gate.
        if let Some((backend, next)) = pending_backend_commit {
            let mut map = match self.backend_status.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            map.insert(backend, next);
        }

        self.fan_out(alert);
    }

    /// CHAP-failure path: per-user counter, fires once the threshold
    /// is reached inside the current window.
    pub fn observe_chap_failure(&self, user: &str, peer: &str) {
        if !self.events.chap_failures || self.chap_failures_threshold == 0 {
            return;
        }
        let (count, window) = {
            let mut state = match self.chap_state.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if state.started_at.elapsed() >= state.window {
                state.counts.clear();
                state.started_at = Instant::now();
            }
            let counter = state.counts.entry(user.to_string()).or_insert(0);
            *counter = counter.saturating_add(1);
            (*counter, state.window.as_secs())
        };
        if count != self.chap_failures_threshold {
            return; // Either still below, or already crossed once this window.
        }
        let mut fields = serde_json::Map::new();
        fields.insert("user".into(), serde_json::Value::String(user.into()));
        fields.insert("peer".into(), serde_json::Value::String(peer.into()));
        fields.insert("count".into(), serde_json::Value::from(count));
        fields.insert("window_seconds".into(), serde_json::Value::from(window));
        let alert = Alert::new(
            AlertClass::ChapFailures,
            Severity::Warn,
            format!("CHAP login: {count} failures from user '{user}' inside {window}s window"),
            fields,
            format!("chap:{user}"),
        );
        // Skip the rate-limiter — the per-user counter already
        // bounds emit rate (one alert per user per window).
        self.fan_out(alert);
    }

    /// Direct one-shot emit through a single named sink, bypassing
    /// the rate limiter + event-class gate. Used by the
    /// `system.alerting.test` CLI verb so an operator can validate
    /// a sink even when its class is masked off.
    pub async fn send_test(&self, sink_name: &str, alert: Alert) -> Result<(), DispatcherError> {
        let sink = self
            .sinks
            .iter()
            .find(|s| s.name() == sink_name)
            .ok_or_else(|| DispatcherError::SinkBuild {
                name: sink_name.to_string(),
                error: "no such sink".into(),
            })?;
        match sink.send(&alert, self.product.name, self.version).await {
            Ok(()) => {
                self.record_outcome(&alert, sink_name, "success");
                Ok(())
            }
            Err(e) => {
                self.record_outcome(&alert, sink_name, "failure");
                Err(DispatcherError::SinkBuild {
                    name: sink_name.to_string(),
                    error: e.to_string(),
                })
            }
        }
    }

    fn fan_out(&self, alert: Alert) {
        let sinks = self.sinks.clone();
        let product = self.product.name;
        let version = self.version;
        let telemetry = self.telemetry.clone();
        let class_label = alert.class.as_str();
        let severity_label = alert.severity.as_str();
        let alert = Arc::new(alert);
        tokio::spawn(async move {
            for sink in &sinks {
                let sink_name = sink.name().to_string();
                match sink.send(&alert, product, version).await {
                    Ok(()) => record_telemetry(
                        &telemetry,
                        class_label,
                        severity_label,
                        &sink_name,
                        "success",
                    ),
                    Err(e) => {
                        tracing::warn!(
                            "alerting: sink '{}' send failed for {}: {}",
                            sink_name,
                            class_label,
                            e
                        );
                        record_telemetry(
                            &telemetry,
                            class_label,
                            severity_label,
                            &sink_name,
                            "failure",
                        );
                    }
                }
            }
        });
    }

    fn record_outcome(&self, alert: &Alert, sink: &str, outcome: &str) {
        record_telemetry(
            &self.telemetry,
            alert.class.as_str(),
            alert.severity.as_str(),
            sink,
            outcome,
        );
    }
}

fn record_telemetry(telemetry: &Telemetry, class: &str, severity: &str, sink: &str, outcome: &str) {
    telemetry.alerts_record(class, severity, sink, outcome);
}

/// Decide whether a backend-reachability status change should fire an
/// alert. Same-status repeats don't fire; a first sighting that's
/// healthy is recorded as a silent baseline (no failure was ever
/// observed, so "recovered" would be spurious). Everything else — a
/// first-seen failure, a healthy->failing drop, a failing->healthy
/// recovery — fires. Pulled out of [`AlertingDispatcher::try_emit`] so
/// the transition policy is unit-testable on its own.
fn backend_reachability_should_fire(prev: Option<BackendStatus>, next: BackendStatus) -> bool {
    match (prev, next) {
        (Some(p), n) if p == n => false,
        (None, BackendStatus::Healthy) => false,
        _ => true,
    }
}

#[cfg(test)]
impl AlertingDispatcher {
    /// Snapshot the current backend-status map for assertions. Test-
    /// only — production code reaches into the mutex directly through
    /// `try_emit`.
    pub(crate) fn backend_status_snapshot(&self) -> HashMap<String, &'static str> {
        let map = self.backend_status.lock().unwrap();
        map.iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        BackendStatus::Healthy => "healthy",
                        BackendStatus::Failing => "failing",
                    },
                )
            })
            .collect()
    }

    /// Snapshot the CHAP per-user counter for assertions.
    pub(crate) fn chap_counter_snapshot(&self, user: &str) -> u32 {
        self.chap_state
            .lock()
            .unwrap()
            .counts
            .get(user)
            .copied()
            .unwrap_or(0)
    }

    /// Force the CHAP window to expire — emulates the passage of
    /// `dedup_window_seconds + 1` so the next `observe_chap_failure`
    /// hits the window-reset branch. Cheaper than `tokio::time::sleep`.
    pub(crate) fn force_chap_window_reset(&self) {
        let mut state = self.chap_state.lock().unwrap();
        state.started_at = Instant::now() - state.window - Duration::from_secs(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertClass, Severity};
    use crate::config::{
        AlertingConfig, EventsConfig, SinkConfig, SinkSpec, WebhookSinkConfig,
        default_chap_failures_threshold, default_dedup_window_seconds,
    };
    use serde_json::Map;
    use shared_naming::DISK;

    fn webhook_sink(name: &str) -> SinkSpec {
        SinkSpec {
            name: name.into(),
            config: SinkConfig::Webhook(WebhookSinkConfig {
                // 127.0.0.1:1 is reserved-low; reqwest fails fast on
                // ECONNREFUSED. We never observe send outcomes here —
                // tests assert the *gating* path that runs before
                // fan_out reaches the sink at all.
                url: "http://127.0.0.1:1/".into(),
                method: "POST".into(),
                headers: HashMap::new(),
                body_template: String::new(),
                timeout_seconds: 1,
            }),
        }
    }

    fn build_dispatcher(cfg: AlertingConfig) -> AlertingDispatcher {
        AlertingDispatcher::build(&cfg, &DISK, "test", Telemetry::noop()).expect("dispatcher build")
    }

    fn default_cfg(threshold: u32) -> AlertingConfig {
        AlertingConfig {
            enabled: true,
            dedup_window_seconds: default_dedup_window_seconds(),
            chap_failures_threshold: threshold,
            events: EventsConfig {
                // Toggle on every class so try_emit doesn't gate on us.
                backend_reachability: true,
                audit_failure: true,
                disk_cache_backpressure: true,
                chap_failures: true,
                orphaned_objects: true,
            },
            sinks: vec![webhook_sink("primary")],
        }
    }

    fn backend_alert(backend: &str, outcome: &str) -> Alert {
        let mut fields = Map::new();
        fields.insert("backend".into(), backend.into());
        fields.insert("outcome".into(), outcome.into());
        Alert::new(
            AlertClass::BackendReachability,
            Severity::Warn,
            format!("backend {backend} {outcome}"),
            fields,
            format!("backend:{backend}:{outcome}"),
        )
    }

    #[test]
    fn build_rejects_empty_sinks_when_enabled() {
        let cfg = AlertingConfig {
            enabled: true,
            sinks: Vec::new(),
            ..AlertingConfig::default()
        };
        match AlertingDispatcher::build(&cfg, &DISK, "test", Telemetry::noop()) {
            Ok(_) => panic!("empty sinks must fail"),
            Err(e) => assert!(matches!(e, DispatcherError::NoSinks)),
        }
    }

    #[test]
    fn build_rejects_duplicate_sink_names() {
        let mut cfg = default_cfg(default_chap_failures_threshold());
        cfg.sinks.push(webhook_sink("primary"));
        match AlertingDispatcher::build(&cfg, &DISK, "test", Telemetry::noop()) {
            Ok(_) => panic!("duplicate sink name must fail"),
            Err(e) => assert!(matches!(e, DispatcherError::DuplicateSink(ref n) if n == "primary")),
        }
    }

    // try_emit's fan_out path spawns through `tokio::spawn`; the
    // backend-reachability state machine itself runs synchronously
    // but the dispatcher still needs an ambient runtime so the
    // following fan-out doesn't panic.
    #[tokio::test]
    async fn backend_reachability_records_first_failing_status() {
        let d = build_dispatcher(default_cfg(default_chap_failures_threshold()));
        d.try_emit(backend_alert("s3-a", "permanent"));
        let snap = d.backend_status_snapshot();
        assert_eq!(snap.get("s3-a").copied(), Some("failing"));
    }

    #[test]
    fn backend_reachability_fire_decision() {
        use BackendStatus::{Failing, Healthy};
        // First-seen failure fires; first-seen healthy is a silent baseline.
        assert!(backend_reachability_should_fire(None, Failing));
        assert!(!backend_reachability_should_fire(None, Healthy));
        // Genuine transitions fire in both directions.
        assert!(backend_reachability_should_fire(Some(Failing), Healthy));
        assert!(backend_reachability_should_fire(Some(Healthy), Failing));
        // Same-status repeats don't.
        assert!(!backend_reachability_should_fire(Some(Healthy), Healthy));
        assert!(!backend_reachability_should_fire(Some(Failing), Failing));
    }

    #[tokio::test]
    async fn backend_reachability_first_seen_healthy_records_baseline() {
        // A fresh backend reported healthy (the ticker's first tick, or
        // a `storage check` against a never-failed backend) records the
        // baseline without treating it as a recovery transition.
        let d = build_dispatcher(default_cfg(default_chap_failures_threshold()));
        d.try_emit(backend_alert("s3-a", "recovery"));
        let snap = d.backend_status_snapshot();
        assert_eq!(snap.get("s3-a").copied(), Some("healthy"));
    }

    #[tokio::test]
    async fn backend_reachability_recovery_flips_status() {
        let d = build_dispatcher(default_cfg(default_chap_failures_threshold()));
        d.try_emit(backend_alert("s3-a", "permanent"));
        d.try_emit(backend_alert("s3-a", "recovery"));
        let snap = d.backend_status_snapshot();
        assert_eq!(snap.get("s3-a").copied(), Some("healthy"));
    }

    #[tokio::test]
    async fn backend_reachability_drops_repeats_in_same_status() {
        // Two failing-state alerts on the same backend land in the
        // same status bucket — the map should reflect that without
        // crashing on the second mutation. We can't directly observe
        // "alert was suppressed before fan_out" but we can prove the
        // *state* doesn't oscillate.
        let d = build_dispatcher(default_cfg(default_chap_failures_threshold()));
        d.try_emit(backend_alert("s3-a", "permanent"));
        d.try_emit(backend_alert("s3-a", "permanent"));
        let snap = d.backend_status_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.get("s3-a").copied(), Some("failing"));
    }

    #[tokio::test]
    async fn suppressed_failing_flap_does_not_poison_status() {
        // Issue #203: fail (fires, opens the permanent-key dedup window) ->
        // recover (fires) -> fail again (the same permanent-key window is
        // still open, so the transition is suppressed). The suppressed
        // transition must NOT commit the status to Failing — otherwise the
        // transition gate would treat every later failing report as a
        // same-status repeat and silence the ongoing outage forever. The
        // status stays Healthy so the outage re-fires once the window
        // expires.
        let d = build_dispatcher(default_cfg(default_chap_failures_threshold()));
        d.try_emit(backend_alert("s3-a", "permanent"));
        d.try_emit(backend_alert("s3-a", "recovery"));
        d.try_emit(backend_alert("s3-a", "permanent")); // suppressed by open window
        let snap = d.backend_status_snapshot();
        assert_eq!(
            snap.get("s3-a").copied(),
            Some("healthy"),
            "a suppressed transition must not commit the new status"
        );
    }

    #[tokio::test]
    async fn class_disabled_skips_state_machine_entirely() {
        let mut cfg = default_cfg(default_chap_failures_threshold());
        cfg.events.backend_reachability = false;
        let d = build_dispatcher(cfg);
        d.try_emit(backend_alert("s3-a", "permanent"));
        // Class was off → backend_status map untouched.
        assert!(d.backend_status_snapshot().is_empty());
    }

    #[test]
    fn chap_counter_increments_per_user() {
        let d = build_dispatcher(default_cfg(5));
        d.observe_chap_failure("alice", "10.0.0.1");
        d.observe_chap_failure("alice", "10.0.0.1");
        d.observe_chap_failure("bob", "10.0.0.2");
        assert_eq!(d.chap_counter_snapshot("alice"), 2);
        assert_eq!(d.chap_counter_snapshot("bob"), 1);
    }

    #[test]
    fn chap_window_reset_clears_per_user_counters() {
        let d = build_dispatcher(default_cfg(5));
        d.observe_chap_failure("alice", "10.0.0.1");
        d.observe_chap_failure("alice", "10.0.0.1");
        assert_eq!(d.chap_counter_snapshot("alice"), 2);

        d.force_chap_window_reset();
        // Next observation triggers the window-reset branch — alice's
        // counter is wiped and starts at 1 for the new window.
        d.observe_chap_failure("alice", "10.0.0.1");
        assert_eq!(d.chap_counter_snapshot("alice"), 1);
    }

    #[test]
    fn chap_zero_threshold_disables_alert_path() {
        // threshold=0 means "fire never" — the producer can still
        // emit per-event, but the dispatcher must not count.
        let d = build_dispatcher(default_cfg(0));
        for _ in 0..10 {
            d.observe_chap_failure("alice", "10.0.0.1");
        }
        assert_eq!(d.chap_counter_snapshot("alice"), 0);
    }

    #[test]
    fn chap_class_disabled_short_circuits_counter() {
        let mut cfg = default_cfg(5);
        cfg.events.chap_failures = false;
        let d = build_dispatcher(cfg);
        d.observe_chap_failure("alice", "10.0.0.1");
        assert_eq!(d.chap_counter_snapshot("alice"), 0);
    }

    #[tokio::test]
    async fn send_test_returns_error_for_unknown_sink() {
        let d = build_dispatcher(default_cfg(default_chap_failures_threshold()));
        let alert = Alert::new(
            AlertClass::AuditFailure,
            Severity::Warn,
            "test",
            Map::new(),
            "k",
        );
        let err = d
            .send_test("does-not-exist", alert)
            .await
            .expect_err("unknown sink must error");
        assert!(
            matches!(err, DispatcherError::SinkBuild { ref name, .. } if name == "does-not-exist")
        );
    }

    #[test]
    fn sink_names_returns_configured_order() {
        let mut cfg = default_cfg(default_chap_failures_threshold());
        cfg.sinks.push(webhook_sink("secondary"));
        let d = build_dispatcher(cfg);
        let names = d.sink_names();
        assert_eq!(names, vec!["primary".to_string(), "secondary".to_string()]);
    }

    #[test]
    fn dedup_window_round_trips_through_builder() {
        let mut cfg = default_cfg(default_chap_failures_threshold());
        cfg.dedup_window_seconds = 42;
        let d = build_dispatcher(cfg);
        assert_eq!(d.dedup_window_seconds(), 42);
    }
}
