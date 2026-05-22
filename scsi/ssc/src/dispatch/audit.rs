// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Best-effort audit-write helpers shared by the drive-LUN
//! dispatcher and (still in thurvtld) the SMC-side
//! handlers. Audit failures must never tear down a SCSI session — log
//! a warning and continue. Mirrors the CLI's `audit_helper::record_*`
//! philosophy: losing one audit entry beats losing the operation.
//!
//! Flood-prone host-driven failure paths (CHAP failures, MOVE MEDIUM
//! refusals) are rate-limited via [`AuditRateLimiter`]: the first
//! event in a 60 s window emits as usual; subsequent events with the
//! same key are silently counted, and the daemon's flush task emits
//! one rollup entry per window. Lifecycle / one-shot events (and
//! SCSI success paths) bypass the limiter entirely.

use core_mediachanger::{
    AuditActor, AuditChannel, AuditRateLimitDecision, AuditRateLimiter, AuditResult,
};

/// Best-effort audit append. Routes through [`ratelimit_key_for`]
/// for flood-prone ops; everything else passes through unconditionally.
pub fn audit_append(
    audit_log: &Option<AuditChannel>,
    rl: &AuditRateLimiter,
    op: &str,
    actor: AuditActor,
    params: serde_json::Value,
    result: AuditResult,
) {
    if let Some(key) = ratelimit_key_for(op, &actor, &params)
        && matches!(rl.decide(key, op, &actor), AuditRateLimitDecision::Suppress)
    {
        return;
    }
    if let Some(chan) = audit_log.as_ref() {
        chan.try_append(op, actor, params, result);
    }
}

/// Single source of truth for which iSCSI audit ops get rate-limited
/// and how their suppression buckets are keyed. Returns `None` when
/// the event should pass through unconditionally — that's the right
/// answer for every state-change emission (drive load/unload Ok,
/// encryption set/clear, drive compression toggle, CHAP success).
///
/// Buckets are keyed by `(op, peer, reason)` for CHAP failures and
/// `(op, peer, refused_reason)` for MOVE MEDIUM refusals so that a
/// single misbehaving initiator can't drown out a *different*
/// failure mode coming from a different peer.
///
/// `iscsi.move_medium` is library-only and emitted only by
/// thurvtl. Keeping the limiter rule in scsi-ssc lets
/// thurvtl reuse the same key-derivation logic for it.
pub fn ratelimit_key_for(
    op: &str,
    actor: &AuditActor,
    params: &serde_json::Value,
) -> Option<String> {
    let peer = actor.addr.as_deref().unwrap_or("unknown");
    match op {
        "iscsi.chap.failure" => {
            let user = params
                .get("chap_user")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let reason = params
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            Some(format!("{op}:{peer}:{user}:{reason}"))
        }
        "iscsi.move_medium" => {
            // Only failure-path emissions carry `refused`; the
            // success-path entry doesn't, so it falls through to a
            // None and bypasses the limiter.
            params
                .get("refused")
                .and_then(|v| v.as_str())
                .map(|reason| format!("{op}:{peer}:{reason}"))
        }
        _ => None,
    }
}
