// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `<product> system audit` — daemon-routed audit log subcommands.
//!
//! The daemon is the steady-state owner of `<data_dir>/audit/*.jsonl`;
//! routing tail / export / verify / rotate through the admin socket
//! removes the cross-process `fs2` lock the CLI used to take and
//! gives `--follow` mode a single source of truth. Generic over
//! `ProductIdentity` so `thurvtl` and `thurvsa` share one
//! implementation.
//!
//! Exit codes per `docs/SPEC.md`:
//!   0 — success / chain valid
//!   1 — chain break detected (verify) / refused without accept_break (rotate)
//!   2 — IO / file-missing error / transport error

use std::path::Path;

use anyhow::{Context, Result};
use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;
use shared_audit::verify_chain;
use shared_naming::ProductIdentity;

pub async fn cmd_tail(product: &'static ProductIdentity, follow: bool, lines: usize) -> Result<u8> {
    let client = AdminClient::auto_discover(product);
    let body = serde_json::json!({"follow": follow, "lines": lines});
    let exit = client
        .run_job("system.audit.tail", &body, relay_event)
        .await
        .context("audit tail stream")?;
    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

pub async fn cmd_export(
    product: &'static ProductIdentity,
    format: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<u8> {
    let client = AdminClient::auto_discover(product);
    let body = serde_json::json!({
        "format": format,
        "from": from,
        "to": to,
    });
    // Export emits one log event per audit row — those land on stdout
    // unchanged so a redirect captures the JSONL/CSV cleanly.
    let exit = client
        .run_job("system.audit.export", &body, |ev| {
            if let JobEvent::Log { level, message } = ev {
                if level == "warn" || level == "error" {
                    eprintln!("{}", message);
                } else {
                    println!("{}", message);
                }
            }
        })
        .await
        .context("audit export stream")?;
    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

pub async fn cmd_verify(product: &'static ProductIdentity) -> Result<u8> {
    let client = AdminClient::auto_discover(product);
    let exit = client
        .run_job("system.audit.verify", &serde_json::json!({}), |ev| {
            relay_event(ev)
        })
        .await
        .context("audit verify stream")?;
    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

/// Verify an audit directory offline (no daemon). Walks every JSONL
/// file under `dir`, recomputing the BLAKE3 chain.
///
/// Exit codes: 0 chain valid, 1 break detected, 2 I/O or parse error.
pub fn cmd_verify_offline(dir: &Path, json: bool) -> Result<u8> {
    let report = match verify_chain(dir) {
        Ok(r) => r,
        Err(e) => {
            // Distinguish chain breaks (exit 1) from filesystem /
            // parse errors (exit 2) so automation can react.
            let msg = e.to_string();
            let exit = if matches!(e, shared_audit::AuditError::ChainBroken { .. }) {
                1
            } else {
                2
            };
            if json {
                let v = serde_json::json!({
                    "ok": false,
                    "error": msg,
                });
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                eprintln!("audit verify-offline: {msg}");
            }
            return Ok(exit);
        }
    };

    if json {
        let v = serde_json::json!({
            "ok": true,
            "entries_checked": report.entries_checked,
            "last_seq": report.last_seq,
            "last_hash": report.last_hash,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "audit verify-offline: OK ({} entries, last_seq={}, last_hash={})",
            report.entries_checked, report.last_seq, report.last_hash,
        );
    }
    Ok(0)
}

pub async fn cmd_rotate(product: &'static ProductIdentity, accept_break: bool) -> Result<u8> {
    if !accept_break {
        eprintln!(
            "audit rotate: refusing without --accept-break.\n\
             This command writes a chain_reset entry that permanently records \
             a break in the audit chain. Confirm with --accept-break to proceed."
        );
        return Ok(1);
    }
    let client = AdminClient::auto_discover(product);
    let body = serde_json::json!({"accept_break": true});
    let exit = client
        .run_job("system.audit.rotate", &body, relay_event)
        .await
        .context("audit rotate stream")?;
    Ok(u8::try_from(exit.max(0)).unwrap_or(2))
}

fn relay_event(ev: JobEvent) {
    match ev {
        JobEvent::Log { level, message } => {
            if level == "warn" || level == "error" {
                eprintln!("{}", message);
            } else {
                println!("{}", message);
            }
        }
        JobEvent::Result { .. } | JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
    }
}
