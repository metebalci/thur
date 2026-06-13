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
    read_entries, read_entries_tail, tail_step, verify_chain,
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

/// Grace after job creation for the CLI's follow-up `GET .../events` to
/// connect before the follow loop concludes nobody is listening.
const TAIL_CONNECT_GRACE: Duration = Duration::from_secs(10);
/// Ring cap on the retained follow-mode event log: bounds daemon heap on
/// a long-lived `audit tail -f` session (issue #140).
const TAIL_EVENT_CAP: usize = 10_000;

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

    // Bounded backlog read off the runtime: gather only the last N
    // entries (newest files first) instead of parsing the whole chain
    // from genesis inline on the async handler (issue #201).
    let lines = params.lines;
    let backlog_dir = dir.clone();
    let initial =
        match tokio::task::spawn_blocking(move || read_entries_tail(&backlog_dir, lines)).await {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                emitter
                    .emit(JobEvent::done_with_error(
                        2,
                        format!("read audit entries: {}", e),
                    ))
                    .await;
                return;
            }
            Err(e) => {
                emitter
                    .emit(JobEvent::done_with_error(
                        2,
                        format!("read audit entries: task join: {}", e),
                    ))
                    .await;
                return;
            }
        };
    for entry in &initial {
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
    // Cap the retained log + self-terminate when the subscriber drops,
    // so an abandoned `audit tail -f` neither leaks the poll task nor
    // grows the event log forever (issue #140). The 500 ms poll cadence
    // means a connected subscriber refreshes its poll well within the
    // grace; an idle window past it means the operator disconnected.
    emitter.set_event_cap(TAIL_EVENT_CAP);
    loop {
        if emitter.should_stop_infinite(TAIL_CONNECT_GRACE) {
            emitter.emit(JobEvent::done(0)).await;
            return;
        }
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
    // Export is inherently full-range; run the synchronous read +
    // decompress off the runtime so it doesn't block a worker shared with
    // the data path (issue #201).
    let export_dir = dir.clone();
    let entries = match tokio::task::spawn_blocking(move || read_entries(&export_dir, from, to))
        .await
    {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("audit export: {}", e)))
                .await;
            return;
        }
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("audit export: task join: {}", e),
                ))
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
    // verify_chain reads + decompresses + hashes the whole chain — run it
    // off the runtime so it doesn't stall a data-path worker (issue #201).
    let verify_dir = dir.clone();
    let verify_result = match tokio::task::spawn_blocking(move || verify_chain(&verify_dir)).await {
        Ok(r) => r,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(
                    2,
                    format!("audit verify: task join: {}", e),
                ))
                .await;
            return;
        }
    };
    match verify_result {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    // `JobEvent` is re-exported by shared-admin-server (see its
    // crate-root `pub use`), so admin-audit needs no direct
    // `shared-admin-proto` dependency.
    use shared_admin_server::{JobEvent, JobRegistry};

    #[test]
    fn defaults_match_the_documented_values() {
        assert_eq!(default_lines(), 20);
        assert_eq!(default_format(), "jsonl");
    }

    #[test]
    fn tail_params_default_to_no_follow_twenty_lines() {
        let p: TailParams = serde_json::from_value(serde_json::json!({})).expect("parse");
        assert!(!p.follow);
        assert_eq!(p.lines, 20);
    }

    #[test]
    fn tail_params_override_follow_and_lines() {
        let p: TailParams =
            serde_json::from_value(serde_json::json!({"follow": true, "lines": 5})).expect("parse");
        assert!(p.follow);
        assert_eq!(p.lines, 5);
    }

    #[test]
    fn export_params_default_to_jsonl_and_no_window() {
        let p: ExportParams = serde_json::from_value(serde_json::json!({})).expect("parse");
        assert_eq!(p.format, "jsonl");
        assert!(p.from.is_none());
        assert!(p.to.is_none());
    }

    #[test]
    fn export_params_round_trip_csv_window() {
        let p: ExportParams = serde_json::from_value(serde_json::json!({
            "format": "csv",
            "from": "2026-05-01",
            "to": "2026-05-23",
        }))
        .expect("parse");
        assert_eq!(p.format, "csv");
        assert_eq!(p.from.as_deref(), Some("2026-05-01"));
        assert_eq!(p.to.as_deref(), Some("2026-05-23"));
    }

    #[test]
    fn rotate_params_default_and_override() {
        let d: RotateParams = serde_json::from_value(serde_json::json!({})).expect("parse");
        assert!(!d.accept_break);
        let y: RotateParams =
            serde_json::from_value(serde_json::json!({"accept_break": true})).expect("parse");
        assert!(y.accept_break);
    }

    #[test]
    fn parse_date_accepts_iso_8601_and_rejects_garbage() {
        let d = parse_date("2026-05-23").expect("ok");
        assert_eq!(d.to_string(), "2026-05-23");
        let err = parse_date("23/05/2026").expect_err("bad format");
        assert!(err.contains("invalid date"));
        assert!(err.contains("YYYY-MM-DD"));
    }

    #[test]
    fn csv_escape_quotes_only_when_needed() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("she said \"hi\""), "\"she said \"\"hi\"\"\"");
        assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
    }

    fn make_entry(seq: u64, op: &str, result: &str) -> AuditEntry {
        AuditEntry {
            seq,
            ts: Utc::now(),
            actor: AuditActor::system(),
            op: op.to_string(),
            params: serde_json::json!({"k": "v"}),
            result: result.to_string(),
            error: None,
            prev_hash: None,
            entry_hash: None,
        }
    }

    #[test]
    fn format_entry_renders_sequence_op_and_result() {
        let e = make_entry(7, "drive.load", "ok");
        let line = format_entry(&e);
        assert!(line.contains("7"));
        assert!(line.contains("drive.load"));
        assert!(line.contains("ok"));
        // params JSON survives the round trip.
        assert!(line.contains("\"k\":\"v\""));
    }

    async fn drain(reg: &JobRegistry, id: &str) -> Vec<JobEvent> {
        let handle = reg.get(id).await.expect("job exists");
        let mut cursor = 0;
        handle.next_events(&mut cursor).await
    }

    fn is_done(events: &[JobEvent]) -> Option<i32> {
        events.iter().find_map(|e| match e {
            JobEvent::Done { exit_code, .. } => Some(*exit_code),
            _ => None,
        })
    }

    #[tokio::test]
    async fn run_tail_on_an_empty_dir_completes_successfully() {
        let reg = JobRegistry::new();
        let (id, _, emitter) = reg.create("audit.tail").await;
        let dir = tempfile::tempdir().expect("temp dir");
        run_tail(emitter, serde_json::json!({}), dir.path().to_path_buf()).await;
        let events = drain(&reg, &id).await;
        assert_eq!(is_done(&events), Some(0));
    }

    #[tokio::test]
    async fn run_tail_with_bad_params_fails_with_exit_code_two() {
        let reg = JobRegistry::new();
        let (id, _, emitter) = reg.create("audit.tail").await;
        let dir = tempfile::tempdir().expect("temp dir");
        // A string body is not a TailParams object.
        run_tail(
            emitter,
            serde_json::json!("not an object"),
            dir.path().to_path_buf(),
        )
        .await;
        let events = drain(&reg, &id).await;
        assert_eq!(is_done(&events), Some(2));
    }

    #[tokio::test]
    async fn run_tail_with_missing_dir_fails_with_exit_code_two() {
        let reg = JobRegistry::new();
        let (id, _, emitter) = reg.create("audit.tail").await;
        run_tail(
            emitter,
            serde_json::json!({}),
            PathBuf::from("/nonexistent/audit-test-dir"),
        )
        .await;
        let events = drain(&reg, &id).await;
        assert_eq!(is_done(&events), Some(2));
    }

    #[tokio::test]
    async fn run_verify_on_an_empty_dir_completes() {
        let reg = JobRegistry::new();
        let (id, _, emitter) = reg.create("audit.verify").await;
        let dir = tempfile::tempdir().expect("temp dir");
        run_verify(emitter, serde_json::json!({}), dir.path().to_path_buf()).await;
        let events = drain(&reg, &id).await;
        // An empty audit dir verifies as a healthy (empty) chain.
        assert!(is_done(&events).is_some());
    }

    #[tokio::test]
    async fn run_export_on_an_empty_dir_completes() {
        let reg = JobRegistry::new();
        let (id, _, emitter) = reg.create("audit.export").await;
        let dir = tempfile::tempdir().expect("temp dir");
        run_export(
            emitter,
            serde_json::json!({"format": "jsonl"}),
            dir.path().to_path_buf(),
        )
        .await;
        let events = drain(&reg, &id).await;
        assert!(is_done(&events).is_some());
    }

    #[tokio::test]
    async fn run_export_rejects_an_unknown_format() {
        let reg = JobRegistry::new();
        let (id, _, emitter) = reg.create("audit.export").await;
        let dir = tempfile::tempdir().expect("temp dir");
        run_export(
            emitter,
            serde_json::json!({"format": "xml"}),
            dir.path().to_path_buf(),
        )
        .await;
        let events = drain(&reg, &id).await;
        assert_eq!(is_done(&events), Some(2));
    }
}
