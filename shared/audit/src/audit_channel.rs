// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Channel-backed producer for the audit log.
//!
//! Why this exists. [`AuditLog::append`] is synchronous: it serializes
//! one JSON line, computes a BLAKE3 chain link, writes the line, and
//! `fsync`s twice (entry file + `chain.state`). Two fsyncs per entry is
//! cheap in absolute terms but becomes a serialization point if many
//! callers append from the iSCSI hot path under a shared
//! `Mutex<AuditWriter>`.
//!
//! [`AuditChannel`] decouples producers from disk: each producer does a
//! non-blocking `try_send` into a bounded mpsc; a single dedicated
//! tokio task drains the queue and calls `AuditLog::append` in arrival
//! order. The chain stays single-writer (still required for
//! [`AuditLog::verify`]); producers never block on disk; channel-full
//! drops are counted (`<product>_audit_queue_drops_total`) and logged
//! once per occurrence.
//!
//! Lifecycle. Build an `AuditLog` synchronously, run any startup-time
//! sync work (`replay_pending`, the bootstrap `daemon.start` entry),
//! then call [`spawn_writer`]. The
//! returned [`AuditChannel`] is the runtime producer handle (Clone,
//! cheap); [`AuditWriterHandle`] is held by the daemon for shutdown.
//! On shutdown the daemon calls [`AuditWriterHandle::shutdown`] which
//! pushes a `Shutdown(oneshot)` sentinel through the same mpsc, awaits
//! the ack, and joins the task — guaranteeing every message queued
//! before shutdown (including the final `daemon.stop`) hits disk.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::audit::{AuditActor, AuditLog, AuditResult};

/// Hook fired when [`AuditLog::append`] returns an error inside the
/// writer task. Installed at boot by daemons that want to surface
/// append failures through their alerting subsystem (cross-crate
/// boundary: `shared-alerting` depends on `shared-audit`, so the
/// reverse direction would cycle — a function-pointer hook avoids
/// that). Called at most once per failed append.
pub type AppendFailureHook = fn(op: &str, error: &str);

static APPEND_FAILURE_HOOK: std::sync::OnceLock<AppendFailureHook> = std::sync::OnceLock::new();

/// Install the append-failure hook. Idempotent: a second call is a
/// no-op (returns Err). Daemons install
/// `shared_alerting::record::audit_append_failed` here.
pub fn set_append_failure_hook(hook: AppendFailureHook) -> Result<(), AppendFailureHook> {
    APPEND_FAILURE_HOOK.set(hook)
}

fn audit_append_failed(op: &str, error: &str) {
    if let Some(hook) = APPEND_FAILURE_HOOK.get() {
        hook(op, error);
    }
}

/// Bounded mpsc capacity. 1024 entries × ~few KiB ≈ a few MiB worst-case
/// in flight. Audit traffic is normally well under one entry per second;
/// this much headroom absorbs bursts (mass-rate-limit-rollup flush at a
/// 60 s tick, replay storms) without backpressuring producers.
pub const AUDIT_CHANNEL_CAPACITY: usize = 1024;

/// Wire form between producer and writer task.
enum AuditMessage {
    Entry {
        op: String,
        actor: AuditActor,
        params: Value,
        result: AuditResult,
    },
    /// Drain marker. Writer processes everything queued before the
    /// marker (FIFO), then sends `()` on the oneshot and exits its
    /// loop. Used by shutdown so the caller can guarantee all prior
    /// `try_append` calls have hit disk before the daemon exits.
    Shutdown(oneshot::Sender<()>),
}

/// Producer handle. Cheaply `Clone`able — every clone is an
/// independent `mpsc::Sender` against the same channel.
#[derive(Clone)]
pub struct AuditChannel {
    tx: mpsc::Sender<AuditMessage>,
}

impl AuditChannel {
    /// Non-blocking enqueue. Returns immediately even when called from
    /// inside a `spawn_blocking` SCSI handler. Channel-full drops are
    /// counted in `<product>_audit_queue_drops_total` and logged at
    /// WARN; losing an audit entry beats stalling a SCSI WRITE.
    pub fn try_append(&self, op: &str, actor: AuditActor, params: Value, result: AuditResult) {
        let msg = AuditMessage::Entry {
            op: op.to_string(),
            actor,
            params,
            result,
        };
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                shared_telemetry::record::audit_queue_drop();
                tracing::warn!(
                    "audit: queue full ({} cap), dropping {op}",
                    AUDIT_CHANNEL_CAPACITY,
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task has exited (post-shutdown or crashed).
                // Silent on the closed path — shutdown is the normal
                // case and would otherwise spam the journal as
                // subsystems wind down concurrently.
                shared_telemetry::record::audit_queue_drop();
            }
        }
    }

    /// Append one rate-limiter rollup entry, formatted identically
    /// wherever a [`Rollup`](crate::audit_ratelimit::Rollup) is emitted —
    /// the periodic flush task, daemon shutdown drain, and the inline
    /// emission `decide` returns when a window expires mid-flood
    /// (issue #202). Best-effort, like [`Self::try_append`].
    pub fn append_rollup(&self, rollup: &crate::audit_ratelimit::Rollup) {
        let params = serde_json::json!({
            "suppressed_count": rollup.suppressed_count,
            "window_seconds": rollup.window_seconds,
            "key": rollup.key,
        });
        let detail = format!(
            "{} additional event(s) suppressed in {}s window",
            rollup.suppressed_count, rollup.window_seconds
        );
        self.try_append(
            &rollup.op,
            rollup.actor.clone(),
            params,
            AuditResult::Error(detail),
        );
    }
}

/// Shutdown handle for the writer task. Held by the daemon's main
/// scope; not cloned, not stored in `DaemonState`.
pub struct AuditWriterHandle {
    /// Carried separately so [`shutdown`] can issue the drain sentinel
    /// without depending on the daemon also dropping its
    /// `AuditChannel` clones first.
    sentinel_tx: mpsc::Sender<AuditMessage>,
    join: JoinHandle<()>,
}

impl AuditWriterHandle {
    /// Push a `Shutdown` marker into the FIFO and await the writer's
    /// ack. Every entry the producer queued before this call has hit
    /// disk by the time the await resolves. Then joins the task.
    ///
    /// Best-effort: if the writer has already exited (e.g. panicked)
    /// the oneshot resolves to `Err` and we return without erroring —
    /// shutdown should never fail loudly.
    pub async fn shutdown(self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        // If the channel is already closed (writer exited) try_send
        // fails; we still attempt to join below so the task's panic
        // (if any) surfaces in the join.
        let _ = self.sentinel_tx.send(AuditMessage::Shutdown(ack_tx)).await;
        // Wait for the writer to confirm it processed everything up to
        // and including the sentinel.
        let _ = ack_rx.await;
        // Drop the sentinel sender so any remaining producer clones
        // don't keep the channel open forever.
        drop(self.sentinel_tx);
        let _ = self.join.await;
    }
}

/// Spawn the dedicated writer task and return a producer handle plus a
/// shutdown handle. Call once, after startup-time sync writes are
/// done.
pub fn spawn_writer(log: Arc<AuditLog>) -> (AuditChannel, AuditWriterHandle) {
    let (tx, rx) = mpsc::channel(AUDIT_CHANNEL_CAPACITY);
    let join = tokio::spawn(writer_loop(rx, log));
    let channel = AuditChannel { tx: tx.clone() };
    let handle = AuditWriterHandle {
        sentinel_tx: tx,
        join,
    };
    (channel, handle)
}

async fn writer_loop(mut rx: mpsc::Receiver<AuditMessage>, log: Arc<AuditLog>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            AuditMessage::Entry {
                op,
                actor,
                params,
                result,
            } => {
                // Run the blocking append on the blocking pool, not inline
                // on a tokio runtime worker: each append does two
                // fsync(2)s (entry file + chain.state), and the first
                // append after UTC midnight additionally reads the whole
                // previous day's file and zstd-compresses it in memory. On
                // a loaded data disk a queued burst would otherwise pin a
                // runtime worker for seconds — delaying co-scheduled
                // iSCSI/NVMe/HTTP tasks and filling the channel into the
                // documented audit_queue_drops (issue #270). Awaited, so
                // entries still append strictly in FIFO chain order.
                let log_for_append = Arc::clone(&log);
                let join = tokio::task::spawn_blocking(move || {
                    log_for_append
                        .append(&op, actor, params, result)
                        .map_err(|e| (op, e))
                })
                .await;
                match join {
                    Ok(Ok(_seq)) => {}
                    Ok(Err((op, e))) => {
                        let err_str = e.to_string();
                        tracing::warn!("audit: writer task: append {op} failed: {err_str}");
                        audit_append_failed(&op, &err_str);
                    }
                    Err(join_err) => {
                        tracing::warn!("audit: writer task: append join failed: {join_err}");
                    }
                }
            }
            AuditMessage::Shutdown(ack) => {
                // FIFO ordering means every prior Entry has already
                // been processed — the writer ran them in arrival
                // order before reaching this match arm.
                let _ = ack.send(());
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditConfig, AuditMode};
    use serde_json::json;
    use tempfile::TempDir;

    fn open_log(dir: &TempDir) -> Arc<AuditLog> {
        let cfg = AuditConfig::new(dir.path(), AuditMode::TamperEvident);
        Arc::new(AuditLog::open(cfg).unwrap())
    }

    #[tokio::test]
    async fn writer_appends_in_order_and_drains_on_shutdown() {
        let dir = TempDir::new().unwrap();
        let log = open_log(&dir);
        let (chan, handle) = spawn_writer(Arc::clone(&log));

        for i in 0..10 {
            chan.try_append(
                "test.op",
                AuditActor::system(),
                json!({"i": i}),
                AuditResult::Ok,
            );
        }
        handle.shutdown().await;

        let report = log.verify().unwrap();
        assert_eq!(report.entries_checked, 10);
        assert_eq!(report.last_seq, 10);
    }

    #[tokio::test]
    async fn shutdown_drains_pending_entries_in_fifo_order() {
        let dir = TempDir::new().unwrap();
        let log = open_log(&dir);
        let (chan, handle) = spawn_writer(Arc::clone(&log));

        // Fill aggressively then immediately request shutdown — every
        // queued entry must still hit disk before shutdown returns.
        for i in 0..100 {
            chan.try_append(
                "burst",
                AuditActor::system(),
                json!({"i": i}),
                AuditResult::Ok,
            );
        }
        handle.shutdown().await;

        let entries = crate::audit::read_entries(dir.path(), None, None).unwrap();
        assert_eq!(entries.len(), 100);
        for (idx, e) in entries.iter().enumerate() {
            assert_eq!(e.op, "burst");
            assert_eq!(e.params["i"].as_u64().unwrap(), idx as u64);
        }
    }

    #[tokio::test]
    async fn try_append_after_shutdown_is_silent_drop() {
        let dir = TempDir::new().unwrap();
        let log = open_log(&dir);
        let (chan, handle) = spawn_writer(Arc::clone(&log));
        handle.shutdown().await;

        // Channel is closed; try_send hits the Closed branch and the
        // call returns without panicking.
        chan.try_append("late", AuditActor::system(), json!({}), AuditResult::Ok);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_producer_concurrent_all_entries_drain() {
        // Four producer tasks × 250 entries each = 1000 entries through
        // a single single-writer-task drained mpsc. Asserts that no
        // entry is dropped, no entry is double-written, and the chain
        // stays verifiable.
        let dir = TempDir::new().unwrap();
        let log = open_log(&dir);
        let (chan, handle) = spawn_writer(Arc::clone(&log));

        let mut tasks = Vec::new();
        for producer in 0..4u64 {
            let c = chan.clone();
            tasks.push(tokio::spawn(async move {
                for i in 0..250u64 {
                    c.try_append(
                        "concurrent",
                        AuditActor::system(),
                        json!({"producer": producer, "i": i}),
                        AuditResult::Ok,
                    );
                    // Tiny yield to interleave producers without
                    // serializing on a sleep_until.
                    if i % 32 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        handle.shutdown().await;

        let entries = crate::audit::read_entries(dir.path(), None, None).unwrap();
        assert_eq!(entries.len(), 1000, "all 4 × 250 entries must hit disk");

        // Per-producer FIFO ordering — within one producer's stream the
        // `i` counter must be monotonic. Cross-producer interleaving is
        // not constrained.
        let mut per_producer: std::collections::HashMap<u64, Vec<u64>> =
            std::collections::HashMap::new();
        for e in &entries {
            assert_eq!(e.op, "concurrent");
            let p = e.params["producer"].as_u64().unwrap();
            let i = e.params["i"].as_u64().unwrap();
            per_producer.entry(p).or_default().push(i);
        }
        assert_eq!(per_producer.len(), 4, "all four producers represented");
        for (p, seq) in &per_producer {
            assert_eq!(
                seq.len(),
                250,
                "producer {p} got {} entries, expected 250",
                seq.len()
            );
            for w in seq.windows(2) {
                assert!(
                    w[0] < w[1],
                    "producer {p} out-of-order: {} then {}",
                    w[0],
                    w[1]
                );
            }
        }

        // Chain verification must pass — single-writer invariant held.
        let report = log.verify().unwrap();
        assert_eq!(report.entries_checked, 1000);
        assert_eq!(report.last_seq, 1000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cloned_channels_are_independent_senders() {
        // Each clone is its own mpsc::Sender against the same queue.
        // Verifies clones can be moved into separate tasks and used
        // concurrently without coordination, and that dropping one
        // clone doesn't close the channel for the others.
        let dir = TempDir::new().unwrap();
        let log = open_log(&dir);
        let (chan, handle) = spawn_writer(Arc::clone(&log));

        let c1 = chan.clone();
        let c2 = chan.clone();
        let c3 = chan.clone();
        drop(chan); // Original goes out of scope; clones must keep going.

        let t1 = tokio::spawn(async move {
            for i in 0..50 {
                c1.try_append("c1", AuditActor::system(), json!({"i": i}), AuditResult::Ok);
            }
        });
        let t2 = tokio::spawn(async move {
            for i in 0..50 {
                c2.try_append("c2", AuditActor::system(), json!({"i": i}), AuditResult::Ok);
            }
        });
        let t3 = tokio::spawn(async move {
            for i in 0..50 {
                c3.try_append("c3", AuditActor::system(), json!({"i": i}), AuditResult::Ok);
            }
        });
        t1.await.unwrap();
        t2.await.unwrap();
        t3.await.unwrap();
        handle.shutdown().await;

        let entries = crate::audit::read_entries(dir.path(), None, None).unwrap();
        assert_eq!(entries.len(), 150);
        let count_op = |op: &str| entries.iter().filter(|e| e.op == op).count();
        assert_eq!(count_op("c1"), 50);
        assert_eq!(count_op("c2"), 50);
        assert_eq!(count_op("c3"), 50);
    }
}
