// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Read-only Web UI (issue #5) embedded in both daemons.
//!
//! This crate owns the two genuinely cross-product halves of the Web
//! UI: the static `/ui` bundle (see [`static_serve`]) and the
//! read-only `/api/v1` handlers that are identical on VTL and VSA
//! (audit tail, recent jobs, monitor snapshot). The auth gate itself
//! is #4's [`shared_admin_auth`]: [`webui_router`] and the per-handler
//! mounts hang on the daemon's existing *protected* route group.
//!
//! The product-specific inventory GETs (VTL library/cartridges/drives,
//! VSA volumes/snapshots) are NOT here — each daemon mounts its own,
//! since they're typed on per-product `AdminState`. The three handlers
//! below are generic over the traits that `AdminState` already
//! implements ([`MonitorState`], [`HasJobs`]) plus the tiny
//! [`AuditLogDir`] this crate defines, so both daemons reuse them as-is.
//!
//! Everything here is READ-ONLY; mutations are issue #91.

#![forbid(unsafe_code)]

mod static_serve;

use std::path::PathBuf;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use shared_admin_auth::AuthState;
use shared_admin_monitor::{MonitorSnapshot, MonitorState, build_payload};
use shared_admin_server::{HasJobs, JobSummary};

pub use static_serve::static_router;

/// Resolved Web UI config, derived from each daemon's `http.webui:`
/// YAML block.
#[derive(Debug, Clone)]
pub struct WebuiConfig {
    /// Serve the `/ui` bundle + read-only `/api/v1` GET subset. When
    /// false the TCP listener keeps only `/health`, `/metrics`,
    /// `/sessions`, and `/info`.
    pub enabled: bool,
    /// On-disk asset directory override. Empty => serve the embedded
    /// bundle. A configured directory lets operators restyle without a
    /// rebuild; a file missing from it falls back to embedded.
    pub asset_dir: PathBuf,
}

impl Default for WebuiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            asset_dir: PathBuf::new(),
        }
    }
}

/// The Web UI's static `/ui` router, gated by the web-admin password.
/// Each daemon `.merge()`s this into its protected group. The
/// read-only API handlers below are mounted separately by each daemon
/// against its own `AdminState`.
pub fn webui_router(cfg: &WebuiConfig, auth: AuthState) -> axum::Router {
    static_router(cfg, auth)
}

// ---- cross-product read-only API handlers --------------------------------

/// `GET /api/v1/monitor` — single-shot monitor snapshot, the same
/// payload the streaming `system.monitor` job emits per tick. Generic
/// over the product `AdminState`, which already implements
/// [`MonitorState`].
pub async fn monitor_snapshot_handler<S: MonitorState>(
    State(state): State<S>,
) -> Json<MonitorSnapshot> {
    Json(build_payload(&state))
}

/// Recent-jobs response wrapper.
#[derive(Serialize)]
pub struct JobsRecent {
    pub jobs: Vec<JobSummary>,
}

/// `GET /api/v1/jobs/recent` — rolling 5-minute window of jobs (see
/// [`shared_admin_server::JobRegistry::list_recent`]). Not a persistent
/// history: finished jobs are reaped 300 s after they end.
pub async fn jobs_recent_handler<S: HasJobs>(State(state): State<S>) -> Json<JobsRecent> {
    Json(JobsRecent {
        jobs: state.jobs().list_recent().await,
    })
}

/// Exposes the daemon's audit-log directory to [`audit_tail_handler`].
/// Both products implement this on their `AdminState`.
pub trait AuditLogDir: Clone + Send + Sync + 'static {
    fn audit_log_dir(&self) -> PathBuf;
}

/// Query string for [`audit_tail_handler`].
#[derive(Deserialize)]
pub struct AuditTailQuery {
    /// Number of trailing entries to return (clamped to 1..=1000).
    #[serde(default = "default_lines")]
    pub lines: usize,
}

fn default_lines() -> usize {
    100
}

/// `GET /api/v1/audit/tail?lines=N` — the last N entries of the
/// BLAKE3-chained audit log. Uses the bounded
/// [`shared_audit::read_entries_tail`] off the runtime on a blocking
/// thread, so a remote client can't force a full-chain
/// decompress+parse on this open-by-default endpoint (issue #201). This
/// is the one-shot "last N" read, not the streaming `audit tail` job.
pub async fn audit_tail_handler<S: AuditLogDir>(
    State(state): State<S>,
    Query(q): Query<AuditTailQuery>,
) -> Response {
    let dir = state.audit_log_dir();
    let lines = q.lines.clamp(1, 1000);
    let read =
        tokio::task::spawn_blocking(move || shared_audit::read_entries_tail(&dir, lines)).await;
    match read {
        Ok(Ok(tail)) => {
            Json(serde_json::json!({ "entries": tail })).into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "audit log read failed" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn webui_config_default_is_enabled_embedded() {
        let c = WebuiConfig::default();
        assert!(c.enabled);
        assert!(c.asset_dir.as_os_str().is_empty());
    }

    #[test]
    fn audit_tail_query_defaults_to_100() {
        let q: AuditTailQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.lines, 100);
    }

    // Minimal HasJobs fake so the recent-jobs wrapper can be exercised
    // without a full daemon state.
    #[derive(Clone)]
    struct FakeJobs(Arc<shared_admin_server::JobRegistry>);
    impl HasJobs for FakeJobs {
        fn jobs(&self) -> &shared_admin_server::JobRegistry {
            &self.0
        }
    }

    #[tokio::test]
    async fn jobs_recent_handler_reports_registered_jobs() {
        let reg = Arc::new(shared_admin_server::JobRegistry::new());
        let (_id, _ts, em) = reg.create("system.gc").await;
        em.emit(shared_admin_server::JobEvent::done(0)).await;
        let state = FakeJobs(reg);
        let Json(resp) = jobs_recent_handler(State(state)).await;
        assert_eq!(resp.jobs.len(), 1);
        assert_eq!(resp.jobs[0].kind, "system.gc");
        assert_eq!(resp.jobs[0].exit_code, Some(0));
    }

    // Minimal AuditLogDir fake over an empty dir: read_entries returns
    // an empty set, the handler returns 200 with `entries: []`.
    #[derive(Clone)]
    struct FakeAudit(PathBuf);
    impl AuditLogDir for FakeAudit {
        fn audit_log_dir(&self) -> PathBuf {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn audit_tail_handler_on_empty_dir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let state = FakeAudit(dir.path().to_path_buf());
        let resp = audit_tail_handler(State(state), Query(AuditTailQuery { lines: 10 })).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
