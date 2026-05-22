// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product CLI implementations for `iscsi users` and
//! `iscsi target` verbs.
//!
//! Each helper is parameterized on `&'static ProductIdentity` so the
//! admin-socket discovery and the daemon name in the
//! socket-unreachable refusal both derive from the per-product
//! identity. The data-path posture is daemon-routed only: the admin
//! socket must answer, the daemon serializes the edit and emits an
//! audit row. When the socket is down the command refuses with a
//! clear "start the daemon" message rather than mutating the JSON
//! file directly behind the daemon's back.

#![forbid(unsafe_code)]

use std::io::BufRead;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared_admin_client::AdminClient;
use shared_naming::ProductIdentity;

// ---------- wire types (CLI-side) ----------

#[derive(Debug, Deserialize, Serialize)]
pub struct UserRow {
    pub username: String,
    pub mutual_chap: bool,
    pub partition: Option<String>,
    pub disabled: bool,
    pub in_grace: bool,
    pub previous_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UsersListResponse {
    pub users: Vec<UserRow>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TargetShowResponse {
    pub username: Option<String>,
    pub password_set: bool,
}

// ---------- iSCSI user verbs ----------

pub async fn users_list(product: &'static ProductIdentity, json: bool) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let resp: UsersListResponse = admin.get_json("/api/v1/iscsi/users").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        print_users_table(&resp.users);
    }
    Ok(())
}

pub async fn users_add(
    product: &'static ProductIdentity,
    name: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
    mutual_chap: bool,
    partition: Option<&str>,
) -> Result<()> {
    let password = resolve_password(password_arg, password_stdin)?;
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({
        "username": name,
        "password": password,
        "mutual_chap": mutual_chap,
        "partition": partition,
    });
    let _row: UserRow = admin.post_json("/api/v1/iscsi/users", &body).await?;
    println!("OK: user '{name}' added");
    Ok(())
}

pub async fn users_remove(product: &'static ProductIdentity, name: &str) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({ "name": name });
    admin.post_unit("/api/v1/iscsi/users/remove", &body).await?;
    println!("OK: user '{name}' removed");
    Ok(())
}

pub async fn users_set_disabled(
    product: &'static ProductIdentity,
    name: &str,
    disabled: bool,
) -> Result<()> {
    let verb = if disabled { "disable" } else { "enable" };
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({ "name": name });
    let path = format!("/api/v1/iscsi/users/{verb}");
    admin.post_unit(&path, &body).await?;
    println!("OK: user '{name}' {verb}d");
    Ok(())
}

pub async fn users_rotate(
    product: &'static ProductIdentity,
    name: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
    grace: &str,
) -> Result<()> {
    let password = resolve_password(password_arg, password_stdin)?;
    let grace_secs = parse_grace(grace)?;
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({
        "name": name,
        "password": password,
        "grace_seconds": grace_secs,
    });
    let row: UserRow = admin.post_json("/api/v1/iscsi/users/rotate", &body).await?;
    let expires = row
        .previous_expires_at
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "?".to_string());
    println!(
        "OK: user '{name}' rotated; previous password honored until {expires} (grace {grace})"
    );
    Ok(())
}

pub async fn users_rotate_cancel(product: &'static ProductIdentity, name: &str) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({ "name": name });
    admin
        .post_unit("/api/v1/iscsi/users/rotate/cancel", &body)
        .await?;
    println!("OK: user '{name}' rotation cancelled; previous password restored");
    Ok(())
}

// ---------- mutual-CHAP target verbs ----------

pub async fn target_show(product: &'static ProductIdentity, json: bool) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let resp: TargetShowResponse = admin.get_json("/api/v1/iscsi/target").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        print_target(&resp);
    }
    Ok(())
}

pub async fn target_set(
    product: &'static ProductIdentity,
    username: &str,
    password_arg: Option<&str>,
    password_stdin: bool,
) -> Result<()> {
    let password = resolve_password(password_arg, password_stdin)?;
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({ "username": username, "password": password });
    admin.post_unit("/api/v1/iscsi/target", &body).await?;
    println!("OK: mutual-CHAP target credential set ({username})");
    Ok(())
}

pub async fn target_clear(product: &'static ProductIdentity) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    admin
        .post_unit("/api/v1/iscsi/target/clear", &serde_json::json!({}))
        .await?;
    println!("OK: mutual-CHAP target credential cleared");
    Ok(())
}

// ---------- helpers ----------

/// Refuse unless the daemon's admin socket answers. The `iscsi
/// users` / `iscsi target` verbs (and VSA's `nvmetcp psks` verbs,
/// which reuse this gate) are daemon-routed only — the daemon
/// serializes the credential edit and emits an audit row. Public so
/// VSA's PSK verbs can share the same refusal.
pub async fn require_daemon(product: &'static ProductIdentity, admin: &AdminClient) -> Result<()> {
    if admin.ping().await {
        return Ok(());
    }
    bail!(
        "{}d admin socket unreachable at {} — this command is daemon-routed \
         so the credential edit is serialized and audited. Start the daemon and retry.",
        product.metric_prefix,
        admin.socket_path().display()
    )
}

/// Read a password from `--password VALUE` (foreground) or
/// `--password-stdin` (script-friendly). Public so VSA's PSK verbs
/// can reuse the same posture.
pub fn resolve_password(arg: Option<&str>, stdin: bool) -> Result<String> {
    match (arg, stdin) {
        (Some(p), false) => Ok(p.to_string()),
        (None, true) => {
            let mut s = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut s)
                .context("reading password from stdin")?;
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            if s.is_empty() {
                bail!("--password-stdin: empty input");
            }
            Ok(s)
        }
        (Some(_), true) => bail!("pass either --password or --password-stdin, not both"),
        (None, false) => bail!("pass either --password VALUE or --password-stdin"),
    }
}

/// Parse a `--grace` humantime string ("24h", "5m", "1d12h"). Public
/// so VSA's PSK rotation can share the same parser.
pub fn parse_grace(s: &str) -> Result<u64> {
    let d: Duration = humantime::parse_duration(s)
        .with_context(|| format!("invalid --grace '{s}' (try '24h', '5m', '1d12h')"))?;
    let secs = d.as_secs();
    if secs == 0 {
        bail!("--grace must be > 0 (use add + remove for an immediate cutover)");
    }
    Ok(secs)
}

fn print_target(resp: &TargetShowResponse) {
    println!(
        "username: {}",
        resp.username.as_deref().unwrap_or("<unset>")
    );
    println!(
        "password: {}",
        if resp.password_set {
            "<set>"
        } else {
            "<unset>"
        }
    );
}

fn print_users_table(rows: &[UserRow]) {
    if rows.is_empty() {
        println!("(no users)");
        return;
    }
    println!(
        "{:<24} {:<8} {:<14} {:<14} GRACE-UNTIL",
        "USERNAME", "MUTUAL", "PARTITION", "STATE"
    );
    for r in rows {
        let state = if r.disabled { "disabled" } else { "active" };
        let part = r.partition.as_deref().unwrap_or("-");
        let grace = match (r.in_grace, r.previous_expires_at) {
            (true, Some(t)) => t.to_rfc3339(),
            _ => "-".to_string(),
        };
        println!(
            "{:<24} {:<8} {:<14} {:<14} {}",
            r.username,
            if r.mutual_chap { "yes" } else { "no" },
            part,
            state,
            grace
        );
    }
}
