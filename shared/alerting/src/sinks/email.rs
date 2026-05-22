// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SMTP email sink via `lettre`.
//!
//! Construction (`EmailSink::build`) resolves `${ENV_VAR}` references
//! in the password field, validates the from/to addresses, and builds
//! the `AsyncSmtpTransport` with either STARTTLS (default) or plain
//! TCP. Per-send: format a plain-text body from the alert payload,
//! ship one envelope per send (no batching).
//!
//! Auth: omitted when `username` is empty (relay accepts the host's
//! IP); PLAIN over the negotiated TLS otherwise. Mechanism is left to
//! `lettre`'s default selection.

use async_trait::async_trait;
use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::alert::Alert;
use crate::config::{EmailSinkConfig, resolve_env};
use crate::sinks::{AlertSink, SinkError};

pub struct EmailSink {
    name: String,
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Vec<Mailbox>,
    subject_prefix: String,
}

impl EmailSink {
    pub fn build(name: String, cfg: &EmailSinkConfig, product: &str) -> Result<Self, SinkError> {
        let from = cfg
            .from
            .parse::<Mailbox>()
            .map_err(|e| SinkError::Config(format!("from address '{}': {e}", cfg.from)))?;
        let mut to = Vec::with_capacity(cfg.to.len());
        for addr in &cfg.to {
            to.push(
                addr.parse::<Mailbox>()
                    .map_err(|e| SinkError::Config(format!("to address '{addr}': {e}")))?,
            );
        }
        if to.is_empty() {
            return Err(SinkError::Config(
                "email sink needs at least one to: address".into(),
            ));
        }

        let builder = if cfg.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
                .map_err(|e| SinkError::Config(format!("STARTTLS relay '{}': {e}", cfg.host)))?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host)
        };
        let builder = builder.port(cfg.port);

        let builder = if cfg.username.is_empty() {
            builder
        } else {
            let password =
                resolve_env(&cfg.password).map_err(|e| SinkError::Config(e.to_string()))?;
            builder.credentials(Credentials::new(cfg.username.clone(), password))
        };

        let transport = builder.build();
        let subject_prefix = cfg
            .subject_prefix
            .clone()
            .unwrap_or_else(|| format!("[{product} ALERT]"));

        Ok(Self {
            name,
            transport,
            from,
            to,
            subject_prefix,
        })
    }

    fn format_body(alert: &Alert, product: &str, version: &str) -> String {
        let mut out = String::with_capacity(256);
        out.push_str(&format!("Product:   {product} {version}\n"));
        out.push_str(&format!("Class:     {}\n", alert.class.as_str()));
        out.push_str(&format!("Severity:  {}\n", alert.severity.as_str()));
        out.push_str(&format!("Timestamp: {}\n", alert.timestamp.to_rfc3339()));
        out.push_str(&format!("\n{}\n", alert.message));
        if !alert.fields.is_empty() {
            out.push_str("\nFields:\n");
            // Stable ordering so the body is reproducible for tests.
            let mut keys: Vec<&String> = alert.fields.keys().collect();
            keys.sort();
            for k in keys {
                let v = &alert.fields[k];
                out.push_str(&format!("  {k}: {v}\n"));
            }
        }
        out
    }
}

#[async_trait]
impl AlertSink for EmailSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, alert: &Alert, product: &str, version: &str) -> Result<(), SinkError> {
        let subject = format!(
            "{} {} — {}",
            self.subject_prefix,
            alert.severity.as_str().to_uppercase(),
            alert.message,
        );
        let body = Self::format_body(alert, product, version);

        let mut builder = Message::builder().from(self.from.clone());
        for addr in &self.to {
            builder = builder.to(addr.clone());
        }
        let msg = builder
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .map_err(|e| SinkError::Config(format!("build email: {e}")))?;

        self.transport
            .send(msg)
            .await
            .map(|_response| ())
            .map_err(|e| SinkError::Transport(format!("smtp send: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertClass, Severity};

    #[test]
    fn body_format_includes_all_top_level_fields() {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "backend".to_string(),
            serde_json::Value::String("primary".to_string()),
        );
        fields.insert("pct".to_string(), serde_json::Value::from(82));
        let alert = Alert::new(
            AlertClass::DiskCacheBackpressure,
            Severity::Warn,
            String::from("Disk cache at 82%"),
            fields,
            String::from("warn"),
        );
        let body = EmailSink::format_body(&alert, "thurvtl", "0.1.0");
        assert!(body.contains("Product:   thurvtl 0.1.0"));
        assert!(body.contains("Class:     disk_cache_backpressure"));
        assert!(body.contains("Severity:  warn"));
        assert!(body.contains("Disk cache at 82%"));
        assert!(body.contains("backend: \"primary\""));
        assert!(body.contains("pct: 82"));
    }

    #[test]
    fn missing_to_addresses_fail_build() {
        let cfg = EmailSinkConfig {
            host: "smtp.example.com".to_string(),
            port: 587,
            starttls: true,
            username: String::new(),
            password: String::new(),
            from: "alerts@example.com".to_string(),
            to: vec![],
            subject_prefix: None,
        };
        // EmailSink doesn't impl Debug (lettre's transport doesn't),
        // so match the Result by pattern instead of unwrap_err().
        match EmailSink::build("ops".to_string(), &cfg, "thurvtl") {
            Ok(_) => panic!("expected build to reject empty to: list"),
            Err(e) => assert!(format!("{e:#}").contains("at least one to:")),
        }
    }
}
