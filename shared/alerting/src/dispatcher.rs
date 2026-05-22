// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Owns the rate limiter, sinks, CHAP per-user counters, and the
//! cloud-check last-status map (for backend-reachability transitions).
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
            let mut map = match self.backend_status.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if map.get(backend).copied() == Some(next) {
                return;
            }
            map.insert(backend.to_string(), next);
            drop(map);
        }

        if !self.rate_limiter.allow(&alert) {
            self.record_outcome(&alert, "all", "suppressed");
            return;
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
