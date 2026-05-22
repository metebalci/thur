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
    AuditChannel, AuditRateLimiter, CloudConfig, CompressionAlgo, Library, PoolBudget, TapeEvent,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

use crate::diagnostics::DiagnosticStore;
use crate::iscsi::drive_manager::DriveManager;
use crate::iscsi::server::CloudBackendRegistry;
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
    /// Runtime registry of constructed `Arc<dyn CloudBackend>`
    /// instances (one per name in `cloud_config.backends`). Built at
    /// boot from `cloud_config`.
    pub cloud_backends: CloudBackendRegistry,
    /// Full `cloud:` section of the YAML conffile — tuning knobs plus
    /// the named backend definitions under `cloud.backends:`. Distinct
    /// from `cloud_backends` (the runtime registry above).
    pub cloud_config: Arc<CloudConfig>,
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
    /// Runtime registry of constructed `Arc<dyn CloudBackend>`
    /// instances (one per name in `cloud_config.backends`).
    pub cloud_backends: CloudBackendRegistry,
    /// Full `cloud:` section of the YAML conffile — tuning knobs plus
    /// the named backend definitions under `cloud.backends:`.
    pub cloud_config: Arc<CloudConfig>,
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
}

impl DaemonState {
    pub fn new(cfg: DaemonStateConfig) -> Self {
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
            cloud_config: cfg.cloud_config,
            keystore_config: cfg.keystore_config,
            diagnostic_store: Arc::new(DiagnosticStore::new()),
            jobs: Arc::new(JobRegistry::new()),
        }
    }
}
