// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

mod admin;
mod audit;
mod auth;
mod config;
mod discovery;
mod http;
mod registry;
mod smoke;
mod upload_worker;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use shared_audit::{AuditActor, AuditResult};
use shared_iscsi::session::SessionManager;
use shared_iscsi::transport::ServerConfig;
use shared_telemetry::{Telemetry, TelemetryConfig};
use tokio::sync::Mutex;

use crate::admin::handlers::AdminState;
use crate::audit::{IscsiDiskLoginAudit, audit_dir, boot_audit_log};
use crate::config::{DEFAULT_CONFIG_PATH, DaemonConfig, NvmetcpTlsMode, Transport};
use crate::http::HttpState;
use scsi_sbc::SbcScsiDispatcher;

// `THURVSA_VERSION` is set by build.rs to "<crate-ver> (<sha>[-dirty])".
pub(crate) const THURVSA_VERSION_STR: &str = match option_env!("THURVSA_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// `service.name` resource attribute on the OTel SDK side. Sourced
/// from [`shared_naming::DISK`] so the operator-visible identity is
/// declared once. Distinguishes thurvsa from the tape-library
/// daemon on shared OTLP backends and in the Prometheus
/// `target_info` series; instrument names stay `thur_*` (single
/// shared-telemetry surface — see CLAUDE.md § shared-telemetry).
const SERVICE_NAME: &str = shared_naming::DISK.name;

/// Default iSCSI listen address. thurvsa binds3260so it can
/// coexist with the VTL on the canonical 3260 (an operator running
/// both daemons on the same host distinguishes by port).
pub const DEFAULT_LISTEN_ADDRESS: &str = "0.0.0.0:3260";

/// Default NVMe/TCP listen address. Picks the IANA-registered
/// nvme-tcp port (4420) — no clash with iSCSI's 3260, so a future
/// "expose both transports concurrently" mode would not need an
/// operator override.
pub const DEFAULT_NVMETCP_LISTEN_ADDRESS: &str = "0.0.0.0:4420";

/// Stale-session sweep cadence used by [`shared_iscsi::transport::run`].
/// Matches the VTL's 5-minute idle threshold — initiators that drop
/// without LOGOUT have their session manager state reaped.
const STALE_SESSION_TIMEOUT_SECS: u64 = 300;

#[derive(Parser)]
#[command(name = "thurvsad", about = "ThurVSA daemon", version = THURVSA_VERSION_STR)]
struct Cli {
    /// Path to thurvsa.yaml. Defaults to /etc/thurvsa/thurvsa.yaml.
    #[arg(short, long)]
    config: Option<String>,

    /// Run in-process smoke tests and exit (does not start daemon).
    #[arg(long)]
    test: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Daemon logs land in the systemd journal / syslog; ANSI color
        // escapes would show up there as raw `#033[..m` codes.
        .with_ansi(false)
        .init();

    let cli = Cli::parse();

    // Operator-visible identity + licensing banner — prints once
    // per daemon start, into the systemd journal. Two-line shape
    // (`<product> <version>` then the shared copyright/license
    // notice) so a future rebrand only touches `shared_naming`.
    // Placed after `Cli::parse()` so `--version` / `--help` exit
    // before the banner fires.
    tracing::info!(
        "{} {}",
        shared_naming::DISK.display_name,
        THURVSA_VERSION_STR
    );
    for line in shared_naming::COPYRIGHT_NOTICE.lines() {
        tracing::info!("{line}");
    }
    let config_path = PathBuf::from(cli.config.as_deref().unwrap_or(DEFAULT_CONFIG_PATH));
    let cfg = DaemonConfig::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;

    let data_dir = PathBuf::from(&cfg.data_dir);
    if !data_dir.is_dir() {
        bail!(
            "data_dir '{}' does not exist or is not a directory",
            data_dir.display()
        );
    }

    // Refuse to start if a legacy `<data_dir>/cloud-backends.json` is
    // still around — pre-alpha.2 kept backend definitions there; they
    // now live in the YAML conffile under `cloud.backends:`. The
    // operator has to copy the entries over and delete the JSON file
    // before the daemon will come up.
    shared_cloud::reject_legacy_cloud_backends_json(&data_dir, &config_path)
        .map_err(anyhow::Error::msg)?;

    // Same one-shot migration guard for the keystore-backends JSON
    // sidecar — now under `keystore.backends:` in the YAML conffile.
    shared_keystore::reject_legacy_keystore_backends_json(&data_dir, &config_path)
        .map_err(anyhow::Error::msg)?;

    // Validate cloud backend definitions (from YAML cloud.backends:).
    cfg.cloud
        .validate_backends()
        .with_context(|| "validate cloud.backends in YAML conffile")?;

    // Daemon-owned operational state files. Empty templates are
    // auto-created on first boot so the operator doesn't have to learn
    // the JSON schema before the daemon will start.
    let iscsi_users_path = data_dir.join("iscsi-users.json");
    let iscsi_users = shared_iscsi::auth::IscsiUsersFile::load_or_create_default(&iscsi_users_path)
        .with_context(|| format!("loading {}", iscsi_users_path.display()))?;

    tracing::info!(
        config = %config_path.display(),
        data_dir = %data_dir.display(),
        backends = cfg.cloud.backends.len(),
        keystore_backends = cfg.keystore.backends.len(),
        users = iscsi_users.users.len(),
        "thurvsad: config loaded"
    );

    // --test mode: run in-process smoke against core_block + scsi_sbc and
    // exit. Skips telemetry / audit / iSCSI bind so it's safe to run
    // alongside a live daemon. Each smoke uses its own tempdir so the
    // operator's data_dir / cloud-backends stay untouched.
    if cli.test {
        return smoke::run_all().await;
    }

    // Telemetry: install the process-global handle before anything
    // else so audit / cloud / iscsi record sites pick it up. Both the
    // `service.name` resource attribute and the instrument-name prefix
    // come from `shared_naming::DISK` so thurvsa's metrics show up as
    // `thurvsa_*` distinct from thurvtl's `thurvtl_*` series.
    let telemetry_cfg = TelemetryConfig {
        service_name: Some(SERVICE_NAME.into()),
        service_instance_id: Some(cfg.data_dir.clone()),
        instrument_prefix: Some(shared_naming::DISK.metric_prefix.into()),
        otlp: None,
    };
    let telemetry = Arc::new(
        Telemetry::new(&telemetry_cfg).context("constructing OpenTelemetry MeterProvider")?,
    );
    if let Err(_already) = shared_telemetry::set_global((*telemetry).clone()) {
        // A second install is a no-op (returns Err with the rejected
        // value). Only `--test` smoke runs would hit this; production
        // boots once.
        tracing::warn!("telemetry: process-global handle already installed");
    }

    // Alerting: build the dispatcher from YAML and install the
    // process-global handle so producer crates (audit append path,
    // iSCSI CHAP failures, disk-cache loops)
    // can emit through `shared_alerting::record::*` without taking
    // a channel arg. Off by default; the dispatcher is only built
    // when `alerting.enabled: true` AND at least one sink is
    // configured. Sink construction is synchronous so a misconfig
    // (bad SMTP host, unset env var) fails the daemon at boot.
    if cfg.alerting.enabled {
        if cfg.alerting.sinks.is_empty() {
            anyhow::bail!(
                "alerting.enabled=true but alerting.sinks is empty; add at least one sink or disable alerting"
            );
        }
        let dispatcher = shared_alerting::AlertingDispatcher::build(
            &cfg.alerting,
            &shared_naming::DISK,
            env!("CARGO_PKG_VERSION"),
            (*telemetry).clone(),
        )
        .context("building alerting dispatcher")?;
        let sink_count = dispatcher.sink_names().len();
        if shared_alerting::set_global(dispatcher).is_err() {
            tracing::warn!("alerting: process-global dispatcher already installed");
        }
        // Bridge audit-append failures into the alerting subsystem.
        // Idempotent on duplicate install — only the first `--test`
        // run would hit the Err branch.
        let _ = shared_audit::set_append_failure_hook(shared_alerting::record::audit_append_failed);
        tracing::info!(
            "alerting: enabled with {} sink(s); dedup window {}s",
            sink_count,
            cfg.alerting.dedup_window_seconds
        );
    } else {
        tracing::info!("alerting: disabled (alerting.enabled=false in config)");
    }

    // Audit log: open + spawn writer task before discovery so the
    // boot rollup ("daemon.start" + per-LUN log lines below) lands
    // chronologically. Disabled by config knob means no log file
    // and no LoginAuditSink — login-phase events fall through to
    // `NoopLoginAudit`.
    let mut audit_lifecycle = None;
    let mut audit_channel_for_admin = None;
    let login_audit: Arc<dyn shared_iscsi::transport::LoginAuditSink> = if cfg.audit.enabled {
        let dir = audit_dir(&cfg.audit, &data_dir);
        let boot = boot_audit_log(dir.clone(), Some(cfg.data_dir.as_str()))
            .await
            .context("open audit log")?;
        let crate::audit::AuditBoot {
            log: _log,
            channel,
            writer,
        } = boot;
        tracing::info!("audit: log opened at {}", dir.display());
        let sink = Arc::new(IscsiDiskLoginAudit::new(channel.clone()));
        audit_channel_for_admin = Some(channel.clone());
        audit_lifecycle = Some((channel, writer));
        sink
    } else {
        tracing::warn!(
            "audit.enabled=false - running without an audit log; not recommended outside dev"
        );
        Arc::new(shared_iscsi::transport::NoopLoginAudit)
    };

    // Resolve the operator's `cloud.upload.max_concurrent` once at
    // boot (auto-scale sentinel `0` -> min(16, num_cpus * 4)). The
    // value caps parallel in-flight page flushes per volume; same
    // knob VTL's upload worker honors. Logged with source so the
    // operator sees "auto-detected from num_cpus=N" or "operator
    // override" in the boot log.
    let (max_concurrent_flushes, max_concurrent_source) = cfg.cloud.upload.resolve_max_concurrent();
    tracing::info!(
        "page-flush concurrency: max_concurrent={} ({})",
        max_concurrent_flushes,
        max_concurrent_source
    );

    // Per-backend pool budgets. Each backend gets its own cap:
    // either the per-entry `disk_cache_size_gb` override from
    // cloud-backends.json, or the YAML `disk_cache.size_gb`
    // default. Both share the `DiskCacheSize` shape (`auto` |
    // <gb>); `auto` entries split the 50%-of-free share evenly so
    // two `auto` backends can't combined commit 100% of free
    // space. `VolumeWriter::write_page` applies upload backpressure
    // when this backend's slice is full. Walk on-disk chunks so
    // the budget reflects whatever survived a previous run —
    // daemon restart must not silently re-grant bytes that are
    // already on disk.
    let pool_budgets: std::collections::HashMap<String, Arc<shared_pool::PoolBudget>> = {
        let disk_free_min_bytes = cfg
            .disk_cache
            .disk_free_min_gb
            .saturating_mul(1024 * 1024 * 1024);
        let soft_pct = cfg.disk_cache.localonly_soft_watermark_pct;
        let default_size = cfg.disk_cache.size_gb;
        let bounds = cfg.disk_cache.bounds();

        let resolved: Vec<(String, core_block::DiskCacheSize, bool)> = cfg
            .cloud
            .backend_names()
            .into_iter()
            .map(|name| {
                let override_size = cfg
                    .cloud
                    .backend_entry(&name)
                    .ok()
                    .and_then(|e| e.disk_cache_size_gb());
                let overridden = override_size.is_some();
                let size = override_size.unwrap_or(default_size);
                (name, size, overridden)
            })
            .collect();
        let auto_backends: u32 = resolved
            .iter()
            .filter(|(_, s, _)| s.is_auto())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);

        let mut map = std::collections::HashMap::new();
        let mut log_lines: Vec<String> = Vec::new();
        for (name, size, overridden) in resolved {
            let cap_bytes = size.resolve_bytes(&data_dir, bounds, auto_backends);
            let budget = Arc::new(shared_pool::PoolBudget::with_backend(
                name.clone(),
                data_dir.clone(),
                cap_bytes,
                disk_free_min_bytes,
                soft_pct,
            ));
            if let Err(e) = core_block::refresh_pool_budget_from_volumes(&budget, &data_dir, &name)
            {
                tracing::warn!(
                    "PoolBudget startup refresh for backend '{}' failed (will assume empty): {}",
                    name,
                    e
                );
            }
            let shape = match size {
                core_block::DiskCacheSize::Auto => "auto",
                core_block::DiskCacheSize::Explicit(_) => "explicit",
            };
            log_lines.push(format!(
                "{}={} GB ({}{})",
                name,
                cap_bytes / (1024 * 1024 * 1024),
                shape,
                if overridden { ", override" } else { "" },
            ));
            map.insert(name, budget);
        }
        tracing::info!(
            "Per-backend pool budgets: {} (soft {}%, disk-free min {} GB)",
            log_lines.join(", "),
            soft_pct,
            cfg.disk_cache.disk_free_min_gb,
        );
        map
    };
    let backpressure_deadline =
        std::time::Duration::from_secs(cfg.disk_cache.backpressure_max_wait_seconds);

    // Async upload-worker channel. The sender goes to every
    // VolumeWriter via `with_upload_sender` so `write_page_unsynced`
    // can enqueue PUTs without awaiting cloud. The worker (spawned
    // after discovery so it sees a fully-populated registry) drains
    // the receiver and calls `apply_page_upload_outcome` per
    // completion. None of this affects the inline test path —
    // VolumeWriter falls back to synchronous `upload_chunk_inert`
    // when no sender is wired.
    let (upload_tx, upload_rx) = upload_worker::upload_channel();

    let (registry, volumes, caches, backends, keystores) = discovery::discover_and_register(
        &data_dir,
        &cfg.cloud,
        &cfg.keystore,
        max_concurrent_flushes,
        &pool_budgets,
        backpressure_deadline,
        Some(upload_tx.clone()),
    )
    .await
    .context("volume discovery")?;
    let registry = Arc::new(registry);

    // Async upload worker. Drains `upload_rx` and applies each
    // outcome back through the owning `VolumeWriter`. Spawned after
    // discovery so registry lookups always resolve. Concurrency cap
    // matches the per-volume flush concurrency — both are sized off
    // the same `cloud.upload.max_concurrent` knob.
    // Wrap discovery's backends map in the same Arc<Mutex> AdminState
    // will hold so runtime adds via `get_or_init_backend` are visible
    // to the worker. Pre-fix the worker held a snapshot taken here;
    // any post-boot `volume create` would never see the new backend
    // in the worker, and every page dispatched against it silently
    // hit "backend unknown" (warned-once-and-drop).
    let admin_backends = Arc::new(Mutex::new(backends));
    let upload_worker_handle = {
        let registry = Arc::clone(&registry);
        let backends = Arc::clone(&admin_backends);
        let max_concurrent = max_concurrent_flushes;
        Some(tokio::spawn(async move {
            if let Err(e) =
                upload_worker::run_upload_worker(upload_rx, registry, backends, max_concurrent)
                    .await
            {
                tracing::error!("upload worker exited with error: {e:?}");
            }
        }))
    };

    // Crash-recovery scan: walk every volume's `upload.idx` and
    // re-enqueue pages still marked `LocalOnly` (chunk in pool,
    // cloud PUT never acked because of a prior crash). Runs before
    // the iSCSI listener accepts host writes so survivors get back
    // into the upload queue ahead of fresh traffic.
    upload_worker::scan_and_enqueue_localonly(&data_dir, &registry, &upload_tx).await;

    // Per-volume write-back flush workers. Each cache wakes on its
    // own dirty notification + a periodic tick; the workers exit
    // when `request_shutdown` is called below. JoinHandles get
    // dropped after the shutdown await — task ends cleanly.
    let mut flush_handles = Vec::with_capacity(caches.len());
    for cache in &caches {
        let cache = Arc::clone(cache);
        flush_handles.push(tokio::spawn(cache.run_flush_worker()));
    }

    if volumes.is_empty() {
        tracing::warn!(
            "no volumes found under {}; create one via `thurvsa volume create` (admin socket) or restart after writing a manifest",
            data_dir.display()
        );
    } else {
        tracing::info!("discovered {} volume(s):", volumes.len());
        for v in &volumes {
            tracing::info!(
                "  LUN {}: name='{}' backend='{}' size={} B page={} B",
                v.lun,
                v.name,
                v.backend,
                v.size_bytes,
                v.page_size_bytes,
            );
        }
    }

    // Transport-independent: always present (iSCSI HTTP `/sessions`
    // returns the live list; NVMe-TCP path stays empty until the
    // session inventory lands as a follow-up).
    let session_manager = Arc::new(SessionManager::new());
    let http_listener_cfg = cfg
        .http
        .listener_config()
        .context("building HTTP listener config")?;

    // iSCSI target IQN — operator override (`iscsi.target_iqn`) or the
    // per-product default. Resolved before the transport match so the
    // `/info` + `/sessions` HTTP surface reports the same IQN the
    // iSCSI branch advertises on the wire.
    let target_iqn = cfg
        .iscsi
        .target_iqn
        .clone()
        .unwrap_or_else(|| shared_naming::DISK.iqn.to_string());
    if let Err(e) = shared_naming::validate_iqn(&target_iqn) {
        anyhow::bail!("invalid iscsi.target_iqn in {}: {e}", config_path.display());
    }

    // Boot the wire-protocol stack selected by `transport:` in YAML.
    // The two branches are mutually exclusive — only one binds. Each
    // produces:
    //   * `transport_fut`: the listener future tokio::select! awaits
    //   * `transport_listen`: address string logged + surfaced in
    //     HttpState (so `/health` reports something meaningful).
    let (transport_fut, transport_listen): (
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>,
        String,
    ) = match cfg.transport {
        Transport::Iscsi => {
            let handler = Arc::new(SbcScsiDispatcher::new(
                Arc::clone(&registry) as Arc<dyn scsi_sbc::VolumeLookup>,
                target_iqn.clone(),
            ));
            tracing::info!(
                "thurvsad: SBC-3 dispatcher ready ({} LUN(s))",
                volumes.len()
            );

            // CHAP authenticator: factory closure that reads
            // `iscsi-users.json` on every login. `method: None`
            // (default) means an unauthenticated target. The
            // initial-boot snapshot is logged once; subsequent
            // sessions pick up file edits without restart.
            let chap_auth = auth::build(
                &cfg.iscsi.auth,
                iscsi_users_path.clone(),
                iscsi_users.users.len(),
            )
            .context("building CHAP authenticator factory")?;

            let iscsi_listen = cfg
                .iscsi
                .listen
                .clone()
                .unwrap_or_else(|| DEFAULT_LISTEN_ADDRESS.to_string());
            let server_config = ServerConfig {
                listen_address: iscsi_listen.clone(),
                session_manager: Arc::clone(&session_manager),
                auth: chap_auth,
                audit: login_audit,
                stale_session_timeout_secs: STALE_SESSION_TIMEOUT_SECS,
            };
            let transport_handler: Arc<dyn shared_iscsi::ScsiHandler> = handler;
            tracing::info!("transport: iscsi (listen={})", iscsi_listen);
            (
                Box::pin(shared_iscsi::transport::run(
                    server_config,
                    transport_handler,
                )),
                iscsi_listen,
            )
        }
        Transport::Nvmetcp => {
            // NVMe-TCP path. Login audit is dropped (CHAP-style
            // per-login auditing doesn't fit the TLS-PSK flow — the
            // host's identity is captured in the TLS handshake; per-
            // connection audit would need a separate hook).
            drop(login_audit);
            let nvmetcp_listen = cfg
                .nvmetcp
                .listen
                .clone()
                .unwrap_or_else(|| DEFAULT_NVMETCP_LISTEN_ADDRESS.to_string());
            // NVMe Subsystem NQN — operator override (`nvmetcp.subnqn`)
            // or the per-product default. Feeds the dispatcher, the
            // TLS-PSK acceptor (the PSK derivation binds to it), and
            // the boot log line.
            let subnqn = cfg
                .nvmetcp
                .subnqn
                .clone()
                .unwrap_or_else(|| shared_naming::DISK.nqn.to_string());
            if let Err(e) = shared_naming::validate_nqn(&subnqn) {
                anyhow::bail!("invalid nvmetcp.subnqn in {}: {e}", config_path.display());
            }
            let handler = Arc::new(nvme_nvm::NvmeNvmDispatcher::new(
                Arc::clone(&registry) as Arc<dyn nvme_nvm::NamespaceLookup>,
                subnqn.clone(),
                // SN: 20 ASCII chars, fingerprint of the data dir.
                // Keep simple for now — the data dir basename plus the
                // volume count is stable across reboots for an
                // unchanged install.
                format!("THURVSA{:013}", volumes.len()),
                shared_naming::DISK_PRODUCT.to_string(),
                // FR: 8 ASCII chars on the wire — IdentifyController
                // truncates anything longer (the SHA suffix is the
                // first thing to fall off, leaving the version core
                // visible to `nvme id-ctrl`).
                THURVSA_VERSION_STR.to_string(),
            ));
            tracing::info!(
                "thurvsad: NVMe NVM dispatcher ready ({} NSID(s))",
                volumes.len()
            );

            // Optional TLS 1.3 PSK acceptor. Disabled = cleartext
            // (legacy default). Psk = register a ClientHelloCallback
            // that reads `nvmetcp-psks.json` and derives every PSK
            // on every handshake. Operator edits via the
            // `nvmetcp psks` CLI verbs take effect on the next
            // session without restart.
            let tls_acceptor = match cfg.nvmetcp.tls.mode {
                NvmetcpTlsMode::Disabled => None,
                NvmetcpTlsMode::Psk => {
                    let path = cfg
                        .nvmetcp
                        .tls
                        .identity_file
                        .as_deref()
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| data_dir.join("nvmetcp-psks.json"));
                    // Touch-or-create the stub on first boot so the
                    // acceptor's load step has something to parse.
                    let initial_file =
                        nvme_tcp::identity::NvmetcpPsksFile::load_or_create_default(&path)
                            .with_context(|| format!("loading {}", path.display()))?;
                    let acceptor = nvme_tcp::tls::build_psk_acceptor(&path, &subnqn)
                        .context("building NVMe/TCP TLS-PSK acceptor")?;
                    tracing::info!(
                        identity_file = %path.display(),
                        psk_count = initial_file.psks.len(),
                        "nvme-tcp: TLS-PSK enabled, parse-on-handshake",
                    );
                    Some(acceptor)
                }
            };

            tracing::info!(
                "transport: nvmetcp (listen={}, subnqn={}, tls={})",
                nvmetcp_listen,
                subnqn,
                tls_acceptor.is_some(),
            );
            let server_cfg = nvme_tcp::ServerConfig {
                listen_address: nvmetcp_listen.clone(),
                handler,
                controller_regs: Arc::new(nvme_base::ControllerRegs::new()),
                tls: tls_acceptor,
            };
            (Box::pin(nvme_tcp::run(server_cfg)), nvmetcp_listen)
        }
    };

    // Admin Unix socket — live volume create / destroy + read APIs +
    // long-running jobs.
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let admin_state = AdminState {
        data_dir: data_dir.clone(),
        cloud: Arc::new(cfg.cloud.clone()),
        registry: Arc::clone(&registry),
        backends: admin_backends,
        audit: audit_channel_for_admin,
        audit_dir: audit_dir(&cfg.audit, &data_dir),
        jobs: Arc::new(shared_admin_server::JobRegistry::new()),
        keystore_config: Arc::new(cfg.keystore.clone()),
        keystore_cache: Arc::new(tokio::sync::RwLock::new(keystores)),
        started_at_unix,
        pool_budgets: pool_budgets.clone(),
        backpressure_deadline,
        upload_tx: upload_tx.clone(),
        sessions: Arc::clone(&session_manager),
    };
    let admin_socket = admin::admin_socket_path();

    // HTTP server — /health + /metrics + /sessions.
    let http_state = HttpState {
        telemetry: Arc::clone(&telemetry),
        registry: Arc::clone(&registry),
        sessions: Arc::clone(&session_manager),
        listen_address: transport_listen.clone(),
        target_iqn,
    };

    // Disk-cache eviction worker. Periodically re-scans every
    // backend's pool slice and evicts oldest chunks (per-volume
    // `lru.idx` sidecar drives the sort) until each backend is
    // back under its `disk_cache.size_gb` cap. The release inside
    // each eviction wakes any `VolumeWriter::write_page` parked on
    // upload backpressure. When the operator picked `size_gb:
    // auto` the worker also re-resolves the cap every tick against
    // current free space, so external disk pressure shrinks the
    // budget reactively.
    let eviction_worker_handle = {
        let data_dir = data_dir.clone();
        let pool_budgets = pool_budgets.clone();
        let interval_secs = cfg.disk_cache.eviction_interval_seconds.max(1);
        let recent_seal_pin_seconds = cfg.disk_cache.recent_seal_pin_seconds;
        let default_size = cfg.disk_cache.size_gb;
        let bounds = cfg.disk_cache.bounds();
        let cloud_config_clone = cfg.cloud.clone();
        let backend_names: Vec<String> = pool_budgets.keys().cloned().collect();
        Some(tokio::spawn(async move {
            run_disk_cache_eviction_worker(
                data_dir,
                pool_budgets,
                backend_names,
                std::time::Duration::from_secs(interval_secs),
                recent_seal_pin_seconds,
                default_size,
                bounds,
                cloud_config_clone,
            )
            .await;
        }))
    };

    // Runtime-counter persist worker. The per-volume byte counters
    // (host/backend read/written) live as in-memory atomics and reach
    // `runtime.json` only at flush boundaries. A read-only volume
    // triggers no flush, so without this backstop its read counters
    // would never persist. Every 60 s this rewrites `runtime.json`
    // for any volume whose counters advanced since the last persist;
    // the dirty-flag gate makes an idle volume a no-op.
    let runtime_persist_handle = {
        let registry = Arc::clone(&registry);
        Some(tokio::spawn(async move {
            run_runtime_persist_worker(registry, std::time::Duration::from_secs(60)).await;
        }))
    };

    let result = tokio::select! {
        res = transport_fut => {
            if let Err(e) = res {
                tracing::error!("data-path transport exited with error: {}", e);
                Err(e)
            } else {
                Ok(())
            }
        }
        res = admin::run_admin_server(admin_socket.clone(), admin_state) => {
            if let Err(e) = res {
                tracing::error!("admin socket exited with error: {}", e);
                Err(e)
            } else {
                Ok(())
            }
        }
        res = {
            let scheme = if http_listener_cfg.tls.is_some() { "https" } else { "http" };
            http::log_route_table(&http_listener_cfg.listen, scheme);
            let router = http::build_router(http_state);
            shared_admin_http::run_http_server(http_listener_cfg, router)
        } => {
            if let Err(e) = res {
                tracing::error!("HTTP server exited with error: {}", e);
                Err(e)
            } else {
                Ok(())
            }
        }
        _ = wait_for_shutdown() => {
            tracing::info!("thurvsad: shutdown signal received, exiting");
            Ok(())
        }
    };

    // Drain dirty pages from every per-volume cache and stop the
    // flush workers before the audit channel shutdown — losing
    // host-acked WRITEs on a clean shutdown would be much worse
    // than waiting a few seconds for cloud uploads.
    //
    // Iterate the live registry (not the boot-time `caches` Vec) so
    // volumes that were created via the admin socket after startup
    // are flushed too. Live-created caches get their own flush
    // worker inside the admin handler but were previously orphaned
    // from the shutdown flush.
    drop(caches);
    let live_caches: Vec<_> = registry
        .entries()
        .into_iter()
        .map(|(_lun, cache)| cache)
        .collect();
    for cache in &live_caches {
        if let Err(e) = cache.flush_all().await {
            tracing::warn!(
                volume = cache.manifest().name.as_str(),
                error = %e,
                "thurvsad: final cache flush failed (host-acked writes may be lost)"
            );
        }
        cache.request_shutdown();
    }
    for handle in flush_handles {
        // Best-effort: workers should exit on their own once they
        // observe the shutdown flag. Don't block forever if a worker
        // is wedged on a cloud retry.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    // Upload worker shutdown. Drop every sender (the discovery-
    // resident clones live in each `VolumeWriter`; PageCache's
    // `flush_all` above already finished and the writers are about
    // to be dropped) and let the worker drain naturally on channel
    // close. Bounded wait so a wedged in-flight PUT doesn't block
    // shutdown — the page stays LocalOnly and the next boot's
    // recovery scan re-enqueues.
    drop(upload_tx);
    if let Some(h) = upload_worker_handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
    }

    if let Some(h) = eviction_worker_handle {
        h.abort();
    }
    // The shutdown flush loop above already persisted every volume's
    // counters via `flush_all`; abort the periodic worker outright.
    if let Some(h) = runtime_persist_handle {
        h.abort();
    }

    // Drain the audit channel before exit so daemon.stop + any
    // in-flight CHAP / volume entries hit disk.
    if let Some((channel, writer)) = audit_lifecycle {
        channel.try_append(
            "daemon.stop",
            AuditActor::system(),
            serde_json::json!({"product": "thurvsad"}),
            AuditResult::Ok,
        );
        writer.shutdown().await;
    }

    // Clean up the admin socket on exit. Stale-socket detection on
    // next boot covers the crash case; this is best-effort cleanup
    // for graceful shutdown.
    let _ = std::fs::remove_file(&admin_socket);

    result
}

#[cfg(unix)]
async fn wait_for_shutdown() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut int_ = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int_.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_shutdown() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("install ctrl-c handler")?;
    Ok(())
}

/// Periodic per-backend cache-eviction worker. On every tick walks
/// each backend's pool slice, computes usage, and trims down to
/// `disk_cache.size_gb` (or the per-entry `disk_cache_size_gb`
/// override) via [`core_block::DiskCacheManager::evict_lru_chunks`].
/// Eviction frees pool bytes *and* wakes any `VolumeWriter` parked
/// on the backend's `PoolBudget` via `release`.
///
/// Mirrors `core_mediachanger::run_disk_cache_eviction_worker`, minus the
/// upload-completion `Notify` source: VSA's write path uploads
/// synchronously inside `write_page`, so there's no batched-upload
/// burst that would justify an event-driven wakeup. The periodic
/// backstop tick (`disk_cache.eviction_interval_seconds`, default
/// 5 min) catches steady-state cache growth.
#[allow(clippy::too_many_arguments)]
async fn run_disk_cache_eviction_worker(
    data_dir: std::path::PathBuf,
    pool_budgets: std::collections::HashMap<String, Arc<shared_pool::PoolBudget>>,
    backend_names: Vec<String>,
    interval: std::time::Duration,
    recent_seal_pin_seconds: u64,
    default_size: core_block::DiskCacheSize,
    bounds: core_block::DiskCacheBounds,
    cloud_config: shared_cloud::CloudConfig,
) {
    use core_block::DiskCacheManager;
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // skip the immediate first tick
    loop {
        tick.tick().await;

        // Recompute per-backend caps for `auto`-mode entries against
        // current free space, then push the new value into each
        // backend's PoolBudget so `try_reserve` immediately sees the
        // updated ceiling. Explicit-mode entries are pinned and skip
        // the recompute. Count auto-mode backends first so the share
        // divisor is stable across the loop.
        let resolved_sizes: Vec<(String, core_block::DiskCacheSize)> = backend_names
            .iter()
            .map(|name| {
                let size = cloud_config
                    .backend_entry(name)
                    .ok()
                    .and_then(|e| e.disk_cache_size_gb())
                    .unwrap_or(default_size);
                (name.clone(), size)
            })
            .collect();
        let auto_backends: u32 = resolved_sizes
            .iter()
            .filter(|(_, s)| s.is_auto())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        for (name, size) in &resolved_sizes {
            let Some(budget) = pool_budgets.get(name) else {
                continue;
            };
            let new_cap = size.resolve_bytes(&data_dir, bounds, auto_backends);
            if budget.cap_bytes() != new_cap {
                if size.is_auto() {
                    tracing::debug!(
                        "disk-cache auto-resize backend '{}': {} -> {} bytes",
                        name,
                        budget.cap_bytes(),
                        new_cap,
                    );
                }
                budget.set_cap_bytes(new_cap);
            }
        }

        for name in &backend_names {
            let Some(budget) = pool_budgets.get(name) else {
                continue;
            };
            let cap = budget.cap_bytes();
            let mut cm = DiskCacheManager::new(data_dir.clone(), name, cap);
            cm.set_pool_budget(budget.clone());
            cm.set_recent_seal_pin_seconds(recent_seal_pin_seconds);
            let used = match cm.calculate_usage() {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(
                        "disk-cache: usage calc for backend '{}' failed: {}",
                        name,
                        e
                    );
                    continue;
                }
            };
            if used <= cap {
                let pct = if cap == 0 {
                    0
                } else {
                    used.saturating_mul(100).checked_div(cap).unwrap_or(0)
                };
                tracing::debug!(
                    "disk-cache pool '{}' {} / {} bytes ({}%), no eviction",
                    name,
                    used,
                    cap,
                    pct,
                );
                // Soft-watermark alert: per-backend dedup keeps this
                // to one emit per dedup window for as long as the
                // pool sits above `localonly_soft_watermark_pct`.
                if budget.over_soft_watermark() {
                    shared_alerting::record::disk_cache_watermark(name, pct, cap);
                }
                continue;
            }
            tracing::info!(
                "disk-cache pool '{}' over budget ({} / {} bytes); LRU eviction",
                name,
                used,
                cap
            );
            match cm.evict_lru_chunks() {
                Ok(freed) if freed > 0 => {
                    tracing::info!("disk-cache: backend '{}' freed {} bytes", name, freed)
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("disk-cache: eviction for backend '{}' failed: {}", name, e)
                }
            }
        }
    }
}

/// Periodically persist each open volume's runtime counters.
///
/// The per-volume byte counters are in-memory atomics flushed to
/// `runtime.json` only at page-flush boundaries. A read-only volume
/// produces no flush, so this 60 s backstop persists any volume whose
/// counters moved since the last write. `persist_runtime_if_dirty` is
/// gated on a dirty flag, so a genuinely idle volume costs nothing.
async fn run_runtime_persist_worker(
    registry: Arc<registry::VolumeRegistry>,
    interval: std::time::Duration,
) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // skip the immediate first tick
    loop {
        tick.tick().await;
        for (_lun, cache) in registry.entries() {
            if let Err(e) = cache.persist_runtime_if_dirty() {
                tracing::warn!(
                    volume = cache.manifest().name.as_str(),
                    error = %e,
                    "thurvsad: periodic runtime-counter persist failed"
                );
            }
        }
    }
}
