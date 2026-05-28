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
    volumes: Option<&[String]>,
) -> Result<()> {
    let password = resolve_password(password_arg, password_stdin)?;
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let mut body = serde_json::json!({
        "username": name,
        "password": password,
        "mutual_chap": mutual_chap,
        "partition": partition,
    });
    if let Some(vs) = volumes {
        body["volumes"] = serde_json::json!(vs);
    }
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

pub async fn users_grant(
    product: &'static ProductIdentity,
    name: &str,
    volumes: &[String],
) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({ "name": name, "volumes": volumes });
    let _row: UserRow = admin.post_json("/api/v1/iscsi/users/grant", &body).await?;
    println!(
        "OK: user '{name}' granted access to: {}",
        volumes.join(", ")
    );
    Ok(())
}

pub async fn users_revoke(
    product: &'static ProductIdentity,
    name: &str,
    volumes: &[String],
) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    require_daemon(product, &admin).await?;
    let body = serde_json::json!({ "name": name, "volumes": volumes });
    let _row: UserRow = admin.post_json("/api/v1/iscsi/users/revoke", &body).await?;
    println!(
        "OK: user '{name}' revoked access to: {}",
        volumes.join(", ")
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- parse_grace ----------

    #[test]
    fn parse_grace_hours_minutes_days() {
        assert_eq!(parse_grace("24h").unwrap(), 24 * 3600);
        assert_eq!(parse_grace("5m").unwrap(), 5 * 60);
        assert_eq!(parse_grace("1d").unwrap(), 24 * 3600);
        assert_eq!(parse_grace("30s").unwrap(), 30);
    }

    #[test]
    fn parse_grace_compound_string() {
        assert_eq!(parse_grace("1d12h").unwrap(), 36 * 3600);
        assert_eq!(parse_grace("1h30m").unwrap(), 5400);
    }

    #[test]
    fn parse_grace_rejects_zero() {
        let err = parse_grace("0s").unwrap_err().to_string();
        assert!(err.contains("must be > 0"), "got: {err}");
    }

    #[test]
    fn parse_grace_rejects_garbage() {
        for bad in ["", "abc", "12", "5x", "h"] {
            let err = parse_grace(bad).unwrap_err().to_string();
            assert!(err.contains("invalid --grace"), "input {bad:?} gave: {err}");
        }
    }

    // ---------- resolve_password ----------

    #[test]
    fn resolve_password_returns_arg_value() {
        let p = resolve_password(Some("hunter2"), false).unwrap();
        assert_eq!(p, "hunter2");
    }

    #[test]
    fn resolve_password_rejects_both_sources() {
        let err = resolve_password(Some("x"), true).unwrap_err().to_string();
        assert!(err.contains("not both"), "got: {err}");
    }

    #[test]
    fn resolve_password_rejects_no_source() {
        let err = resolve_password(None, false).unwrap_err().to_string();
        assert!(
            err.contains("--password VALUE or --password-stdin"),
            "got: {err}"
        );
    }

    // ---------- wire-type serde round-trips ----------

    #[test]
    fn user_row_serde_round_trip() {
        let json = r#"{
            "username": "alice",
            "mutual_chap": true,
            "partition": "p1",
            "disabled": false,
            "in_grace": true,
            "previous_expires_at": "2026-05-22T10:00:00Z"
        }"#;
        let row: UserRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.username, "alice");
        assert!(row.mutual_chap);
        assert_eq!(row.partition.as_deref(), Some("p1"));
        assert!(row.in_grace);
        assert!(row.previous_expires_at.is_some());
        // Re-serialize and parse back — fields must survive the trip.
        let again: UserRow = serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
        assert_eq!(again.username, "alice");
    }

    #[test]
    fn user_row_accepts_null_partition_and_grace() {
        let json = r#"{
            "username": "bob",
            "mutual_chap": false,
            "partition": null,
            "disabled": true,
            "in_grace": false,
            "previous_expires_at": null
        }"#;
        let row: UserRow = serde_json::from_str(json).unwrap();
        assert!(row.partition.is_none());
        assert!(row.previous_expires_at.is_none());
        assert!(row.disabled);
    }

    #[test]
    fn users_list_response_deserializes() {
        let json = r#"{"users":[]}"#;
        let resp: UsersListResponse = serde_json::from_str(json).unwrap();
        assert!(resp.users.is_empty());
    }

    #[test]
    fn target_show_response_serde() {
        let set: TargetShowResponse =
            serde_json::from_str(r#"{"username":"t","password_set":true}"#).unwrap();
        assert_eq!(set.username.as_deref(), Some("t"));
        assert!(set.password_set);

        let unset: TargetShowResponse =
            serde_json::from_str(r#"{"username":null,"password_set":false}"#).unwrap();
        assert!(unset.username.is_none());
        assert!(!unset.password_set);
    }

    // ---------- print helpers (smoke; confirm no panic on shapes) ----------

    #[test]
    fn print_users_table_handles_empty_and_populated() {
        print_users_table(&[]);
        let rows = vec![
            UserRow {
                username: "alice".into(),
                mutual_chap: true,
                partition: Some("p1".into()),
                disabled: false,
                in_grace: true,
                previous_expires_at: Some(Utc::now()),
            },
            UserRow {
                username: "bob".into(),
                mutual_chap: false,
                partition: None,
                disabled: true,
                in_grace: false,
                previous_expires_at: None,
            },
        ];
        print_users_table(&rows);
    }

    #[test]
    fn print_target_handles_set_and_unset() {
        print_target(&TargetShowResponse {
            username: Some("tgt".into()),
            password_set: true,
        });
        print_target(&TargetShowResponse {
            username: None,
            password_set: false,
        });
    }

    // ---------- require_daemon ----------

    #[tokio::test]
    async fn require_daemon_refuses_when_socket_dead() {
        // A path with no daemon bound: ping() fails, so require_daemon
        // must bail with the daemon-routed refusal message.
        let dead = std::path::PathBuf::from("/tmp/thur-cli-iscsi-test-no-such.sock");
        let admin = AdminClient::new(dead, shared_naming::TAPE_LIBRARY.name);
        let err = require_daemon(&shared_naming::TAPE_LIBRARY, &admin)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("admin socket unreachable") && err.contains("daemon-routed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn require_daemon_message_names_the_socket_path() {
        let dead = std::path::PathBuf::from("/tmp/thur-cli-iscsi-test-named.sock");
        let admin = AdminClient::new(dead, shared_naming::DISK.name);
        let err = require_daemon(&shared_naming::DISK, &admin)
            .await
            .unwrap_err()
            .to_string();
        // The refusal embeds the socket path so an operator with
        // multiple data_dirs can tell which daemon to start.
        assert!(err.contains("thur-cli-iscsi-test-named.sock"), "got: {err}");
    }
}
