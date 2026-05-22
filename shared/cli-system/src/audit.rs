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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use shared_audit::{AuditActor, AuditConfig, AuditLog, AuditMode, AuditResult};
    use shared_naming::TAPE_LIBRARY;

    /// Build a valid tamper-evident audit chain of `n` entries in
    /// `dir`. Returns the entry count actually written.
    fn seed_chain(dir: &Path, n: u64) -> u64 {
        let log = AuditLog::open(AuditConfig::new(dir, AuditMode::TamperEvident))
            .expect("open audit log");
        for i in 0..n {
            log.append(
                "test.op",
                AuditActor::system(),
                json!({ "i": i }),
                AuditResult::Ok,
            )
            .expect("append entry");
        }
        n
    }

    #[test]
    fn verify_offline_empty_dir_is_valid() {
        let tmp = tempfile::tempdir().unwrap();
        // No audit-*.jsonl files: verify_chain returns an empty report,
        // so the offline verify reports OK (exit 0).
        let exit = cmd_verify_offline(tmp.path(), false).unwrap();
        assert_eq!(exit, 0);
    }

    #[test]
    fn verify_offline_valid_chain_exit_zero_text() {
        let tmp = tempfile::tempdir().unwrap();
        seed_chain(tmp.path(), 5);
        let exit = cmd_verify_offline(tmp.path(), false).unwrap();
        assert_eq!(exit, 0);
    }

    #[test]
    fn verify_offline_valid_chain_exit_zero_json() {
        let tmp = tempfile::tempdir().unwrap();
        seed_chain(tmp.path(), 3);
        let exit = cmd_verify_offline(tmp.path(), true).unwrap();
        assert_eq!(exit, 0);
    }

    #[test]
    fn verify_offline_corrupted_chain_exit_one() {
        let tmp = tempfile::tempdir().unwrap();
        seed_chain(tmp.path(), 4);
        // Tamper with a chain entry so the BLAKE3 recompute mismatches.
        // verify_chain returns AuditError::ChainBroken -> exit code 1.
        let file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("audit-") && n.ends_with(".jsonl"))
                    .unwrap_or(false)
            })
            .expect("audit jsonl file");
        let contents = std::fs::read_to_string(&file).unwrap();
        let tampered = contents.replacen("\"i\":0", "\"i\":999", 1);
        assert_ne!(contents, tampered, "expected to mutate a chain entry");
        std::fs::write(&file, tampered).unwrap();

        let exit = cmd_verify_offline(tmp.path(), false).unwrap();
        assert_eq!(exit, 1);
        // The JSON branch must also classify the break as exit 1.
        let exit_json = cmd_verify_offline(tmp.path(), true).unwrap();
        assert_eq!(exit_json, 1);
    }

    #[test]
    fn verify_offline_unparsable_file_exit_two() {
        let tmp = tempfile::tempdir().unwrap();
        // A file matching the audit-*.jsonl glob but holding garbage:
        // read_all_entries fails to parse -> not a ChainBroken error,
        // so the offline verify maps it to exit 2.
        std::fs::write(
            tmp.path().join("audit-2026-05-22.jsonl"),
            b"this is not json\n",
        )
        .unwrap();
        let exit = cmd_verify_offline(tmp.path(), false).unwrap();
        assert_eq!(exit, 2);
        let exit_json = cmd_verify_offline(tmp.path(), true).unwrap();
        assert_eq!(exit_json, 2);
    }

    #[tokio::test]
    async fn rotate_refuses_without_accept_break() {
        // The refusal fires before any admin-socket await, so this is
        // a pure daemon-down path: exit 1, no transport touched.
        let exit = cmd_rotate(&TAPE_LIBRARY, false).await.unwrap();
        assert_eq!(exit, 1);
    }

    #[test]
    fn relay_event_accepts_every_variant() {
        // Smoke: relay_event must handle each JobEvent arm without
        // panicking (log levels route to stdout vs stderr).
        relay_event(JobEvent::Log {
            level: "info".into(),
            message: "an info line".into(),
        });
        relay_event(JobEvent::Log {
            level: "warn".into(),
            message: "a warning".into(),
        });
        relay_event(JobEvent::Log {
            level: "error".into(),
            message: "an error".into(),
        });
        relay_event(JobEvent::Result { data: json!({}) });
        relay_event(JobEvent::Progress {
            stage: "scanning".into(),
            current: 1,
            total: Some(2),
        });
        relay_event(JobEvent::Done {
            exit_code: 0,
            error: None,
        });
    }
}
