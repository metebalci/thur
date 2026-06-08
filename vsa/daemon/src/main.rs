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
use crate::audit::{IscsiDiskLoginAudit, NvmetcpLoginAudit, audit_dir, boot_audit_log};
use crate::config::{
    DEFAULT_CONFIG_PATH, DaemonConfig, NvmetcpAuthMode, NvmetcpTlsMode, Transport,
};
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

/// Resolve the `(TRADDR, TRSVCID)` the NVMe/TCP discovery log advertises
/// for the I/O subsystem (issue #84).
///
/// `advertise` (a full `ip:port`), when set, overrides both verbatim —
/// for hosts behind NAT / Docker bridge + published ports / reverse
/// proxy where the bind address isn't reachable; a wildcard advertise is
/// rejected. Otherwise the value derives from the I/O `listen` bind: a
/// concrete IP is advertised as-is, while a wildcard bind yields `None`
/// so the discovery controller reflects the address each request landed
/// on.
fn resolve_discovery_traddr(
    listen: &str,
    advertise: Option<&str>,
) -> Result<(Option<std::net::IpAddr>, u16)> {
    if let Some(adv) = advertise {
        let sa = adv
            .parse::<std::net::SocketAddr>()
            .with_context(|| format!("nvmetcp.advertise must be ip:port, got {adv:?}"))?;
        if sa.ip().is_unspecified() {
            anyhow::bail!(
                "nvmetcp.advertise address {adv} is a wildcard; \
                 it must be a concrete reachable address"
            );
        }
        return Ok((Some(sa.ip()), sa.port()));
    }
    let io_sockaddr = listen.parse::<std::net::SocketAddr>().ok();
    let port = io_sockaddr
        .map(|s| s.port())
        .or_else(|| listen.rsplit(':').next().and_then(|p| p.parse().ok()))
        .unwrap_or(4420);
    let traddr = io_sockaddr
        .map(|s| s.ip())
        .filter(|ip| !ip.is_unspecified());
    Ok((traddr, port))
}

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
    shared_object_store::reject_legacy_cloud_backends_json(&data_dir, &config_path)
        .map_err(anyhow::Error::msg)?;

    // Same one-shot migration guard for the keystore-backends JSON
    // sidecar — now under `keystore.backends:` in the YAML conffile.
    shared_keystore::reject_legacy_keystore_backends_json(&data_dir, &config_path)
        .map_err(anyhow::Error::msg)?;

    // Validate cloud backend definitions (from YAML cloud.backends:).
    cfg.storage
        .validate_backends()
        .with_context(|| "validate cloud.backends in YAML conffile")?;

    // Daemon-owned operational state files. Empty templates are
    // auto-created on first boot so the operator doesn't have to learn
    // the JSON schema before the daemon will start.
    let iscsi_users_path = data_dir.join("iscsi-users.json");
    let iscsi_users = shared_iscsi::auth::IscsiUsersFile::load_or_create_default(&iscsi_users_path)
        .with_context(|| format!("loading {}", iscsi_users_path.display()))?;

    // Live per-CHAP-user admission view (VSA dynamic admission): seeded
    // from iscsi-users.json here and kept current by the admin
    // add / grant / revoke / remove handlers, so an `iscsi users grant`
    // reaches sessions that are already connected. Shared between the
    // SBC dispatcher (reads the current set per command) and the admin
    // socket (mutates it + fans the REPORTED LUNS DATA HAS CHANGED UA).
    // A login-time snapshot still gates `auth.method: None` deployments;
    // this only takes effect for CHAP sessions (issue #15, CSI per-node
    // CHAP).
    let admission_view = Arc::new(shared_iscsi::AdmissionView::new());
    admission_view.seed(
        iscsi_users
            .users
            .iter()
            .map(|u| (u.username.clone(), u.volumes.clone().unwrap_or_default())),
    );

    // NVMe-TCP identity files honor the optional
    // `nvmetcp.{tls,auth}.identity_file` override, else default under
    // `<data_dir>/`. Resolved once here so the transport listener and
    // the `nvmetcp psks` / `nvmetcp dhchap` admin handlers agree on the
    // path (issue #69) — a mismatch silently refuses every host.
    let nvmetcp_psks_path = cfg.nvmetcp.psks_path(&data_dir);
    let nvmetcp_dhchap_path = cfg.nvmetcp.dhchap_path(&data_dir);

    tracing::info!(
        config = %config_path.display(),
        data_dir = %data_dir.display(),
        backends = cfg.storage.backends.len(),
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
    // Holds the login-failure rate-limiter + its flush-task handle when
    // auditing is on. Drained at shutdown so an in-flight window's
    // suppression count lands in the chain before exit.
    let mut audit_ratelimit: Option<(
        Arc<shared_audit::AuditRateLimiter>,
        tokio::task::JoinHandle<()>,
    )> = None;
    // Both transports get a login-audit sink off the same channel: iSCSI
    // CHAP and NVMe/TCP DH-HMAC-CHAP each emit success/failure rows and
    // feed the shared `chap_failures` alert class (issue #68). The
    // failure rows are bounded by a shared `AuditRateLimiter` (issue
    // #101) so a brute-force can't flood the BLAKE3 chain.
    let login_audit: Arc<dyn shared_iscsi::transport::LoginAuditSink>;
    let nvmetcp_login_audit: Arc<dyn nvme_tcp::LoginAuditSink>;
    if cfg.audit.enabled {
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
        let ratelimiter = crate::audit::new_audit_ratelimiter();
        let flush_handle = tokio::spawn(crate::audit::run_audit_ratelimit_flush(
            Arc::clone(&ratelimiter),
            channel.clone(),
        ));
        login_audit = Arc::new(IscsiDiskLoginAudit::new(
            channel.clone(),
            Arc::clone(&ratelimiter),
        ));
        nvmetcp_login_audit = Arc::new(NvmetcpLoginAudit::new(
            channel.clone(),
            Arc::clone(&ratelimiter),
        ));
        audit_channel_for_admin = Some(channel.clone());
        audit_ratelimit = Some((ratelimiter, flush_handle));
        audit_lifecycle = Some((channel, writer));
    } else {
        tracing::warn!(
            "audit.enabled=false - running without an audit log; not recommended outside dev"
        );
        login_audit = Arc::new(shared_iscsi::transport::NoopLoginAudit);
        nvmetcp_login_audit = Arc::new(nvme_tcp::NoopLoginAudit);
    };

    // Resolve the operator's `cloud.upload.max_concurrent` once at
    // boot (auto-scale sentinel `0` -> min(16, num_cpus * 4)). The
    // value caps parallel in-flight page flushes per volume; same
    // knob VTL's upload worker honors. Logged with source so the
    // operator sees "auto-detected from num_cpus=N" or "operator
    // override" in the boot log.
    let (max_concurrent_flushes, max_concurrent_source) =
        cfg.storage.upload.resolve_max_concurrent();
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
            .storage
            .backend_names()
            .into_iter()
            .map(|name| {
                let override_size = cfg
                    .storage
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

    // Per-backend ghost lists, parallel to pool_budgets. Drives the
    // cache_miss_after_eviction histogram via the read-path miss site
    // in `VolumeWriter::read_page` and the eviction-unlink site in
    // `DiskCacheManager`.
    let ghost_lists: std::collections::HashMap<String, Arc<shared_pool::GhostList>> = {
        let mut map = std::collections::HashMap::new();
        for name in cfg.storage.backend_names() {
            map.insert(
                name.clone(),
                Arc::new(shared_pool::GhostList::new(
                    name,
                    cfg.disk_cache.ghost_ring_size,
                )),
            );
        }
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
        &cfg.storage,
        &cfg.keystore,
        max_concurrent_flushes,
        &pool_budgets,
        &ghost_lists,
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

    // PERSISTENT RESERVE state (PTPL, issue #57). Reload any persisted
    // reservations from `<data_dir>/reservations.json`, keyed by stable
    // volume UUID (resolved to the current LUN via the registry so a
    // reused LUN never inherits a deleted volume's fence), and keep
    // persisting APTPL/CPTPL-set mutations. Built here — after discovery
    // populates the registry and before either transport's dispatcher —
    // and shared by the SBC and NVMe arms (only one runs per boot, but
    // the manager carries no single-transport assumption).
    let reservations = Arc::new(scsi_spc::reservations::ReservationManager::load_from(
        data_dir.join("reservations.json"),
        Arc::new(registry::VolumeUuidResolver::new(Arc::clone(&registry))),
    ));

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
    let webui_cfg = cfg.http.webui_config();
    let http_password_required = cfg.http.auth.method == shared_admin_auth::AuthMethod::Password;

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

    // Boot every wire-protocol stack listed in `transports:`. Each
    // listed transport binds its own listener concurrently against the
    // shared VolumeRegistry + ReservationManager built above, so a
    // volume is reachable as a SCSI LUN and an NVMe namespace at once.
    // Each arm pushes:
    //   * a `TransportFut` into `transport_futs` — the JoinSet below
    //     drives them; the first to exit (clean, error, or bind
    //     failure) tears the daemon down.
    //   * its listen address(es) into `transport_listens` — logged +
    //     surfaced in HttpState so `/health` reports every bound
    //     portal. NVMe/TCP contributes one entry; iSCSI may add many.
    type TransportFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
    let mut transport_futs: Vec<TransportFut> = Vec::with_capacity(cfg.transports.len());
    let mut transport_listens: Vec<String> = Vec::new();
    // The NVMe AER hub, lifted out of the nvmetcp arm so the admin
    // socket's volume create / destroy can fan a Namespace Attribute
    // Changed notice to connected NVMe controllers (issue #64). `None`
    // when nvmetcp isn't a configured transport; the handlers then skip
    // the notify entirely.
    let mut aer_hub_for_admin: Option<Arc<nvme_nvm::ControllerRegistry>> = None;
    // The iSCSI Unit Attention queue, lifted out of the iSCSI arm so the
    // admin socket's volume resize can fan a CAPACITY DATA HAS CHANGED UA
    // to connected iSCSI sessions (issue #76). `None` when iSCSI isn't a
    // configured transport; the handler then skips the iSCSI notify.
    let mut ua_tracker_for_admin: Option<Arc<shared_iscsi::unit_attention::UnitAttentionTracker>> =
        None;
    // The login-audit sink is consumed only by the iSCSI arm; park it in
    // an Option so an NVMe-only boot leaves it untouched (dropped after
    // the loop). Config de-dups `transports`, so iSCSI runs at most once
    // and the `.take()` below can't double-fire.
    let mut login_audit_slot = Some(login_audit);
    let mut nvmetcp_login_audit_slot = Some(nvmetcp_login_audit);
    for transport in &cfg.transports {
        match transport {
            Transport::Iscsi => {
                // Portal list comes first — the ALUA topology + listener
                // bind set both key off it.
                let iscsi_portals = cfg.iscsi.listen.clone().unwrap_or_else(|| {
                    vec![shared_iscsi::transport::Portal {
                        bind: DEFAULT_LISTEN_ADDRESS.to_string(),
                        advertise: None,
                        tpgt: 1,
                    }]
                });
                let iscsi_listens: Vec<String> =
                    iscsi_portals.iter().map(|p| p.bind.clone()).collect();

                // ALUA topology — built from the advertised portals with
                // the target IQN as the per-port NAA-3 namespace so two
                // daemons on the same host pick distinct identifiers.
                let alua = Arc::new(shared_iscsi::alua::AluaTopology::from_portals(
                    &iscsi_portals,
                    target_iqn.clone(),
                ));
                // Per-(TSIH, LUN) Unit Attention queue, shared between the
                // SBC dispatcher's per-command pop and the reservation-UA
                // sink's enqueue (issue #67).
                let ua_tracker =
                    Arc::new(shared_iscsi::unit_attention::UnitAttentionTracker::new());
                ua_tracker_for_admin = Some(Arc::clone(&ua_tracker));
                let collapse_isid = cfg.iscsi.reservations.initiator_port.collapse_isid();
                let handler = Arc::new(
                    SbcScsiDispatcher::with_alua(
                        Arc::clone(&registry) as Arc<dyn scsi_sbc::VolumeLookup>,
                        target_iqn.clone(),
                        alua,
                        Arc::clone(&reservations),
                        collapse_isid,
                        Some(Arc::clone(&ua_tracker)),
                    )
                    .with_admission(Arc::clone(&admission_view)),
                );
                // Proactive reservation-change notification (issue #67): a
                // reservation preempted/released over iSCSI or NVMe raises
                // a RESERVATIONS PREEMPTED / RELEASED UA on the affected
                // iSCSI initiators' next command. Registered before the
                // listener binds, so no session can race it.
                reservations.register_observer(Arc::new(shared_iscsi::IscsiReservationSink::new(
                    Arc::clone(&ua_tracker),
                    Arc::clone(&session_manager),
                    collapse_isid,
                )));
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
                let server_config = ServerConfig {
                    listen_portals: iscsi_portals.clone(),
                    session_manager: Arc::clone(&session_manager),
                    auth: chap_auth,
                    audit: login_audit_slot
                        .take()
                        .expect("transports de-duped: iscsi runs at most once"),
                    stale_session_timeout_secs: STALE_SESSION_TIMEOUT_SECS,
                };
                let transport_handler: Arc<dyn shared_iscsi::ScsiHandler> = handler;
                tracing::info!(
                    "transport: iscsi (listen={})",
                    iscsi_portals
                        .iter()
                        .map(|p| match &p.advertise {
                            Some(adv) => format!("{} (advertise {}),tpgt={}", p.bind, adv, p.tpgt),
                            None => format!("{},tpgt={}", p.bind, p.tpgt),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                transport_listens.extend(iscsi_listens);
                transport_futs.push(Box::pin(shared_iscsi::transport::run(
                    server_config,
                    transport_handler,
                )));
            }
            Transport::Nvmetcp => {
                // NVMe-TCP path. The DH-HMAC-CHAP login phase gets its own
                // login-audit sink (issue #68): refused in-band auths emit
                // `nvmetcp.dhchap.failure` rows + feed the `chap_failures`
                // alert class, parity with iSCSI CHAP. (TLS-PSK identity is
                // captured in the handshake and needs no per-connection
                // hook; this sink only fires when `auth.mode = dhchap`.)
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
                // Per-subsystem controller registry + AER hub. One instance
                // shared between the dispatcher (reservation-event producer)
                // and the NVMe/TCP transport (CNTLID allocator + AER
                // consumer) — same construct-once-at-boot pattern as
                // `controller_regs` below.
                let aer_hub = Arc::new(nvme_nvm::ControllerRegistry::new());
                // Hand the admin socket the same hub so volume lifecycle
                // changes can drive Namespace Attribute Changed AERs
                // (issue #64).
                aer_hub_for_admin = Some(Arc::clone(&aer_hub));
                // Proactive reservation-change notification (issue #67): a
                // reservation preempted/released over iSCSI or NVMe drives
                // a LID 0x80 + AER to the affected NVMe controllers.
                // Registered before the listener binds.
                reservations.register_observer(Arc::new(nvme_nvm::AerReservationSink::new(
                    Arc::clone(&aer_hub),
                )));
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
                    Arc::clone(&aer_hub),
                    Arc::clone(&reservations),
                ));
                tracing::info!(
                    "thurvsad: NVMe NVM dispatcher ready ({} NSID(s))",
                    volumes.len()
                );

                // Path to `nvmetcp-psks.json`. Used by TLS-PSK and by
                // per-hostnqn volume admission. We always have a path
                // (resolved once at boot from `tls.identity_file` or the
                // `<data_dir>/` default); the server treats a missing or
                // empty file as "no admission fence."
                let psks_path = nvmetcp_psks_path.clone();

                // Optional TLS 1.3 PSK acceptor. Disabled = cleartext
                // (legacy default). Psk = register a ClientHelloCallback
                // that reads `nvmetcp-psks.json` and derives every PSK
                // on every handshake. Operator edits via the
                // `nvmetcp psks` CLI verbs take effect on the next
                // session without restart.
                let tls_acceptor = match cfg.nvmetcp.tls.mode {
                    NvmetcpTlsMode::Disabled => None,
                    NvmetcpTlsMode::Psk => {
                        // Touch-or-create the stub on first boot so the
                        // acceptor's load step has something to parse.
                        let initial_file =
                            nvme_tcp::identity::NvmetcpPsksFile::load_or_create_default(&psks_path)
                                .with_context(|| format!("loading {}", psks_path.display()))?;
                        let acceptor = nvme_tcp::tls::build_psk_acceptor(&psks_path, &subnqn)
                            .context("building NVMe/TCP TLS-PSK acceptor")?;
                        tracing::info!(
                            identity_file = %psks_path.display(),
                            psk_count = initial_file.psks.len(),
                            "nvme-tcp: TLS-PSK enabled, parse-on-handshake",
                        );
                        Some(acceptor)
                    }
                };

                // Optional DH-HMAC-CHAP in-band auth. Independent of TLS:
                // `dhchap` can run with or without a TLS-PSK channel. When
                // on, every Connect asserts AUTHREQ and per-host secrets +
                // volume admission load from `nvmetcp-dhchap.json` on each
                // handshake. Operator edits via `nvmetcp dhchap` take
                // effect on the next session without restart.
                let dhchap_path = match cfg.nvmetcp.auth.mode {
                    NvmetcpAuthMode::None => None,
                    NvmetcpAuthMode::Dhchap => {
                        let p = nvmetcp_dhchap_path.clone();
                        let initial =
                            nvme_tcp::identity::NvmetcpDhchapFile::load_or_create_default(&p)
                                .with_context(|| format!("loading {}", p.display()))?;
                        tracing::info!(
                            identity_file = %p.display(),
                            entries = initial.dhchap.len(),
                            "nvme-tcp: DH-HMAC-CHAP enabled, parse-on-handshake",
                        );
                        Some(p)
                    }
                };

                tracing::info!(
                    "transport: nvmetcp (listen={}, subnqn={}, tls={}, dhchap={})",
                    nvmetcp_listen,
                    subnqn,
                    tls_acceptor.is_some(),
                    dhchap_path.is_some(),
                );
                // Pair admission with auth. DH-HMAC-CHAP owns admission
                // when on (its entries carry `volumes`); otherwise TLS-PSK
                // on -> admission lookup applies (mandatory); both off ->
                // see-everything (mirror of iSCSI no-CHAP).
                let admission_psks_path = if dhchap_path.is_some() {
                    None
                } else if tls_acceptor.is_some() {
                    Some(psks_path)
                } else {
                    None
                };
                let server_cfg = nvme_tcp::ServerConfig {
                    listen_address: nvmetcp_listen.clone(),
                    handler,
                    controller_regs: Arc::new(nvme_base::ControllerRegs::new()),
                    aer: aer_hub,
                    tls: tls_acceptor,
                    psks_path: admission_psks_path,
                    dhchap_path,
                    audit: nvmetcp_login_audit_slot
                        .take()
                        .expect("transports de-duped: nvmetcp runs at most once"),
                };
                transport_listens.push(nvmetcp_listen.clone());
                transport_futs.push(Box::pin(nvme_tcp::run(server_cfg)));

                // Discovery controller (issue #58). A second, cleartext,
                // unauthenticated listener on the conventional discovery
                // port answers the well-known discovery NQN so
                // `nvme discover` / `nvme connect-all` work without
                // out-of-band distribution of the SUBNQN / address /
                // port. It refers hosts to the I/O subsystem above; the
                // Discovery Log record advertises that subsystem's TLS
                // requirement (TREQ + TSAS.SECTYPE) so the host secures
                // the real Connect. Default on whenever nvmetcp is.
                if cfg.nvmetcp.discovery.enabled() {
                    let discovery_listen = cfg.nvmetcp.discovery.listen_addr();
                    // Resolve the (TRADDR, TRSVCID) the discovery log
                    // record advertises for the I/O subsystem. An explicit
                    // `nvmetcp.advertise` overrides both; otherwise it
                    // derives from the I/O bind (concrete IP advertised
                    // verbatim, wildcard -> reflect the request's local
                    // addr). See `resolve_discovery_traddr` (issue #84).
                    let (io_traddr, io_port) = resolve_discovery_traddr(
                        &nvmetcp_listen,
                        cfg.nvmetcp.advertise.as_deref(),
                    )?;
                    let (sectype, treq) = match cfg.nvmetcp.tls.mode {
                        NvmetcpTlsMode::Psk => (
                            nvme_base::log_page::disc_sectype::TLS13,
                            nvme_base::log_page::disc_treq::REQUIRED,
                        ),
                        NvmetcpTlsMode::Disabled => (
                            nvme_base::log_page::disc_sectype::NONE,
                            nvme_base::log_page::disc_treq::NOT_REQUIRED,
                        ),
                    };
                    let discovery_handler = Arc::new(nvme_nvm::DiscoveryHandler::new(
                        subnqn.clone(),
                        io_port,
                        io_traddr,
                        sectype,
                        treq,
                        format!("THURVSA{:013}", volumes.len()),
                        shared_naming::DISK_PRODUCT.to_string(),
                        THURVSA_VERSION_STR.to_string(),
                    ));
                    let discovery_cfg = nvme_tcp::ServerConfig {
                        listen_address: discovery_listen.clone(),
                        handler: discovery_handler,
                        controller_regs: Arc::new(nvme_base::ControllerRegs::new()),
                        aer: Arc::new(nvme_nvm::ControllerRegistry::new()),
                        tls: None,
                        psks_path: None,
                        dhchap_path: None,
                        audit: Arc::new(nvme_tcp::NoopLoginAudit),
                    };
                    tracing::info!(
                        "transport: nvmetcp-discovery (listen={}, advertises subnqn={}, traddr={}, port={}, sectype={})",
                        discovery_listen,
                        subnqn,
                        io_traddr
                            .map(|ip| ip.to_string())
                            .unwrap_or_else(|| "<reflect>".to_string()),
                        io_port,
                        sectype,
                    );
                    transport_listens.push(discovery_listen.clone());
                    transport_futs.push(Box::pin(nvme_tcp::run(discovery_cfg)));
                }
            }
        }
    }
    // If a transport wasn't listed, its audit sink was never consumed.
    drop(login_audit_slot);
    drop(nvmetcp_login_audit_slot);
    // Config guarantees >= 1 transport; guard anyway so the JoinSet is
    // never empty (an empty data path would silently serve nothing).
    if transport_futs.is_empty() {
        anyhow::bail!("no transports configured; set transports: [iscsi] and/or [nvmetcp]");
    }

    // Admin Unix socket — live volume create / destroy + read APIs +
    // long-running jobs.
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Web-admin password gate (#4): seed the live verifier from
    // <data_dir>/admin-password.json. Absent file = unconfigured (with
    // `http.auth.method: Password` the TCP listener's protected routes
    // fail closed); a malformed file is a hard startup error. The
    // configured method (#92) decides whether the gate is enforced at
    // all — default `None` serves the protected routes open. One handle,
    // cloned into the admin setter (AdminState) and the HTTP middleware
    // (HttpState).
    let auth_state =
        shared_admin_auth::AuthState::load_from(&shared_admin_auth::admin_password_path(&data_dir))
            .map_err(|e| anyhow::anyhow!("loading admin-password.json: {e}"))?
            .with_method(cfg.http.auth.method);
    if cfg.http.auth.method == shared_admin_auth::AuthMethod::None && auth_state.is_configured() {
        tracing::warn!(
            "http.auth.method is None but a web-admin password is configured; the password is NOT enforced (set http.auth.method: Password to enforce it)"
        );
    }

    let admin_state = AdminState {
        data_dir: data_dir.clone(),
        nvmetcp_psks_path,
        nvmetcp_dhchap_path,
        storage: Arc::new(cfg.storage.clone()),
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
        reservations: Arc::clone(&reservations),
        aer_hub: aer_hub_for_admin,
        ua_tracker: ua_tracker_for_admin,
        admission: Arc::clone(&admission_view),
        auth: auth_state.clone(),
    };
    let admin_socket = admin::admin_socket_path();

    // HTTP server — /health + /metrics + /sessions (+ read-only Web UI
    // on /ui and /api/v1 when enabled).
    let http_state = HttpState {
        telemetry: Arc::clone(&telemetry),
        registry: Arc::clone(&registry),
        sessions: Arc::clone(&session_manager),
        listen_addresses: transport_listens.clone(),
        target_iqn,
        auth: auth_state,
        admin: admin_state.clone(),
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
        let ghost_lists = ghost_lists.clone();
        let interval_secs = cfg.disk_cache.eviction_interval_seconds.max(1);
        let recent_seal_pin_seconds = cfg.disk_cache.recent_seal_pin_seconds;
        let default_size = cfg.disk_cache.size_gb;
        let bounds = cfg.disk_cache.bounds();
        let cloud_config_clone = cfg.storage.clone();
        let backend_names: Vec<String> = pool_budgets.keys().cloned().collect();
        Some(tokio::spawn(async move {
            run_disk_cache_eviction_worker(
                data_dir,
                pool_budgets,
                ghost_lists,
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

    // Periodic backend-reachability ticker. Opt-in via
    // `storage.check_interval_seconds` (0 = off, the default). When on,
    // it probes every configured backend on a timer and fires
    // `backend_reachability` failure/recovery transitions, so a backend
    // that goes unreachable overnight is caught without an operator
    // running `system storage check` by hand.
    let reachability_ticker_handle = {
        let interval = cfg.storage.check_interval_seconds;
        if interval > 0 {
            let storage_config = Arc::new(cfg.storage.clone());
            Some(tokio::spawn(async move {
                shared_admin_cloud_check::run_reachability_ticker(storage_config, interval).await;
            }))
        } else {
            None
        }
    };

    // Drive every data-path listener concurrently. The first to finish
    // (clean shutdown, error, bind failure, or task panic) resolves the
    // select arm and tears the daemon down — the same fail-fast the
    // single-transport boot had. Remaining listeners are aborted when
    // `transport_set` drops at end of scope.
    let mut transport_set: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
    for fut in transport_futs {
        transport_set.spawn(fut);
    }

    let result = tokio::select! {
        joined = transport_set.join_next() => {
            match joined {
                Some(Ok(Ok(()))) => Ok(()),
                Some(Ok(Err(e))) => {
                    tracing::error!("data-path transport exited with error: {}", e);
                    Err(e)
                }
                Some(Err(join_err)) => {
                    tracing::error!("data-path transport task panicked: {}", join_err);
                    Err(anyhow::anyhow!(join_err))
                }
                // Unreachable: the set is non-empty (guarded above), so
                // join_next never yields None before a task completes.
                None => Ok(()),
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
            http::log_route_table(
                &http_listener_cfg.listen,
                scheme,
                webui_cfg.enabled,
                http_password_required,
            );
            let router = http::build_router(http_state, &webui_cfg);
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
    if let Some(h) = reachability_ticker_handle {
        h.abort();
    }

    // Stop the rate-limit flush task and drain every still-open
    // suppression window so an in-flight 60 s window's count lands in
    // the chain before daemon.stop, not lost on exit.
    if let Some((limiter, flush_handle)) = audit_ratelimit {
        flush_handle.abort();
        if let Some((channel, _)) = audit_lifecycle.as_ref() {
            for rollup in limiter.flush_all() {
                crate::audit::emit_audit_ratelimit_rollup(channel, &rollup);
            }
        }
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
    ghost_lists: std::collections::HashMap<String, Arc<shared_pool::GhostList>>,
    backend_names: Vec<String>,
    interval: std::time::Duration,
    recent_seal_pin_seconds: u64,
    default_size: core_block::DiskCacheSize,
    bounds: core_block::DiskCacheBounds,
    storage_config: shared_object_store::ObjectStoreConfig,
) {
    use core_block::DiskCacheManager;
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // skip the immediate first tick
    // Budget divergence detector cadence — see `reconcile` below.
    let mut last_reconcile = std::time::Instant::now();
    loop {
        tick.tick().await;

        // Recompute per-backend caps for `auto`-mode entries against
        // current free space and push the new ceilings into each
        // backend's PoolBudget. Shared with VTL — see
        // `shared_disk_evict::resolve_and_apply_caps`.
        shared_disk_evict::resolve_and_apply_caps(
            &backend_names,
            &pool_budgets,
            &storage_config,
            &data_dir,
            default_size,
            bounds,
        );

        // Once per `BUDGET_RECONCILE_INTERVAL` (not every tick), do one
        // full-pool walk per backend purely to confirm the O(1)
        // budget-derived usage still matches on-disk reality, warning if
        // an un-instrumented mutation site has leaked drift (#49). This
        // is the only place the per-tick walk we removed still happens,
        // and it runs far less often than the eviction tick.
        let reconcile = last_reconcile.elapsed() >= shared_disk_evict::BUDGET_RECONCILE_INTERVAL;
        if reconcile {
            last_reconcile = std::time::Instant::now();
        }

        for name in &backend_names {
            let Some(budget) = pool_budgets.get(name) else {
                continue;
            };
            let cap = budget.cap_bytes();
            // O(1) usage read from the per-backend budget instead of the
            // old full-pool `calculate_usage` rescan. The budget is exact
            // across every pool mutation site (seal / eviction / GC /
            // read-miss refetch), so this preserves the old eviction
            // decision while dropping the per-tick walk (#49). VSA has no
            // `.staging/` step, so the budget total IS the on-disk pool
            // total. The pool walk now happens only inside
            // `evict_lru_chunks`, and only when over cap.
            let used = budget.current_bytes();
            // Low-cadence safety reconcile: walk the pool and warn if the
            // budget has drifted from on-disk reality (detection only).
            if reconcile {
                let mut cm_r = DiskCacheManager::new(data_dir.clone(), name, cap);
                if let Ok(Ok(actual)) =
                    tokio::task::spawn_blocking(move || cm_r.calculate_usage()).await
                {
                    shared_disk_evict::warn_on_budget_divergence(name, used, actual);
                }
            }
            // Within-budget log + soft-watermark alert (shared with VTL).
            if !shared_disk_evict::check_usage_or_alert(name, used, cap, budget) {
                continue;
            }
            let mut cm = DiskCacheManager::new(data_dir.clone(), name, cap);
            cm.set_pool_budget(budget.clone());
            if let Some(gl) = ghost_lists.get(name) {
                cm.set_ghost_list(gl.clone());
            }
            cm.set_recent_seal_pin_seconds(recent_seal_pin_seconds);
            cm.set_current_usage(used);
            // Synchronous fs-only eviction (no cloud round-trip): offload
            // the candidate-enumeration walk + fs::remove_file loop to a
            // blocking thread.
            let result = match tokio::task::spawn_blocking(move || cm.evict_lru_chunks()).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "disk-cache: eviction task for backend '{}' panicked: {}",
                        name,
                        e
                    );
                    continue;
                }
            };
            match result {
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

#[cfg(test)]
mod tests {
    use super::resolve_discovery_traddr;
    use std::net::IpAddr;

    #[test]
    fn discovery_traddr_derives_from_concrete_bind() {
        let (traddr, port) = resolve_discovery_traddr("10.0.0.5:4420", None).unwrap();
        assert_eq!(traddr, Some("10.0.0.5".parse::<IpAddr>().unwrap()));
        assert_eq!(port, 4420);
    }

    #[test]
    fn discovery_traddr_wildcard_bind_reflects() {
        // Wildcard bind -> None so the discovery controller reflects the
        // request's local addr; port is still carried.
        let (traddr, port) = resolve_discovery_traddr("0.0.0.0:4420", None).unwrap();
        assert_eq!(traddr, None);
        assert_eq!(port, 4420);
    }

    #[test]
    fn discovery_traddr_advertise_overrides_both_verbatim() {
        // Bind wildcard, advertise a concrete reachable ip:port — both
        // TRADDR and TRSVCID come from advertise (issue #84).
        let (traddr, port) =
            resolve_discovery_traddr("0.0.0.0:4420", Some("192.0.2.50:9420")).unwrap();
        assert_eq!(traddr, Some("192.0.2.50".parse::<IpAddr>().unwrap()));
        assert_eq!(port, 9420);
    }

    #[test]
    fn discovery_traddr_rejects_wildcard_advertise() {
        let err = resolve_discovery_traddr("0.0.0.0:4420", Some("0.0.0.0:4420")).unwrap_err();
        assert!(
            err.to_string().contains("wildcard"),
            "want 'wildcard' in error, got: {err}"
        );
    }

    #[test]
    fn discovery_traddr_rejects_malformed_advertise() {
        let err = resolve_discovery_traddr("0.0.0.0:4420", Some("not-an-addr")).unwrap_err();
        assert!(
            err.to_string().contains("ip:port"),
            "want 'ip:port' in error, got: {err}"
        );
    }
}
