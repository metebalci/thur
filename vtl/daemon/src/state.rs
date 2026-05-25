// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared daemon state.
//!
//! `DaemonState` owns the long-lived handles every subsystem (iSCSI
//! protocol handlers, HTTP read endpoints, the future Unix-socket
//! admin API) reads through. The struct is constructed once at startup
//! and held behind `Arc`; subsystems clone the `Arc` into their own
//! tasks. Locks live on the individual fields, not on the outer
//! struct, so concurrent reads through different fields don't
//! contend.
//!
//! Construction order matters: the daemon's startup pipeline builds
//! the `Library`, audit log, pool budgets, and cloud-backend registry
//! before instantiating `DaemonState`, so this constructor only does
//! the cheap wiring (DriveManager, SessionManager, UnitAttention,
//! DiagnosticStore) and the `Arc` wrapping.

use core_mediachanger::{
    AuditChannel, AuditRateLimiter, CompressionAlgo, Library, ObjectStoreConfig, PoolBudget,
    TapeEvent,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

use crate::diagnostics::DiagnosticStore;
use crate::iscsi::drive_manager::DriveManager;
use crate::iscsi::server::ObjectStoreRegistry;
use crate::iscsi::session::SessionManager;
use crate::iscsi::unit_attention::UnitAttentionTracker;
use scsi_smc::changer::ElementAddressConfig;
use shared_admin_server::JobRegistry;

/// Inputs the daemon assembles before constructing `DaemonState`.
/// Grouped into a single struct because the constructor argument
/// list otherwise grows past ten parameters.
pub struct DaemonStateConfig {
    pub data_dir: PathBuf,
    pub tapes_root: PathBuf,
    pub library: Arc<Mutex<Library>>,
    pub element_config: ElementAddressConfig,
    /// Configured iSCSI target IQN. Read-only after boot. Threaded
    /// into state so HTTP `/sessions` can answer without holding an
    /// `Arc<IscsiServer>`.
    pub target_iqn: String,
    /// Configured iSCSI listen address (`ip:port`). Read-only after
    /// boot. Same rationale as `target_iqn`.
    pub listen_address: String,
    pub event_tx: broadcast::Sender<TapeEvent>,
    /// Producer-side handle for the audit log writer task. Cloned by
    /// every emitter; runtime appends never touch the underlying
    /// `AuditLog` mutex directly. See [`core_mediachanger::audit_channel`].
    pub audit_log: Option<AuditChannel>,
    /// On-disk directory the writer task is appending to. Resolved
    /// from `cfg.audit.dir` (with `<data_dir>/audit` fallback).
    /// Threaded into state because the daemon hands the `AuditLog`
    /// handle to the writer task and never holds it elsewhere — but
    /// admin endpoints (`audit tail/export/verify/rotate`) still need
    /// to know where the files live to read them out of band.
    pub audit_dir: PathBuf,
    pub audit_ratelimiter: Arc<AuditRateLimiter>,
    /// Runtime registry of constructed `Arc<dyn ObjectStoreBackend>`
    /// instances (one per name in `storage_config.backends`). Built at
    /// boot from `storage_config`.
    pub cloud_backends: ObjectStoreRegistry,
    /// Full `cloud:` section of the YAML conffile — tuning knobs plus
    /// the named backend definitions under `cloud.backends:`. Distinct
    /// from `cloud_backends` (the runtime registry above).
    pub storage_config: Arc<ObjectStoreConfig>,
    /// Full `keystore:` section of the YAML conffile — the named
    /// `keystore.backends:` map. Used by the cartridge-create admin
    /// handler to resolve the at-rest keystore backend, and by
    /// `cartridge key {migrate,show}` for the same lookups.
    pub keystore_config: Arc<shared_keystore::KeystoreYamlConfig>,
    pub num_drives: usize,
    pub drive_compression_algorithm: CompressionAlgo,
    pub drive_compression_zstd_level: i32,
    pub pool_budgets: HashMap<String, Arc<PoolBudget>>,
    pub backpressure_max_wait: Duration,
}

/// Long-lived state shared by every daemon subsystem. Instantiated
/// once during startup; subsystems hold `Arc<DaemonState>`.
pub struct DaemonState {
    pub data_dir: PathBuf,
    pub library: Arc<Mutex<Library>>,
    pub drive_manager: Arc<DriveManager>,
    pub session_manager: Arc<SessionManager>,
    pub ua_tracker: Arc<Mutex<UnitAttentionTracker>>,
    pub element_config: ElementAddressConfig,
    /// Configured iSCSI target IQN. Read-only after boot.
    pub target_iqn: String,
    /// Configured iSCSI listen address (`ip:port`). Read-only after boot.
    pub listen_address: String,
    pub event_tx: broadcast::Sender<TapeEvent>,
    /// Producer-side handle for the audit log writer task. Cloned by
    /// every emitter; runtime appends never touch the underlying
    /// `AuditLog` mutex directly. See [`core_mediachanger::audit_channel`].
    pub audit_log: Option<AuditChannel>,
    /// Configured audit directory. Read-only side door for admin
    /// endpoints that need to walk files (`audit tail/export/verify`)
    /// without asking the writer task.
    pub audit_dir: PathBuf,
    /// Rate-limiter for flood-prone host-driven failure events
    /// (CHAP failures, MOVE MEDIUM refusals). One instance shared by
    /// every emission site that opts in; lifecycle events bypass it.
    pub audit_ratelimiter: Arc<AuditRateLimiter>,
    /// Runtime registry of constructed `Arc<dyn ObjectStoreBackend>`
    /// instances (one per name in `storage_config.backends`).
    pub cloud_backends: ObjectStoreRegistry,
    /// Full `cloud:` section of the YAML conffile — tuning knobs plus
    /// the named backend definitions under `cloud.backends:`.
    pub storage_config: Arc<ObjectStoreConfig>,
    /// Full `keystore:` section of the YAML conffile (named
    /// `keystore.backends:` map). Read at boot, shared via Arc with
    /// admin handlers (`cartridge_create` for the at-rest wrap
    /// target).
    pub keystore_config: Arc<shared_keystore::KeystoreYamlConfig>,
    pub diagnostic_store: Arc<DiagnosticStore>,
    /// Long-running admin jobs (`system gc`, `verify`, `cloud check`,
    /// …). Populated as work is dispatched through the
    /// `/api/v1/jobs/*` endpoints; see `admin::jobs`.
    pub jobs: Arc<JobRegistry>,
    /// Unix epoch seconds the daemon started at. Captured once in
    /// `DaemonState::new`. Surfaces in `system monitor`'s header row
    /// (uptime = now - started_at).
    pub started_at_unix: i64,
    /// Cloned copy of the pool-budgets map for read-only consumers
    /// (the `system.monitor` job handler). The authoritative copy
    /// lives inside `DriveManager`; both share the same
    /// `Arc<PoolBudget>` instances so reads are coherent.
    pub pool_budgets: HashMap<String, Arc<PoolBudget>>,
}

impl DaemonState {
    pub fn new(cfg: DaemonStateConfig) -> Self {
        // Clone the budgets before handing them to DriveManager so the
        // `system.monitor` handler can read used / cap / waiters_now
        // without going through the manager's internal accessors.
        let pool_budgets = cfg.pool_budgets.clone();
        let drive_manager = {
            let mut dm = DriveManager::with_compression_settings(
                cfg.num_drives,
                cfg.tapes_root,
                cfg.drive_compression_algorithm,
                cfg.drive_compression_zstd_level,
            );
            dm.set_pool_budgets(cfg.pool_budgets, cfg.backpressure_max_wait);
            // Capture the library-wide drive LTO generation so
            // load_cartridge can gate on cartridge generation at
            // runtime (higher-gen cartridge into lower-gen drive
            // refusal).
            let library_lto_gen = cfg
                .library
                .lock()
                .map(|lib| lib.lto_generation())
                .unwrap_or(0);
            dm.set_library_lto_generation(library_lto_gen);
            Arc::new(dm)
        };

        Self {
            data_dir: cfg.data_dir,
            library: cfg.library,
            drive_manager,
            session_manager: Arc::new(SessionManager::new()),
            ua_tracker: Arc::new(Mutex::new(UnitAttentionTracker::new())),
            element_config: cfg.element_config,
            target_iqn: cfg.target_iqn,
            listen_address: cfg.listen_address,
            event_tx: cfg.event_tx,
            audit_log: cfg.audit_log,
            audit_dir: cfg.audit_dir,
            audit_ratelimiter: cfg.audit_ratelimiter,
            cloud_backends: cfg.cloud_backends,
            storage_config: cfg.storage_config,
            keystore_config: cfg.keystore_config,
            diagnostic_store: Arc::new(DiagnosticStore::new()),
            jobs: Arc::new(JobRegistry::new()),
            started_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            pool_budgets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scsi_smc::changer::ElementAddressConfig;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    #[test]
    fn daemon_state_new_wires_every_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib_root = dir.path().join("library");
        let tapes_root = dir.path().join("tapes");
        let library = core_mediachanger::Library::initialize(
            &lib_root,
            &tapes_root,
            10,
            2,
            3,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .expect("init library");

        let (event_tx, _event_rx) = broadcast::channel(16);
        let element_config = ElementAddressConfig::new(0, 1001, 10, 101, 2, 1, 3);

        let cfg = DaemonStateConfig {
            data_dir: dir.path().to_path_buf(),
            tapes_root: tapes_root.clone(),
            library: Arc::new(Mutex::new(library)),
            element_config,
            target_iqn: "iqn.2025-10.com.metebalci:thurvtl".to_string(),
            listen_address: "0.0.0.0:3260".to_string(),
            event_tx,
            audit_log: None,
            audit_dir: dir.path().join("audit"),
            audit_ratelimiter: Arc::new(AuditRateLimiter::new(Duration::from_secs(60))),
            cloud_backends: Arc::new(TokioMutex::new(HashMap::new())),
            storage_config: Arc::new(ObjectStoreConfig::default()),
            keystore_config: Arc::new(shared_keystore::KeystoreYamlConfig::default()),
            num_drives: 3,
            drive_compression_algorithm: CompressionAlgo::Lz4,
            drive_compression_zstd_level: 3,
            pool_budgets: HashMap::new(),
            backpressure_max_wait: Duration::from_secs(30),
        };

        let state = DaemonState::new(cfg);
        assert_eq!(state.target_iqn, "iqn.2025-10.com.metebalci:thurvtl");
        assert_eq!(state.listen_address, "0.0.0.0:3260");
        assert_eq!(state.element_config.storage_count, 10);
        assert_eq!(state.element_config.data_transfer_count, 3);
        assert!(state.audit_log.is_none());
        // Library is reachable and reports its initialized topology.
        let lib_drives = state
            .library
            .lock()
            .map(|l| l.lto_generation())
            .unwrap_or(0);
        assert_eq!(lib_drives, 8);
    }
}
