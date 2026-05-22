// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Alert wire shape: the JSON payload every sink renders from.
//!
//! Producers build an [`Alert`] via [`Alert::new`] (typically through
//! the [`crate::record`] helpers, not directly). The dispatcher's
//! rate-limiter keys on `(class, dedup_key)`; the sinks render the
//! whole struct.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Event-class enum. Adding a variant means: pick a string label
/// (`as_str`), add a YAML on/off knob in
/// [`crate::AlertingConfig::events`], wire a producer call site, and
/// update [`crate::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertClass {
    BackendReachability,
    AuditFailure,
    DiskCacheBackpressure,
    ChapFailures,
}

impl AlertClass {
    pub const ALL: &'static [AlertClass] = &[
        AlertClass::BackendReachability,
        AlertClass::AuditFailure,
        AlertClass::DiskCacheBackpressure,
        AlertClass::ChapFailures,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            AlertClass::BackendReachability => "backend_reachability",
            AlertClass::AuditFailure => "audit_failure",
            AlertClass::DiskCacheBackpressure => "disk_cache_backpressure",
            AlertClass::ChapFailures => "chap_failures",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// The full payload a sink renders. `dedup_key` is the rate-limiter
/// key and is not serialized to the wire — it's a per-alert
/// distinguisher (e.g. `"backend:primary:failure"`) so different
/// backends each get their own dedup window.
#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    pub class: AlertClass,
    pub severity: Severity,
    pub message: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
    pub timestamp: DateTime<Utc>,
    #[serde(skip)]
    pub dedup_key: String,
}

impl Alert {
    pub fn new(
        class: AlertClass,
        severity: Severity,
        message: impl Into<String>,
        fields: serde_json::Map<String, serde_json::Value>,
        dedup_key: impl Into<String>,
    ) -> Self {
        Self {
            class,
            severity,
            message: message.into(),
            fields,
            timestamp: Utc::now(),
            dedup_key: dedup_key.into(),
        }
    }

    /// Build the JSON object exposed to webhook body templates and
    /// included as the email body fallback when the user picked the
    /// default template.
    pub fn to_json(&self, product: &str, version: &str) -> serde_json::Value {
        serde_json::json!({
            "product": product,
            "version": version,
            "class": self.class.as_str(),
            "severity": self.severity.as_str(),
            "message": self.message,
            "fields": self.fields,
            "timestamp": self.timestamp.to_rfc3339(),
        })
    }
}
