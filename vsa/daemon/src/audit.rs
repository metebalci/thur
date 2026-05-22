// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! thurvsad audit wiring.
//!
//! thurvsa today has exactly one audit emitter — the shared-iscsi
//! login phase (CHAP success / failure). The whole audit
//! infrastructure (chain hashing, daily rotation, mpsc producer
//! decoupling, replay, verify, ratelimit) lives in `shared-audit`,
//! lifted out of core-mediachanger (Step 5 — shared-audit, 2026-05-09).
//! This module is the thurvsa-side glue: an [`AuditLog`] opener +
//! writer-task spawn, and a [`IscsiDiskLoginAudit`] that implements
//! [`shared_iscsi::transport::LoginAuditSink`] by forwarding into
//! the shared `AuditChannel`.
//!
//! There's no rate-limiter wired up yet — thurvsa has only the one
//! emitter, and CHAP failure storms haven't shown up in practice.
//! When thurvsa grows more host-driven failure paths (e.g. WORM
//! refusals, ACL violations against volume admin ops) the
//! `AuditRateLimiter` from shared-audit drops in unchanged.

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
