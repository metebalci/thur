// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! thurvsad audit wiring.
//!
//! thurvsa's audit emitters are the two transport login phases:
//! the shared-iscsi CHAP path ([`IscsiDiskLoginAudit`]) and the
//! NVMe/TCP DH-HMAC-CHAP path ([`NvmetcpLoginAudit`]). Both emit
//! success/failure rows and feed the shared `chap_failures` alert
//! class (issue #68). The whole audit infrastructure (chain hashing,
//! daily rotation, mpsc producer decoupling, replay, verify,
//! ratelimit) lives in `shared-audit`, lifted out of
//! core-mediachanger (Step 5 — shared-audit, 2026-05-09). This module
//! is the thurvsa-side glue: an [`AuditLog`] opener + writer-task
//! spawn, plus the two [`LoginAuditSink`](shared_iscsi::transport::LoginAuditSink)
//! / [`nvme_tcp::LoginAuditSink`] adapters that forward into the
//! shared `AuditChannel`.
//!
//! Both login-failure paths are guarded by an [`AuditRateLimiter`]
//! (60 s window, matching VTL's `scsi-ssc` data-path limiter): the
//! first failure for a given `<op>:<peer>:<user-or-nqn>:<reason>` key
//! emits a chain row as usual, same-key repeats inside the window are
//! silently counted, and the daemon's flush task ([`run_audit_ratelimit_flush`])
//! drains each expired window into a single rollup row. This bounds a
//! CHAP / DH-HMAC-CHAP brute-force from flooding the BLAKE3-chained
//! log with one row per attempt. Success rows and lifecycle events
//! (`daemon.start`) bypass the limiter — same opt-in policy as VTL.
//! The per-host/per-NQN brute-force *alert* is a separate mechanism,
//! already deduped + thresholded in `shared-alerting` (issue #68).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use shared_audit::{
    AuditActor, AuditChannel, AuditConfig, AuditLog, AuditMode, AuditRateLimitDecision,
    AuditRateLimitRollup, AuditRateLimiter, AuditResult, AuditWriterHandle, spawn_writer,
};
use shared_iscsi::transport::{LoginAuditEvent, LoginAuditSink};

use crate::config::AuditSettings;

/// Suppression window for the login-failure audit rate-limiter. 60 s
/// to match VTL's `scsi-ssc` data-path limiter.
pub const AUDIT_RATELIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Cadence at which [`run_audit_ratelimit_flush`] drains expired
/// suppression windows. Well below [`AUDIT_RATELIMIT_WINDOW`] so the
/// steady-state lag between window expiry and rollup emission is
/// bounded.
pub const AUDIT_RATELIMIT_FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// Construct the shared login-failure audit rate-limiter. Cloned into
/// both [`LoginAuditSink`] adapters and the flush task; drained at
/// shutdown.
pub fn new_audit_ratelimiter() -> Arc<AuditRateLimiter> {
    Arc::new(AuditRateLimiter::new(AUDIT_RATELIMIT_WINDOW))
}

/// Resolve the on-disk audit directory. Defaults to
/// `<data_dir>/audit/` when `audit.dir` is unset.
pub fn audit_dir(settings: &AuditSettings, data_dir: &Path) -> PathBuf {
    settings
        .dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("audit"))
}

/// Everything `main` needs after booting the audit log.
pub struct AuditBoot {
    pub log: Arc<AuditLog>,
    pub channel: AuditChannel,
    pub writer: AuditWriterHandle,
}

/// Open the thurvsa audit log and spawn its single-writer task.
///
/// Returns the producer handle (cloneable, used to wire the
/// `LoginAuditSink`) and the writer handle (held by `main` for
/// graceful shutdown so every queued entry hits disk before exit).
/// Stamps a `daemon.start` entry through the producer before
/// returning; on shutdown `main` should push a matching
/// `daemon.stop` and then `handle.shutdown().await`.
pub async fn boot_audit_log(dir: PathBuf, instance_id: Option<&str>) -> Result<AuditBoot> {
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create audit dir: {}", dir.display()))?;

    let cfg = AuditConfig::new(&dir, AuditMode::TamperEvident);
    let log = Arc::new(
        AuditLog::open(cfg).with_context(|| format!("open audit log at {}", dir.display()))?,
    );

    // Replay any daemon-down audit entries (none yet for thurvsa —
    // there is no daemon-down admin surface — but the symmetry with
    // thurvtl is intentional: when thurvsa grows offline volume ops
    // the queue is already drained at boot).
    if let Err(e) = log.replay_pending() {
        tracing::warn!("audit: replay_pending failed: {}", e);
    }

    let (channel, handle) = spawn_writer(Arc::clone(&log));

    channel.try_append(
        "daemon.start",
        AuditActor::system(),
        serde_json::json!({
            "product": "thurvsad",
            "instance_id": instance_id,
            "version": env!("CARGO_PKG_VERSION"),
        }),
        AuditResult::Ok,
    );

    Ok(AuditBoot {
        log,
        channel,
        writer: handle,
    })
}

/// `LoginAuditSink` adapter for thurvsad. Forwards the two
/// shared-iscsi login-phase events into the audit channel as
/// `iscsi.chap.success` / `iscsi.chap.failure` entries — same op
/// names thurvtl emits, so a multi-product audit chain reads
/// uniformly.
pub struct IscsiDiskLoginAudit {
    channel: AuditChannel,
    ratelimiter: Arc<AuditRateLimiter>,
}

impl IscsiDiskLoginAudit {
    pub fn new(channel: AuditChannel, ratelimiter: Arc<AuditRateLimiter>) -> Self {
        Self {
            channel,
            ratelimiter,
        }
    }
}

impl LoginAuditSink for IscsiDiskLoginAudit {
    fn record(&self, event: LoginAuditEvent<'_>) {
        match event {
            LoginAuditEvent::ChapSuccess {
                peer,
                initiator,
                user,
                algorithm,
            } => {
                let actor = AuditActor::iscsi(initiator.map(str::to_string), peer.to_string());
                self.channel.try_append(
                    "iscsi.chap.success",
                    actor,
                    serde_json::json!({
                        "chap_user": user,
                        "initiator": initiator,
                        "algorithm": algorithm,
                    }),
                    AuditResult::Ok,
                );
            }
            LoginAuditEvent::ChapFailure {
                peer,
                initiator,
                user,
                reason,
                error,
            } => {
                // Alert side runs unconditionally (independent of the
                // audit rate-limiter): the alerting dispatcher keeps
                // its own per-user counter across the window and fires
                // WARN once `alerting.chap_failures_threshold` is hit.
                if let Some(u) = user {
                    shared_alerting::record::chap_failure(u, peer);
                }
                let actor = AuditActor::iscsi(initiator.map(str::to_string), peer.to_string());
                // Rate-limit the chain row: one emission per distinct
                // (peer, user, reason) tuple in the window, then a
                // rollup. Caps a brute-force from flooding the chain.
                let user_label = user.unwrap_or("-");
                let key = format!("iscsi.chap.failure:{peer}:{user_label}:{reason}");
                if matches!(
                    self.ratelimiter.decide(key, "iscsi.chap.failure", &actor),
                    AuditRateLimitDecision::Suppress
                ) {
                    return;
                }
                self.channel.try_append(
                    "iscsi.chap.failure",
                    actor,
                    serde_json::json!({
                        "chap_user": user,
                        "initiator": initiator,
                        "reason": reason,
                    }),
                    AuditResult::Error(error),
                );
            }
        }
    }
}

/// `LoginAuditSink` adapter for thurvsad's NVMe/TCP transport. The
/// NVMe counterpart of [`IscsiDiskLoginAudit`]: it forwards
/// DH-HMAC-CHAP login-phase events into the same audit channel as
/// `nvmetcp.dhchap.{success,failure}` rows and feeds the shared
/// `chap_failures` alert class on each refused auth — closing the
/// security-observability gap where DH-HMAC-CHAP failures only hit a
/// `tracing::warn!` (issue #68). Same forensic + brute-force-alert
/// shape iSCSI CHAP already has.
pub struct NvmetcpLoginAudit {
    channel: AuditChannel,
    ratelimiter: Arc<AuditRateLimiter>,
}

impl NvmetcpLoginAudit {
    pub fn new(channel: AuditChannel, ratelimiter: Arc<AuditRateLimiter>) -> Self {
        Self {
            channel,
            ratelimiter,
        }
    }
}

impl nvme_tcp::LoginAuditSink for NvmetcpLoginAudit {
    fn record(&self, event: nvme_tcp::LoginAuditEvent<'_>) {
        match event {
            nvme_tcp::LoginAuditEvent::DhchapSuccess {
                peer,
                host_nqn,
                admitted_volumes,
            } => {
                let actor = AuditActor::nvme(host_nqn, peer.to_string());
                self.channel.try_append(
                    "nvmetcp.dhchap.success",
                    actor,
                    serde_json::json!({
                        "host_nqn": host_nqn,
                        "admitted_volumes": admitted_volumes,
                    }),
                    AuditResult::Ok,
                );
            }
            nvme_tcp::LoginAuditEvent::DhchapFailure {
                peer,
                host_nqn,
                reason,
                error,
            } => {
                // Alert side runs unconditionally: the host NQN is the
                // brute-force counter key (NVMe's equivalent of the
                // CHAP username). The WARN alert fires only when this
                // host's failure count inside the dedup window crosses
                // `alerting.chap_failures_threshold`.
                shared_alerting::record::chap_failure(host_nqn, peer);
                let actor = AuditActor::nvme(host_nqn, peer.to_string());
                // Rate-limit the chain row the same way the iSCSI sink
                // does — keyed by (peer, host_nqn, reason).
                let key = format!("nvmetcp.dhchap.failure:{peer}:{host_nqn}:{reason}");
                if matches!(
                    self.ratelimiter
                        .decide(key, "nvmetcp.dhchap.failure", &actor),
                    AuditRateLimitDecision::Suppress
                ) {
                    return;
                }
                self.channel.try_append(
                    "nvmetcp.dhchap.failure",
                    actor,
                    serde_json::json!({
                        "host_nqn": host_nqn,
                        "reason": reason,
                    }),
                    AuditResult::Error(error),
                );
            }
        }
    }
}

/// Append one rate-limit rollup row to the audit chain. Best-effort:
/// the original first emission and the suppression already told the
/// host something is flooding, so a dropped rollup is non-fatal. Same
/// shape VTL's `emit_audit_ratelimit_rollup` writes — `Error` result
/// with the suppressed count + window so a chain reader spots it.
pub fn emit_audit_ratelimit_rollup(channel: &AuditChannel, rollup: &AuditRateLimitRollup) {
    let params = serde_json::json!({
        "suppressed_count": rollup.suppressed_count,
        "window_seconds": rollup.window_seconds,
        "key": rollup.key,
    });
    let detail = format!(
        "{} additional event(s) suppressed in {}s window",
        rollup.suppressed_count, rollup.window_seconds
    );
    channel.try_append(
        &rollup.op,
        rollup.actor.clone(),
        params,
        AuditResult::Error(detail),
    );
}

/// Periodic flush task. Drains expired suppression windows every
/// [`AUDIT_RATELIMIT_FLUSH_INTERVAL`] and writes one rollup row per
/// window. Runs until aborted at shutdown; the shutdown path does a
/// final `flush_all` to drain still-open windows.
pub async fn run_audit_ratelimit_flush(limiter: Arc<AuditRateLimiter>, channel: AuditChannel) {
    let mut ticker = tokio::time::interval(AUDIT_RATELIMIT_FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(
        "audit ratelimit: flush task started (interval={}s, window={}s)",
        AUDIT_RATELIMIT_FLUSH_INTERVAL.as_secs(),
        limiter.window().as_secs(),
    );
    loop {
        ticker.tick().await;
        for rollup in limiter.flush_expired() {
            emit_audit_ratelimit_rollup(&channel, &rollup);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn audit_dir_defaults_under_data_dir() {
        let settings = AuditSettings::default();
        let data_dir = Path::new("/var/lib/thurvsa");
        assert_eq!(
            audit_dir(&settings, data_dir),
            PathBuf::from("/var/lib/thurvsa/audit")
        );
    }

    #[test]
    fn audit_dir_honors_explicit_override() {
        let settings = AuditSettings {
            enabled: true,
            dir: Some("/var/log/thurvsa/audit".to_string()),
        };
        assert_eq!(
            audit_dir(&settings, Path::new("/var/lib/thurvsa")),
            PathBuf::from("/var/log/thurvsa/audit")
        );
    }

    #[tokio::test]
    async fn boot_audit_log_creates_dir_and_stamps_start() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let boot = boot_audit_log(dir.clone(), Some("inst-123"))
            .await
            .expect("audit log boots");
        // The audit directory is created on demand.
        assert!(dir.is_dir());
        // Drain the writer so the queued daemon.start entry hits disk.
        boot.writer.shutdown().await;
        // A daily-rotating JSONL file exists and carries the start row.
        let mut found = false;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let body = std::fs::read_to_string(&path).unwrap();
                if body.contains("daemon.start") {
                    found = true;
                }
            }
        }
        assert!(found, "daemon.start entry must be persisted");
    }

    #[tokio::test]
    async fn login_audit_sink_forwards_chap_events() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let boot = boot_audit_log(dir.clone(), None)
            .await
            .expect("audit log boots");
        let sink = IscsiDiskLoginAudit::new(boot.channel.clone(), new_audit_ratelimiter());

        sink.record(LoginAuditEvent::ChapSuccess {
            peer: "10.0.0.1:3260",
            initiator: Some("iqn.host"),
            user: "alice",
            algorithm: "SHA-256",
        });
        sink.record(LoginAuditEvent::ChapFailure {
            peer: "10.0.0.2:3260",
            initiator: Some("iqn.bad"),
            user: Some("mallory"),
            reason: "secret mismatch",
            error: "auth failed".to_string(),
        });

        boot.writer.shutdown().await;

        let mut combined = String::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                combined.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
        assert!(combined.contains("iscsi.chap.success"));
        assert!(combined.contains("iscsi.chap.failure"));
        assert!(combined.contains("alice"));
        assert!(combined.contains("mallory"));
    }

    #[tokio::test]
    async fn nvmetcp_login_audit_sink_forwards_dhchap_events() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let boot = boot_audit_log(dir.clone(), None)
            .await
            .expect("audit log boots");
        let sink = NvmetcpLoginAudit::new(boot.channel.clone(), new_audit_ratelimiter());

        nvme_tcp::LoginAuditSink::record(
            &sink,
            nvme_tcp::LoginAuditEvent::DhchapSuccess {
                peer: "10.0.0.1:4420",
                host_nqn: "nqn.2014-08.org.nvmexpress:uuid:good",
                admitted_volumes: 2,
            },
        );
        nvme_tcp::LoginAuditSink::record(
            &sink,
            nvme_tcp::LoginAuditEvent::DhchapFailure {
                peer: "10.0.0.2:4420",
                host_nqn: "nqn.2014-08.org.nvmexpress:uuid:bad",
                reason: "reply_invalid",
                error: "DH-HMAC-CHAP host response invalid".to_string(),
            },
        );

        boot.writer.shutdown().await;

        let mut combined = String::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                combined.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
        assert!(combined.contains("nvmetcp.dhchap.success"));
        assert!(combined.contains("nvmetcp.dhchap.failure"));
        assert!(combined.contains("reply_invalid"));
        // Actor kind for NVMe host events is "nvme", host NQN as user.
        assert!(combined.contains("\"nvme\""));
        assert!(combined.contains("uuid:good"));
        assert!(combined.contains("uuid:bad"));
    }

    /// Count chain lines for a given op, split into normal rows and
    /// rate-limit rollup rows (the latter carry `suppressed_count`).
    fn count_op_rows(dir: &Path, op: &str) -> (usize, usize) {
        let (mut rows, mut rollups) = (0, 0);
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                if !line.contains(op) {
                    continue;
                }
                if line.contains("suppressed_count") {
                    rollups += 1;
                } else {
                    rows += 1;
                }
            }
        }
        (rows, rollups)
    }

    /// A burst of same-key CHAP failures collapses to one chain row
    /// plus one rollup carrying `suppressed_count = N-1` — the core
    /// brute-force-flood guarantee (issue #101).
    #[tokio::test]
    async fn chap_failure_burst_collapses_to_one_row_plus_rollup() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let boot = boot_audit_log(dir.clone(), None)
            .await
            .expect("audit log boots");
        let rl = new_audit_ratelimiter();
        let sink = IscsiDiskLoginAudit::new(boot.channel.clone(), Arc::clone(&rl));

        const N: u64 = 8;
        for _ in 0..N {
            sink.record(LoginAuditEvent::ChapFailure {
                peer: "10.0.0.2:3260",
                initiator: Some("iqn.bad"),
                user: Some("mallory"),
                reason: "secret mismatch",
                error: "auth failed".to_string(),
            });
        }

        // Drain the still-open window the way the shutdown path does.
        let rollups = rl.flush_all();
        assert_eq!(rollups.len(), 1, "one suppression window drained");
        assert_eq!(rollups[0].op, "iscsi.chap.failure");
        assert_eq!(rollups[0].suppressed_count, N - 1);
        emit_audit_ratelimit_rollup(&boot.channel, &rollups[0]);

        boot.writer.shutdown().await;

        let (rows, rollup_rows) = count_op_rows(&dir, "iscsi.chap.failure");
        assert_eq!(rows, 1, "all but the first same-key failure suppressed");
        assert_eq!(rollup_rows, 1, "one rollup carries the suppressed count");
    }

    /// Distinct failure reasons key independent windows, so a second
    /// failure mode is never masked by a flood of the first.
    #[tokio::test]
    async fn distinct_reasons_each_emit_a_row() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("audit");
        let boot = boot_audit_log(dir.clone(), None)
            .await
            .expect("audit log boots");
        let rl = new_audit_ratelimiter();
        let sink = IscsiDiskLoginAudit::new(boot.channel.clone(), Arc::clone(&rl));

        for reason in ["secret mismatch", "unknown user"] {
            sink.record(LoginAuditEvent::ChapFailure {
                peer: "10.0.0.2:3260",
                initiator: Some("iqn.bad"),
                user: Some("mallory"),
                reason,
                error: "auth failed".to_string(),
            });
        }
        assert!(rl.flush_all().is_empty(), "no suppressions across keys");

        boot.writer.shutdown().await;

        let (rows, rollup_rows) = count_op_rows(&dir, "iscsi.chap.failure");
        assert_eq!(rows, 2, "each distinct reason emits its own row");
        assert_eq!(rollup_rows, 0, "no rollups when nothing was suppressed");
    }
}
