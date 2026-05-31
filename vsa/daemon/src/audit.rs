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
//! There's no audit-chain rate-limiter wired up yet — the failure
//! rows write one-per-event (the per-host/per-NQN brute-force *alert*
//! is already deduped + thresholded in `shared-alerting`), and chain
//! floods haven't shown up in practice. When thurvsa grows more
//! host-driven failure paths (e.g. WORM refusals, ACL violations
//! against volume admin ops) the `AuditRateLimiter` from shared-audit
//! drops in unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use shared_audit::{
    AuditActor, AuditChannel, AuditConfig, AuditLog, AuditMode, AuditResult, AuditWriterHandle,
    spawn_writer,
};
use shared_iscsi::transport::{LoginAuditEvent, LoginAuditSink};

use crate::config::AuditSettings;

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
}

impl IscsiDiskLoginAudit {
    pub fn new(channel: AuditChannel) -> Self {
        Self { channel }
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
                let actor = AuditActor::iscsi(initiator.map(str::to_string), peer.to_string());
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
                // Alert side: per-user counter with threshold from
                // `alerting.chap_failures_threshold`. Audit row goes
                // out every time; the WARN alert only when the count
                // for this user inside the dedup window crosses N.
                if let Some(u) = user {
                    shared_alerting::record::chap_failure(u, peer);
                }
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
}

impl NvmetcpLoginAudit {
    pub fn new(channel: AuditChannel) -> Self {
        Self { channel }
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
                let actor = AuditActor::nvme(host_nqn, peer.to_string());
                self.channel.try_append(
                    "nvmetcp.dhchap.failure",
                    actor,
                    serde_json::json!({
                        "host_nqn": host_nqn,
                        "reason": reason,
                    }),
                    AuditResult::Error(error),
                );
                // Alert side: the host NQN is the brute-force counter
                // key (NVMe's equivalent of the CHAP username). Audit
                // row goes out every time; the WARN alert only when this
                // host's failure count inside the dedup window crosses
                // `alerting.chap_failures_threshold`.
                shared_alerting::record::chap_failure(host_nqn, peer);
            }
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
        let sink = IscsiDiskLoginAudit::new(boot.channel.clone());

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
        let sink = NvmetcpLoginAudit::new(boot.channel.clone());

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
}
