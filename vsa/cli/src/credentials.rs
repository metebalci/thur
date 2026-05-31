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
use nvme_tcp::auth::parse_dhchap_secret;
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

// ---------- nvmetcp psks + dhchap (VSA only) ----------
//
// The TLS-PSK and DH-HMAC-CHAP CLI surfaces are daemon-routed mirrors:
// build a JSON body, POST it to the admin socket, print an OK line. The
// only per-surface differences — the API base path, the secret-format
// validator, and the noun used in the OK messages — are captured in a
// [`CredSurface`] descriptor so the two can't drift (issue #70). The
// daemon enforces the actual lifecycle; this layer is presentation.

struct CredSurface {
    /// Admin API base path, e.g. `/api/v1/nvmetcp/psks`.
    base: &'static str,
    /// Noun for OK messages, e.g. `PSK` / `DH-HMAC-CHAP secret`.
    noun: &'static str,
    /// Secret word used in the rotate messages: `key` / `secret`.
    secret_word: &'static str,
    /// Secret-format validator (interchange string / `DHHC-1:` secret),
    /// returning the parser's error text on rejection.
    validate: fn(&str) -> std::result::Result<(), String>,
}

const PSKS: CredSurface = CredSurface {
    base: "/api/v1/nvmetcp/psks",
    noun: "PSK",
    secret_word: "key",
    validate: v_interchange,
};

const DHCHAP: CredSurface = CredSurface {
    base: "/api/v1/nvmetcp/dhchap",
    noun: "DH-HMAC-CHAP secret",
    secret_word: "secret",
    validate: v_dhchap,
};

fn v_interchange(s: &str) -> std::result::Result<(), String> {
    parse_interchange_key(s)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn v_dhchap(s: &str) -> std::result::Result<(), String> {
    parse_dhchap_secret(s)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Discover the admin socket and refuse if the daemon is down — these
/// credential verbs are daemon-routed only so the edit is always
/// serialized + audited.
async fn connect() -> Result<AdminClient> {
    let admin = AdminClient::auto_discover(PRODUCT);
    shared_cli_iscsi::require_daemon(PRODUCT, &admin).await?;
    Ok(admin)
}

async fn cred_list<T>(base: &str, json: bool, print_table: impl Fn(&T)) -> Result<()>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let admin = connect().await?;
    let resp: T = admin.get_json(base).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        print_table(&resp);
    }
    Ok(())
}

async fn cred_add(
    s: &CredSurface,
    host_nqn: &str,
    key: &str,
    ctrl_key: Option<&str>,
    volumes: Option<&[String]>,
) -> Result<()> {
    (s.validate)(key).map_err(|e| anyhow!("invalid --key: {e}"))?;
    if let Some(c) = ctrl_key {
        (s.validate)(c).map_err(|e| anyhow!("invalid --ctrl-key: {e}"))?;
    }
    let admin = connect().await?;
    let mut body = serde_json::json!({ "host_nqn": host_nqn, "key": key });
    if let Some(c) = ctrl_key {
        body["ctrl_key"] = serde_json::json!(c);
    }
    if let Some(vs) = volumes {
        body["volumes"] = serde_json::json!(vs);
    }
    let _: serde_json::Value = admin.post_json(s.base, &body).await?;
    println!("OK: {} for host '{host_nqn}' added", s.noun);
    Ok(())
}

async fn cred_grant(s: &CredSurface, host_nqn: &str, volumes: &[String]) -> Result<()> {
    let admin = connect().await?;
    let body = serde_json::json!({ "host_nqn": host_nqn, "volumes": volumes });
    let _: serde_json::Value = admin.post_json(&format!("{}/grant", s.base), &body).await?;
    println!(
        "OK: host '{host_nqn}' granted access to: {}",
        volumes.join(", ")
    );
    Ok(())
}

async fn cred_revoke(s: &CredSurface, host_nqn: &str, volumes: &[String]) -> Result<()> {
    let admin = connect().await?;
    let body = serde_json::json!({ "host_nqn": host_nqn, "volumes": volumes });
    let _: serde_json::Value = admin
        .post_json(&format!("{}/revoke", s.base), &body)
        .await?;
    println!(
        "OK: host '{host_nqn}' revoked access to: {}",
        volumes.join(", ")
    );
    Ok(())
}

async fn cred_remove(s: &CredSurface, host_nqn: &str) -> Result<()> {
    let admin = connect().await?;
    let body = serde_json::json!({ "host_nqn": host_nqn });
    admin
        .post_unit(&format!("{}/remove", s.base), &body)
        .await?;
    println!("OK: {} for host '{host_nqn}' removed", s.noun);
    Ok(())
}

async fn cred_set_disabled(s: &CredSurface, host_nqn: &str, disabled: bool) -> Result<()> {
    let verb = if disabled { "disable" } else { "enable" };
    let admin = connect().await?;
    let body = serde_json::json!({ "host_nqn": host_nqn });
    admin
        .post_unit(&format!("{}/{verb}", s.base), &body)
        .await?;
    println!("OK: {} for host '{host_nqn}' {verb}d", s.noun);
    Ok(())
}

async fn cred_rotate(s: &CredSurface, host_nqn: &str, key: &str, grace: &str) -> Result<()> {
    (s.validate)(key).map_err(|e| anyhow!("invalid --key: {e}"))?;
    let grace_secs = shared_cli_iscsi::parse_grace(grace)?;
    let admin = connect().await?;
    let body = serde_json::json!({
        "host_nqn": host_nqn,
        "key": key,
        "grace_seconds": grace_secs,
    });
    let row: RotateRow = admin
        .post_json(&format!("{}/rotate", s.base), &body)
        .await?;
    let expires = row
        .previous_expires_at
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "?".to_string());
    println!(
        "OK: {} for host '{host_nqn}' rotated; previous {} honored until {expires} (grace {grace})",
        s.noun, s.secret_word
    );
    Ok(())
}

async fn cred_rotate_cancel(s: &CredSurface, host_nqn: &str) -> Result<()> {
    let admin = connect().await?;
    let body = serde_json::json!({ "host_nqn": host_nqn });
    admin
        .post_unit(&format!("{}/rotate/cancel", s.base), &body)
        .await?;
    println!(
        "OK: {} for host '{host_nqn}' rotation cancelled; previous {} restored",
        s.noun, s.secret_word
    );
    Ok(())
}

// ---------- nvmetcp psks (VSA only) ----------

pub async fn psks_list(json: bool) -> Result<()> {
    cred_list(PSKS.base, json, |r: &PsksListResponse| {
        print_psks_table(&r.psks)
    })
    .await
}

pub async fn psks_add(host_nqn: &str, key: &str, volumes: Option<&[String]>) -> Result<()> {
    cred_add(&PSKS, host_nqn, key, None, volumes).await
}

pub async fn psks_grant(host_nqn: &str, volumes: &[String]) -> Result<()> {
    cred_grant(&PSKS, host_nqn, volumes).await
}

pub async fn psks_revoke(host_nqn: &str, volumes: &[String]) -> Result<()> {
    cred_revoke(&PSKS, host_nqn, volumes).await
}

pub async fn psks_remove(host_nqn: &str) -> Result<()> {
    cred_remove(&PSKS, host_nqn).await
}

pub async fn psks_set_disabled(host_nqn: &str, disabled: bool) -> Result<()> {
    cred_set_disabled(&PSKS, host_nqn, disabled).await
}

pub async fn psks_rotate(host_nqn: &str, key: &str, grace: &str) -> Result<()> {
    cred_rotate(&PSKS, host_nqn, key, grace).await
}

pub async fn psks_rotate_cancel(host_nqn: &str) -> Result<()> {
    cred_rotate_cancel(&PSKS, host_nqn).await
}

// ---------- nvmetcp dhchap (VSA only) ----------

pub async fn dhchap_list(json: bool) -> Result<()> {
    cred_list(DHCHAP.base, json, |r: &DhchapListResponse| {
        print_dhchap_table(&r.dhchap)
    })
    .await
}

pub async fn dhchap_add(
    host_nqn: &str,
    key: &str,
    ctrl_key: Option<&str>,
    volumes: Option<&[String]>,
) -> Result<()> {
    cred_add(&DHCHAP, host_nqn, key, ctrl_key, volumes).await
}

pub async fn dhchap_grant(host_nqn: &str, volumes: &[String]) -> Result<()> {
    cred_grant(&DHCHAP, host_nqn, volumes).await
}

pub async fn dhchap_revoke(host_nqn: &str, volumes: &[String]) -> Result<()> {
    cred_revoke(&DHCHAP, host_nqn, volumes).await
}

pub async fn dhchap_remove(host_nqn: &str) -> Result<()> {
    cred_remove(&DHCHAP, host_nqn).await
}

pub async fn dhchap_set_disabled(host_nqn: &str, disabled: bool) -> Result<()> {
    cred_set_disabled(&DHCHAP, host_nqn, disabled).await
}

pub async fn dhchap_rotate(host_nqn: &str, key: &str, grace: &str) -> Result<()> {
    cred_rotate(&DHCHAP, host_nqn, key, grace).await
}

pub async fn dhchap_rotate_cancel(host_nqn: &str) -> Result<()> {
    cred_rotate_cancel(&DHCHAP, host_nqn).await
}

// DH-HMAC-CHAP-only: controller secret (mutual auth). No PSK analog.

pub async fn dhchap_set_ctrl_key(host_nqn: &str, key: &str) -> Result<()> {
    (DHCHAP.validate)(key).map_err(|e| anyhow!("invalid --key: {e}"))?;
    let admin = connect().await?;
    let body = serde_json::json!({ "host_nqn": host_nqn, "ctrl_key": key });
    let _: serde_json::Value = admin
        .post_json(&format!("{}/ctrl-key/set", DHCHAP.base), &body)
        .await?;
    println!("OK: controller secret set for host '{host_nqn}' (mutual auth enabled)");
    Ok(())
}

pub async fn dhchap_clear_ctrl_key(host_nqn: &str) -> Result<()> {
    let admin = connect().await?;
    let body = serde_json::json!({ "host_nqn": host_nqn });
    admin
        .post_unit(&format!("{}/ctrl-key/clear", DHCHAP.base), &body)
        .await?;
    println!("OK: controller secret cleared for host '{host_nqn}' (mutual auth disabled)");
    Ok(())
}

// ---------- helpers ----------

// Re-export the shared helpers so tests + future PSK extensions can
// reach them without renaming.
#[cfg(test)]
use shared_cli_iscsi::{parse_grace, resolve_password};

// Minimal rotate-response view — both surfaces' rows carry
// `previous_expires_at`; serde ignores the rest.
#[derive(serde::Deserialize)]
struct RotateRow {
    previous_expires_at: Option<DateTime<Utc>>,
}

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

// ---------- DH-HMAC-CHAP wire/table types ----------

#[derive(serde::Deserialize, serde::Serialize)]
struct DhchapRow {
    host_nqn: String,
    #[serde(default)]
    volumes: Option<Vec<String>>,
    mutual: bool,
    disabled: bool,
    in_grace: bool,
    previous_expires_at: Option<DateTime<Utc>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct DhchapListResponse {
    dhchap: Vec<DhchapRow>,
}

fn print_dhchap_table(rows: &[DhchapRow]) {
    if rows.is_empty() {
        println!("(no DH-HMAC-CHAP entries)");
        return;
    }
    println!(
        "{:<60} {:<10} {:<7} GRACE-UNTIL",
        "HOST_NQN", "STATE", "MUTUAL"
    );
    for r in rows {
        let state = if r.disabled { "disabled" } else { "active" };
        let mutual = if r.mutual { "yes" } else { "no" };
        let grace = match (r.in_grace, r.previous_expires_at) {
            (true, Some(t)) => t.to_rfc3339(),
            _ => "-".to_string(),
        };
        println!("{:<60} {:<10} {:<7} {}", r.host_nqn, state, mutual, grace);
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
