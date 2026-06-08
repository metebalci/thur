// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// iSCSI Server - Main server logic
//
// This module contains the iSCSI server that listens for connections
// and handles the iSCSI protocol.

#![allow(dead_code)] // Server infrastructure

use anyhow::{Result, anyhow};
use core_mediachanger::{
    AuditActor, AuditChannel, AuditRateLimitDecision, AuditRateLimiter, AuditResult,
    ObjectStoreBackend,
};
use shared_iscsi::transport::{ChapAuthFactory, LoginAuditEvent, LoginAuditSink, ServerConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use tracing::info;

use super::auth::ChapAuthenticator;
use super::config::IscsiConfig;
use super::handler::IscsiLibraryHandler;
use crate::state::DaemonState;
use shared_iscsi::auth::parse_chap_algorithms;

/// Shared storage-backend registry: backend-name → already-initialized
/// `ObjectStoreBackend` handle. Lazy-populated on first use; the iSCSI READ
/// prefetch hook and the daemon's upload/cache workers all resolve
/// backend handles through this map.
pub type ObjectStoreRegistry = Arc<TokioMutex<HashMap<String, Box<dyn ObjectStoreBackend>>>>;

/// Per-backend background read-prefetch managers (issue #97). Lazily
/// populated by the SCSI READ prefetch hook on first cache miss for a
/// backend; cached so each manager's in-flight-task table (the
/// `prefetch_queue_depth` source + the dedup that stops re-fetching a
/// chunk already downloading) persists across reads. Process-lifetime,
/// shared via the handler.
pub type PrefetchManagerRegistry =
    Arc<TokioMutex<HashMap<String, Arc<core_mediachanger::PrefetchManager>>>>;

/// iSCSI Server
///
/// Owns the protocol-specific state (target IQN, listen address,
/// CHAP factory) and a handle to the shared `DaemonState`
/// every other subsystem reads through.
pub struct IscsiServer {
    config: IscsiConfig,
    auth: Option<ChapAuthFactory>,
    state: Arc<DaemonState>,
}

impl IscsiServer {
    /// Create a new iSCSI server instance.
    ///
    /// `iscsi_users` is the boot-time snapshot used for the startup
    /// log line; the factory closure built here reads
    /// `iscsi-users.json` from `users_path` on every login, so file
    /// edits take effect on the next session without restart.
    pub fn new(
        config: IscsiConfig,
        iscsi_users: shared_iscsi::auth::IscsiUsersFile,
        users_path: PathBuf,
        state: Arc<DaemonState>,
    ) -> Result<Self> {
        let auth = if config.iscsi.auth.method.is_chap() {
            let allowed_algorithms = parse_chap_algorithms(&config.iscsi.auth.allowed_algorithms)
                .map_err(|e| anyhow!("{e}"))?;
            let algo_names: Vec<&str> = allowed_algorithms.iter().map(|x| x.name()).collect();
            info!(
                "CHAP authentication enabled with {} user(s) at boot, algorithms={:?}, parse-on-login",
                iscsi_users.users.len(),
                algo_names
            );
            let path = users_path;
            let allowed = allowed_algorithms;
            let factory: ChapAuthFactory = Arc::new(move || -> Result<ChapAuthenticator> {
                let file = shared_iscsi::auth::IscsiUsersFile::load(&path)
                    .map_err(|e| anyhow!("loading {}: {}", path.display(), e))?;
                ChapAuthenticator::from_file(
                    &file,
                    shared_iscsi::auth::AuthMethod::Chap,
                    allowed.clone(),
                )
                .ok_or_else(|| {
                    anyhow!(
                        "CHAP method active but ChapAuthenticator::from_file returned None ({})",
                        path.display()
                    )
                })
            });
            Some(factory)
        } else {
            info!("CHAP authentication disabled (method=None)");
            None
        };

        Ok(Self {
            config,
            auth,
            state,
        })
    }

    /// Stale-session sweep timeout (seconds) handed to the shared
    /// transport's `ServerConfig`, sourced from
    /// `iscsi.session_timeout_seconds`. Issue #96: the transport
    /// previously hardcoded this to 300 and silently ignored any
    /// configured value.
    fn stale_session_timeout_secs(config: &IscsiConfig) -> u64 {
        config.iscsi.session_timeout_seconds as u64
    }

    /// Run the iSCSI server.
    ///
    /// Step 3c phase 2 lifted the connection lifecycle (PDU framing,
    /// login phase, R2T loop, dispatch loop) into
    /// `shared_iscsi::transport`. We now construct a
    /// [`IscsiLibraryHandler`] from `DaemonState` and hand it +
    /// `ServerConfig` to [`shared_iscsi::transport::run`]. The
    /// background drive-lock sweeper still lives here because it's
    /// thurvtl-specific (drive-lock TTL is a tape concept).
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let portals = self.config.iscsi.listen_portals.clone();
        info!(
            "Configuration: {} drives (LTO-{})",
            self.config.library.num_drives, self.config.library.lto_generation
        );

        // Drive-lock cleanup runs alongside the shared transport's
        // own session sweep — sessions are cross-product, drive
        // locks are tape-specific.
        let drive_mgr_sweep = Arc::clone(&self.state.drive_manager);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                drive_mgr_sweep.cleanup_stale_locks(60);
            }
        });

        // ALUA topology — built from the advertised portal list +
        // the chassis serial namespace so per-port NAA-3 identifiers
        // are stable across daemon restarts and globally distinct
        // from any other thurvtl/thurvsa daemon.
        let chassis_ser = {
            let lib = self.state.library.lock().expect("library mutex poisoned");
            lib.chassis_serial().to_string()
        };
        let alua = Arc::new(shared_iscsi::alua::AluaTopology::from_portals(
            &portals,
            chassis_ser,
        ));

        let handler = Arc::new(IscsiLibraryHandler {
            drive_manager: Arc::clone(&self.state.drive_manager),
            library: Arc::clone(&self.state.library),
            ua_tracker: Arc::clone(&self.state.ua_tracker),
            element_config: self.state.element_config,
            event_tx: self.state.event_tx.clone(),
            data_dir: self.state.data_dir.clone(),
            audit_log: self.state.audit_log.clone(),
            audit_ratelimiter: Arc::clone(&self.state.audit_ratelimiter),
            storage_backends: Arc::clone(&self.state.storage_backends),
            storage_config: Arc::clone(&self.state.storage_config),
            pool_budgets: self.state.pool_budgets.clone(),
            diagnostic_store: Arc::clone(&self.state.diagnostic_store),
            target_iqn: self.config.iscsi.target_iqn.clone(),
            alua,
            reservations: Arc::clone(&self.state.reservations),
            pr_collapse_isid: self
                .config
                .iscsi
                .reservations
                .initiator_port
                .collapse_isid(),
            prefetch_managers: Arc::clone(&self.state.prefetch_managers),
            read_prefetch_chunks_ahead: self.state.read_prefetch_chunks_ahead,
        });
        let transport_handler: Arc<dyn shared_iscsi::ScsiHandler> = handler;

        // Proactive reservation-change notification (issue #67): a
        // reservation preempted/released over iSCSI raises a RESERVATIONS
        // PREEMPTED / RELEASED Unit Attention on the affected initiators'
        // next command — for tape drive LUNs and the changer LUN alike.
        // Registered before the listener binds, so no session can race it.
        self.state.reservations.register_observer(Arc::new(
            shared_iscsi::IscsiReservationSink::new(
                Arc::clone(&self.state.ua_tracker),
                Arc::clone(&self.state.session_manager),
                self.config
                    .iscsi
                    .reservations
                    .initiator_port
                    .collapse_isid(),
            ),
        ));

        let audit = Arc::new(IscsiLibraryLoginAudit {
            audit_log: self.state.audit_log.clone(),
            ratelimiter: Arc::clone(&self.state.audit_ratelimiter),
        });

        let server_config = ServerConfig {
            listen_portals: portals,
            session_manager: Arc::clone(&self.state.session_manager),
            auth: self.auth.clone(),
            audit,
            stale_session_timeout_secs: Self::stale_session_timeout_secs(&self.config),
        };

        shared_iscsi::transport::run(server_config, transport_handler).await
    }
}

/// `LoginAuditSink` adapter: forwards shared-iscsi login-phase audit
/// events into thurvtl's `AuditChannel` + `AuditRateLimiter`. Mirrors
/// the per-event policy `scsi_ssc::dispatch::audit::audit_append` /
/// `ratelimit_key_for` enforce on the data-path side
/// (rate-limited CHAP-failure events keyed by
/// `(op, peer, user, reason)`; success bypasses the limiter).
struct IscsiLibraryLoginAudit {
    audit_log: Option<AuditChannel>,
    ratelimiter: Arc<AuditRateLimiter>,
}

impl LoginAuditSink for IscsiLibraryLoginAudit {
    fn record(&self, event: LoginAuditEvent<'_>) {
        let Some(chan) = self.audit_log.as_ref() else {
            return;
        };
        match event {
            LoginAuditEvent::ChapSuccess {
                peer,
                initiator,
                user,
                algorithm,
            } => {
                let actor = AuditActor::iscsi(initiator.map(str::to_string), peer.to_string());
                chan.try_append(
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
                // Alert side runs unconditionally (independent of the
                // audit rate-limiter): the alerting dispatcher keeps
                // its own per-user counter across the window and
                // fires WARN once the configured threshold is hit.
                if let Some(u) = user {
                    shared_alerting::record::chap_failure(u, peer);
                }
                let actor = AuditActor::iscsi(initiator.map(str::to_string), peer.to_string());
                let user_label = user.unwrap_or("-");
                let key = format!("iscsi.chap.failure:{peer}:{user_label}:{reason}");
                if matches!(
                    self.ratelimiter.decide(key, "iscsi.chap.failure", &actor),
                    AuditRateLimitDecision::Suppress
                ) {
                    return;
                }
                chan.try_append(
                    "iscsi.chap.failure",
                    actor,
                    serde_json::json!({
                        "chap_user": user,
                        "initiator": initiator,
                        "reason": reason,
                    }),
                    AuditResult::Error(error),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #96: a non-default `session_timeout_seconds` must reach
    /// the transport's stale-session sweep, not be silently dropped in
    /// favor of the old hardcoded 300.
    #[test]
    fn non_default_session_timeout_reaches_server_config() {
        let mut config = IscsiConfig::default();
        config.iscsi.session_timeout_seconds = 120;
        assert_eq!(IscsiServer::stale_session_timeout_secs(&config), 120);
    }

    #[test]
    fn default_session_timeout_is_300() {
        let config = IscsiConfig::default();
        assert_eq!(IscsiServer::stale_session_timeout_secs(&config), 300);
    }
}
