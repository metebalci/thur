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

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(peer: &str) -> AuditActor {
        AuditActor::iscsi(None::<String>, peer.to_string())
    }

    #[test]
    fn chap_failure_keys_by_peer_user_and_reason() {
        let params = serde_json::json!({"chap_user": "alice", "reason": "bad_secret"});
        let key = ratelimit_key_for("iscsi.chap.failure", &actor("10.0.0.1"), &params);
        assert_eq!(
            key.as_deref(),
            Some("iscsi.chap.failure:10.0.0.1:alice:bad_secret"),
        );
    }

    #[test]
    fn chap_failure_falls_back_when_fields_are_absent() {
        let key = ratelimit_key_for(
            "iscsi.chap.failure",
            &actor("10.0.0.1"),
            &serde_json::json!({}),
        );
        assert_eq!(
            key.as_deref(),
            Some("iscsi.chap.failure:10.0.0.1:-:unknown")
        );
    }

    #[test]
    fn move_medium_is_rate_limited_only_on_refusal() {
        let refused = serde_json::json!({"refused": "partition_fence"});
        assert_eq!(
            ratelimit_key_for("iscsi.move_medium", &actor("p"), &refused).as_deref(),
            Some("iscsi.move_medium:p:partition_fence"),
        );
        // The success-path emission carries no `refused` field, so it
        // bypasses the rate limiter entirely.
        let ok = serde_json::json!({"action": "load"});
        assert_eq!(
            ratelimit_key_for("iscsi.move_medium", &actor("p"), &ok),
            None,
        );
    }

    #[test]
    fn other_ops_are_never_rate_limited() {
        assert_eq!(
            ratelimit_key_for("iscsi.drive.load", &actor("p"), &serde_json::json!({})),
            None,
        );
    }

    #[test]
    fn audit_append_with_no_channel_is_a_silent_noop() {
        let rl = AuditRateLimiter::new(std::time::Duration::from_secs(60));
        let no_channel: Option<AuditChannel> = None;
        // Must not panic when audit_log is None — the rate-limited
        // path and the pass-through path are both exercised.
        audit_append(
            &no_channel,
            &rl,
            "iscsi.chap.failure",
            actor("p"),
            serde_json::json!({"chap_user": "u", "reason": "r"}),
            AuditResult::Ok,
        );
        audit_append(
            &no_channel,
            &rl,
            "iscsi.drive.load",
            actor("p"),
            serde_json::json!({}),
            AuditResult::Ok,
        );
    }
}
