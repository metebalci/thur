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
//!
//! ## Pool rows
//!
//! One [`PoolEntry`] is emitted per (backend, namespace) pair. The
//! cap, waiters_now, and backpressure_waits_total counters are
//! backend-wide; they are repeated verbatim across every row that
//! shares a backend. The CLI renderer groups by backend and prints
//! those columns once per group. `namespace = None` is the
//! global-dedup bucket (shared per-backend pool); a `Some(ns)` row is
//! a `DedupScope::Local` volume / cartridge. Every backend always
//! emits at least its `None` row even when the global bucket is empty
//! so the cap and backpressure counters stay visible.

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
    /// All cloud-backend pool budgets, keyed by backend name. The
    /// monitor renderer flattens each budget's per-namespace
    /// breakdown into one [`PoolEntry`] per (backend, namespace).
    fn pool_budgets(&self) -> HashMap<String, Arc<PoolBudget>>;
    /// Human-readable label for a pool namespace, if any. Called once
    /// per (backend, namespace) at build time; the renderer falls
    /// back to the raw namespace string when this returns `None`.
    ///
    /// VSA: namespace is `hex(volume_uuid)`; resolver looks the
    /// volume name up in the registry.
    ///
    /// VTL: namespace is already the cartridge label, so the
    /// resolver just echoes it back.
    fn pool_namespace_label(&self, backend: &str, namespace: &str) -> Option<String>;
    /// Per-product fields. VSA returns `Vsa { volumes_online,
    /// iscsi_sessions, nvmetcp_sessions }`; VTL returns `Vtl { … }`.
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
        /// Active iSCSI sessions.
        iscsi_sessions: u64,
        /// Active NVMe/TCP controller associations. 0 when the NVMe/TCP
        /// transport isn't enabled.
        nvmetcp_sessions: u64,
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
    pub storage: Vec<StorageEntry>,
    /// Per-backend lifetime dedup byte totals (one row per backend that
    /// has sealed at least one chunk). `logical / unique` is the
    /// cumulative-since-restart dedup ratio. Append-only — ignores
    /// eviction / delete, so it is a trend, not a current-on-disk
    /// figure (that is the on-demand `system stats` scan).
    pub dedup: Vec<DedupEntry>,
    pub audit: AuditEntry,
}

/// One row of the Pool table — keyed on (backend, namespace).
///
/// `namespace = None` is the global-dedup bucket; `Some(ns)` is a
/// local-dedup volume / cartridge. `label` is the human-readable
/// name resolved via [`MonitorState::pool_namespace_label`] (volume
/// name on VSA, cartridge label on VTL); `None` for the global row.
///
/// `cap_bytes`, `waiters_now`, and `backpressure_waits_total` are
/// backend-wide and repeated identically across every row sharing a
/// backend — the CLI renderer prints them once per backend group.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PoolEntry {
    pub backend: String,
    pub namespace: Option<String>,
    pub label: Option<String>,
    pub used_bytes: u64,
    pub cap_bytes: u64,
    pub waiters_now: i64,
    pub backpressure_waits_total: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StorageEntry {
    pub backend: String,
    pub put_ops_total: u64,
    pub get_ops_total: u64,
    pub put_bytes_total: u64,
    pub get_bytes_total: u64,
    pub errors_total: u64,
}

/// One row of the Dedup table — per backend. `logical_bytes` is every
/// byte sealed into this backend's pool (pre-dedup); `unique_bytes` is
/// only the bytes that actually grew the pool (first-time-ever seals).
/// `logical / unique` is the lifetime dedup ratio, summed across
/// local + global scope. Cumulative since daemon start.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DedupEntry {
    pub backend: String,
    pub logical_bytes: u64,
    pub unique_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuditEntry {
    pub entries_total: u64,
}

/// Tick loop. Runs until the subscriber drops the stream — the
/// `JobRegistry` reaper handles cancellation; we never emit `Done`.
pub async fn run_monitor<S: MonitorState>(emitter: JobEmitter, _body: serde_json::Value, state: S) {
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

/// Compose one [`MonitorSnapshot`] from the current state. The tick
/// loop calls this once per second; the Web UI's single-shot
/// `/api/v1/monitor` handler (`shared-admin-webui`) calls it once per
/// request, which is why it's `pub`.
pub fn build_payload<S: MonitorState>(state: &S) -> MonitorSnapshot {
    let live = state.live_stats();
    let snap = live.snapshot();
    let ts_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut pool: Vec<PoolEntry> = Vec::new();
    for (backend_name, budget) in state.pool_budgets() {
        let backend_snap = snap.pool.get(&backend_name).copied().unwrap_or_default();
        let cap = budget.cap_bytes();
        let breakdown = budget.per_namespace_used();
        let has_global = breakdown.iter().any(|(ns, _)| ns.is_none());
        // Always emit at least the (backend, None) row so cap +
        // backpressure counters are visible even when the global
        // bucket is empty.
        if !has_global {
            pool.push(PoolEntry {
                backend: backend_name.clone(),
                namespace: None,
                label: None,
                used_bytes: 0,
                cap_bytes: cap,
                waiters_now: backend_snap.waiters_now,
                backpressure_waits_total: backend_snap.waits_total,
            });
        }
        for (ns, used) in breakdown {
            let label = ns
                .as_deref()
                .and_then(|n| state.pool_namespace_label(&backend_name, n));
            pool.push(PoolEntry {
                backend: backend_name.clone(),
                namespace: ns,
                label,
                used_bytes: used,
                cap_bytes: cap,
                waiters_now: backend_snap.waiters_now,
                backpressure_waits_total: backend_snap.waits_total,
            });
        }
    }
    // Sort by (backend, namespace). `None` (global) ahead of any
    // `Some(_)` so the backend's global row prints first.
    pool.sort_by(|a, b| {
        a.backend
            .cmp(&b.backend)
            .then_with(|| a.namespace.cmp(&b.namespace))
    });

    let mut storage: Vec<StorageEntry> = snap
        .storage
        .iter()
        .map(|(name, c)| StorageEntry {
            backend: name.clone(),
            put_ops_total: c.put_ops,
            get_ops_total: c.get_ops,
            put_bytes_total: c.put_bytes,
            get_bytes_total: c.get_bytes,
            errors_total: c.errors,
        })
        .collect();
    storage.sort_by(|a, b| a.backend.cmp(&b.backend));

    let mut dedup: Vec<DedupEntry> = snap
        .chunk
        .iter()
        .map(|(name, c)| DedupEntry {
            backend: name.clone(),
            logical_bytes: c.logical_bytes,
            unique_bytes: c.unique_bytes,
        })
        .collect();
    dedup.sort_by(|a, b| a.backend.cmp(&b.backend));

    MonitorSnapshot {
        ts_unix,
        daemon: state.daemon_name().to_string(),
        version: state.version().to_string(),
        started_at_unix: state.started_at_unix(),
        product: state.snapshot_product(),
        pool,
        storage,
        dedup,
        audit: AuditEntry {
            entries_total: snap.audit_entries_total,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Clone)]
    struct FakeState {
        started_at: i64,
        live: Arc<LiveStats>,
        budgets: HashMap<String, Arc<PoolBudget>>,
        labels: HashMap<(String, String), String>,
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
        fn pool_namespace_label(&self, backend: &str, namespace: &str) -> Option<String> {
            self.labels
                .get(&(backend.to_string(), namespace.to_string()))
                .cloned()
        }
        fn snapshot_product(&self) -> ProductSnapshot {
            ProductSnapshot::Vsa {
                volumes_online: 3,
                iscsi_sessions: 1,
                nvmetcp_sessions: 2,
            }
        }
    }

    #[test]
    fn build_payload_sorts_backends_and_threads_counters() {
        let live = Arc::new(LiveStats::default());
        // Bump a couple of counters so the snapshot is non-empty.
        live.record_storage_op("z-backend", "put", "ok", 100);
        live.record_storage_op("a-backend", "get", "ok", 50);
        live.record_audit_entry();
        live.record_audit_entry();
        // Dedup bytes only on one backend — the dedup table should
        // carry just that row, and sort by backend.
        live.record_chunk_logical_bytes("z-backend", 1000);
        live.record_chunk_unique_bytes("z-backend", 250);

        let state = FakeState {
            started_at: 1_000_000,
            live,
            budgets: HashMap::new(),
            labels: HashMap::new(),
        };
        let snap = build_payload(&state);

        assert_eq!(snap.daemon, "fake");
        assert_eq!(snap.started_at_unix, 1_000_000);
        assert_eq!(snap.storage.len(), 2);
        assert_eq!(snap.storage[0].backend, "a-backend");
        assert_eq!(snap.storage[0].get_ops_total, 1);
        assert_eq!(snap.storage[1].backend, "z-backend");
        assert_eq!(snap.storage[1].put_ops_total, 1);
        assert_eq!(snap.audit.entries_total, 2);

        // Only the backend with seals appears in the dedup table.
        assert_eq!(snap.dedup.len(), 1);
        assert_eq!(snap.dedup[0].backend, "z-backend");
        assert_eq!(snap.dedup[0].logical_bytes, 1000);
        assert_eq!(snap.dedup[0].unique_bytes, 250);

        match snap.product {
            ProductSnapshot::Vsa {
                volumes_online,
                iscsi_sessions,
                nvmetcp_sessions,
            } => {
                assert_eq!(volumes_online, 3);
                assert_eq!(iscsi_sessions, 1);
                assert_eq!(nvmetcp_sessions, 2);
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
            labels: HashMap::new(),
        };
        let snap = build_payload(&state);
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: MonitorSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.started_at_unix, 42);
        assert_eq!(parsed.daemon, "fake");
    }

    /// Two namespaces under one backend → two rows, sorted with the
    /// global (`None`) row first, then alphabetical. Each row carries
    /// the same cap and backpressure counters; the per-namespace
    /// `used_bytes` differs.
    #[test]
    fn build_payload_emits_one_row_per_backend_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let budget = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 4096, 0, 80));
        budget
            .try_reserve(100, None, Duration::from_secs(1))
            .unwrap();
        budget
            .try_reserve(200, Some("0011aabb"), Duration::from_secs(1))
            .unwrap();
        budget
            .try_reserve(300, Some("0099ccdd"), Duration::from_secs(1))
            .unwrap();

        let mut labels = HashMap::new();
        labels.insert(
            ("primary".to_string(), "0011aabb".to_string()),
            "vol-a".to_string(),
        );
        labels.insert(
            ("primary".to_string(), "0099ccdd".to_string()),
            "vol-b".to_string(),
        );

        let state = FakeState {
            started_at: 0,
            live: Arc::new(LiveStats::default()),
            budgets: HashMap::from([("primary".to_string(), budget)]),
            labels,
        };
        let snap = build_payload(&state);

        assert_eq!(snap.pool.len(), 3);
        assert_eq!(snap.pool[0].namespace, None);
        assert_eq!(snap.pool[0].label, None);
        assert_eq!(snap.pool[0].used_bytes, 100);
        assert_eq!(snap.pool[1].namespace.as_deref(), Some("0011aabb"));
        assert_eq!(snap.pool[1].label.as_deref(), Some("vol-a"));
        assert_eq!(snap.pool[1].used_bytes, 200);
        assert_eq!(snap.pool[2].namespace.as_deref(), Some("0099ccdd"));
        assert_eq!(snap.pool[2].label.as_deref(), Some("vol-b"));
        assert_eq!(snap.pool[2].used_bytes, 300);
        // Cap is repeated.
        assert!(snap.pool.iter().all(|p| p.cap_bytes == 4096));
        // Backend is repeated.
        assert!(snap.pool.iter().all(|p| p.backend == "primary"));
    }

    /// A backend with no reservations still emits one synthetic
    /// (backend, None) row so the cap and backpressure counters are
    /// visible on an empty pool.
    #[test]
    fn build_payload_emits_synthetic_global_row_on_empty_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let budget = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 8192, 0, 80));

        let state = FakeState {
            started_at: 0,
            live: Arc::new(LiveStats::default()),
            budgets: HashMap::from([("primary".to_string(), budget)]),
            labels: HashMap::new(),
        };
        let snap = build_payload(&state);

        assert_eq!(snap.pool.len(), 1);
        assert_eq!(snap.pool[0].backend, "primary");
        assert_eq!(snap.pool[0].namespace, None);
        assert_eq!(snap.pool[0].used_bytes, 0);
        assert_eq!(snap.pool[0].cap_bytes, 8192);
    }

    /// Two backends side by side, each with its own per-namespace
    /// breakdown — rows are sorted (backend, namespace) globally.
    #[test]
    fn build_payload_sorts_two_backends_with_breakdowns_together() {
        let tmp = tempfile::tempdir().unwrap();
        let b_primary = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 4096, 0, 80));
        b_primary
            .try_reserve(100, None, Duration::from_secs(1))
            .unwrap();
        b_primary
            .try_reserve(200, Some("vol-a"), Duration::from_secs(1))
            .unwrap();
        let b_secondary = Arc::new(PoolBudget::new(tmp.path().to_path_buf(), 4096, 0, 80));
        b_secondary
            .try_reserve(50, Some("vol-z"), Duration::from_secs(1))
            .unwrap();

        let state = FakeState {
            started_at: 0,
            live: Arc::new(LiveStats::default()),
            budgets: HashMap::from([
                ("primary".to_string(), b_primary),
                ("secondary".to_string(), b_secondary),
            ]),
            labels: HashMap::new(),
        };
        let snap = build_payload(&state);

        // primary/None, primary/vol-a, secondary/None (synthetic), secondary/vol-z
        assert_eq!(snap.pool.len(), 4);
        assert_eq!(snap.pool[0].backend, "primary");
        assert_eq!(snap.pool[0].namespace, None);
        assert_eq!(snap.pool[1].backend, "primary");
        assert_eq!(snap.pool[1].namespace.as_deref(), Some("vol-a"));
        assert_eq!(snap.pool[2].backend, "secondary");
        assert_eq!(snap.pool[2].namespace, None);
        assert_eq!(snap.pool[3].backend, "secondary");
        assert_eq!(snap.pool[3].namespace.as_deref(), Some("vol-z"));
    }
}
