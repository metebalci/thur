// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)] // Main daemon - some infrastructure unused but kept

mod admin;
mod diagnostics;
mod http;
mod iscsi;
mod memory_buffer_manager;
mod smoke;
mod state;
mod upload_recovery;
mod upload_worker;

use upload_worker::run_event_driven_upload_worker;

use anyhow::{Context, Result};
use clap::Parser;
use memory_buffer_manager::MemoryBufferManager;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

// `THURVTL_VERSION` is set by build.rs to "<crate-ver> (<sha>[-dirty])".
pub(crate) const THURVTL_VERSION_STR: &str = match option_env!("THURVTL_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser, Debug)]
#[command(name = "thurvtld", about = "ThurVTL daemon", version = THURVTL_VERSION_STR)]
struct Args {
    /// Path to config file (YAML)
    /// Defaults to /etc/thurvtl/thurvtl.yaml then ./thurvtl.yaml if exists
    #[arg(short, long)]
    config: Option<String>,

    /// Run smoke tests and exit (does not start daemon)
    #[arg(long)]
    test: bool,
}

impl Args {
    /// Resolve the config path. `--config PATH` wins; otherwise the
    /// production location at `/etc/thurvtl/thurvtl.yaml`. We
    /// deliberately don't fall back to `./thurvtl.yaml` — devs
    /// running outside `/etc/thurvtl/` should pass `--config`
    /// explicitly so the loaded config is unambiguous in logs.
    fn get_config_path(&self) -> String {
        if let Some(ref path) = self.config {
            return path.clone();
        }
        shared_naming::TAPE_LIBRARY.config_path.to_string()
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    data_dir: String, // local storage for buffers/manifests
    /// Per-tape RAM-staged read/write buffers. See `MemoryBuffersConfig`.
    #[serde(default)]
    memory_buffers: MemoryBuffersConfig,
    /// Shared content-addressed chunk pool budget on disk. See `DiskCacheConfig`.
    #[serde(default)]
    disk_cache: DiskCacheConfig,
    #[serde(default)]
    cloud: CloudConfig, // Cloud upload / compression / retention-check knobs (backends live in <data_dir>/cloud-backends.json)
    #[serde(default)]
    http: Option<HttpConfig>,
    #[serde(default)]
    iscsi: Option<IscsiConfig>,
    #[serde(default)]
    drive: Option<DriveConfig>,
    /// Audit log configuration. Always-on (writes to
    /// `<data_dir>/audit/` by default), tamper-evident BLAKE3 chain.
    /// No `enabled` knob — the FETB telemetry meter and the audit
    /// chain are co-engineered, and disabling either silently
    /// breaks the other.
    #[serde(default)]
    audit: AuditConfig,
    /// Telemetry / observability. The Prometheus `/metrics` endpoint
    /// on the daemon's HTTP server is always wired (no config knob —
    /// it lives on the same listener as `/health` and `/sessions`).
    /// The optional `telemetry.otlp.*` sub-block enables a parallel
    /// OTLP push exporter to a Collector or SaaS backend. See
    /// `TelemetryFileConfig`.
    #[serde(default)]
    telemetry: TelemetryFileConfig,
    /// Appliance-side at-rest encryption. `keystore.backends:` holds
    /// the named keystore-backend map (`local` / `awskms` / `vault` /
    /// `azurekv` / `gcpkms` / `kmip`). Encryption is opt-in per
    /// cartridge via `cartridge create --encrypt --keystore NAME`.
    /// Independent of host-driven AME (SSC-4 SECURITY PROTOCOL).
    #[serde(default)]
    keystore: shared_keystore::KeystoreYamlConfig,
    /// First-party alerting (email + generic webhook). Off by
    /// default; opt in by setting `alerting.enabled: true` and
    /// listing at least one sink. Full schema in `shared-alerting`.
    #[serde(default)]
    alerting: shared_alerting::AlertingConfig,
}

/// Hard floor on the operator-configured audit retention. The FETB
/// telemetry meter counts the trailing 28-day (4-week) window of
/// `fetb.sample` audit rows on every daemon startup; below 40 days
/// of retention there isn't enough margin between that window and
/// the audit-rotation cliff. Drop below 40 → daemon refuses to
/// start. No silent bump.
const MIN_AUDIT_RETENTION_DAYS: u32 = 40;

#[derive(Debug, Deserialize, Clone)]
struct AuditConfig {
    /// Directory for audit files. Default: `<data_dir>/audit`. Override
    /// with an absolute path to put the log on a different filesystem.
    #[serde(default)]
    dir: Option<String>,
    /// zstd-compress yesterday's `audit-YYYY-MM-DD.jsonl` file at
    /// rollover. Default: true. Set false to keep rotated files plain
    /// for `grep`-friendliness.
    #[serde(default = "default_audit_compress_rotated")]
    compress_rotated: bool,
    /// How many days of audit history the daemon keeps locally before
    /// pruning rotated files. The FETB telemetry meter counts the
    /// trailing 28-day (4-week) window on startup; the floor
    /// enforces ~12 days of margin over that window. Default 90;
    /// minimum 40 (refuse-to-start below).
    #[serde(default = "default_audit_retention_days")]
    retention_days: u32,
}

fn default_audit_compress_rotated() -> bool {
    true
}

fn default_audit_retention_days() -> u32 {
    90
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            dir: None,
            compress_rotated: default_audit_compress_rotated(),
            retention_days: default_audit_retention_days(),
        }
    }
}

/// File representation of the `telemetry:` block. Mapped to
/// `core_mediachanger::TelemetryConfig` at startup.
///
/// The Prometheus `/metrics` endpoint is always on — it shares the
/// daemon's HTTP listener with `/health` / `/sessions` / `/drives` and
/// has no knob. To turn metrics off entirely, drop the `http:` block.
#[derive(Debug, Deserialize, Clone, Default)]
struct TelemetryFileConfig {
    /// Optional OTLP push exporter. Operators with a Prometheus stack
    /// only ever need the pull endpoint; OTLP is for shipping to a
    /// Collector / managed SaaS / multi-destination fan-out.
    #[serde(default)]
    otlp: Option<OtlpFileConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct OtlpFileConfig {
    /// Set false to disable without removing the block.
    #[serde(default = "default_true")]
    enabled: bool,
    /// Collector / SaaS endpoint. Default points at a local Collector
    /// gRPC listener. Override per-deployment.
    #[serde(default = "default_otlp_endpoint")]
    endpoint: String,
    /// `grpc` (default, port 4317) | `http`/`http_protobuf` (port 4318).
    #[serde(default = "default_otlp_protocol")]
    protocol: String,
    /// Push interval in seconds. 30 mirrors most managed backends.
    #[serde(default = "default_otlp_interval_seconds")]
    interval_seconds: u64,
    /// Headers attached to every push (e.g. `authorization: Bearer …`
    /// for SaaS). Empty for an unauthenticated Collector.
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}
fn default_otlp_endpoint() -> String {
    "http://localhost:4317".to_string()
}
fn default_otlp_protocol() -> String {
    "grpc".to_string()
}
fn default_otlp_interval_seconds() -> u64 {
    30
}

impl OtlpFileConfig {
    fn into_core(self) -> Result<core_mediachanger::OtlpExporterConfig, String> {
        let protocol = match self.protocol.to_ascii_lowercase().as_str() {
            "grpc" | "grpc_tonic" => core_mediachanger::OtlpProtocol::Grpc,
            "http" | "http_protobuf" | "http-protobuf" => {
                core_mediachanger::OtlpProtocol::HttpProtobuf
            }
            other => {
                return Err(format!(
                    "telemetry.otlp.protocol: unknown '{other}', expected 'grpc' or 'http'"
                ));
            }
        };
        Ok(core_mediachanger::OtlpExporterConfig {
            endpoint: self.endpoint,
            protocol,
            interval: std::time::Duration::from_secs(self.interval_seconds.max(1)),
            headers: self.headers.into_iter().collect(),
        })
    }
}

/// Per-tape memory buffers: RAM-staged write/read scratch the daemon
/// keeps alive between iSCSI ops. Distinct from `disk_cache` (the
/// shared on-disk chunk pool). Tune these up if a single tape's
/// working set is large (sequential streams with deep prefetch) and
/// you have RAM to spare.
#[derive(Debug, Deserialize, Clone)]
struct MemoryBuffersConfig {
    /// Write-staging buffer size per tape, in GB. Holds bytes
    /// between iSCSI WRITE and chunk seal.
    #[serde(default = "default_write_gb_per_tape")]
    write_gb_per_tape: u64,
    /// Read-prefetch buffer size per tape, in GB. Caches chunks
    /// fetched ahead of the current read position.
    #[serde(default = "default_read_gb_per_tape")]
    read_gb_per_tape: u64,
    /// How many chunks ahead of the current read LBA the prefetcher
    /// pulls. 0 disables prefetch; 1-3 typical.
    #[serde(default = "default_read_prefetch_chunks_ahead")]
    read_prefetch_chunks_ahead: u32,
}

fn default_write_gb_per_tape() -> u64 {
    10
}
fn default_read_gb_per_tape() -> u64 {
    5
}
fn default_read_prefetch_chunks_ahead() -> u32 {
    2
}

impl Default for MemoryBuffersConfig {
    fn default() -> Self {
        Self {
            write_gb_per_tape: default_write_gb_per_tape(),
            read_gb_per_tape: default_read_gb_per_tape(),
            read_prefetch_chunks_ahead: default_read_prefetch_chunks_ahead(),
        }
    }
}

/// Content-addressed chunk pool budget, applied **per cloud backend**.
/// Each backend gets its own slice at `<data_dir>/chunks/<backend>/...`
/// independent of per-tape `memory_buffers` (those are RAM staging).
/// `size_gb` is a **hard cap**: chunk-seal applies upload backpressure
/// at the SCSI layer (NOT READY + retry) when that backend's pool would
/// exceed the cap. Eviction (refcount-aware LRU over `Both`-state
/// chunks) and successful uploads are what create headroom.
#[derive(Debug, Deserialize, Clone)]
struct DiskCacheConfig {
    /// Default per-backend disk-cache budget. Either an explicit GB
    /// integer or the literal string `auto` (the default): under
    /// `auto`, the eviction worker statvfs's `data_dir` on every
    /// tick and pins the cap to `min(50% of free, max_size_gb)`,
    /// floored at `min_size_gb`. Multi-backend installs with several
    /// `auto` entries split the 50%-of-free share evenly so two
    /// `auto` backends can't combined commit 100% of free space.
    /// Individual `cloud-backends.json` entries may override per-
    /// entry via their own `disk_cache_size_gb` field — same shape
    /// (`auto | <gb>`) so explicit and auto entries can coexist on
    /// one daemon.
    #[serde(default)]
    size_gb: core_mediachanger::DiskCacheSize,
    /// Floor (GB) for the `auto`-derived cap. Honored only when
    /// `size_gb: auto`; explicit values ignore both bounds (operator
    /// chose). Matches today's pre-`auto` default.
    #[serde(default = "default_min_size_gb")]
    min_size_gb: u64,
    /// Ceiling (GB) for the `auto`-derived cap. Honored only when
    /// `size_gb: auto`. Bounds the eviction-worker scan cost on very
    /// large filesystems where 50% of free could otherwise be
    /// terabytes.
    #[serde(default = "default_max_size_gb")]
    max_size_gb: u64,
    /// Soft watermark as a percentage of `size_gb`. Crossing it
    /// fires a warn-level log and bumps the
    /// `upload_backpressure_active` Prometheus gauge — early signal
    /// for operators before the hard cap is hit. Range 1-100.
    #[serde(default = "default_localonly_soft_watermark_pct")]
    localonly_soft_watermark_pct: u8,
    /// Reserve of free filesystem bytes (GB) below which chunk-seal
    /// also backpressures, regardless of pool occupancy. Catches
    /// disk-fill from sources outside the pool: bloated audit
    /// retention, manifest growth on huge tapes, external writers
    /// on the same partition. Set to 0 to disable.
    #[serde(default = "default_disk_free_min_gb")]
    disk_free_min_gb: u64,
    /// Pin pool chunks whose most recent `lru.idx` touch (seal OR
    /// read) is within this many seconds against LRU eviction.
    /// Counters the verify-after-write pattern (Veeam / NetBackup
    /// re-read freshly-written tapes within seconds), at the cost
    /// of capping effective cache capacity by the volume of recent
    /// writes-plus-reads. Default 0 disables the pin and restores
    /// pure LRU — see `ROADMAP.md` § Pin recent sealed chunks for
    /// the RC/GA validation task.
    #[serde(default = "default_recent_seal_pin_seconds")]
    recent_seal_pin_seconds: u64,
}

impl DiskCacheConfig {
    fn bounds(&self) -> core_mediachanger::DiskCacheBounds {
        core_mediachanger::DiskCacheBounds {
            min_gb: self.min_size_gb,
            max_gb: self.max_size_gb,
        }
    }
}

fn default_min_size_gb() -> u64 {
    core_mediachanger::DiskCacheBounds::DEFAULT.min_gb
}

fn default_max_size_gb() -> u64 {
    core_mediachanger::DiskCacheBounds::DEFAULT.max_gb
}

fn default_localonly_soft_watermark_pct() -> u8 {
    80
}

fn default_disk_free_min_gb() -> u64 {
    5
}

fn default_recent_seal_pin_seconds() -> u64 {
    0
}

impl Default for DiskCacheConfig {
    fn default() -> Self {
        Self {
            size_gb: core_mediachanger::DiskCacheSize::default(),
            min_size_gb: default_min_size_gb(),
            max_size_gb: default_max_size_gb(),
            localonly_soft_watermark_pct: default_localonly_soft_watermark_pct(),
            disk_free_min_gb: default_disk_free_min_gb(),
            recent_seal_pin_seconds: default_recent_seal_pin_seconds(),
        }
    }
}

// Cloud backend configuration is shared with the CLI in core-mediachanger.
// Re-exported via aliases here so existing field/var names keep working.
use core_mediachanger::CloudConfig;

#[derive(Debug, Deserialize, Clone)]
struct HttpConfig {
    #[serde(default = "default_http_listen")]
    listen: String,
    #[serde(default)]
    tls: HttpTlsConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct HttpTlsConfig {
    #[serde(default)]
    cert_file: String,
    #[serde(default)]
    key_file: String,
    #[serde(default)]
    client_ca_file: String,
    #[serde(default)]
    extra_sans: Vec<String>,
}

fn default_http_listen() -> String {
    "0.0.0.0:9090".to_string()
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: default_http_listen(),
            tls: HttpTlsConfig::default(),
        }
    }
}

impl HttpConfig {
    /// Coerce the YAML block into the `shared-admin-http` listener
    /// config. Fails fast at boot if the TLS triple is in a half-set
    /// state.
    fn listener_config(&self) -> anyhow::Result<shared_admin_http::HttpListenerConfig> {
        let tls = shared_admin_http::TlsConfig::from_yaml(
            &self.tls.cert_file,
            &self.tls.key_file,
            &self.tls.client_ca_file,
            &self.tls.extra_sans,
        )?;
        Ok(shared_admin_http::HttpListenerConfig {
            listen: self.listen.clone(),
            tls,
        })
    }
}

#[derive(Debug, Deserialize, Clone)]
struct IscsiConfig {
    #[serde(default = "default_iscsi_listen")]
    listen: String,
    #[serde(default = "default_target_iqn")]
    target_iqn: String,
    #[serde(default = "default_max_sessions")]
    max_sessions: u32,
    #[serde(default = "default_session_timeout")]
    session_timeout_seconds: u32,
    #[serde(default)]
    auth: AuthConfig,
}

fn default_iscsi_listen() -> String {
    "0.0.0.0:3260".to_string()
}
fn default_target_iqn() -> String {
    "iqn.2025-10.com.metebalci:thurvtl".to_string()
}
fn default_max_sessions() -> u32 {
    10
}
fn default_session_timeout() -> u32 {
    300
}

impl Default for IscsiConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:3260".to_string(),
            target_iqn: "iqn.2025-10.com.metebalci:thurvtl".to_string(),
            max_sessions: 10,
            session_timeout_seconds: 300,
            auth: AuthConfig::default(),
        }
    }
}

/// Drive-level configuration. Distinct from iSCSI because it covers
/// behavior of the emulated tape drive itself, not the iSCSI transport
/// or SCSI MEDIUM CHANGER addressing. Today only drive-side compression
/// lives here; future drive knobs (default encryption posture, etc.)
/// belong here too.
#[derive(Debug, Deserialize, Clone, Default)]
struct DriveConfig {
    #[serde(default)]
    compression: DriveCompressionConfig,
}

/// Drive-side compression config. There is intentionally NO "DCE
/// default" knob here: real LTO drives ship with DCE off at every
/// cartridge load, and the host turns it on via MODE SELECT page 0x0F
/// per session. The host is the source of truth for "should this
/// session compress?" — exposing a daemon-level override would
/// diverge from hardware behavior without buying anything operators
/// can't get from the host. The `algorithm` and `zstd_level` knobs
/// only take effect when the host eventually flips DCE on.
#[derive(Debug, Deserialize, Clone)]
struct DriveCompressionConfig {
    /// Algorithm used when DCE is on. `lz4` (default) | `zstd` | `sldc`
    /// (reserved — selecting it errors, codec not yet shipped).
    /// Recorded per-block in the manifest so changing this knob does
    /// not break reads of older blocks.
    #[serde(default = "default_drive_compression_algorithm")]
    algorithm: core_mediachanger::CompressionAlgo,
    /// Zstd compression level. Only consulted when `algorithm: zstd`.
    /// 1..=22; 3 is the broadly-balanced default. Ignored for lz4 / sldc.
    #[serde(default = "default_zstd_level")]
    zstd_level: i32,
}

fn default_drive_compression_algorithm() -> core_mediachanger::CompressionAlgo {
    core_mediachanger::CompressionAlgo::Lz4
}

fn default_zstd_level() -> i32 {
    core_mediachanger::ZSTD_DEFAULT_LEVEL
}

impl Default for DriveCompressionConfig {
    fn default() -> Self {
        Self {
            algorithm: core_mediachanger::CompressionAlgo::Lz4,
            zstd_level: core_mediachanger::ZSTD_DEFAULT_LEVEL,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
struct AuthConfig {
    #[serde(default)]
    method: shared_iscsi::auth::AuthMethod,
    #[serde(default = "default_chap_algorithms")]
    allowed_algorithms: Vec<String>,
}

fn default_chap_algorithms() -> Vec<String> {
    vec![
        "SHA3-256".to_string(),
        "SHA-256".to_string(),
        "SHA-1".to_string(),
        "MD5".to_string(),
    ]
}

/// Peek a cartridge's `manifest.json` and return its sticky cloud
/// backend name. Returns None if the manifest is missing, unreadable,
/// malformed, or has no `backend` field — the caller should treat
/// that as "skip this cartridge for now," because the manifest can be
/// lazily restored from cloud at load time.
fn read_cartridge_backend(tapes_root: &std::path::Path, tape_id: &str) -> Option<String> {
    let manifest_path = tapes_root.join(tape_id).join("manifest.json");
    let json = std::fs::read_to_string(&manifest_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    v.get("backend")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        // Daemon logs land in the systemd journal / syslog; ANSI color
        // escapes would show up there as raw `#033[..m` codes.
        .with_ansi(false)
        .init();

    let args = Args::parse();

    // Operator-visible identity + licensing banner — prints once
    // per daemon start, into the systemd journal. Two-line shape
    // (`<product> <version>` then the shared copyright/license
    // notice) so a future rebrand only touches `shared_naming`.
    info!(
        "{} {}",
        shared_naming::TAPE_LIBRARY.display_name,
        THURVTL_VERSION_STR
    );
    for line in shared_naming::COPYRIGHT_NOTICE.lines() {
        info!("{line}");
    }

    let config_path = args.get_config_path();
    info!("Using config file: {}", config_path);
    let cfg_text = tokio::fs::read_to_string(&config_path).await?;
    let cfg: Config = serde_yaml::from_str(&cfg_text)?;

    info!("thurvtl starting...");
    info!("data_dir: {}", cfg.data_dir);
    info!(
        "write memory buffer per tape: {} GiB, read memory buffer per tape: {} GiB, per-backend disk cache default: {}",
        cfg.memory_buffers.write_gb_per_tape,
        cfg.memory_buffers.read_gb_per_tape,
        match cfg.disk_cache.size_gb {
            core_mediachanger::DiskCacheSize::Auto => format!(
                "auto (min {} GiB, max {} GiB)",
                cfg.disk_cache.min_size_gb, cfg.disk_cache.max_size_gb,
            ),
            core_mediachanger::DiskCacheSize::Explicit(n) => format!("{n} GiB"),
        }
    );

    // Create data_dir if it doesn't exist
    tokio::fs::create_dir_all(&cfg.data_dir).await?;

    // Acquire daemon lock to prevent CLI operations while running
    info!("Acquiring daemon lock...");
    let _daemon_lock = core_mediachanger::DaemonLock::acquire(&cfg.data_dir)
        .map_err(|e| anyhow::anyhow!("Failed to acquire daemon lock: {}", e))?;
    info!("Daemon lock acquired");

    // Refuse to start if a legacy `<data_dir>/cloud-backends.json` is
    // still around — pre-alpha.2 kept backend definitions there; they
    // now live in the YAML conffile under `cloud.backends:`. Force the
    // operator to migrate the entries rather than silently ignore them.
    let data_dir_path = std::path::PathBuf::from(&cfg.data_dir);
    let config_path_buf = std::path::PathBuf::from(&config_path);
    shared_cloud::reject_legacy_cloud_backends_json(&data_dir_path, &config_path_buf)
        .map_err(anyhow::Error::msg)?;
    shared_keystore::reject_legacy_keystore_backends_json(&data_dir_path, &config_path_buf)
        .map_err(anyhow::Error::msg)?;
    cfg.cloud
        .validate_backends()
        .map_err(|e| anyhow::anyhow!("validate cloud.backends in {}: {}", config_path, e))?;
    info!("cloud: {} backend(s) configured", cfg.cloud.backends.len());

    let iscsi_users_path = data_dir_path.join("iscsi-users.json");
    let iscsi_users_file =
        shared_iscsi::auth::IscsiUsersFile::load_or_create_default(&iscsi_users_path)
            .map_err(|e| anyhow::anyhow!("loading {}: {}", iscsi_users_path.display(), e))?;
    info!(
        "iscsi-users.json loaded: {} user(s)",
        iscsi_users_file.users.len()
    );

    // Load library manifest (must be initialized via CLI). Done
    // before cloud validation so the cartridge ↔ backend referential
    // check below has the manifest list available.
    info!("Loading library manifest...");
    let lib_root = std::path::PathBuf::from(&cfg.data_dir).join("library");
    let tapes_root = std::path::PathBuf::from(&cfg.data_dir).join("tapes");

    let library = core_mediachanger::Library::open(&lib_root, &tapes_root)
        .map_err(|e| anyhow::anyhow!(
            "Failed to load library: {}. Initialize with: thurvtl library init --slots N --drives M --lto-generation G",
            e
        ))?;

    info!(
        "Library loaded: {} slots, {} mail slots, {} drives, LTO-{}",
        library.storage_slots().len(),
        library.mail_slots().len(),
        library.drives().len(),
        library.lto_generation()
    );

    // 🔸 Validate every named backend up front. Sequential, bails on
    // first failure so the operator gets a focused error instead of a
    // wall of partial results.
    info!("Validating cloud backend configuration...");
    for name in cfg.cloud.backend_names() {
        info!("  -> validating backend '{}'", name);
        core_mediachanger::validate_cloud_backend(&cfg.cloud, &name, |step| {
            info!("    [pass] {}: {}", step.name, step.detail);
        })
        .await
        .map_err(|e| anyhow::anyhow!("backend '{}': {}", name, e))?;
    }
    info!("Cloud backend validation complete");

    // 🔸 Cartridge ↔ backend referential integrity. Every cartridge
    // manifest carries a sticky `backend` field; refuse to start if
    // any cartridge references a name not in `cloud.backends`. The
    // operator must either re-add the missing backend or
    // export-and-delete the orphaned cartridge.
    {
        let backend_names: std::collections::HashSet<String> =
            cfg.cloud.backend_names().into_iter().collect();
        let tapes_dir = std::path::Path::new(&cfg.data_dir).join("tapes");
        if tapes_dir.is_dir() {
            for entry in std::fs::read_dir(&tapes_dir)? {
                let entry = entry?;
                let manifest_path = entry.path().join("manifest.json");
                if !manifest_path.is_file() {
                    continue;
                }
                let json = std::fs::read_to_string(&manifest_path)?;
                let v: serde_json::Value = match serde_json::from_str(&json) {
                    Ok(v) => v,
                    // Corrupt manifests are recoverable from cloud at
                    // load time; skip rather than block startup.
                    Err(_) => continue,
                };
                let cartridge_backend = match v.get("backend").and_then(|s| s.as_str()) {
                    Some(s) if !s.is_empty() => s,
                    _ => continue, // missing/empty: cartridge can't open anyway, surfaces at load
                };
                if !backend_names.contains(cartridge_backend) {
                    let label = entry
                        .path()
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(String::from)
                        .unwrap_or_else(|| "<unknown>".to_string());
                    return Err(anyhow::anyhow!(
                        "cartridge '{}' references cloud backend '{}' which is not configured \
                         in cloud.backends ({}). Either add the backend to thurvtl.yaml or \
                         export and delete the cartridge.",
                        label,
                        cartridge_backend,
                        backend_names.iter().cloned().collect::<Vec<_>>().join(", "),
                    ));
                }
            }
        }
    }

    // 🔸 Audit retention floor. The FETB telemetry meter counts the
    // trailing 28-day window on every startup; below
    // `MIN_AUDIT_RETENTION_DAYS` we lose the margin we need.
    if cfg.audit.retention_days < MIN_AUDIT_RETENTION_DAYS {
        return Err(anyhow::anyhow!(
            "audit.retention_days = {} is below the {}-day floor. The FETB telemetry \
             meter requires at least {} days of audit history. Raise audit.retention_days \
             in thurvtl.yaml.",
            cfg.audit.retention_days,
            MIN_AUDIT_RETENTION_DAYS,
            MIN_AUDIT_RETENTION_DAYS,
        ));
    }

    // 🔸 Open the audit log. Always on — there is no enabled knob.
    // The daemon is the sole writer once started; daemon-down CLI
    // flows (`library init` / `library modify`) drop their entries
    // into `<audit_dir>/pending/` and we drain that queue right after
    // open. A broken chain refuses to start (both tiers) — the
    // FETB telemetry meter counts the chain on every startup.
    let audit_log_dir = cfg.audit.dir.as_ref().map_or_else(
        || std::path::PathBuf::from(&cfg.data_dir).join("audit"),
        std::path::PathBuf::from,
    );

    // Open the audit log synchronously and run all startup-time sync
    // writes (replay queue drain, daemon.start, bootstrap FETB sample)
    // through the underlying `Arc<AuditLog>` directly. Once those are
    // done, [`spawn_audit_writer`] takes over: every subsequent runtime
    // append goes through the channel-backed writer task so iSCSI and
    // admin handlers never sit on the chain mutex.
    let audit_log_arc: Option<std::sync::Arc<core_mediachanger::AuditLog>> = {
        let mut audit_cfg = core_mediachanger::AuditConfig::new(
            audit_log_dir.clone(),
            core_mediachanger::AuditMode::TamperEvident,
        );
        audit_cfg.compress_rotated = cfg.audit.compress_rotated;
        match core_mediachanger::AuditLog::open(audit_cfg) {
            Ok(log) => {
                info!(
                    "Audit log opened: dir={} retention={}d",
                    audit_log_dir.display(),
                    cfg.audit.retention_days
                );
                let log = std::sync::Arc::new(log);
                match log.replay_pending() {
                    Ok((replayed, failed)) if replayed > 0 || failed > 0 => {
                        info!(
                            "audit replay: drained pending queue ({} appended, {} quarantined)",
                            replayed, failed
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("audit replay: scan failed (continuing): {}", e);
                    }
                }
                let params = serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "data_dir": cfg.data_dir,
                    "library": {
                        "drives": library.drives().len(),
                        "storage_slots": library.storage_slots().len(),
                        "mail_slots": library.mail_slots().len(),
                        "lto_generation": library.lto_generation(),
                    },
                });
                if let Err(e) = log.append(
                    "daemon.start",
                    core_mediachanger::AuditActor::daemon(),
                    params,
                    core_mediachanger::AuditResult::Ok,
                ) {
                    warn!("audit: failed to record daemon.start: {}", e);
                }
                // Stays on the sync `Arc<AuditLog>` path because the
                // channel writer task hasn't been spawned yet — this
                // entry, plus the bootstrap fetb sample below, are the
                // only writes that hit the chain mutex directly.
                Some(log)
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "audit log: failed to open at {}: {}. The FETB telemetry meter \
                     depends on the audit chain; the daemon refuses to start until the \
                     chain is healthy. Investigate with `thurvtl system audit verify`, \
                     then either fix the underlying issue or run \
                     `thurvtl system audit rotate --accept-break` to acknowledge the \
                     break and start a fresh chain.",
                    audit_log_dir.display(),
                    e
                ));
            }
        }
    };

    // Log cloud backend configuration. One line per named entry.
    for name in cfg.cloud.backend_names() {
        match cfg.cloud.backend_entry(&name) {
            Ok(core_mediachanger::BackendEntry::S3(s3)) => info!(
                "Cloud backend '{}': S3 (bucket={} prefix={} region={})",
                name, s3.bucket, s3.prefix, s3.region
            ),
            Ok(core_mediachanger::BackendEntry::Gcs(gcs)) => info!(
                "Cloud backend '{}': GCS (bucket={} prefix={} project={})",
                name, gcs.bucket, gcs.prefix, gcs.project_id
            ),
            Ok(core_mediachanger::BackendEntry::Azure(a)) => info!(
                "Cloud backend '{}': Azure (storage_account={} container={} prefix={})",
                name, a.storage_account, a.container, a.prefix
            ),
            Ok(core_mediachanger::BackendEntry::Local(l)) => {
                info!("Cloud backend '{}': Local (root_dir={})", name, l.root_dir)
            }
            Err(e) => warn!("Cloud backend '{}': {}", name, e),
        }
    }
    let enabled = cfg.memory_buffers.read_prefetch_chunks_ahead > 0;
    info!(
        "Prefetch: enabled={} chunks_ahead={}",
        enabled, cfg.memory_buffers.read_prefetch_chunks_ahead
    );

    // Initialize telemetry. The Prometheus reader is always wired
    // (served on `/metrics` by the daemon's HTTP server). The OTLP
    // push reader is opt-in via `telemetry.otlp.enabled` — when
    // configured, the OTel SDK pushes the same instruments to a
    // Collector / SaaS backend on the chosen interval.
    let http_cfg = cfg.http.as_ref().cloned().unwrap_or_default();
    let mut otlp = None;
    if let Some(ref o) = cfg.telemetry.otlp
        && o.enabled
    {
        match o.clone().into_core() {
            Ok(core) => {
                info!(
                    "Telemetry OTLP push: endpoint={} protocol={} interval={}s",
                    core.endpoint,
                    match core.protocol {
                        core_mediachanger::OtlpProtocol::Grpc => "grpc",
                        core_mediachanger::OtlpProtocol::HttpProtobuf => "http",
                    },
                    core.interval.as_secs()
                );
                otlp = Some(core);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("telemetry config: {}", e));
            }
        }
    }
    let telemetry_cfg = core_mediachanger::TelemetryConfig {
        service_name: Some(shared_naming::TAPE_LIBRARY.name.into()),
        service_instance_id: std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        instrument_prefix: Some(shared_naming::TAPE_LIBRARY.metric_prefix.into()),
        otlp,
    };
    let metrics = core_mediachanger::Telemetry::new(&telemetry_cfg)?;
    metrics.daemon_set_start_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );
    // Install the process-global handle so core call sites
    // (cartridge / cloud / audit / iscsi) can record without taking a
    // Telemetry argument. set_global is idempotent — a second `--test`
    // pass is a no-op rather than a panic.
    let _ = core_mediachanger::metrics::set_global(metrics.clone());

    // Alerting: build the dispatcher from YAML and install the
    // process-global handle so producer crates (iSCSI CHAP failures,
    // disk-cache loops) emit through `shared_alerting::record::*`
    // without taking a channel arg. Off by default; the dispatcher is
    // only built
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
            &shared_naming::TAPE_LIBRARY,
            env!("CARGO_PKG_VERSION"),
            metrics.clone(),
        )
        .context("building alerting dispatcher")?;
        let sink_count = dispatcher.sink_names().len();
        if shared_alerting::set_global(dispatcher).is_err() {
            tracing::warn!("alerting: process-global dispatcher already installed");
        }
        // Bridge audit-append failures into the alerting subsystem.
        // Idempotent on duplicate install — only `--test` runs would
        // hit the Err branch.
        let _ = shared_audit::set_append_failure_hook(shared_alerting::record::audit_append_failed);
        tracing::info!(
            "alerting: enabled with {} sink(s); dedup window {}s",
            sink_count,
            cfg.alerting.dedup_window_seconds
        );
    } else {
        tracing::info!("alerting: disabled (alerting.enabled=false in config)");
    }

    // shared-iscsi has its own pluggable metrics sink (it can't reach
    // into core-mediachanger directly without a circular dep). Install a
    // forwarder so the session manager's gauge updates land in the
    // same OTel MeterProvider as the rest of the daemon.
    struct CoreMetricsSink;
    impl shared_iscsi::metrics::MetricsSink for CoreMetricsSink {
        fn sessions_active(&self, n: i64) {
            core_mediachanger::metrics::record::iscsi_sessions_active(n);
        }
    }
    let _ = shared_iscsi::metrics::install_sink(Box::new(CoreMetricsSink));
    // Reflect the initial session count once at boot — this gauge
    // has no other natural update site after this point.
    metrics.iscsi_set_sessions_active(0);
    info!("HTTP server: {}", http_cfg.listen);

    // 🧪 Test mode: run smoke tests and exit
    if args.test {
        info!("Running smoke tests (--test mode)...");

        let mut all_passed = true;

        // Run basic cartridge smoke test
        info!(">>> Running cartridge smoke test...");
        if let Err(e) = smoke::run_smoke_test(&cfg).await {
            warn!("Cartridge smoke test failed: {e:?}");
            all_passed = false;
        } else {
            info!("Cartridge smoke test passed");
        }

        // Run library/changer smoke test
        info!(">>> Running library/changer smoke test...");
        if let Err(e) = smoke::run_changer_smoke_test(&cfg).await {
            warn!("Library smoke test failed: {e:?}");
            all_passed = false;
        } else {
            info!("Library smoke test passed");
        }

        // Run cloud backend smoke tests (cloud backend is always configured now)
        info!(">>> Running S3 smoke test...");
        if let Err(e) = smoke::run_s3_smoke_test(&cfg).await {
            warn!("S3 smoke test failed: {e:?}");
            all_passed = false;
        } else {
            info!("S3 smoke test passed");
        }

        info!(">>> Running prefetch smoke test...");
        if let Err(e) = smoke::run_prefetch_smoke_test(&cfg).await {
            warn!("Prefetch smoke test failed: {e:?}");
            all_passed = false;
        } else {
            info!("Prefetch smoke test passed");
        }

        info!(">>> Running parallel upload smoke test...");
        if let Err(e) = smoke::run_parallel_upload_smoke_test(&cfg).await {
            warn!("Parallel upload smoke test failed: {e:?}");
            all_passed = false;
        } else {
            info!("Parallel upload smoke test passed");
        }

        info!(">>> Running upload worker smoke test...");
        if let Err(e) = smoke::run_upload_worker_smoke_test(&cfg).await {
            warn!("Upload worker smoke test failed: {e:?}");
            all_passed = false;
        } else {
            info!("Upload worker smoke test passed");
        }

        info!(">>> Running performance benchmarks...");
        if let Err(e) = smoke::run_performance_benchmarks(&cfg).await {
            warn!("Performance benchmarks failed: {e:?}");
            all_passed = false;
        } else {
            info!("Performance benchmarks passed");
        }

        info!(">>> Running failure scenario tests...");
        if let Err(e) = smoke::run_failure_scenario_tests(&cfg).await {
            warn!("Failure scenario tests failed: {e:?}");
            all_passed = false;
        } else {
            info!("Failure scenario tests passed");
        }

        // Summary
        info!("{}", "=".repeat(60));
        if all_passed {
            info!("All smoke tests passed!");
            return Ok(());
        } else {
            warn!("Some smoke tests failed");
            return Err(anyhow::anyhow!("Smoke tests failed"));
        }
    }

    // 🚀 Normal daemon mode: start background workers
    info!("Starting daemon mode (use --test to run smoke tests)");

    // 🔸 Spawn the audit writer task. Every runtime audit emission from
    // here on (iSCSI handlers, admin endpoints, the FETB sampler,
    // gc) goes through the bounded mpsc and the dedicated
    // task drains it FIFO into the chain. Producers never sit on the
    // chain mutex; channel-full drops surface in
    // `thurvtl_audit_queue_drops_total`. The shutdown handle is
    // joined at the bottom of `main` so daemon.stop is the last entry
    // in the chain.
    let (audit_log, audit_writer_handle) = match audit_log_arc.as_ref() {
        Some(arc) => {
            let (chan, handle) = core_mediachanger::spawn_audit_writer(std::sync::Arc::clone(arc));
            (Some(chan), Some(handle))
        }
        None => (None, None),
    };

    // 🔸 Create event bus for tape operations (Phase 2: Event-Driven Architecture)
    let (event_tx, event_rx) =
        tokio::sync::broadcast::channel::<core_mediachanger::TapeEvent>(1000);
    info!("Event bus created (capacity: 1000 events)");

    // 🔸 Create upload request channel (Phase 4: Event-Driven Uploads)
    let (upload_tx, upload_rx) =
        tokio::sync::mpsc::channel::<memory_buffer_manager::UploadRequest>(100);
    info!("Upload request channel created (capacity: 100 requests)");

    // 🔸 Create prefetch request channel (Phase 5: Event-Driven Prefetch)
    let (prefetch_tx, prefetch_rx) =
        tokio::sync::mpsc::channel::<memory_buffer_manager::PrefetchRequest>(100);
    info!("Prefetch request channel created (capacity: 100 requests)");

    // 🔸 Cache-eviction wakeup signal. Notify coalesces a burst of
    // upload-completion notifications into a single eviction pass.
    let disk_cache_evict_notify = Arc::new(tokio::sync::Notify::new());

    // 🔸 Per-backend pool budgets. Each backend gets its own cap: either
    // the per-entry `disk_cache_size_gb` override from cloud-backends.json,
    // or the YAML `disk_cache.size_gb` default. Both share the
    // `DiskCacheSize` shape (`auto` | <gb>); `auto` entries split the
    // 50%-of-free share evenly so two `auto` backends can't combined
    // commit 100% of free space. Chunk-seal applies upload backpressure
    // when this backend's slice is full. Refresh from disk so the
    // budget reflects whatever LocalOnly chunks survived a previous
    // run.
    let pool_budgets: std::collections::HashMap<String, Arc<core_mediachanger::PoolBudget>> = {
        let disk_free_min_bytes = cfg
            .disk_cache
            .disk_free_min_gb
            .saturating_mul(1024 * 1024 * 1024);
        let soft_pct = cfg.disk_cache.localonly_soft_watermark_pct;
        let default_size = cfg.disk_cache.size_gb;
        let bounds = cfg.disk_cache.bounds();
        let data_dir = std::path::PathBuf::from(&cfg.data_dir);

        // Resolve per-backend size shapes once, then count `auto`
        // entries before any of them resolve so the share divisor is
        // stable.
        let resolved: Vec<(String, core_mediachanger::DiskCacheSize, bool)> = cfg
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
            let budget = Arc::new(core_mediachanger::PoolBudget::with_backend(
                name.clone(),
                data_dir.clone(),
                cap_bytes,
                disk_free_min_bytes,
                soft_pct,
            ));
            if let Err(e) =
                core_mediachanger::refresh_pool_budget_from_tapes(&budget, &data_dir, &name)
            {
                warn!(
                    "PoolBudget startup refresh for backend '{}' failed (will assume empty): {}",
                    name, e
                );
            }
            let shape = match size {
                core_mediachanger::DiskCacheSize::Auto => "auto",
                core_mediachanger::DiskCacheSize::Explicit(_) => "explicit",
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
        info!(
            "Per-backend pool budgets: {} (soft {}%, disk-free min {} GB)",
            log_lines.join(", "),
            soft_pct,
            cfg.disk_cache.disk_free_min_gb,
        );
        map
    };

    // 🔸 Start event-driven upload worker (Phase 4: Event-Driven Uploads)
    info!("Starting event-driven cloud upload worker");
    let cfg_clone = cfg.clone();
    let disk_cache_evict_notify_upload = Arc::clone(&disk_cache_evict_notify);
    let upload_worker_handle = tokio::spawn(async move {
        if let Err(e) =
            run_event_driven_upload_worker(&cfg_clone, upload_rx, disk_cache_evict_notify_upload)
                .await
        {
            warn!("Upload worker error: {e:?}");
        }
    });

    // 🔸 Start event-driven prefetch worker (Phase 5: Event-Driven Prefetch)
    info!("Starting event-driven cloud prefetch worker");
    let cfg_clone = cfg.clone();
    let prefetch_worker_handle = tokio::spawn(async move {
        if let Err(e) = run_event_driven_prefetch_worker(&cfg_clone, prefetch_rx).await {
            warn!("Prefetch worker error: {e:?}");
        }
    });

    // 🔸 Start cache-pool eviction worker (event-driven on upload-completion)
    info!("Starting cache-pool eviction worker (event-driven)");
    let cfg_clone = cfg.clone();
    let disk_cache_evict_notify_worker = Arc::clone(&disk_cache_evict_notify);
    let pool_budgets_for_eviction = pool_budgets.clone();
    let disk_cache_worker_handle = tokio::spawn(async move {
        run_disk_cache_eviction_worker(
            &cfg_clone,
            disk_cache_evict_notify_worker,
            pool_budgets_for_eviction,
        )
        .await;
    });

    // 🔸 Start the FETB telemetry sampler. Take a bootstrap sample
    // now so the gauges carry a real number before the first
    // periodic tick, then loop every 6 h: sum each cartridge's
    // host_bytes_written, emit one `fetb.sample` audit row, recount
    // the rolling window, publish the two Prometheus gauges.
    let fetb_sampler_handle = {
        let data_dir = std::path::PathBuf::from(&cfg.data_dir);
        let audit_dir_for_fetb = audit_log_dir.clone();
        let audit_log_for_fetb = audit_log.clone();
        shared_audit::fetb::record_fetb_sample(
            &data_dir,
            &audit_dir_for_fetb,
            "tapes",
            audit_log_for_fetb.as_ref(),
        )
        .await;
        Some(tokio::spawn(async move {
            shared_audit::fetb::run_fetb_sampler(
                data_dir,
                audit_dir_for_fetb,
                "tapes",
                audit_log_for_fetb,
            )
            .await;
        }))
    };

    // 🔸 Construct the audit rate-limiter. Bounds host-driven failure
    // floods (CHAP failures, MOVE MEDIUM refusals) on the audit chain.
    // 60 s window: a misconfigured initiator gets one chain entry per
    // distinct (op, peer, reason) tuple plus a single rollup at window
    // expiry, instead of one entry per retry.
    let audit_ratelimiter = std::sync::Arc::new(core_mediachanger::AuditRateLimiter::new(
        std::time::Duration::from_secs(60),
    ));

    // 🔸 Start audit-ratelimit flush task. Drains expired suppression
    // windows every 10 s and writes the rollup ("N events suppressed
    // in window") into the audit chain. Cadence is well below the
    // 60 s window so the steady-state lag between window expiry and
    // rollup emission is bounded.
    let audit_ratelimit_flush_handle = {
        let audit_log_for_rl = audit_log.clone();
        let limiter = std::sync::Arc::clone(&audit_ratelimiter);
        Some(tokio::spawn(async move {
            run_audit_ratelimit_flush(limiter, audit_log_for_rl).await;
        }))
    };

    // 🔸 Start MemoryBufferManager (Phase 3: Per-Tape Buffer Tracking, Phase 4: Event-Driven Uploads, Phase 5: Event-Driven Prefetch)
    let write_buffer_gb = cfg.memory_buffers.write_gb_per_tape;
    let read_buffer_gb = cfg.memory_buffers.read_gb_per_tape;
    // Clone the upload sender before passing it into the manager so the
    // boot-time orphan-upload scan can dispatch directly to the same
    // worker mpsc without going through the manager's event loop.
    let upload_tx_for_recovery = upload_tx.clone();
    let memory_buffer_manager = MemoryBufferManager::new(
        event_rx,
        write_buffer_gb,
        read_buffer_gb,
        upload_tx,
        prefetch_tx,
    );
    let memory_buffer_manager_handle = tokio::spawn(async move {
        if let Err(e) = memory_buffer_manager.run().await {
            warn!("MemoryBufferManager error: {e:?}");
        }
    });
    info!("MemoryBufferManager started");

    // 🔸 Background scan for orphan chunks left behind by a previous
    // daemon kill mid-PUT. Walks every cartridge's `chunks.idx`,
    // finds sealed-but-not-uploaded entries, and re-queues them via
    // the existing UploadRequest mpsc. The scan runs concurrently
    // with iSCSI traffic — cartridges currently loaded into a drive
    // have their own live-flush path; orphan re-queues touch the
    // same `apply_chunk_upload_outcome` lock chain so concurrent
    // mutation is safe.
    {
        let data_dir = std::path::PathBuf::from(&cfg.data_dir);
        let audit_for_recovery = audit_log.clone();
        tokio::spawn(async move {
            upload_recovery::scan_and_enqueue_orphans(
                data_dir,
                upload_tx_for_recovery,
                audit_for_recovery,
            )
            .await;
        });
    }

    // 🔸 Build shared DaemonState and start iSCSI target server.
    // Use defaults if iscsi / drive sections are missing (matching http
    // section behavior). Drive-side compression lives under the
    // top-level `drive` section in the YAML; the iSCSI server still
    // needs the values to seed each newly-loaded cartridge's
    // DriveCompressionState.
    let iscsi_cfg = cfg.iscsi.as_ref().cloned().unwrap_or_default();
    if let Err(e) = shared_naming::validate_iqn(&iscsi_cfg.target_iqn) {
        anyhow::bail!("invalid iscsi.target_iqn in {config_path}: {e}");
    }
    let drive_cfg = cfg.drive.as_ref().cloned().unwrap_or_default();

    let lib_storage_slots = library.storage_slots().len() as u16;
    let lib_mail_slots = library.mail_slots().len() as u16;
    let lib_drives = library.drives().len() as u16;
    let lib_lto_generation = library.lto_generation();

    let element_config = scsi_smc::changer::ElementAddressConfig::new(
        library.transport_base(),
        library.storage_base(),
        lib_storage_slots,
        library.import_export_base(),
        lib_mail_slots,
        library.data_transfer_base(),
        lib_drives,
    );

    let mail_range_log = if element_config.import_export_count == 0 {
        "none".to_string()
    } else {
        format!(
            "{}-{}",
            element_config.import_export_start,
            element_config.import_export_start + element_config.import_export_count - 1
        )
    };
    info!(
        "Element addressing: transport {}, storage {}-{}, mail {}, drives {}-{}",
        element_config.transport_start,
        element_config.storage_start,
        element_config.storage_start + element_config.storage_count - 1,
        mail_range_log,
        element_config.data_transfer_start,
        element_config.data_transfer_start + element_config.data_transfer_count - 1
    );

    // The audit log is shared so SCSI/CHAP entries land in the same
    // chain as daemon.start / library / cartridge mutations. Cloud
    // backend registry is shared with upload / cache workers so we
    // don't pay the auth round-trip more than once per backend; the
    // SCSI READ prefetch hook resolves cartridges' sticky
    // `manifest.backend` through it on cache miss.
    let cloud_backends_registry: iscsi::server::CloudBackendRegistry =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cloud_config_arc = std::sync::Arc::new(cfg.cloud.clone());
    let keystore_config_arc = std::sync::Arc::new(cfg.keystore.clone());
    let backpressure_max_wait =
        std::time::Duration::from_secs(cfg.cloud.upload.backpressure_max_wait_seconds.into());

    let library_arc = std::sync::Arc::new(std::sync::Mutex::new(library));

    let daemon_state = std::sync::Arc::new(state::DaemonState::new(state::DaemonStateConfig {
        data_dir: std::path::PathBuf::from(&cfg.data_dir),
        tapes_root: tapes_root.clone(),
        library: std::sync::Arc::clone(&library_arc),
        element_config,
        target_iqn: iscsi_cfg.target_iqn.clone(),
        listen_address: iscsi_cfg.listen.clone(),
        event_tx: event_tx.clone(),
        audit_log: audit_log.clone(),
        audit_dir: audit_log_dir.clone(),
        audit_ratelimiter: std::sync::Arc::clone(&audit_ratelimiter),
        cloud_backends: std::sync::Arc::clone(&cloud_backends_registry),
        cloud_config: std::sync::Arc::clone(&cloud_config_arc),
        keystore_config: std::sync::Arc::clone(&keystore_config_arc),
        num_drives: lib_drives as usize,
        drive_compression_algorithm: drive_cfg.compression.algorithm,
        drive_compression_zstd_level: drive_cfg.compression.zstd_level,
        pool_budgets: pool_budgets.clone(),
        backpressure_max_wait,
    }));

    // 🔸 At-rest DEK pre-unwrap: walk every cartridge dir, peek
    // `manifest.json` for an `encryption` block, and (if present)
    // resolve the named keystore backend and unwrap the wrapped DEK.
    // Caches the plaintext DEK in `DriveManager::dek_cache` so the
    // synchronous SCSI MOVE MEDIUM hot path can pick it up without
    // touching the keystore. A keystore that's unreachable at boot
    // surfaces as a `load_cartridge` refusal later — better than a
    // mixed plaintext + ciphertext pool.
    if let Ok(mut entries) = tokio::fs::read_dir(&tapes_root).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Some(label) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !entry
                .file_type()
                .await
                .ok()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let (uuid, meta_opt) =
                match core_mediachanger::Cartridge::read_manifest_identity(&tapes_root, &label) {
                    Ok(p) => p,
                    Err(_) => continue, // missing/corrupt manifest — load_cartridge will refuse
                };
            let Some(meta) = meta_opt else { continue };
            let ks = match daemon_state
                .keystore_config
                .create_backend_named(&meta.keystore_backend, &daemon_state.data_dir)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    warn!(
                        "At-rest DEK pre-unwrap: keystore '{}' for cartridge '{}' \
                         not usable: {e}. Cartridge load will refuse until the \
                         keystore is reachable.",
                        meta.keystore_backend, label
                    );
                    continue;
                }
            };
            let wrapped = match meta.wrapped_dek.as_deref() {
                Some(b64) => {
                    use base64::Engine as _;
                    match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(
                                "At-rest DEK pre-unwrap: manifest.encryption.wrapped_dek \
                                 for cartridge '{}' is not valid base64: {e}",
                                label
                            );
                            continue;
                        }
                    }
                }
                None => Vec::new(), // `local` backend manages the blob itself
            };
            match ks.unwrap(&uuid, &wrapped).await {
                Ok(dek) => {
                    daemon_state
                        .drive_manager
                        .set_cartridge_dek(&label, *dek.as_bytes());
                    info!(
                        "At-rest DEK cached for cartridge '{}' (keystore '{}')",
                        label, meta.keystore_backend
                    );
                }
                Err(e) => {
                    warn!(
                        "At-rest DEK pre-unwrap: keystore '{}' unwrap for cartridge '{}' \
                         failed: {e}. Cartridge load will refuse until the keystore is \
                         reachable.",
                        meta.keystore_backend, label
                    );
                }
            }
        }
    }

    // 🔸 Start admin Unix-socket server. CLI mutating commands will
    // reach the daemon through this surface as the CLI/daemon
    // migration lands; today it serves a single /api/v1/health
    // smoke endpoint.
    let admin_socket_path = admin::admin_socket_path();
    let admin_server_handle = {
        let socket_path = admin_socket_path.clone();
        let daemon_state_for_admin = std::sync::Arc::clone(&daemon_state);
        Some(tokio::spawn(async move {
            if let Err(e) = admin::run_admin_server(socket_path, daemon_state_for_admin).await {
                warn!("Admin server error: {e:?}");
            }
        }))
    };

    let iscsi_result = {
        info!("Starting iSCSI target server on {}", iscsi_cfg.listen);

        // Convert daemon's config to iscsi module's config
        let iscsi_config = iscsi::config::IscsiConfig {
            iscsi: iscsi::config::IscsiSettings {
                listen_address: iscsi_cfg.listen.clone(),
                target_iqn: iscsi_cfg.target_iqn.clone(),
                max_sessions: iscsi_cfg.max_sessions,
                session_timeout_seconds: iscsi_cfg.session_timeout_seconds,
                auth: iscsi::config::AuthSettings {
                    method: iscsi_cfg.auth.method,
                    allowed_algorithms: iscsi_cfg.auth.allowed_algorithms.clone(),
                },
                drive_compression_algorithm: drive_cfg.compression.algorithm,
                drive_compression_zstd_level: drive_cfg.compression.zstd_level,
            },
            library: iscsi::config::LibrarySettings {
                num_storage_slots: lib_storage_slots,
                num_mail_slots: lib_mail_slots,
                num_drives: lib_drives,
                lto_generation: lib_lto_generation,
            },
        };

        match iscsi::IscsiServer::new(
            iscsi_config,
            iscsi_users_file.clone(),
            iscsi_users_path.clone(),
            std::sync::Arc::clone(&daemon_state),
        ) {
            Ok(server) => {
                let iscsi_server = std::sync::Arc::new(server);

                // Spawn iSCSI server task
                let server_clone = iscsi_server.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = server_clone.run().await {
                        warn!("iSCSI server error: {e:?}");
                    }
                });

                Some(handle)
            }
            Err(e) => {
                warn!("Failed to start iSCSI server: {e:?}");
                None
            }
        }
    };

    let iscsi_server_handle = iscsi_result;

    // 🔸 Start unified HTTP server. All endpoints read through
    // DaemonState, so the HTTP surface is decoupled from the iSCSI
    // task — /health, /metrics, /sessions, and /drives all work
    // whether or not iSCSI itself is running.
    //
    // Bind + serve (and optional TLS termination + self-signed
    // auto-gen) live in `shared-admin-http`; this module only
    // builds the per-product Router.
    let listener_cfg = http_cfg.listener_config()?;
    let http_server_handle = {
        info!("Starting unified HTTP server");
        let metrics_arc = std::sync::Arc::new(metrics.clone());
        let daemon_state_for_http = std::sync::Arc::clone(&daemon_state);
        let state = http::HttpState {
            metrics: metrics_arc,
            daemon_state: daemon_state_for_http,
        };
        let router = http::build_router(state);
        let scheme = if listener_cfg.tls.is_some() {
            "https"
        } else {
            "http"
        };
        http::log_route_table(&listener_cfg.listen, scheme);
        Some(tokio::spawn(async move {
            if let Err(e) = shared_admin_http::run_http_server(listener_cfg, router).await {
                warn!("HTTP server error: {e:?}");
            }
        }))
    };

    // park runtime
    tokio::signal::ctrl_c().await?;
    info!("Shutting down.");

    // Drain any pending rate-limited suppressions so the rollup
    // entries land in the chain before daemon.stop. Otherwise an
    // in-flight 60 s window's count is lost when the daemon exits.
    for rollup in audit_ratelimiter.flush_all() {
        emit_audit_ratelimit_rollup(audit_log.as_ref(), &rollup);
    }

    // Record daemon.stop before tearing down workers — keeps the audit
    // entry in the chain even if a worker abort path is the cause of
    // shutdown. Pushed through the channel so it preserves FIFO order
    // with any in-flight runtime emissions; the writer-task drain
    // immediately below guarantees it hits disk before we exit.
    if let Some(ref chan) = audit_log {
        chan.try_append(
            "daemon.stop",
            core_mediachanger::AuditActor::daemon(),
            serde_json::json!({"reason": "sigint"}),
            core_mediachanger::AuditResult::Ok,
        );
    }

    // Drain the audit channel and join the writer task. Order matters:
    // every producer-side emission queued before this point — including
    // daemon.stop — is guaranteed on disk by the time `shutdown`
    // returns (FIFO mpsc + sentinel oneshot). After this, late
    // producers (e.g. iSCSI tasks still winding down) hit the
    // channel-closed branch and are silently dropped.
    if let Some(handle) = audit_writer_handle {
        handle.shutdown().await;
    }

    // Cancel workers if running
    if let Some(handle) = http_server_handle {
        handle.abort();
    }
    upload_worker_handle.abort();
    prefetch_worker_handle.abort();
    disk_cache_worker_handle.abort();
    if let Some(handle) = audit_ratelimit_flush_handle {
        handle.abort();
    }
    if let Some(handle) = fetb_sampler_handle {
        handle.abort();
    }
    if let Some(handle) = iscsi_server_handle {
        handle.abort();
    }
    if let Some(handle) = admin_server_handle {
        handle.abort();
    }
    if admin_socket_path.exists()
        && let Err(e) = std::fs::remove_file(&admin_socket_path)
    {
        warn!(
            "failed to remove admin socket {} on shutdown: {}",
            admin_socket_path.display(),
            e
        );
    }
    memory_buffer_manager_handle.abort();

    Ok(())
}

/// Background worker that periodically scans for pending chunk uploads
/// and uploads them to S3 in parallel. Runs indefinitely until cancelled.
///
/// Milestone 3 Phase 2: Parallel uploads with retry logic
async fn run_upload_worker(cfg: &Config) -> Result<()> {
    use core_mediachanger::{Cartridge, CartridgeOpenMode};
    use std::time::Duration;
    use tokio::task::JoinSet;

    // Create S3 backend
    let cloud_backend = cfg
        .cloud
        .create_backend_named(&cfg.cloud.backend_names()[0])
        .await?;

    let upload_cfg = &cfg.cloud.upload;
    let max_concurrent = upload_cfg.max_concurrent;
    let retry_max_attempts = upload_cfg.retry_max_attempts;

    info!(
        "S3 upload worker initialized (max_concurrent={}, retry_max_attempts={})",
        max_concurrent, retry_max_attempts
    );

    let tapes_root = std::path::Path::new(&cfg.data_dir).join("tapes");

    loop {
        // Sleep first to avoid tight loop on errors
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Scan tapes directory for cartridges
        let Ok(mut entries) = tokio::fs::read_dir(&tapes_root).await else {
            warn!("Failed to read tapes directory");
            continue;
        };

        let mut tape_labels = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry
                .file_type()
                .await
                .ok()
                .map(|t| t.is_dir())
                .unwrap_or(false)
                && let Some(name) = entry.file_name().to_str()
            {
                tape_labels.push(name.to_string());
            }
        }

        // Process each cartridge
        for label in tape_labels {
            // Open cartridge with S3 backend
            let mut cart = match Cartridge::open_with_cloud(
                &tapes_root,
                &label,
                CartridgeOpenMode::Open,
                Some(cloud_backend.clone()),
            ) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to open cartridge {}: {e:?}", label);
                    continue;
                }
            };

            // Get pending uploads
            let mut pending = cart.get_pending_uploads();

            if pending.is_empty() {
                continue;
            }

            info!("Cartridge {} has {} pending uploads", label, pending.len());

            // Upload chunks in parallel batches
            let mut successful_uploads = Vec::new();

            while !pending.is_empty() {
                let mut join_set = JoinSet::new();

                // Take up to max_concurrent chunks for this batch
                let batch: Vec<_> = pending.drain(..pending.len().min(max_concurrent)).collect();

                info!("Starting parallel upload batch: {} chunks", batch.len());

                // Spawn upload tasks
                for (chunk_id, _s3_key, _local_path) in batch {
                    // Clone cart for the async task
                    let mut cart_clone = match Cartridge::open_with_cloud(
                        &tapes_root,
                        &label,
                        CartridgeOpenMode::Open,
                        Some(cloud_backend.clone()),
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            warn!("Failed to open cartridge {} for upload: {e:?}", label);
                            continue;
                        }
                    };

                    let label_clone = label.clone();

                    join_set.spawn(async move {
                        // Single attempt — the per-backend retry inside
                        // `upload_chunk_to_cloud → upload_chunk_inert` already
                        // does jittered exponential retries with
                        // classify-and-fail-fast on permanent errors. The
                        // outer worker loop iterates `pending` again on the
                        // next 10 s tick; chunks left with `uploaded=false`
                        // come back through `cart.get_pending_uploads()`.
                        match cart_clone.upload_chunk_to_cloud(chunk_id).await {
                            Ok(_cloud_key) => {
                                info!(
                                    "Successfully uploaded chunk {} from {}",
                                    chunk_id, label_clone
                                );
                                Ok(chunk_id)
                            }
                            Err(e) => {
                                warn!(
                                    "Upload failed for chunk {} from {} after backend retries: {e:?}",
                                    chunk_id, label_clone
                                );
                                Err((chunk_id, e))
                            }
                        }
                    });
                }

                // Wait for all tasks in this batch to complete
                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok(Ok(chunk_id)) => {
                            successful_uploads.push(chunk_id);
                        }
                        Ok(Err((chunk_id, e))) => {
                            warn!("Chunk {} upload failed permanently: {e:?}", chunk_id);
                            // Chunk stays in pending state, will retry in next iteration
                        }
                        Err(e) => {
                            warn!("Upload task panicked: {e:?}");
                        }
                    }
                }
            }

            // After uploading chunks, backup manifest to S3
            if !successful_uploads.is_empty() {
                match cart.backup_manifest_to_cloud().await {
                    Ok(_keys) => {
                        info!(
                            "Backed up manifest for {} to S3 ({} chunks uploaded)",
                            label,
                            successful_uploads.len()
                        );
                    }
                    Err(e) => {
                        warn!("Failed to backup manifest for {}: {e:?}", label);
                    }
                }
            }
        }
    }
}

/// Event-driven prefetch worker (Phase 5: Event-Driven Prefetch)
///
/// Listens for prefetch requests from MemoryBufferManager.
/// Triggered by sequential read pattern detection.
///
/// Note: In the current architecture, chunks are automatically downloaded from S3
/// when there's a cache miss during read_block() operations. This worker tracks
/// prefetch hints to optimize future reads, but actual downloads happen on-demand.
/// A full implementation would pre-download chunks to local cache.
async fn run_event_driven_prefetch_worker(
    _cfg: &Config,
    mut prefetch_rx: tokio::sync::mpsc::Receiver<memory_buffer_manager::PrefetchRequest>,
) -> Result<()> {
    info!("Event-driven prefetch worker initialized (hint tracking only)");

    // Listen for prefetch requests
    while let Some(request) = prefetch_rx.recv().await {
        debug!(
            "Prefetch hint for {}: chunks {:?} (downloads will happen on next read)",
            request.tape_id, request.chunk_ids
        );

        // Phase 5 MVP: Just log prefetch hints
        // Full implementation would:
        // 1. Check if chunks exist locally
        // 2. Download from S3 if missing
        // 3. Update MemoryBufferManager read_buffer_usage
        // 4. Track prefetch success/failure
        //
        // For now, the existing read_block() logic handles downloads automatically
        // when there's a cache miss, so prefetch is implicit.
    }

    info!("Prefetch worker shutting down (channel closed)");
    Ok(())
}

// Phase 5: Periodic eviction worker removed - now handled by event-driven MemoryBufferManager
// The old run_eviction_worker() and cleanup_old_manifests() functions have been removed
// as eviction is now triggered immediately when read buffer is full (on_block_read)

/// Event-driven worker that enforces the global `cache_gb` budget on
/// the shared content-addressed chunk pool. Per-tape read/write buffers
/// are tracked separately by `MemoryBufferManager` and don't share this
/// ceiling.
///
/// Wakeup sources:
/// - `disk_cache_evict_notify`: fired by the upload worker when one or more
///   chunks finish uploading (Local -> Both transition). Notify
///   coalesces, so a burst of completions becomes a single pass.
/// - 5-minute backstop tick: catches read-miss downloads that grew the
///   pool without going through the upload worker (S3Only -> Both in
///   `Cartridge::read_block_async`).
///
/// On wakeup the worker debounces ~250 ms so a parallel-upload batch
/// finishing in quick succession produces a single eviction pass, then
/// calls `DiskCacheManager::evict_lru_chunks` (refcount-aware: checks every
/// cartridge manifest before deleting a pool file).
async fn run_disk_cache_eviction_worker(
    cfg: &Config,
    notify: Arc<tokio::sync::Notify>,
    pool_budgets: std::collections::HashMap<String, Arc<core_mediachanger::PoolBudget>>,
) {
    use core_mediachanger::DiskCacheManager;
    use std::path::PathBuf;
    use std::time::Duration;

    let data_dir = PathBuf::from(&cfg.data_dir);

    // Backstop tick covers cache growth paths that don't go through the
    // upload worker (notably read-miss downloads). 5 min is loose
    // enough that we're not polling for nothing under steady-state, and
    // tight enough that a read-only workload over a tight cache_gb
    // budget still makes progress.
    let mut backstop = tokio::time::interval(Duration::from_secs(300));
    backstop.tick().await; // skip immediate first tick at startup

    let backend_names: Vec<String> = cfg.cloud.backend_names();

    loop {
        tokio::select! {
            _ = notify.notified() => {
                // Coalesce additional notifies from the same upload
                // batch before doing the (relatively expensive) full
                // pool walk.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            _ = backstop.tick() => {}
        }

        // Recompute per-backend caps for `auto`-mode entries against
        // current free space, then push the new value into each
        // backend's PoolBudget so `try_reserve` immediately sees the
        // updated ceiling. External disk pressure shrinks the cap
        // reactively; recovery grows it. Explicit-mode entries are
        // pinned and skip the recompute. Count auto-mode backends
        // first so the share divisor is stable across the loop.
        let bounds = cfg.disk_cache.bounds();
        let default_size = cfg.disk_cache.size_gb;
        let resolved_sizes: Vec<(String, core_mediachanger::DiskCacheSize)> = backend_names
            .iter()
            .map(|name| {
                let size = cfg
                    .cloud
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
                    debug!(
                        "disk-cache auto-resize backend '{}': {} -> {} bytes",
                        name,
                        budget.cap_bytes(),
                        new_cap,
                    );
                }
                budget.set_cap_bytes(new_cap);
            }
        }

        // Per-backend DiskCacheManagers. Each manager is scoped to one
        // named backend and evicts down to *that backend's* cap (read
        // from the per-backend PoolBudget the construction phase built,
        // possibly re-resolved above for `auto`-mode entries this
        // tick). Per-backend pools are sharded under
        // `<data_dir>/chunks/<backend>/`, so identical hashes in two
        // backends are physically distinct files — per-backend LRU is
        // also globally correct.
        let mut managers: Vec<DiskCacheManager> = backend_names
            .iter()
            .map(|name| {
                let cap = pool_budgets.get(name).map(|b| b.cap_bytes()).unwrap_or(0);
                let mut cm = DiskCacheManager::new(data_dir.clone(), name, cap);
                if let Some(budget) = pool_budgets.get(name) {
                    cm.set_pool_budget(budget.clone());
                }
                cm.set_recent_seal_pin_seconds(cfg.disk_cache.recent_seal_pin_seconds);
                cm
            })
            .collect();

        for cm in managers.iter_mut() {
            let used = match cm.calculate_usage() {
                Ok(u) => u,
                Err(e) => {
                    warn!(
                        "Cache usage calculation for backend '{}' failed: {e}",
                        cm.backend_name()
                    );
                    continue;
                }
            };
            let cap = cm.capacity();
            if used <= cap {
                let pct = if cap == 0 {
                    0
                } else {
                    used.saturating_mul(100).checked_div(cap).unwrap_or(0)
                };
                debug!(
                    "Cache pool '{}' {} / {} bytes ({}%), no eviction",
                    cm.backend_name(),
                    used,
                    cap,
                    pct,
                );
                // Soft-watermark alert: per-backend dedup keeps this
                // to one emit per dedup window for as long as the
                // pool sits above `localonly_soft_watermark_pct`.
                if let Some(budget) = pool_budgets.get(cm.backend_name())
                    && budget.over_soft_watermark()
                {
                    shared_alerting::record::disk_cache_watermark(cm.backend_name(), pct, cap);
                }
                continue;
            }

            info!(
                "Cache pool '{}' over budget ({} / {} bytes); attempting LRU eviction",
                cm.backend_name(),
                used,
                cap,
            );

            // Build a fresh cloud backend for the eviction pass.
            // Eviction is rare; the construction cost is negligible.
            let cloud_backend = match cfg.cloud.create_backend_named(cm.backend_name()).await {
                Ok(b) => Some(b),
                Err(e) => {
                    warn!(
                        "Cache eviction: backend '{}' init failed ({e}); \
                         evicting without cloud backup",
                        cm.backend_name()
                    );
                    None
                }
            };

            match cm.evict_lru_chunks(cloud_backend.as_deref()).await {
                Ok(freed) if freed > 0 => {
                    info!(
                        "Cache eviction freed {} bytes from backend '{}'",
                        freed,
                        cm.backend_name()
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "Cache eviction for backend '{}' failed: {e}",
                    cm.backend_name()
                ),
            }
        }
    }
}

// Phase 6: Old run_metrics_server() function removed - replaced by http::run_http_server()
// The new unified HTTP server provides /health, /metrics, and /sessions endpoints

/// Cadence at which the audit-ratelimit flush task drains expired
/// suppression windows. Must be shorter than the limiter's window
/// (currently 60 s) so steady-state rollup latency stays bounded.
const AUDIT_RATELIMIT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Drain the audit rate-limiter periodically. For every expired
/// window with a non-zero suppression count, append a rollup entry
/// to the audit log: same op as the original event, params carrying
/// `suppressed_count` + `window_seconds` so a chain reader can spot
/// the rollup, and an `Error` result string explaining the
/// suppression.
async fn run_audit_ratelimit_flush(
    limiter: Arc<core_mediachanger::AuditRateLimiter>,
    audit_log: Option<core_mediachanger::AuditChannel>,
) {
    let mut ticker = tokio::time::interval(AUDIT_RATELIMIT_FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!(
        "audit ratelimit: flush task started (interval={}s, window={}s)",
        AUDIT_RATELIMIT_FLUSH_INTERVAL.as_secs(),
        limiter.window().as_secs(),
    );
    loop {
        ticker.tick().await;
        for rollup in limiter.flush_expired() {
            emit_audit_ratelimit_rollup(audit_log.as_ref(), &rollup);
        }
    }
}

/// Append one rate-limit rollup entry to the audit log. Best-effort:
/// a failed append is logged via `tracing::warn` so the host is
/// already aware via the original first emission and the suppression
/// metric.
fn emit_audit_ratelimit_rollup(
    audit_log: Option<&core_mediachanger::AuditChannel>,
    rollup: &core_mediachanger::AuditRateLimitRollup,
) {
    let Some(log) = audit_log else {
        return;
    };
    let params = serde_json::json!({
        "suppressed_count": rollup.suppressed_count,
        "window_seconds": rollup.window_seconds,
        "key": rollup.key,
    });
    let detail = format!(
        "{} additional event(s) suppressed in {}s window",
        rollup.suppressed_count, rollup.window_seconds
    );
    log.try_append(
        &rollup.op,
        rollup.actor.clone(),
        params,
        core_mediachanger::AuditResult::Error(detail),
    );
}

#[cfg(test)]
mod config_parse_tests {
    use super::*;

    /// Path to the canonical `dist/thurvtl.defaults.yaml`. Same
    /// lookup the CLI in-sync guard rail uses. Layout C puts this
    /// crate at `<workspace>/vtl/daemon/`, so the workspace root
    /// is two parents up.
    fn defaults_yaml() -> String {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = std::path::PathBuf::from(manifest_dir)
            .parent()
            .and_then(|p| p.parent())
            .expect("thurvtld must live two levels under the repo root")
            .join("dist/thurvtl.defaults.yaml");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
    }

    /// `thurvtl.defaults.yaml` leaves `data_dir` and the contents of
    /// `cloud.backends:` commented out (operator-set). Inject the
    /// minimum required values and parse — proves the daemon accepts
    /// every default key.
    #[test]
    fn daemon_parses_defaults_yaml() {
        // Replace the empty `backends:` line with one that has a
        // single local backend, so the required-but-unset map satisfies
        // CloudConfig's `backends: BTreeMap<...>` field.
        let raw = defaults_yaml();
        let injected = raw.replace(
            "  backends:\n",
            "  backends:\n    testdev:\n      type: local\n      root_dir: \"/tmp/thur-test-backend\"\n",
        );
        let yaml = format!("data_dir: /tmp/thur-test\n{}", injected);
        let cfg: Config =
            serde_yaml::from_str(&yaml).expect("daemon must parse thurvtl.defaults.yaml");
        assert_eq!(cfg.data_dir, "/tmp/thur-test");
    }

    #[test]
    fn minimal_yaml_only_data_dir_fills_every_default() {
        // A bare `data_dir:` config: every other section must take
        // its `#[serde(default)]` fallback.
        let cfg: Config =
            serde_yaml::from_str("data_dir: /srv/thur\n").expect("minimal config parses");
        assert_eq!(cfg.data_dir, "/srv/thur");
        assert_eq!(cfg.audit.retention_days, default_audit_retention_days());
        assert!(cfg.audit.compress_rotated);
        assert_eq!(
            cfg.memory_buffers.write_gb_per_tape,
            default_write_gb_per_tape()
        );
        assert_eq!(
            cfg.memory_buffers.read_gb_per_tape,
            default_read_gb_per_tape()
        );
        // The optional sections default to absent.
        assert!(cfg.iscsi.is_none());
        assert!(cfg.http.is_none());
        assert!(cfg.drive.is_none());
    }

    #[test]
    fn iscsi_config_block_fills_serde_defaults() {
        let cfg: Config = serde_yaml::from_str("data_dir: /srv/thur\niscsi: {}\n")
            .expect("config with empty iscsi block");
        let iscsi = cfg.iscsi.expect("iscsi block present");
        assert_eq!(iscsi.listen, default_iscsi_listen());
        assert_eq!(iscsi.target_iqn, default_target_iqn());
        assert_eq!(iscsi.max_sessions, default_max_sessions());
    }

    #[test]
    fn http_config_block_fills_serde_defaults() {
        let cfg: Config = serde_yaml::from_str("data_dir: /srv/thur\nhttp: {}\n")
            .expect("config with empty http block");
        let http = cfg.http.expect("http block present");
        assert_eq!(http.listen, default_http_listen());
    }

    #[test]
    fn config_default_helper_values() {
        assert!(default_true());
        assert!(default_audit_compress_rotated());
        assert_eq!(default_audit_retention_days(), 90);
        assert_eq!(default_otlp_protocol(), "grpc");
        assert_eq!(default_otlp_interval_seconds(), 30);
        assert_eq!(default_session_timeout(), 300);
        assert_eq!(default_max_sessions(), 10);
        assert_eq!(
            default_drive_compression_algorithm(),
            core_mediachanger::CompressionAlgo::Lz4
        );
        assert!(default_otlp_endpoint().contains("4317"));
        assert!(!default_chap_algorithms().is_empty());
    }

    #[test]
    fn args_get_config_path_override_and_default() {
        let args = Args {
            config: Some("/etc/x.yaml".to_string()),
            test: false,
        };
        assert_eq!(args.get_config_path(), "/etc/x.yaml");
        let args = Args {
            config: None,
            test: false,
        };
        assert_eq!(
            args.get_config_path(),
            shared_naming::TAPE_LIBRARY.config_path
        );
    }

    #[test]
    fn args_clap_parses_config_and_test_flags() {
        use clap::Parser;
        let args = Args::parse_from(["thurvtld", "--config", "/tmp/c.yaml", "--test"]);
        assert_eq!(args.config.as_deref(), Some("/tmp/c.yaml"));
        assert!(args.test);
        let args = Args::parse_from(["thurvtld"]);
        assert!(args.config.is_none());
        assert!(!args.test);
    }

    #[test]
    fn read_cartridge_backend_missing_manifest_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_cartridge_backend(dir.path(), "ABSENT").is_none());
    }

    #[test]
    fn read_cartridge_backend_reads_backend_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tape = dir.path().join("TAPE001");
        std::fs::create_dir_all(&tape).expect("mkdir tape");
        std::fs::write(tape.join("manifest.json"), r#"{"backend":"s3b"}"#).expect("write manifest");
        assert_eq!(
            read_cartridge_backend(dir.path(), "TAPE001"),
            Some("s3b".to_string())
        );
    }

    #[test]
    fn read_cartridge_backend_empty_backend_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tape = dir.path().join("TAPE001");
        std::fs::create_dir_all(&tape).expect("mkdir tape");
        std::fs::write(tape.join("manifest.json"), r#"{"backend":""}"#).expect("write manifest");
        assert!(read_cartridge_backend(dir.path(), "TAPE001").is_none());
    }

    #[test]
    fn read_cartridge_backend_malformed_json_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tape = dir.path().join("TAPE001");
        std::fs::create_dir_all(&tape).expect("mkdir tape");
        std::fs::write(tape.join("manifest.json"), "{not json").expect("write manifest");
        assert!(read_cartridge_backend(dir.path(), "TAPE001").is_none());
    }

    #[test]
    fn emit_audit_ratelimit_rollup_noop_without_log() {
        let rollup = core_mediachanger::AuditRateLimitRollup {
            op: "scsi.move_medium".to_string(),
            actor: core_mediachanger::AuditActor::cli("tester".to_string()),
            key: "drive-0".to_string(),
            suppressed_count: 7,
            window_seconds: 60,
        };
        // No audit channel — must return without panicking.
        emit_audit_ratelimit_rollup(None, &rollup);
    }
}
