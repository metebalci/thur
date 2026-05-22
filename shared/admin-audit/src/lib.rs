// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.audit.*` jobs — daemon-routed audit log operations.
//!
//! Four kinds: `tail` / `export` / `verify` / `rotate`. The daemon
//! is the steady-state writer of `<data_dir>/audit/*.jsonl`, so
//! routing reads through it removes the cross-process `fs2` lock the
//! CLI used to take. Tail's follow mode keeps the connection open
//! and emits a log event for every new entry.
//!
//! Cross-product: both `thurvtld` and `thurvsad` route their
//! `system.audit.*` job kinds here. The only per-product variation is
//! the audit directory path, passed in as a plain `PathBuf`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use chrono::NaiveDate;
use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};
use shared_audit::{
    AuditActor, AuditConfig, AuditEntry, AuditError, AuditLog, AuditMode, AuditTailCursor,
    read_entries, tail_step, verify_chain,
};

#[derive(Debug, Default, Deserialize)]
pub struct TailParams {
    #[serde(default)]
    pub follow: bool,
    #[serde(default = "default_lines")]
    pub lines: usize,
}
fn default_lines() -> usize {
    20
}

#[derive(Debug, Default, Deserialize)]
pub struct ExportParams {
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}
fn default_format() -> String {
    "jsonl".to_string()
}

#[derive(Debug, Default, Deserialize)]
pub struct RotateParams {
    #[serde(default)]
    pub accept_break: bool,
}

pub async fn run_tail(emitter: JobEmitter, body: serde_json::Value, dir: PathBuf) {
    let params: TailParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };
    if !dir.exists() {
        emitter
            .emit(JobEvent::done_with_error(
                2,
                format!("audit dir not found: {}", dir.display()),
            ))
            .await;
        return;
    }

    let initial = match read_entries(&dir, None, None) {
        Ok(e) => e,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("read audit entries: {}", e),
                ))
                .await;
            return;
        }
    };
    let start = initial.len().saturating_sub(params.lines);
    for entry in &initial[start..] {
        emitter.info(format_entry(entry)).await;
    }
    let mut last_seq = initial.last().map(|e| e.seq).unwrap_or(0);

    if !params.follow {
        emitter.emit(JobEvent::done(0)).await;
        return;
    }

    // Follow loop. Polls today's file every 500ms. Runs until the
    // CLI subscriber disconnects (stream drop) or the daemon shuts
    // down. Tokio task is owned by the registry; if it sticks around
    // forever, the registry's reaper will skip it (finished=false).
    //
    // Each tick incrementally reads only bytes appended since the
    // last poll via `tail_step` — re-parsing every JSONL file from
    // genesis on every 500 ms would scan hundreds of MB on a
    // multi-month chain, instead. Initial backlog above already
    // covered everything currently on disk, so seed the cursor with
    // the active file's current size to skip straight to "appended
    // from here on out".
    let mut cursor = AuditTailCursor::new();
    if let Err(e) = cursor.skip_to_end(&dir) {
        emitter
            .emit(JobEvent::done_with_error(
                2,
                format!("audit tail seed cursor: {}", e),
            ))
            .await;
        return;
    }
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let appended = match tail_step(&mut cursor, &dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in appended {
            if entry.seq > last_seq {
                emitter.info(format_entry(&entry)).await;
                last_seq = entry.seq;
            }
        }
    }
}

pub async fn run_export(emitter: JobEmitter, body: serde_json::Value, dir: PathBuf) {
    let params: ExportParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };
    let from = match params.from.as_deref().map(parse_date).transpose() {
        Ok(v) => v,
        Err(e) => {
            emitter.emit(JobEvent::done_with_error(2, e)).await;
            return;
        }
    };
    let to = match params.to.as_deref().map(parse_date).transpose() {
        Ok(v) => v,
        Err(e) => {
            emitter.emit(JobEvent::done_with_error(2, e)).await;
            return;
        }
    };
    let entries = match read_entries(&dir, from, to) {
        Ok(e) => e,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("audit export: {}", e)))
                .await;
            return;
        }
    };

    match params.format.as_str() {
        "jsonl" => {
            for entry in &entries {
                if let Ok(line) = serde_json::to_string(entry) {
                    emitter.info(line).await;
                }
            }
        }
        "csv" => {
            emitter
                .info("seq,ts,actor_kind,actor_user,op,result,error,params,prev_hash,entry_hash")
                .await;
            for e in &entries {
                emitter
                    .info(format!(
                        "{},{},{},{},{},{},{},{},{},{}",
                        e.seq,
                        e.ts.to_rfc3339(),
                        csv_escape(&e.actor.kind),
                        csv_escape(e.actor.user.as_deref().unwrap_or("")),
                        csv_escape(&e.op),
                        csv_escape(&e.result),
                        csv_escape(e.error.as_deref().unwrap_or("")),
                        csv_escape(&serde_json::to_string(&e.params).unwrap_or_default()),
                        csv_escape(e.prev_hash.as_deref().unwrap_or("")),
                        csv_escape(e.entry_hash.as_deref().unwrap_or("")),
                    ))
                    .await;
            }
        }
        other => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("unknown format '{}'; use jsonl or csv", other),
                ))
                .await;
            return;
        }
    }
    emitter.emit(JobEvent::done(0)).await;
}

pub async fn run_verify(emitter: JobEmitter, _body: serde_json::Value, dir: PathBuf) {
    if !dir.exists() {
        emitter
            .emit(JobEvent::done_with_error(
                2,
                format!("audit dir not found: {}", dir.display()),
            ))
            .await;
        return;
    }
    match verify_chain(&dir) {
        Ok(report) => {
            emitter
                .info(format!(
                    "audit verify: OK ({} entries checked, last_seq={}, last_hash={})",
                    report.entries_checked, report.last_seq, report.last_hash
                ))
                .await;
            emitter
                .emit(JobEvent::result(serde_json::json!({
                    "entries_checked": report.entries_checked,
                    "last_seq": report.last_seq,
                    "last_hash": report.last_hash,
                })))
                .await;
            emitter.emit(JobEvent::done(0)).await;
        }
        Err(AuditError::ChainBroken {
            seq,
            stored,
            actual,
        }) => {
            emitter
                .error(format!(
                    "audit verify: CHAIN BROKEN at seq {seq}\n  stored:     {stored}\n  recomputed: {actual}"
                ))
                .await;
            emitter
                .emit(JobEvent::result(serde_json::json!({
                    "chain_broken": true,
                    "seq": seq,
                    "stored": stored,
                    "actual": actual,
                })))
                .await;
            emitter.emit(JobEvent::done(1)).await;
        }
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("audit verify error: {}", e),
                ))
                .await;
        }
    }
}

pub async fn run_rotate(emitter: JobEmitter, body: serde_json::Value, dir: PathBuf) {
    let params: RotateParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };
    if !params.accept_break {
        emitter
            .emit(JobEvent::done_with_error(
                1,
                "audit rotate: refusing without accept_break=true",
            ))
            .await;
        return;
    }
    let mut audit_cfg = AuditConfig::new(dir.clone(), AuditMode::TamperEvident);
    // Both daemons default `audit.compress_rotated` to true; rotation
    // recovery opens the log standalone, so honor that default.
    audit_cfg.compress_rotated = true;

    let log = match AuditLog::open_for_recovery(audit_cfg, None) {
        Ok(l) => l,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("open audit for recovery: {}", e),
                ))
                .await;
            return;
        }
    };
    let actor = AuditActor::cli("daemon".to_string());
    match log.rotate_after_break(actor) {
        Ok(seq) => {
            emitter
                .info(format!(
                    "audit rotate: chain reset entry written at seq {}",
                    seq
                ))
                .await;
            emitter
                .emit(JobEvent::result(serde_json::json!({"reset_seq": seq})))
                .await;
            emitter.emit(JobEvent::done(0)).await;
        }
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("rotate failed: {}", e),
                ))
                .await;
        }
    }
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| format!("invalid date '{}'; expected YYYY-MM-DD", s))
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn format_entry(e: &AuditEntry) -> String {
    format!(
        "{:>6}  {}  {:<14}  {}  {}",
        e.seq,
        e.ts.to_rfc3339(),
        e.op,
        e.result,
        serde_json::to_string(&e.params).unwrap_or_default(),
    )
}
