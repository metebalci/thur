// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Wire types shared between the admin-socket server and client.
//!
//! Pure type definitions — no transport, no axum, no hyper. This
//! crate is the single source of truth for everything that crosses
//! the admin Unix socket so the daemon (`shared-admin-server`
//! consumer) and the CLI (`shared-admin-client` consumer) can't
//! drift apart on the wire.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// One line in the NDJSON event stream for long-running admin jobs.
///
/// Tagged-union form so the CLI can match on `type` to decide how
/// to render. The optional fields (`total`, `error`) carry both
/// `skip_serializing_if` (to keep the wire compact on emit) and
/// `default` (so the read side accepts streams that omit them),
/// which makes the type wire-compatible with the historical
/// daemon-side and CLI-side definitions it consolidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    /// Free-form log line. CLI prints to stdout/stderr depending on
    /// `level`. `level` mirrors `tracing::Level` lowercased.
    Log { level: String, message: String },
    /// Progress tick. CLI may render as a single-line counter; rate-
    /// limit on the producer side (jobs that emit >100/sec should
    /// coalesce).
    Progress {
        stage: String,
        current: u64,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        total: Option<u64>,
    },
    /// Structured result blob. Most CLI commands consume this as the
    /// "what happened" payload (counts, paths, deltas) and pretty-
    /// print it post-stream.
    Result { data: serde_json::Value },
    /// Terminal event. Always the last line; signals the connection
    /// can close. `exit_code` is the process-style exit the CLI
    /// should adopt. `error` carries a human message when
    /// `exit_code != 0`.
    Done {
        exit_code: i32,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        error: Option<String>,
    },
}

impl JobEvent {
    pub fn log(level: &str, message: impl Into<String>) -> Self {
        JobEvent::Log {
            level: level.to_string(),
            message: message.into(),
        }
    }
    pub fn info(message: impl Into<String>) -> Self {
        Self::log("info", message)
    }
    pub fn warn(message: impl Into<String>) -> Self {
        Self::log("warn", message)
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self::log("error", message)
    }
    pub fn progress(stage: impl Into<String>, current: u64, total: Option<u64>) -> Self {
        JobEvent::Progress {
            stage: stage.into(),
            current,
            total,
        }
    }
    pub fn result(data: serde_json::Value) -> Self {
        JobEvent::Result { data }
    }
    pub fn done(exit_code: i32) -> Self {
        JobEvent::Done {
            exit_code,
            error: None,
        }
    }
    pub fn done_with_error(exit_code: i32, msg: impl Into<String>) -> Self {
        JobEvent::Done {
            exit_code,
            error: Some(msg.into()),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, JobEvent::Done { .. })
    }
}

/// Body of a successful `POST /api/v1/jobs/<kind>` response.
///
/// `started_at` is an RFC3339 timestamp string so this crate can
/// stay chrono-free; the server-side serializes via
/// `chrono::Utc::now().to_rfc3339()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobAccepted {
    pub job_id: String,
    pub kind: String,
    pub started_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reserialize(ev: &JobEvent) -> serde_json::Value {
        let s = serde_json::to_string(ev).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn job_event_round_trip_log() {
        let ev = JobEvent::Log {
            level: "info".into(),
            message: "hello".into(),
        };
        let wire = serde_json::to_string(&ev).unwrap();
        let parsed: JobEvent = serde_json::from_str(&wire).unwrap();
        match parsed {
            JobEvent::Log { level, message } => {
                assert_eq!(level, "info");
                assert_eq!(message, "hello");
            }
            _ => panic!("expected Log, got {:?}", parsed),
        }
    }

    #[test]
    fn job_event_round_trip_progress() {
        let ev = JobEvent::Progress {
            stage: "upload".into(),
            current: 42,
            total: Some(100),
        };
        let parsed: JobEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        match parsed {
            JobEvent::Progress {
                stage,
                current,
                total,
            } => {
                assert_eq!(stage, "upload");
                assert_eq!(current, 42);
                assert_eq!(total, Some(100));
            }
            _ => panic!("expected Progress, got {:?}", parsed),
        }
    }

    #[test]
    fn job_event_round_trip_result() {
        let ev = JobEvent::Result {
            data: json!({"count": 7, "paths": ["a", "b"]}),
        };
        let parsed: JobEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        match parsed {
            JobEvent::Result { data } => {
                assert_eq!(data["count"], 7);
                assert_eq!(data["paths"][1], "b");
            }
            _ => panic!("expected Result, got {:?}", parsed),
        }
    }

    #[test]
    fn job_event_round_trip_done() {
        let ev = JobEvent::Done {
            exit_code: 0,
            error: None,
        };
        let parsed: JobEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        match parsed {
            JobEvent::Done { exit_code, error } => {
                assert_eq!(exit_code, 0);
                assert!(error.is_none());
            }
            _ => panic!("expected Done, got {:?}", parsed),
        }
    }

    #[test]
    fn job_event_done_with_error_round_trip() {
        let ev = JobEvent::done_with_error(2, "missing license");
        let v = reserialize(&ev);
        assert_eq!(v["type"], "done");
        assert_eq!(v["exit_code"], 2);
        assert_eq!(v["error"], "missing license");
    }

    #[test]
    fn job_event_wire_discriminants() {
        // The CLI matches on the `type` tag; pin each variant's discriminant.
        assert_eq!(reserialize(&JobEvent::info("x"))["type"], "log");
        assert_eq!(
            reserialize(&JobEvent::progress("s", 0, None))["type"],
            "progress"
        );
        assert_eq!(
            reserialize(&JobEvent::result(json!(null)))["type"],
            "result"
        );
        assert_eq!(reserialize(&JobEvent::done(0))["type"], "done");
    }

    #[test]
    fn job_event_progress_omits_optional_total() {
        let ev = JobEvent::Progress {
            stage: "s".into(),
            current: 1,
            total: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(!s.contains("total"), "wire was: {}", s);
    }

    #[test]
    fn job_event_done_omits_optional_error_when_none() {
        let s = serde_json::to_string(&JobEvent::done(0)).unwrap();
        assert!(!s.contains("error"), "wire was: {}", s);
    }

    #[test]
    fn job_event_progress_accepts_missing_total_on_read() {
        // Read-side compat: streams that omit `total` must still parse.
        let parsed: JobEvent =
            serde_json::from_str(r#"{"type":"progress","stage":"s","current":5}"#).unwrap();
        match parsed {
            JobEvent::Progress {
                stage,
                current,
                total,
            } => {
                assert_eq!(stage, "s");
                assert_eq!(current, 5);
                assert!(total.is_none());
            }
            _ => panic!("expected Progress"),
        }
    }

    #[test]
    fn job_event_is_terminal() {
        assert!(JobEvent::done(0).is_terminal());
        assert!(JobEvent::done_with_error(1, "x").is_terminal());
        assert!(!JobEvent::info("x").is_terminal());
        assert!(
            !JobEvent::progress("s", 0, None).is_terminal(),
            "Progress is not terminal"
        );
        assert!(!JobEvent::result(json!({})).is_terminal());
    }

    #[test]
    fn job_event_constructor_helpers() {
        match JobEvent::info("hi") {
            JobEvent::Log { level, message } => {
                assert_eq!(level, "info");
                assert_eq!(message, "hi");
            }
            _ => panic!("info() should build Log"),
        }
        match JobEvent::warn("hi") {
            JobEvent::Log { level, .. } => assert_eq!(level, "warn"),
            _ => panic!(),
        }
        match JobEvent::error("hi") {
            JobEvent::Log { level, .. } => assert_eq!(level, "error"),
            _ => panic!(),
        }
    }

    #[test]
    fn job_accepted_round_trip() {
        let ja = JobAccepted {
            job_id: "abc-123".into(),
            kind: "verify".into(),
            started_at: "2026-05-12T08:00:00Z".into(),
        };
        let parsed: JobAccepted =
            serde_json::from_str(&serde_json::to_string(&ja).unwrap()).unwrap();
        assert_eq!(parsed.job_id, "abc-123");
        assert_eq!(parsed.kind, "verify");
        assert_eq!(parsed.started_at, "2026-05-12T08:00:00Z");
    }
}
