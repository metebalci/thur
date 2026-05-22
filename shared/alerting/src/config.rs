// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! YAML schema for the `alerting:` block.
//!
//! Deserializes a tagged-enum list of named sinks. The shape mirrors
//! `shared_keystore`'s `keystore.backends:` schema — `type:` is the
//! discriminator, per-type config flows in alongside it.

use std::collections::HashMap;

use serde::Deserialize;

/// Top-level `alerting:` block in `thur{vtl,vsa}.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct AlertingConfig {
    /// Off by default. Operators flip to `true` and add at least one
    /// sink before alerts fire.
    #[serde(default)]
    pub enabled: bool,

    /// Per-event-class dedup window. Same key drops repeats inside
    /// the window. Also gates the CHAP-failure-per-user threshold
    /// (the window resets the counter).
    #[serde(default = "default_dedup_window_seconds")]
    pub dedup_window_seconds: u64,

    /// CHAP failure WARN fires after this many failures from the
    /// same user inside one dedup window. 0 disables the alert
    /// (still individually emits per audit row).
    #[serde(default = "default_chap_failures_threshold")]
    pub chap_failures_threshold: u32,

    /// Per-event-class on/off knobs. Unset = on for audit_failure /
    /// chap_failures (high-signal), off for backend_reachability /
    /// disk_cache_backpressure (noisier).
    #[serde(default)]
    pub events: EventsConfig,

    /// Named sinks. Empty when `enabled: false`; at least one when
    /// `enabled: true`.
    #[serde(default)]
    pub sinks: Vec<SinkSpec>,
}

impl Default for AlertingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dedup_window_seconds: default_dedup_window_seconds(),
            chap_failures_threshold: default_chap_failures_threshold(),
            events: EventsConfig::default(),
            sinks: Vec::new(),
        }
    }
}

pub const fn default_dedup_window_seconds() -> u64 {
    300
}

pub const fn default_chap_failures_threshold() -> u32 {
    3
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventsConfig {
    #[serde(default = "default_event_off")]
    pub backend_reachability: bool,
    #[serde(default = "default_event_enabled")]
    pub audit_failure: bool,
    #[serde(default = "default_event_off")]
    pub disk_cache_backpressure: bool,
    #[serde(default = "default_event_enabled")]
    pub chap_failures: bool,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            backend_reachability: default_event_off(),
            audit_failure: default_event_enabled(),
            disk_cache_backpressure: default_event_off(),
            chap_failures: default_event_enabled(),
        }
    }
}

const fn default_event_enabled() -> bool {
    true
}

const fn default_event_off() -> bool {
    false
}

/// One named sink — operator-chosen `name:` plus the type-discriminated
/// per-sink config.
#[derive(Debug, Clone, Deserialize)]
pub struct SinkSpec {
    pub name: String,
    #[serde(flatten)]
    pub config: SinkConfig,
}

/// Tagged-enum dispatch on `type:`. Mirrors
/// `shared_keystore::KeyStoreBackend` shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SinkConfig {
    Email(EmailSinkConfig),
    Webhook(WebhookSinkConfig),
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailSinkConfig {
    pub host: String,
    pub port: u16,
    /// When true, the connection negotiates STARTTLS on the
    /// submission port (typical: 587). When false, plain TCP with
    /// no encryption — only safe inside a trusted network.
    #[serde(default = "default_starttls")]
    pub starttls: bool,
    /// SMTP AUTH username. Empty disables AUTH entirely (sendmail-
    /// relay style — only safe when the relay accepts unauthenticated
    /// connections from this host's IP).
    #[serde(default)]
    pub username: String,
    /// SMTP AUTH password. Supports `${ENV_VAR}` interpolation at
    /// config-load time; the raw string never lives in memory longer
    /// than the dispatcher needs it.
    #[serde(default)]
    pub password: String,
    pub from: String,
    pub to: Vec<String>,
    /// Optional subject prefix. Default `[thurvtl ALERT]` / equivalent.
    #[serde(default)]
    pub subject_prefix: Option<String>,
}

const fn default_starttls() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookSinkConfig {
    pub url: String,
    /// HTTP method. Default POST.
    #[serde(default = "default_method")]
    pub method: String,
    /// Extra headers (Content-Type, Authorization, X-Routing-Key, …).
    /// Values support `${ENV_VAR}` interpolation. Content-Type
    /// defaults to `application/json` when unset.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Per-sink Tera template rendered with the Alert struct fields.
    /// When empty, the canonical JSON payload is sent as-is. Tera
    /// vars: `class`, `severity`, `message`, `timestamp`, `product`,
    /// `version`, plus everything under `fields.*`.
    #[serde(default)]
    pub body_template: String,
    /// Request timeout. Default 10 s.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_method() -> String {
    "POST".to_string()
}

const fn default_timeout_seconds() -> u64 {
    10
}

/// Apply `${ENV_VAR}` interpolation to a string. Returns the input
/// unchanged when no `${...}` pattern is present. Errors if a
/// referenced variable is unset (so a misconfigured sink fails fast
/// at boot, not at first alert).
pub(crate) fn resolve_env(input: &str) -> anyhow::Result<String> {
    if !input.contains("${") {
        return Ok(input.to_string());
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow::anyhow!("unterminated ${{...}} in '{input}'"))?;
        let var = &after[..end];
        let value = std::env::var(var)
            .map_err(|_| anyhow::anyhow!("env var '{var}' referenced by config is unset"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_email_sink() {
        let yaml = r#"
enabled: true
sinks:
  - name: ops
    type: email
    host: smtp.example.com
    port: 587
    from: alerts@example.com
    to: [ops@example.com]
"#;
        let cfg: AlertingConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.sinks.len(), 1);
        assert_eq!(cfg.sinks[0].name, "ops");
        match &cfg.sinks[0].config {
            SinkConfig::Email(e) => {
                assert_eq!(e.host, "smtp.example.com");
                assert!(e.starttls);
            }
            _ => panic!("expected email sink"),
        }
    }

    #[test]
    fn parses_webhook_with_template() {
        let yaml = r#"
enabled: true
sinks:
  - name: pagerduty
    type: webhook
    url: https://events.pagerduty.com/v2/enqueue
    body_template: '{"summary": "{{message}}"}'
"#;
        let cfg: AlertingConfig = serde_yaml::from_str(yaml).unwrap();
        match &cfg.sinks[0].config {
            SinkConfig::Webhook(w) => {
                assert_eq!(w.method, "POST");
                assert_eq!(w.timeout_seconds, 10);
                assert!(w.body_template.contains("{{message}}"));
            }
            _ => panic!("expected webhook sink"),
        }
    }

    #[test]
    fn defaults_when_block_absent() {
        let cfg: AlertingConfig = serde_yaml::from_str("").unwrap_or_default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.dedup_window_seconds, 300);
        assert_eq!(cfg.chap_failures_threshold, 3);
    }

    #[test]
    fn env_var_passthrough_when_no_interpolation() {
        assert_eq!(resolve_env("plain string").unwrap(), "plain string");
    }

    #[test]
    fn env_var_unset_errors() {
        let err = resolve_env("${ALERT_NONEXISTENT_VAR_XYZ}").unwrap_err();
        assert!(err.to_string().contains("ALERT_NONEXISTENT_VAR_XYZ"));
    }

    #[test]
    fn env_var_unterminated_errors() {
        let err = resolve_env("Bearer ${NOPE").unwrap_err();
        assert!(err.to_string().contains("unterminated"));
    }
}
