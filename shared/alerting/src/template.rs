// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Tera template rendering for webhook bodies.
//!
//! The template is rendered with a small fixed context:
//!
//! - `class` (string) — alert class label
//! - `severity` (string) — info | warn | error
//! - `message` (string) — operator-readable message
//! - `timestamp` (string, RFC 3339)
//! - `product` (string) — `thurvtl` / `thurvsa`
//! - `version` (string) — daemon version
//! - `fields.*` — every key from the alert's `fields` map
//!
//! No filters, no inheritance, no include — Tera is used as a plain
//! string-substitution engine. Render failures surface as
//! [`crate::SinkError::Render`].

use tera::{Context, Tera};

use crate::alert::Alert;

pub(crate) fn render(
    template: &str,
    alert: &Alert,
    product: &str,
    version: &str,
) -> Result<String, tera::Error> {
    let mut tera = Tera::default();
    // Register the template inline. Single-template, no caching —
    // alerts are bursty but never hot enough to justify a per-sink
    // cached `Tera` instance.
    tera.add_raw_template("body", template)?;

    let mut ctx = Context::new();
    ctx.insert("class", alert.class.as_str());
    ctx.insert("severity", alert.severity.as_str());
    ctx.insert("message", &alert.message);
    ctx.insert("timestamp", &alert.timestamp.to_rfc3339());
    ctx.insert("product", product);
    ctx.insert("version", version);
    ctx.insert("fields", &alert.fields);

    tera.render("body", &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertClass, Severity};

    fn make_alert() -> Alert {
        let mut fields = serde_json::Map::new();
        fields.insert("pct".to_string(), serde_json::Value::from(82));
        fields.insert(
            "band".to_string(),
            serde_json::Value::String("warn".to_string()),
        );
        Alert::new(
            AlertClass::DiskCacheBackpressure,
            Severity::Warn,
            String::from("Disk cache at 82%"),
            fields,
            String::from("warn"),
        )
    }

    #[test]
    fn pagerduty_v2_template_renders() {
        let template = r#"{
  "routing_key": "R0UTING",
  "event_action": "trigger",
  "payload": {
    "summary": "{{message}}",
    "severity": "{{severity}}",
    "source": "{{product}}",
    "custom_details": {
      "class": "{{class}}",
      "pct": {{fields.pct}}
    }
  }
}"#;
        let out = render(template, &make_alert(), "thurvtl", "0.1.0").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("renders to valid JSON");
        assert_eq!(parsed["payload"]["summary"], "Disk cache at 82%");
        assert_eq!(parsed["payload"]["severity"], "warn");
        assert_eq!(parsed["payload"]["custom_details"]["pct"], 82);
    }

    #[test]
    fn slack_incoming_webhook_template_renders() {
        let template = r#"{"text": "[{{severity}}] {{product}} {{class}}: {{message}}"}"#;
        let out = render(template, &make_alert(), "thurvsa", "0.1.0").unwrap();
        assert!(out.contains("[warn] thurvsa disk_cache_backpressure: Disk cache at 82%"));
    }

    #[test]
    fn missing_field_propagates_render_error() {
        let template = "{{nonexistent}}";
        let result = render(template, &make_alert(), "thurvtl", "0.1.0");
        assert!(result.is_err(), "expected render error for unknown var");
    }
}
