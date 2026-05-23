// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Append-only audit chain shared across both products.
//!
//! Lifted out of `core-mediachanger` (Step 5 — shared-audit, 2026-05-09) so
//! the sibling thurvsad can produce login-phase / volume-lifecycle
//! audit entries against the same chain format without a tape-product
//! dependency. The three modules are:
//!
//! - [`audit`] — `AuditLog`, daily rotation, BLAKE3 chain,
//!   `replay_pending`, `verify`, `read_entries`, the `AuditActor` /
//!   `AuditEntry` / `AuditResult` shape that callers populate.
//! - [`audit_channel`] — `AuditChannel` (cloneable producer handle)
//!   plus `AuditWriterHandle` (single-writer drainer task). Decouples
//!   the SCSI hot path from `fsync` cost.
//! - [`audit_ratelimit`] — host-driven failure rollups
//!   (`iscsi.chap.failure`, `iscsi.move_medium`, …) over a 60 s
//!   window with a 10 s flush sweep.
//!
//! Internal `record::*` calls forward into `shared-telemetry` for the
//! `audit_entries_total`, `audit_chain_resets_total`, and
//! `audit_queue_drops_total` counters; both thurvtl and thurvsa install
//! the global `Telemetry` handle at boot, after which these calls
//! land on the same Prometheus / OTLP surface.

pub mod audit;
pub mod audit_channel;
pub mod audit_ratelimit;

pub use audit::{
    AuditActor, AuditConfig, AuditEntry, AuditError, AuditLog, AuditMode, AuditResult,
    AuditTailCursor, CHAIN_STATE_FILE, GENESIS_PREV_HASH, PENDING_AUDIT_DIR, PendingAuditEntry,
    VerifyReport, compute_entry_hash, queue_pending, read_entries, tail_step, verify_chain,
};
pub use audit_channel::{
    AUDIT_CHANNEL_CAPACITY, AppendFailureHook, AuditChannel, AuditWriterHandle,
    set_append_failure_hook, spawn_writer,
};
pub use audit_ratelimit::{
    AuditRateLimiter, Decision as AuditRateLimitDecision, Rollup as AuditRateLimitRollup,
};
