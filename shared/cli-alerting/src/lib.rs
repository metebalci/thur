// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product CLI implementations for the `system alerting`
//! verbs.
//!
//! Both verbs are daemon-routed only — `list` reads the live
//! dispatcher state via `GET /api/v1/system/alerting`, `test`
//! exercises one configured sink via the `system.alerting.test`
//! job. There is no daemon-down fallback: alerting config lives
//! inside the daemon YAML + memory, not on disk in a separate
//! file, so without a daemon there's nothing to talk to.

#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;
use shared_naming::ProductIdentity;

#[derive(Debug, Deserialize)]
pub struct AlertingListResponse {
    pub enabled: bool,
    pub dedup_window_seconds: u64,
    pub sinks: Vec<AlertingSinkRow>,
}

#[derive(Debug, Deserialize)]
pub struct AlertingSinkRow {
    pub name: String,
    pub r#type: String,
}

pub async fn list(product: &'static ProductIdentity, json: bool) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    if !admin.ping().await {
        bail!(
            "{} admin socket {} unreachable — start the daemon first",
            product.name,
            admin.socket_path().display(),
        );
    }
    let resp: AlertingListResponse = admin
        .get_json("/api/v1/system/alerting")
        .await
        .context("GET /api/v1/system/alerting")?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "enabled": resp.enabled,
                "dedup_window_seconds": resp.dedup_window_seconds,
                "sinks": resp.sinks.iter().map(|s| serde_json::json!({
                    "name": s.name,
                    "type": s.r#type,
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    if !resp.enabled {
        println!(
            "alerting: disabled (set `alerting.enabled: true` in {} to turn on)",
            product.config_path
        );
        return Ok(());
    }
    println!(
        "alerting: enabled (dedup window {} s)",
        resp.dedup_window_seconds
    );
    if resp.sinks.is_empty() {
        println!("  (no sinks configured)");
        return Ok(());
    }
    println!("  Sinks:");
    for s in &resp.sinks {
        println!("    - {} ({})", s.name, s.r#type);
    }
    Ok(())
}

pub async fn test(
    product: &'static ProductIdentity,
    sink_name: &str,
    severity: &str,
) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    if !admin.ping().await {
        bail!(
            "{} admin socket {} unreachable — start the daemon first",
            product.name,
            admin.socket_path().display(),
        );
    }

    let body = serde_json::json!({
        "sink": sink_name,
        "severity": severity,
    });
    let exit = admin
        .run_job("system.alerting.test", &body, |ev| match ev {
            JobEvent::Log { message, .. } => println!("{}", message),
            JobEvent::Result { data } => {
                if let Ok(s) = serde_json::to_string_pretty(&data) {
                    println!("{}", s);
                }
            }
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await?;
    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_naming::{DISK, TAPE_LIBRARY};

    // ---------- wire-type serde ----------

    #[test]
    fn alerting_list_response_deserializes_full_shape() {
        let json = r#"{
            "enabled": true,
            "dedup_window_seconds": 300,
            "sinks": [
                {"name": "ops-email", "type": "email"},
                {"name": "pager", "type": "webhook"}
            ]
        }"#;
        let resp: AlertingListResponse = serde_json::from_str(json).unwrap();
        assert!(resp.enabled);
        assert_eq!(resp.dedup_window_seconds, 300);
        assert_eq!(resp.sinks.len(), 2);
        assert_eq!(resp.sinks[0].name, "ops-email");
        assert_eq!(resp.sinks[0].r#type, "email");
        assert_eq!(resp.sinks[1].r#type, "webhook");
    }

    #[test]
    fn alerting_list_response_disabled_no_sinks() {
        let json = r#"{"enabled": false, "dedup_window_seconds": 0, "sinks": []}"#;
        let resp: AlertingListResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.enabled);
        assert!(resp.sinks.is_empty());
    }

    #[test]
    fn alerting_sink_row_type_field_uses_raw_keyword() {
        // `type` is a Rust keyword; the wire field must still bind via
        // the raw identifier `r#type`.
        let row: AlertingSinkRow =
            serde_json::from_str(r#"{"name":"s","type":"webhook"}"#).unwrap();
        assert_eq!(row.name, "s");
        assert_eq!(row.r#type, "webhook");
    }

    // ---------- daemon-down refusal ----------

    /// Whether the product's discovered admin socket is live. The
    /// refusal-path tests only assert when no daemon is reachable.
    async fn daemon_absent(product: &'static ProductIdentity) -> bool {
        !AdminClient::auto_discover(product).ping().await
    }

    #[tokio::test]
    async fn list_bails_when_daemon_unreachable() {
        if !daemon_absent(&TAPE_LIBRARY).await {
            return; // a real daemon is up; skip the refusal assertion
        }
        let err = list(&TAPE_LIBRARY, false).await.unwrap_err().to_string();
        assert!(
            err.contains("unreachable") && err.contains("start the daemon"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_bails_when_daemon_unreachable() {
        if !daemon_absent(&DISK).await {
            return;
        }
        let err = test(&DISK, "some-sink", "warning")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unreachable"), "got: {err}");
    }
}
