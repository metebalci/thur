// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! CLI implementations for `iscsi users`, `iscsi target`, and
//! `nvmetcp psks` verbs.
//!
//! `users_*` and `target_*` are thin trampolines over the
//! cross-product [`shared_cli_iscsi`] helpers — VTL and VSA share the
//! daemon-routed-only posture, wire shapes, and audit op names.
//!
//! `psks_*` is VSA-only (NVMe-TCP TLS-PSK lifecycle) — VTL has no
//! NVMe-TCP transport. Same posture as the iSCSI users surface:
//! daemon-routed only, refusing when the admin socket is down so the
//! credential edit is always serialized + audited.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use nvme_tcp::tls::parse_interchange_key;
use shared_admin_client::AdminClient;

const PRODUCT: &shared_naming::ProductIdentity = &shared_naming::DISK;

// ---------- iSCSI users + target verbs (shared with VTL) ----------

pub async fn users_list(json: bool) -> Result<()> {
    shared_cli_iscsi::users_list(PRODUCT, json).await
}

pub async fn users_add(
    name: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
    mutual_chap: bool,
    partition: Option<&str>,
    volumes: Option<&[String]>,
) -> Result<()> {
    shared_cli_iscsi::users_add(
        PRODUCT,
        name,
        password_arg,
        password_stdin,
        mutual_chap,
        partition,
        volumes,
    )
    .await
}

pub async fn users_remove(name: &str) -> Result<()> {
    shared_cli_iscsi::users_remove(PRODUCT, name).await
}

pub async fn users_grant(name: &str, volumes: &[String]) -> Result<()> {
    shared_cli_iscsi::users_grant(PRODUCT, name, volumes).await
}

pub async fn users_revoke(name: &str, volumes: &[String]) -> Result<()> {
    shared_cli_iscsi::users_revoke(PRODUCT, name, volumes).await
}

pub async fn users_set_disabled(name: &str, disabled: bool) -> Result<()> {
    shared_cli_iscsi::users_set_disabled(PRODUCT, name, disabled).await
}

pub async fn users_rotate(
    name: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
    grace: &str,
) -> Result<()> {
    shared_cli_iscsi::users_rotate(PRODUCT, name, password_arg, password_stdin, grace).await
}

pub async fn users_rotate_cancel(name: &str) -> Result<()> {
    shared_cli_iscsi::users_rotate_cancel(PRODUCT, name).await
}

pub async fn target_show(json: bool) -> Result<()> {
    shared_cli_iscsi::target_show(PRODUCT, json).await
}

pub async fn target_set(
    username: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
) -> Result<()> {
    shared_cli_iscsi::target_set(PRODUCT, username, password_arg, password_stdin).await
}

pub async fn target_clear() -> Result<()> {
    shared_cli_iscsi::target_clear(PRODUCT).await
}

// ---------- nvmetcp psks (VSA only) ----------

pub async fn psks_list(json: bool) -> Result<()> {
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let resp: PsksListResponse = admin.get_json("/api/v1/nvmetcp/psks").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        print_psks_table(&resp.psks);
    }
    Ok(())
}

pub async fn psks_add(
    host_nqn: &str,
    interchange_key: &str,
    volumes: Option<&[String]>,
) -> Result<()> {
    parse_interchange_key(interchange_key).map_err(|e| anyhow!("invalid --key: {e}"))?;
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let mut body = serde_json::json!({
        "host_nqn": host_nqn,
        "interchange_key": interchange_key,
    });
    if let Some(vs) = volumes {
        body["volumes"] = serde_json::json!(vs);
    }
    let _: PskRow = admin.post_json("/api/v1/nvmetcp/psks", &body).await?;
    println!("OK: PSK for host '{host_nqn}' added");
    Ok(())
}

pub async fn psks_grant(host_nqn: &str, volumes: &[String]) -> Result<()> {
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let body = serde_json::json!({ "host_nqn": host_nqn, "volumes": volumes });
    let _: PskRow = admin.post_json("/api/v1/nvmetcp/psks/grant", &body).await?;
    println!(
        "OK: host '{host_nqn}' granted access to: {}",
        volumes.join(", ")
    );
    Ok(())
}

pub async fn psks_revoke(host_nqn: &str, volumes: &[String]) -> Result<()> {
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let body = serde_json::json!({ "host_nqn": host_nqn, "volumes": volumes });
    let _: PskRow = admin
        .post_json("/api/v1/nvmetcp/psks/revoke", &body)
        .await?;
    println!(
        "OK: host '{host_nqn}' revoked access to: {}",
        volumes.join(", ")
    );
    Ok(())
}

pub async fn psks_remove(host_nqn: &str) -> Result<()> {
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let body = serde_json::json!({ "host_nqn": host_nqn });
    admin
        .post_unit("/api/v1/nvmetcp/psks/remove", &body)
        .await?;
    println!("OK: PSK for host '{host_nqn}' removed");
    Ok(())
}

pub async fn psks_set_disabled(host_nqn: &str, disabled: bool) -> Result<()> {
    let verb = if disabled { "disable" } else { "enable" };
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let body = serde_json::json!({ "host_nqn": host_nqn });
    let path = format!("/api/v1/nvmetcp/psks/{verb}");
    admin.post_unit(&path, &body).await?;
    println!("OK: PSK for host '{host_nqn}' {verb}d");
    Ok(())
}

pub async fn psks_rotate(host_nqn: &str, interchange_key: &str, grace: &str) -> Result<()> {
    parse_interchange_key(interchange_key).map_err(|e| anyhow!("invalid --key: {e}"))?;
    let grace_secs = shared_cli_iscsi::parse_grace(grace)?;
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let body = serde_json::json!({
        "host_nqn": host_nqn,
        "interchange_key": interchange_key,
        "grace_seconds": grace_secs,
    });
    let row: PskRow = admin
        .post_json("/api/v1/nvmetcp/psks/rotate", &body)
        .await?;
    let expires = row
        .previous_expires_at
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "?".to_string());
    println!(
        "OK: PSK for host '{host_nqn}' rotated; previous key honored until {expires} (grace {grace})"
    );
    Ok(())
}

pub async fn psks_rotate_cancel(host_nqn: &str) -> Result<()> {
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    let body = serde_json::json!({ "host_nqn": host_nqn });
    admin
        .post_unit("/api/v1/nvmetcp/psks/rotate/cancel", &body)
        .await?;
    println!("OK: PSK for host '{host_nqn}' rotation cancelled; previous key restored");
    Ok(())
}

// ---------- helpers ----------

// Re-export the shared helpers so tests + future PSK extensions can
// reach them without renaming.
#[cfg(test)]
use shared_cli_iscsi::{parse_grace, resolve_password};

// ---------- PSK wire/table types ----------

#[derive(serde::Deserialize, serde::Serialize)]
struct PskRow {
    host_nqn: String,
    disabled: bool,
    in_grace: bool,
    previous_expires_at: Option<DateTime<Utc>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct PsksListResponse {
    psks: Vec<PskRow>,
}

fn print_psks_table(rows: &[PskRow]) {
    if rows.is_empty() {
        println!("(no PSKs)");
        return;
    }
    println!("{:<60} {:<10} GRACE-UNTIL", "HOST_NQN", "STATE");
    for r in rows {
        let state = if r.disabled { "disabled" } else { "active" };
        let grace = match (r.in_grace, r.previous_expires_at) {
            (true, Some(t)) => t.to_rfc3339(),
            _ => "-".to_string(),
        };
        println!("{:<60} {:<10} {}", r.host_nqn, state, grace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grace_accepts_humantime() {
        assert_eq!(parse_grace("24h").unwrap(), 24 * 3600);
        assert_eq!(parse_grace("5m").unwrap(), 300);
        assert_eq!(parse_grace("1d12h").unwrap(), 86400 + 43200);
        assert_eq!(parse_grace("30s").unwrap(), 30);
    }

    #[test]
    fn parse_grace_rejects_zero_and_garbage() {
        assert!(parse_grace("0s").is_err());
        assert!(parse_grace("hello").is_err());
        assert!(parse_grace("").is_err());
    }

    #[test]
    fn resolve_password_value_branch() {
        assert_eq!(
            resolve_password(Some("hunter2-secret"), false).unwrap(),
            "hunter2-secret"
        );
    }

    #[test]
    fn resolve_password_rejects_both_or_neither() {
        assert!(resolve_password(Some("x"), true).is_err());
        assert!(resolve_password(None, false).is_err());
    }
}
