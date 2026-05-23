// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.monitor` job — daemon-routed live activity feed.
//!
//! One job kind, same shape on both products. The handler is an
//! infinite tick loop that emits one `MonitorSnapshot` (JSON-encoded
//! into the `JobEvent::Log.message`) per second until the CLI
//! subscriber drops the stream. The CLI side keeps a small ring
//! buffer of recent snapshots and diffs to compute the 60 s / 5 m
//! windows it displays.
//!
//! Stream-drop is the only stop signal — there is no `Done` emission.
//! The job registry's reaper cleans the spawned task up when the
//! emitter goes silent.
//!
//! Cross-product: VTL and VSA both `impl MonitorState for AdminState`
//! and dispatch `"system.monitor"` here. The product-specific bits
//! (volumes vs cartridges, drives, sessions) flow through the
//! [`MonitorState::snapshot_product`] hook.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use shared_admin_server::{JobEmitter, JobEvent};
use shared_pool::PoolBudget;
use shared_telemetry::LiveStats;

/// Hooks the monitor handler needs at each tick. Both daemons impl
/// this on their `AdminState`. Keep the surface narrow: anything that
/// can be computed per-tick from existing read accessors stays in
/// `build_payload`; this trait only carries the inputs.
pub trait MonitorState: Clone + Send + Sync + 'static {
    /// Daemon name for the header row, e.g. `"thurvsad"`.
    fn daemon_name(&self) -> &str;
    /// Version string matching `system daemon-health`.
    fn version(&self) -> &str;
    /// Unix epoch seconds the daemon started at.
    fn started_at_unix(&self) -> i64;
    /// In-process counter sidecar from `shared-telemetry`. Cloned per
    /// tick; the underlying counters are shared.
    fn live_stats(&self) -> Arc<LiveStats>;
    /// All cloud-backend pool budgets, keyed by backend name. Used
    /// for the pool used/cap rows.
    fn pool_budgets(&self) -> HashMap<String, Arc<PoolBudget>>;
    /// Per-product fields. VSA returns `Vsa { volumes_online,
    /// sessions_active }`; VTL returns `Vtl { … }`.
    fn snapshot_product(&self) -> ProductSnapshot;
}

/// Product-specific portion of the monitor snapshot. The tag-discriminated
/// JSON keeps the CLI render path generic — it switches on `product.kind`
/// to pick which two summary lines to draw.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ProductSnapshot {
    Vsa {
        volumes_online: u64,
        sessions_active: u64,
    },
    Vtl {
        cartridges_loaded: u64,
        cartridges_total: u64,
        drives_busy: u64,
        drives_total: u64,
        sessions_active: u64,
    },
}

/// One tick of the monitor feed. Encoded as a JSON string in the
/// `JobEvent::Log.message` field.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MonitorSnapshot {
    /// Wall-clock seconds the snapshot was taken. CLI uses this
    /// minus `started_at_unix` to render the uptime in the header.
    pub ts_unix: i64,
    pub daemon: String,
    pub version: String,
    pub started_at_unix: i64,
    pub product: ProductSnapshot,
    pub pool: Vec<PoolEntry>,
    pub cloud: Vec<CloudEntry>,
    pub audit: AuditEntry,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PoolEntry {
    pub backend: String,
    pub used_bytes: u64,
    pub cap_bytes: u64,
    pub waiters_now: i64,
    pub backpressure_waits_total: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CloudEntry {
    pub backend: String,
    pub put_ops_total: u64,
    pub get_ops_total: u64,
    pub put_bytes_total: u64,
    pub get_bytes_total: u64,
    pub errors_total: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuditEntry {
    pub entries_total: u64,
}

/// Tick loop. Runs until the subscriber drops the stream — the
/// `JobRegistry` reaper handles cancellation; we never emit `Done`.
pub async fn run_monitor<S: MonitorState>(
    emitter: JobEmitter,
    _body: serde_json::Value,
    state: S,
) {
    loop {
        let payload = build_payload(&state);
        let json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                // Serialization failure is a structural bug — fail loudly
                // via Done so the operator sees it, rather than silently
                // looping.
                emitter
                    .emit(JobEvent::done_with_error(
                        2,
                        format!("monitor: serialize payload: {}", e),
                    ))
                    .await;
                return;
            }
        };
        emitter.info(json).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn build_payload<S: MonitorState>(state: &S) -> MonitorSnapshot {
    let live = state.live_stats();
    let snap = live.snapshot();
    let ts_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut pool: Vec<PoolEntry> = state
        .pool_budgets()
        .into_iter()
        .map(|(name, b)| {
            let pool_snap = snap.pool.get(&name).copied().unwrap_or_default();
            PoolEntry {
                backend: name,
                used_bytes: b.current_bytes(),
                cap_bytes: b.cap_bytes(),
                waiters_now: pool_snap.waiters_now,
                backpressure_waits_total: pool_snap.waits_total,
            }
        })
        .collect();
    pool.sort_by(|a, b| a.backend.cmp(&b.backend));

    let mut cloud: Vec<CloudEntry> = snap
        .cloud
        .iter()
        .map(|(name, c)| CloudEntry {
            backend: name.clone(),
            put_ops_total: c.put_ops,
            get_ops_total: c.get_ops,
            put_bytes_total: c.put_bytes,
            get_bytes_total: c.get_bytes,
            errors_total: c.errors,
        })
        .collect();
    cloud.sort_by(|a, b| a.backend.cmp(&b.backend));

    MonitorSnapshot {
        ts_unix,
        daemon: state.daemon_name().to_string(),
        version: state.version().to_string(),
        started_at_unix: state.started_at_unix(),
        product: state.snapshot_product(),
        pool,
        cloud,
        audit: AuditEntry {
            entries_total: snap.audit_entries_total,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeState {
        started_at: i64,
        live: Arc<LiveStats>,
        budgets: HashMap<String, Arc<PoolBudget>>,
    }

    impl MonitorState for FakeState {
        fn daemon_name(&self) -> &str {
            "fake"
        }
        fn version(&self) -> &str {
            "0.0.0-test"
        }
        fn started_at_unix(&self) -> i64 {
            self.started_at
        }
        fn live_stats(&self) -> Arc<LiveStats> {
            Arc::clone(&self.live)
        }
        fn pool_budgets(&self) -> HashMap<String, Arc<PoolBudget>> {
            self.budgets.clone()
        }
        fn snapshot_product(&self) -> ProductSnapshot {
            ProductSnapshot::Vsa {
                volumes_online: 3,
                sessions_active: 1,
            }
        }
    }

    #[test]
    fn build_payload_sorts_backends_and_threads_counters() {
        let live = Arc::new(LiveStats::default());
        // Bump a couple of counters so the snapshot is non-empty.
        live.record_cloud_op("z-backend", "put", "ok", 100);
        live.record_cloud_op("a-backend", "get", "ok", 50);
        live.record_audit_entry();
        live.record_audit_entry();

        let state = FakeState {
            started_at: 1_000_000,
            live,
            budgets: HashMap::new(),
        };
        let snap = build_payload(&state);

        assert_eq!(snap.daemon, "fake");
        assert_eq!(snap.started_at_unix, 1_000_000);
        assert_eq!(snap.cloud.len(), 2);
        assert_eq!(snap.cloud[0].backend, "a-backend");
        assert_eq!(snap.cloud[0].get_ops_total, 1);
        assert_eq!(snap.cloud[1].backend, "z-backend");
        assert_eq!(snap.cloud[1].put_ops_total, 1);
        assert_eq!(snap.audit.entries_total, 2);

        match snap.product {
            ProductSnapshot::Vsa {
                volumes_online,
                sessions_active,
            } => {
                assert_eq!(volumes_online, 3);
                assert_eq!(sessions_active, 1);
            }
            _ => panic!("expected Vsa variant"),
        }
    }

    #[test]
    fn payload_serializes_round_trip() {
        let live = Arc::new(LiveStats::default());
        let state = FakeState {
            started_at: 42,
            live,
            budgets: HashMap::new(),
        };
        let snap = build_payload(&state);
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: MonitorSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.started_at_unix, 42);
        assert_eq!(parsed.daemon, "fake");
    }
}
