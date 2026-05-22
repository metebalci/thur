// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product first-party alerting subsystem.
//!
//! Two opt-in sinks:
//!
//! - **Email** — SMTP relay (PLAIN + STARTTLS), via `lettre`.
//! - **Webhook** — HTTP POST with a per-sink Tera template body, via
//!   `reqwest`. One code path covers PagerDuty, Slack, Discord,
//!   ntfy.sh, ServiceNow, Jira — operators write the upstream JSON
//!   shape inline in YAML, no per-vendor glue here.
//!
//! Four event classes (all fire from the daemon, all individually
//! mutable via YAML on/off knobs):
//!
//! - `BackendReachability` — emitted from `system cloud_check` job.
//! - `AuditFailure` — emitted from the audit writer task on append
//!   failure (disk write / fsync / chain-state).
//! - `DiskCacheBackpressure` — emitted on watermark crossing +
//!   backpressure timeout.
//! - `ChapFailures` — emitted on repeated CHAP login failures from
//!   the same user inside one dedup window.
//!
//! Rate-limiting: re-uses [`shared_audit::AuditRateLimiter`]. Per-
//! `(class, dedup_key)` window collapses repeats; first event in a
//! window goes out, the rest are silently counted.
//!
//! Failure policy: no retries. Sink-send failures are logged at WARN
//! and counted in `<product>_alerts_fired_total{outcome="failure"}`.
//! The plan is explicit: drop on failure; operators see gaps via the
//! counter.
//!
//! Architecture mirrors [`shared_telemetry`]:
//!
//! 1. Daemon's `main.rs` builds an [`AlertingDispatcher`] from YAML
//!    at boot (only when `alerting.enabled: true`).
//! 2. Daemon installs it as the process-global handle via
//!    [`set_global`].
//! 3. Producer crates call the [`record`] free functions; each is a
//!    no-op when the global isn't installed.
//!
//! Wiring is process-global, not per-call-site plumbing — alerting
//! cuts across every subsystem and re-using the telemetry pattern
//! keeps the diff narrow.

#![forbid(unsafe_code)]

pub mod admin_job;
mod alert;
mod config;
mod dispatcher;
#[cfg(feature = "http")]
pub mod http;
mod ratelimit;
mod sinks;
mod template;

pub use alert::{Alert, AlertClass, Severity};
pub use config::{
    AlertingConfig, EmailSinkConfig, SinkConfig, SinkSpec, WebhookSinkConfig,
    default_chap_failures_threshold, default_dedup_window_seconds,
};
pub use dispatcher::{AlertingDispatcher, DispatcherError};
pub use sinks::{AlertSink, SinkError, SinkOutcome};

use std::sync::OnceLock;

/// Process-global dispatcher handle. Installed once at daemon boot
/// via [`set_global`]. Producer crates record through the [`record`]
/// free functions, which no-op when the global is unset (CLI / unit
/// tests / `--test` smoke runs).
///
/// Mirrors the [`shared_telemetry`] pattern so a producer subsystem
/// doesn't need to take an `Option<AlertChannel>` in every signature.
static GLOBAL: OnceLock<AlertingDispatcher> = OnceLock::new();

/// Install the process-global dispatcher. Idempotent: a second call
/// is a no-op (returns Err with the rejected value) so the daemon's
/// normal path and `--test` smoke runs don't fight over the slot.
/// `Err` is intentionally a return of the rejected dispatcher (so
/// the caller can recover or log; `OnceLock::set` requires it). The
/// large-Err clippy lint is silenced — boxing would distort the API
/// for one cold boot-time call.
#[allow(clippy::result_large_err)]
pub fn set_global(d: AlertingDispatcher) -> Result<(), AlertingDispatcher> {
    GLOBAL.set(d)
}

/// Borrow the global dispatcher if installed.
pub fn global() -> Option<&'static AlertingDispatcher> {
    GLOBAL.get()
}

/// Per-event-class emission helpers. Each looks up the global and
/// forwards if set. The intent matches `shared_telemetry::record`:
/// one-liner call sites at every producer, with no Option-handling
/// in the producer.
pub mod record {
    use super::{Alert, AlertClass, Severity, global};

    fn emit(alert: Alert) {
        if let Some(d) = global() {
            d.try_emit(alert);
        }
    }

    /// `cloud check` cycle: a backend either started failing or
    /// recovered. `outcome` is `"failure"` or `"recovery"`.
    pub fn backend_reachability(backend: &str, outcome: &str, error: Option<&str>) {
        let severity = match outcome {
            "recovery" => Severity::Info,
            _ => Severity::Error,
        };
        let mut fields = serde_json::Map::new();
        fields.insert("backend".into(), serde_json::Value::String(backend.into()));
        fields.insert("outcome".into(), serde_json::Value::String(outcome.into()));
        if let Some(e) = error {
            fields.insert("error".into(), serde_json::Value::String(e.into()));
        }
        let message = match outcome {
            "recovery" => format!("Cloud backend '{backend}' recovered"),
            _ => format!("Cloud backend '{backend}' unreachable"),
        };
        emit(Alert::new(
            AlertClass::BackendReachability,
            severity,
            message,
            fields,
            format!("{backend}:{outcome}"),
        ));
    }

    /// Audit-writer task hit an append failure (chain write / fsync /
    /// chain-state).
    pub fn audit_append_failed(op: &str, error: &str) {
        let mut fields = serde_json::Map::new();
        fields.insert("op".into(), serde_json::Value::String(op.into()));
        fields.insert("error".into(), serde_json::Value::String(error.into()));
        emit(Alert::new(
            AlertClass::AuditFailure,
            Severity::Error,
            format!("Audit append failed for {op}: {error}"),
            fields,
            op.to_string(),
        ));
    }

    /// Disk-cache pool crossed the soft watermark
    /// (`localonly_soft_watermark_pct`). One emit per backend per
    /// dedup window.
    pub fn disk_cache_watermark(backend: &str, used_pct: u64, cap_bytes: u64) {
        let mut fields = serde_json::Map::new();
        fields.insert("backend".into(), serde_json::Value::String(backend.into()));
        fields.insert("used_pct".into(), serde_json::Value::from(used_pct));
        fields.insert("cap_bytes".into(), serde_json::Value::from(cap_bytes));
        emit(Alert::new(
            AlertClass::DiskCacheBackpressure,
            Severity::Warn,
            format!("Disk cache for backend '{backend}' at {used_pct}% of cap"),
            fields,
            format!("{backend}:watermark"),
        ));
    }

    /// A chunk seal blocked on the pool budget past
    /// `backpressure_max_wait_seconds`.
    pub fn disk_cache_backpressure_timeout(backend: &str, waited_seconds: u64) {
        let mut fields = serde_json::Map::new();
        fields.insert("backend".into(), serde_json::Value::String(backend.into()));
        fields.insert(
            "waited_seconds".into(),
            serde_json::Value::from(waited_seconds),
        );
        emit(Alert::new(
            AlertClass::DiskCacheBackpressure,
            Severity::Error,
            format!(
                "Backend '{backend}' backpressure timeout after {waited_seconds}s — host write refused"
            ),
            fields,
            format!("{backend}:backpressure"),
        ));
    }

    /// One CHAP login failure. The dispatcher tracks per-user failures
    /// inside the dedup window and emits a WARN alert once the count
    /// reaches the configured threshold.
    pub fn chap_failure(user: &str, peer: &str) {
        if let Some(d) = global() {
            d.observe_chap_failure(user, peer);
        }
    }
}
