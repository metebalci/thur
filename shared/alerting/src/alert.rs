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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_class_labels_are_stable() {
        assert_eq!(
            AlertClass::BackendReachability.as_str(),
            "backend_reachability",
        );
        assert_eq!(AlertClass::AuditFailure.as_str(), "audit_failure");
        assert_eq!(
            AlertClass::DiskCacheBackpressure.as_str(),
            "disk_cache_backpressure",
        );
        assert_eq!(AlertClass::ChapFailures.as_str(), "chap_failures");
        // ALL must enumerate every variant exactly once.
        assert_eq!(AlertClass::ALL.len(), 4);
    }

    #[test]
    fn alert_class_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&AlertClass::ChapFailures).expect("serialize"),
            "\"chap_failures\"",
        );
        let back: AlertClass = serde_json::from_str("\"audit_failure\"").expect("deserialize");
        assert_eq!(back, AlertClass::AuditFailure);
    }

    #[test]
    fn severity_labels_match_the_wire_strings() {
        assert_eq!(Severity::Info.as_str(), "info");
        assert_eq!(Severity::Warn.as_str(), "warn");
        assert_eq!(Severity::Error.as_str(), "error");
    }

    #[test]
    fn alert_new_then_to_json_carries_every_field() {
        let mut fields = serde_json::Map::new();
        fields.insert("backend".to_string(), serde_json::json!("primary"));
        let alert = Alert::new(
            AlertClass::BackendReachability,
            Severity::Error,
            "backend unreachable",
            fields,
            "backend:primary:failure",
        );
        // dedup_key is the rate-limiter key, kept off the wire.
        assert_eq!(alert.dedup_key, "backend:primary:failure");

        let json = alert.to_json("thurvtl", "1.2.3");
        assert_eq!(json["product"], "thurvtl");
        assert_eq!(json["version"], "1.2.3");
        assert_eq!(json["class"], "backend_reachability");
        assert_eq!(json["severity"], "error");
        assert_eq!(json["message"], "backend unreachable");
        assert_eq!(json["fields"]["backend"], "primary");
        assert!(json["timestamp"].is_string());
    }
}
