// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Generic HTTP webhook sink.
//!
//! One POST (or operator-chosen method) per alert. Body is either the
//! canonical Alert JSON (when `body_template` is empty) or the
//! operator-supplied Tera template rendered with the alert context.
//! Headers honor `${ENV_VAR}` interpolation at construction time so a
//! credential leaks neither into the YAML nor into the in-memory
//! config struct.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::alert::Alert;
use crate::config::{WebhookSinkConfig, resolve_env};
use crate::sinks::{AlertSink, SinkError};
use crate::template;

pub struct WebhookSink {
    name: String,
    client: reqwest::Client,
    url: String,
    method: Method,
    headers: HeaderMap,
    body_template: String,
}

impl WebhookSink {
    pub fn build(name: String, cfg: &WebhookSinkConfig) -> Result<Self, SinkError> {
        let method: Method = cfg
            .method
            .parse()
            .map_err(|_| SinkError::Config(format!("unknown HTTP method '{}'", cfg.method)))?;

        let headers = resolve_headers(&cfg.headers)?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds))
            .build()
            .map_err(|e| SinkError::Config(format!("build reqwest client: {e}")))?;

        Ok(Self {
            name,
            client,
            url: cfg.url.clone(),
            method,
            headers,
            body_template: cfg.body_template.clone(),
        })
    }

    fn render_body(
        &self,
        alert: &Alert,
        product: &str,
        version: &str,
    ) -> Result<String, SinkError> {
        if self.body_template.is_empty() {
            // Canonical Alert JSON. Serialization can't fail for our
            // own struct, but propagate via SinkError for symmetry.
            let payload = alert.to_json(product, version);
            return serde_json::to_string(&payload)
                .map_err(|e| SinkError::Render(format!("canonical JSON: {e}")));
        }
        template::render(&self.body_template, alert, product, version)
            .map_err(|e| SinkError::Render(e.to_string()))
    }
}

fn resolve_headers(input: &HashMap<String, String>) -> Result<HeaderMap, SinkError> {
    let mut out = HeaderMap::with_capacity(input.len() + 1);
    let mut saw_content_type = false;
    for (k, v) in input {
        if k.eq_ignore_ascii_case("content-type") {
            saw_content_type = true;
        }
        let value = resolve_env(v).map_err(|e| SinkError::Config(e.to_string()))?;
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| SinkError::Config(format!("header name '{k}': {e}")))?;
        let hval = HeaderValue::from_str(&value)
            .map_err(|e| SinkError::Config(format!("header value for '{k}': {e}")))?;
        out.insert(name, hval);
    }
    if !saw_content_type {
        out.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
    }
    Ok(out)
}

#[async_trait]
impl AlertSink for WebhookSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, alert: &Alert, product: &str, version: &str) -> Result<(), SinkError> {
        let body = self.render_body(alert, product, version)?;
        let resp = self
            .client
            .request(self.method.clone(), &self.url)
            .headers(self.headers.clone())
            .body(body)
            .send()
            .await
            .map_err(|e| SinkError::Transport(format!("POST {}: {e}", self.url)))?;

        let status = resp.status();
        if !status.is_success() {
            let snippet = resp.text().await.unwrap_or_default();
            let head: String = snippet.chars().take(200).collect();
            return Err(SinkError::Transport(format!(
                "{} {} -> HTTP {} ({})",
                self.method,
                self.url,
                status.as_u16(),
                head.trim()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertClass, Severity};

    fn make_alert() -> Alert {
        let mut fields = serde_json::Map::new();
        fields.insert("pct".to_string(), serde_json::Value::from(82));
        Alert::new(
            AlertClass::DiskCacheBackpressure,
            Severity::Warn,
            String::from("Disk cache at 82%"),
            fields,
            String::from("warn"),
        )
    }

    #[test]
    fn canonical_body_serializes_alert_json() {
        let cfg = WebhookSinkConfig {
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            headers: HashMap::new(),
            body_template: String::new(),
            timeout_seconds: 10,
        };
        let sink = WebhookSink::build("test".into(), &cfg).unwrap();
        let body = sink.render_body(&make_alert(), "thurvtl", "0.1.0").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["product"], "thurvtl");
        assert_eq!(parsed["class"], "disk_cache_backpressure");
        assert_eq!(parsed["severity"], "warn");
        assert_eq!(parsed["fields"]["pct"], 82);
    }

    #[test]
    fn template_renders_pagerduty_shape() {
        let cfg = WebhookSinkConfig {
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            headers: HashMap::new(),
            body_template: r#"{"summary": "{{message}}", "severity": "{{severity}}"}"#.into(),
            timeout_seconds: 10,
        };
        let sink = WebhookSink::build("pd".into(), &cfg).unwrap();
        let body = sink.render_body(&make_alert(), "thurvtl", "0.1.0").unwrap();
        assert!(body.contains(r#""summary": "Disk cache at 82%""#));
        assert!(body.contains(r#""severity": "warn""#));
    }

    #[test]
    fn default_content_type_is_json() {
        let cfg = WebhookSinkConfig {
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            headers: HashMap::new(),
            body_template: String::new(),
            timeout_seconds: 10,
        };
        let sink = WebhookSink::build("test".into(), &cfg).unwrap();
        assert_eq!(
            sink.headers.get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn operator_content_type_wins() {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "text/plain".to_string());
        let cfg = WebhookSinkConfig {
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            headers,
            body_template: String::new(),
            timeout_seconds: 10,
        };
        let sink = WebhookSink::build("test".into(), &cfg).unwrap();
        assert_eq!(
            sink.headers.get("content-type").unwrap().to_str().unwrap(),
            "text/plain"
        );
    }
}
