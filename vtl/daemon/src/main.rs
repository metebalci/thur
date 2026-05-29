// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)] // Main daemon - some infrastructure unused but kept

mod admin;
mod diagnostics;
mod http;
mod iscsi;
mod memory_buffer_manager;
mod memory_buffers_size;
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

/// Build a `DeclaredTopology` from the YAML `library:` block.
/// Surfaces every missing required field at once (one error line per
/// missing field) so the operator doesn't fix them one at a time.
fn build_declared_topology(
    block: Option<&LibraryConfig>,
    config_path: &str,
) -> Result<core_mediachanger::library::DeclaredTopology> {
    let block = block.ok_or_else(|| {
        anyhow::anyhow!(
            "library: block missing from {}. Required fields: num_slots, num_drives, lto_generation.",
            config_path,
        )
    })?;
    let mut missing: Vec<&'static str> = Vec::new();
    if block.num_slots.is_none() {
        missing.push("num_slots");
    }
    if block.num_drives.is_none() {
        missing.push("num_drives");
    }
    if block.lto_generation.is_none() {
        missing.push("lto_generation");
    }
    if !missing.is_empty() {
        let lines: Vec<String> = missing.iter().map(|f| format!("library.{}", f)).collect();
        return Err(anyhow::anyhow!(
            "{}: required field(s) missing from library: block: {}",
            config_path,
            lines.join(", "),
        ));
    }
    Ok(core_mediachanger::library::DeclaredTopology {
        num_storage_slots: block.num_slots.expect("checked Some above"),
        num_drives: block.num_drives.expect("checked Some above"),
        lto_generation: block.lto_generation.expect("checked Some above"),
        firmware: block.firmware.clone(),
    })
}

/// Walk the reconcile-event vector from `open_or_materialize` and
/// emit the matching audit rows. Ordering matches the vector: every
/// `DriveEvacuated` precedes the summary row.
fn emit_reconcile_audit(
    log: &std::sync::Arc<core_mediachanger::AuditLog>,
    events: &[core_mediachanger::library::reconcile::ReconcileEvent],
) {
    use core_mediachanger::library::reconcile::ReconcileEvent;
    for ev in events {
        match ev {
            ReconcileEvent::DriveEvacuated(d) => {
                let params = serde_json::json!({
                    "drive_id": d.drive_id,
                    "barcode": d.barcode,
                    "origin_slot": d.origin_slot,
                    "trigger": "library.reconcile",
                });
                if let Err(e) = log.append(
                    "inventory.move_medium",
                    core_mediachanger::AuditActor::daemon(),
                    params,
                    core_mediachanger::AuditResult::Ok,
                ) {
                    warn!("audit: failed to record inventory.move_medium: {}", e);
                }
            }
            ReconcileEvent::Materialized => {
                if let Err(e) = log.append(
                    "library.materialize",
                    core_mediachanger::AuditActor::daemon(),
                    serde_json::json!({}),
                    core_mediachanger::AuditResult::Ok,
                ) {
                    warn!("audit: failed to record library.materialize: {}", e);
                }
            }
            ReconcileEvent::Reconciled => {
                if let Err(e) = log.append(
                    "library.reconcile",
                    core_mediachanger::AuditActor::daemon(),
                    serde_json::json!({}),
                    core_mediachanger::AuditResult::Ok,
                ) {
                    warn!("audit: failed to record library.reconcile: {}", e);
                }
            }
        }
    }
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
    storage: ObjectStoreConfig, // Upload / compression / retention-check knobs (backend list lives under `storage.backends:` in YAML)
    #[serde(default)]
    http: Option<HttpConfig>,
    #[serde(default)]
    iscsi: Option<IscsiConfig>,
    #[serde(default)]
    drive: Option<DriveConfig>,
    /// Audit log configuration. Always-on (writes to
    /// `<data_dir>/audit/` by default), tamper-evident BLAKE3 chain.
    /// No `enabled` knob — audit is unconditional.
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
    /// Chassis topology declaration. The three counts
    /// (`num_slots`, `num_drives`, `lto_generation`) are compulsory
    /// with no defaults — the daemon refuses to start naming each
    /// missing field. Fields are modeled as `Option<...>` here so
    /// the daemon produces its own missing-field error instead of
    /// serde's generic message. The optional `firmware` overrides
    /// the per-LTO default (`TVL7`/`TVL8`/`TVL0`).
    ///
    /// Phase 0 adds the block to the schema but does not yet read
    /// it at startup; Phase 2 wires
    /// `reconcile::open_or_materialize` to consume it.
    #[serde(default)]
    library: Option<LibraryConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct LibraryConfig {
    /// Number of storage slots. REQUIRED.
    num_slots: Option<u32>,
    /// Number of tape drives. REQUIRED.
    num_drives: Option<u32>,
    /// LTO generation: 7 or 8. REQUIRED.
    lto_generation: Option<u8>,
    /// 1-4 ASCII chars; INQUIRY revision override. Defaults
    /// to `TVL<gen>` per `default_firmware_for_lto`.
    #[serde(default)]
    firmware: Option<String>,
}

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
    /// pruning rotated files. Default 90.
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
///
/// `write_gb_per_tape` / `read_gb_per_tape` each accept either an
/// integer GB count or the literal `"auto"`. Under `auto` the
/// daemon reads `/proc/meminfo MemTotal` once at boot, divides
/// `auto_host_fraction_pct` of host RAM across `library.num_drives`,
/// splits 2:1 between write and read, and clamps each field to
/// `[auto_min_gb_per_tape, auto_max_gb_per_tape]`. Total resolved
/// footprint (`(write + read) × num_drives`) is then safety-checked
/// against `safety_max_host_fraction_pct` of MemTotal and the
/// daemon refuses to start if exceeded.
#[derive(Debug, Deserialize, Clone)]
struct MemoryBuffersConfig {
    /// Write-staging buffer size per tape. Integer GB or `"auto"`.
    #[serde(default)]
    write_gb_per_tape: memory_buffers_size::MemoryBuffersSize,
    /// Read-prefetch buffer size per tape. Integer GB or `"auto"`.
    #[serde(default)]
    read_gb_per_tape: memory_buffers_size::MemoryBuffersSize,
    /// How many chunks ahead of the current read LBA the prefetcher
    /// pulls. 0 disables prefetch; 1-3 typical.
    #[serde(default = "default_read_prefetch_chunks_ahead")]
    read_prefetch_chunks_ahead: u32,
    /// Fraction (percent) of `/proc/meminfo MemTotal` that the
    /// auto-resolver budgets for **all** memory_buffers. Honored
    /// only when at least one field is `auto`; explicit GB values
    /// don't consult this knob. Range 1-100.
    #[serde(default = "default_auto_host_fraction_pct")]
    auto_host_fraction_pct: u64,
    /// Fraction (percent) of `MemTotal` that the resolved total
    /// memory_buffers footprint (`(write + read) × num_drives`) is
    /// not allowed to exceed. Applies to both auto- and explicit-
    /// resolved fields — catches operator overrides that overcommit.
    /// The daemon refuses to start if exceeded. Range 1-100.
    #[serde(default = "default_safety_max_host_fraction_pct")]
    safety_max_host_fraction_pct: u64,
    /// Floor (GB) for the per-tape auto-resolved value. Honored
    /// only when the field is `auto`; explicit values ignore it.
    #[serde(default = "default_auto_min_gb_per_tape")]
    auto_min_gb_per_tape: u64,
    /// Ceiling (GB) for the per-tape auto-resolved value. Honored
    /// only when the field is `auto`. Caps the auto budget on
    /// hosts with hundreds of GB of RAM and few drives.
    #[serde(default = "default_auto_max_gb_per_tape")]
    auto_max_gb_per_tape: u64,
}

fn default_read_prefetch_chunks_ahead() -> u32 {
    2
}
fn default_auto_host_fraction_pct() -> u64 {
    50
}
fn default_safety_max_host_fraction_pct() -> u64 {
    75
}
fn default_auto_min_gb_per_tape() -> u64 {
    1
}
fn default_auto_max_gb_per_tape() -> u64 {
    32
}

impl Default for MemoryBuffersConfig {
    fn default() -> Self {
        Self {
            write_gb_per_tape: memory_buffers_size::MemoryBuffersSize::default(),
            read_gb_per_tape: memory_buffers_size::MemoryBuffersSize::default(),
            read_prefetch_chunks_ahead: default_read_prefetch_chunks_ahead(),
            auto_host_fraction_pct: default_auto_host_fraction_pct(),
            safety_max_host_fraction_pct: default_safety_max_host_fraction_pct(),
            auto_min_gb_per_tape: default_auto_min_gb_per_tape(),
            auto_max_gb_per_tape: default_auto_max_gb_per_tape(),
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

    /// Per-backend ghost-list ring size. Each entry is ~100 B
    /// (32 B BLAKE3 + 8 B timestamp + HashMap overhead); the default
    /// 100,000 sets each backend's ring to roughly 10 MB. The ring
    /// drives the `cache_miss_after_eviction_seconds` histogram — on
    /// every cache miss the chunk hash is looked up against the ring
    /// to bucket "how long ago was this evicted?" Set to `0` to
    /// disable the ring (no histogram observations).
    #[serde(default = "default_ghost_ring_size")]
    ghost_ring_size: usize,
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

fn default_ghost_ring_size() -> usize {
    100_000
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
            ghost_ring_size: default_ghost_ring_size(),
        }
    }
}

// Cloud backend configuration is shared with the CLI in core-mediachanger.
// Re-exported via aliases here so existing field/var names keep working.
use core_mediachanger::ObjectStoreConfig;

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
    /// One TCP listen portal, or a list of portals for multi-portal
    /// path redundancy. Each portal carries an iSCSI Target Portal
    /// Group Tag — operators who want grouped portals (one TPG,
    /// many paths) keep the same `tpgt`; operators preparing for
    /// ALUA give each portal its own. Bare-string entries auto-assign
    /// sequential TPGTs from their input position (1, 2, …) so the
    /// single-portal `listen: "0.0.0.0:3260"` happy path keeps
    /// TPGT=1. SendTargets discovery advertises every entry;
    /// wildcards (`0.0.0.0:*`, `[::]:*`) are substituted with the
    /// connection's actual local IP.
    #[serde(
        default = "default_iscsi_listen",
        deserialize_with = "deserialize_listen"
    )]
    listen: Vec<shared_iscsi::transport::Portal>,
    #[serde(default = "default_target_iqn")]
    target_iqn: String,
    #[serde(default = "default_max_sessions")]
    max_sessions: u32,
    #[serde(default = "default_session_timeout")]
    session_timeout_seconds: u32,
    #[serde(default)]
    auth: AuthConfig,
}

fn default_iscsi_listen() -> Vec<shared_iscsi::transport::Portal> {
    vec![shared_iscsi::transport::Portal {
        address: "0.0.0.0:3260".to_string(),
        tpgt: 1,
    }]
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

/// Accept either a single TCP address (scalar) or a list of portal
/// entries (sequence of either bare `"ip:port"` strings or
/// `{address, tpgt}` objects); normalizes both forms to
/// `Vec<Portal>`. Bare strings auto-assign `tpgt = position` (1-indexed)
/// so the single-string happy path advertises TPGT=1. Empty lists are
/// rejected — the daemon needs at least one portal to bind.
fn deserialize_listen<'de, D>(
    de: D,
) -> std::result::Result<Vec<shared_iscsi::transport::Portal>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Bare(String),
        Full { address: String, tpgt: u16 },
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<Entry>),
    }
    let entries = match OneOrMany::deserialize(de)? {
        OneOrMany::One(s) => vec![Entry::Bare(s)],
        OneOrMany::Many(v) => {
            if v.is_empty() {
                return Err(serde::de::Error::custom(
                    "iscsi.listen must contain at least one address",
                ));
            }
            v
        }
    };
    Ok(entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let position = (i as u16) + 1;
            match e {
                Entry::Bare(address) => shared_iscsi::transport::Portal {
                    address,
                    tpgt: position,
                },
                Entry::Full { address, tpgt } => shared_iscsi::transport::Portal { address, tpgt },
            }
        })
        .collect())
}

impl Default for IscsiConfig {
    fn default() -> Self {
        Self {
            listen: default_iscsi_listen(),
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
    let fmt_mem = |v: memory_buffers_size::MemoryBuffersSize| match v {
        memory_buffers_size::MemoryBuffersSize::Auto => format!(
            "auto (min {} GiB, max {} GiB per tape)",
            cfg.memory_buffers.auto_min_gb_per_tape, cfg.memory_buffers.auto_max_gb_per_tape,
        ),
        memory_buffers_size::MemoryBuffersSize::Explicit(n) => format!("{n} GiB"),
    };
    info!(
        "write memory buffer per tape: {}, read memory buffer per tape: {}, per-backend disk cache default: {}",
        fmt_mem(cfg.memory_buffers.write_gb_per_tape),
        fmt_mem(cfg.memory_buffers.read_gb_per_tape),
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
    shared_object_store::reject_legacy_cloud_backends_json(&data_dir_path, &config_path_buf)
        .map_err(anyhow::Error::msg)?;
    shared_keystore::reject_legacy_keystore_backends_json(&data_dir_path, &config_path_buf)
        .map_err(anyhow::Error::msg)?;
    cfg.storage
        .validate_backends()
        .map_err(|e| anyhow::anyhow!("validate cloud.backends in {}: {}", config_path, e))?;
    info!(
        "cloud: {} backend(s) configured",
        cfg.storage.backends.len()
    );

    let iscsi_users_path = data_dir_path.join("iscsi-users.json");
    let iscsi_users_file =
        shared_iscsi::auth::IscsiUsersFile::load_or_create_default(&iscsi_users_path)
            .map_err(|e| anyhow::anyhow!("loading {}: {}", iscsi_users_path.display(), e))?;
    info!(
        "iscsi-users.json loaded: {} user(s)",
        iscsi_users_file.users.len()
    );

    // Bring up the library: materialize from the YAML `library:`
    // block on first start, diff-and-reconcile on every subsequent
    // start. Done before cloud validation so the cartridge ↔ backend
    // referential check below has the manifest list available.
    info!("Bringing up library from YAML library: block...");
    let lib_root = std::path::PathBuf::from(&cfg.data_dir).join("library");
    let tapes_root = std::path::PathBuf::from(&cfg.data_dir).join("tapes");

    let declared = build_declared_topology(cfg.library.as_ref(), &config_path)?;
    let (library, reconcile_events) = core_mediachanger::library::reconcile::open_or_materialize(
        &lib_root,
        &tapes_root,
        &declared,
    )
    .map_err(|e| anyhow::anyhow!("Failed to bring up library: {}", e))?;

    for ev in &reconcile_events {
        if let core_mediachanger::library::reconcile::ReconcileEvent::DriveEvacuated(d) = ev {
            info!(
                "Reconcile: evacuated drive {} ({}) to origin slot {}",
                d.drive_id, d.barcode, d.origin_slot,
            );
        }
    }

    info!(
        "Library ready: {} slots, {} drives, LTO-{}",
        library.storage_slots().len(),
        library.drives().len(),
        library.lto_generation()
    );

    // 🔸 Validate every named backend up front. Sequential, bails on
    // first failure so the operator gets a focused error instead of a
    // wall of partial results.
    info!("Validating cloud backend configuration...");
    for name in cfg.storage.backend_names() {
        info!("  -> validating backend '{}'", name);
        core_mediachanger::validate_object_store_backend(&cfg.storage, &name, |step| {
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
            cfg.storage.backend_names().into_iter().collect();
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
                        "cartridge '{}' references storage backend '{}' which is not configured \
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

    // 🔸 Open the audit log. Always on — there is no enabled knob.
    // The daemon is the sole writer once started; remaining daemon-
    // down CLI flows (`library partition`, `library restore`) drop
    // their entries into `<audit_dir>/pending/` and we drain that
    // queue right after open. A broken chain refuses to start (both
    // tiers).
    let audit_log_dir = cfg.audit.dir.as_ref().map_or_else(
        || std::path::PathBuf::from(&cfg.data_dir).join("audit"),
        std::path::PathBuf::from,
    );

    // Open the audit log synchronously and run all startup-time sync
    // writes (replay queue drain, daemon.start) through the underlying
    // `Arc<AuditLog>` directly. Once those are done,
    // [`spawn_audit_writer`] takes over: every subsequent runtime
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
                // Emit reconcile audit trail: per-evacuation
                // `inventory.move_medium` rows precede a single
                // summary row (`library.materialize` on first start,
                // `library.reconcile` on subsequent diffs).
                emit_reconcile_audit(&log, &reconcile_events);
                // Stays on the sync `Arc<AuditLog>` path because the
                // channel writer task hasn't been spawned yet — this
                // entry is the only write that hits the chain mutex
                // directly before [`spawn_audit_writer`] takes over.
                Some(log)
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "audit log: failed to open at {}: {}. The daemon refuses to start \
                     until the chain is healthy. Investigate with \
                     `thurvtl system audit verify`, then either fix the underlying \
                     issue or run `thurvtl system audit rotate --accept-break` to \
                     acknowledge the break and start a fresh chain.",
                    audit_log_dir.display(),
                    e
                ));
            }
        }
    };

    // Log cloud backend configuration. One line per named entry.
    for name in cfg.storage.backend_names() {
        match cfg.storage.backend_entry(&name) {
            Ok(core_mediachanger::BackendEntry::S3(s3)) => info!(
                "Storage backend '{}': S3 (bucket={} prefix={} region={})",
                name, s3.bucket, s3.prefix, s3.region
            ),
            Ok(core_mediachanger::BackendEntry::Gcs(gcs)) => info!(
                "Storage backend '{}': GCS (bucket={} prefix={} project={})",
                name, gcs.bucket, gcs.prefix, gcs.project_id
            ),
            Ok(core_mediachanger::BackendEntry::Azure(a)) => info!(
                "Storage backend '{}': Azure (storage_account={} container={} prefix={})",
                name, a.storage_account, a.container, a.prefix
            ),
            Ok(core_mediachanger::BackendEntry::Local(l)) => {
                info!(
                    "Storage backend '{}': Local (root_dir={})",
                    name, l.root_dir
                )
            }
            Err(e) => warn!("Storage backend '{}': {}", name, e),
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
    // here on (iSCSI handlers, admin endpoints, gc) goes through the
    // bounded mpsc and the dedicated
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

    // 🔸 Build per-backend ghost lists used by the cache-miss
    // telemetry path (cache_miss_after_eviction_seconds histogram).
    // One Arc<GhostList> per backend, parallel to pool_budgets.
    // Capacity comes from disk_cache.ghost_ring_size; 0 disables.
    let ghost_lists: std::collections::HashMap<String, Arc<core_mediachanger::GhostList>> = {
        let mut map = std::collections::HashMap::new();
        for name in cfg.storage.backend_names() {
            map.insert(
                name.clone(),
                Arc::new(core_mediachanger::GhostList::new(
                    name,
                    cfg.disk_cache.ghost_ring_size,
                )),
            );
        }
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
    let ghost_lists_for_eviction = ghost_lists.clone();
    let disk_cache_worker_handle = tokio::spawn(async move {
        run_disk_cache_eviction_worker(
            &cfg_clone,
            disk_cache_evict_notify_worker,
            pool_budgets_for_eviction,
            ghost_lists_for_eviction,
        )
        .await;
    });

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

    // 🔸 Resolve per-tape memory buffers against host RAM + drive
    // count. Each field is either an explicit GB integer or `auto`;
    // auto sizes against `/proc/meminfo MemTotal` once at boot,
    // splitting `auto_host_fraction_pct` of host RAM across drives
    // and then 2:1 between write and read. Resolution is one-shot
    // (no mid-run resize), matching how `upload.max_concurrent`
    // resolves at start; full design is in issue #27.
    let host_mem_bytes = memory_buffers_size::read_host_mem_bytes();
    let bounds = memory_buffers_size::MemoryBuffersBounds {
        min_gb: cfg.memory_buffers.auto_min_gb_per_tape,
        max_gb: cfg.memory_buffers.auto_max_gb_per_tape,
    };
    let num_drives_u32 = library.drives().len() as u32;
    let write_buffer_limit = cfg.memory_buffers.write_gb_per_tape.resolve_bytes(
        host_mem_bytes,
        num_drives_u32,
        cfg.memory_buffers.auto_host_fraction_pct,
        memory_buffers_size::AUTO_WRITE_SHARE_NUM,
        memory_buffers_size::AUTO_WRITE_SHARE_DEN,
        bounds,
    );
    let read_buffer_limit = cfg.memory_buffers.read_gb_per_tape.resolve_bytes(
        host_mem_bytes,
        num_drives_u32,
        cfg.memory_buffers.auto_host_fraction_pct,
        memory_buffers_size::AUTO_READ_SHARE_NUM,
        memory_buffers_size::AUTO_READ_SHARE_DEN,
        bounds,
    );
    let bytes_per_gib = 1024u64 * 1024 * 1024;
    let write_source = if cfg.memory_buffers.write_gb_per_tape.is_auto() {
        "auto-detected from /proc/meminfo"
    } else {
        "operator override"
    };
    let read_source = if cfg.memory_buffers.read_gb_per_tape.is_auto() {
        "auto-detected from /proc/meminfo"
    } else {
        "operator override"
    };
    info!(
        "memory_buffers resolved: write={} GiB ({}), read={} GiB ({}); host_mem={} GiB, drives={}",
        write_buffer_limit / bytes_per_gib,
        write_source,
        read_buffer_limit / bytes_per_gib,
        read_source,
        host_mem_bytes / bytes_per_gib,
        num_drives_u32,
    );

    // Safety check: the resolved per-drive total times num_drives
    // must not exceed safety_max_host_fraction_pct of MemTotal.
    // Auto resolution can't exceed this by construction (auto fraction
    // <= safety fraction in default config), so this catches explicit
    // operator overrides that overcommit on a small host.
    if host_mem_bytes > 0 {
        let total_footprint =
            (write_buffer_limit + read_buffer_limit).saturating_mul(num_drives_u32.max(1) as u64);
        let safety_fraction = cfg.memory_buffers.safety_max_host_fraction_pct.min(100);
        let safety_limit = host_mem_bytes.saturating_mul(safety_fraction) / 100;
        if total_footprint > safety_limit {
            anyhow::bail!(
                "memory_buffers total footprint ({} GiB = (write {} + read {}) GiB * {} drives) \
                 exceeds safety_max_host_fraction_pct={}% of host RAM ({} GiB of {} GiB). \
                 Lower memory_buffers.write_gb_per_tape / read_gb_per_tape, raise \
                 memory_buffers.safety_max_host_fraction_pct, or switch to `auto`.",
                total_footprint / bytes_per_gib,
                write_buffer_limit / bytes_per_gib,
                read_buffer_limit / bytes_per_gib,
                num_drives_u32,
                safety_fraction,
                safety_limit / bytes_per_gib,
                host_mem_bytes / bytes_per_gib,
            );
        }
    }

    // 🔸 Start MemoryBufferManager (Phase 3: Per-Tape Buffer Tracking, Phase 4: Event-Driven Uploads, Phase 5: Event-Driven Prefetch)
    // Clone the upload sender before passing it into the manager so the
    // boot-time orphan-upload scan can dispatch directly to the same
    // worker mpsc without going through the manager's event loop.
    let upload_tx_for_recovery = upload_tx.clone();
    let memory_buffer_manager = MemoryBufferManager::new(
        event_rx,
        write_buffer_limit,
        read_buffer_limit,
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
    let cloud_backends_registry: iscsi::server::ObjectStoreRegistry =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let storage_config_arc = std::sync::Arc::new(cfg.storage.clone());
    let keystore_config_arc = std::sync::Arc::new(cfg.keystore.clone());
    let backpressure_max_wait =
        std::time::Duration::from_secs(cfg.storage.upload.backpressure_max_wait_seconds.into());

    let library_arc = std::sync::Arc::new(std::sync::Mutex::new(library));

    let daemon_state = std::sync::Arc::new(state::DaemonState::new(state::DaemonStateConfig {
        data_dir: std::path::PathBuf::from(&cfg.data_dir),
        tapes_root: tapes_root.clone(),
        library: std::sync::Arc::clone(&library_arc),
        element_config,
        target_iqn: iscsi_cfg.target_iqn.clone(),
        listen_addresses: iscsi_cfg.listen.iter().map(|p| p.address.clone()).collect(),
        event_tx: event_tx.clone(),
        audit_log: audit_log.clone(),
        audit_dir: audit_log_dir.clone(),
        audit_ratelimiter: std::sync::Arc::clone(&audit_ratelimiter),
        cloud_backends: std::sync::Arc::clone(&cloud_backends_registry),
        storage_config: std::sync::Arc::clone(&storage_config_arc),
        keystore_config: std::sync::Arc::clone(&keystore_config_arc),
        num_drives: lib_drives as usize,
        drive_compression_algorithm: drive_cfg.compression.algorithm,
        drive_compression_zstd_level: drive_cfg.compression.zstd_level,
        pool_budgets: pool_budgets.clone(),
        ghost_lists: ghost_lists.clone(),
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
        info!(
            "Starting iSCSI target server on {}",
            iscsi_cfg
                .listen
                .iter()
                .map(|p| format!("{},tpgt={}", p.address, p.tpgt))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Convert daemon's config to iscsi module's config
        let iscsi_config = iscsi::config::IscsiConfig {
            iscsi: iscsi::config::IscsiSettings {
                listen_portals: iscsi_cfg.listen.clone(),
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
    ghost_lists: std::collections::HashMap<String, Arc<core_mediachanger::GhostList>>,
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

    let backend_names: Vec<String> = cfg.storage.backend_names();
    // Budget divergence detector cadence — see `reconcile` below. The
    // worker wakes on every upload-completion notify (frequent under
    // load), so the reconcile is gated on elapsed wall-time, not wakeup
    // count, to keep the full walk to ~once per hour regardless.
    let mut last_reconcile = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = notify.notified() => {
                // Coalesce additional notifies from the same upload
                // batch before doing the eviction pass.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            _ = backstop.tick() => {}
        }

        // Recompute per-backend caps for `auto`-mode entries against
        // current free space and push the new ceilings into each
        // backend's PoolBudget. Shared with VSA — see
        // `shared_disk_evict::resolve_and_apply_caps`.
        shared_disk_evict::resolve_and_apply_caps(
            &backend_names,
            &pool_budgets,
            &cfg.storage,
            &data_dir,
            cfg.disk_cache.size_gb,
            cfg.disk_cache.bounds(),
        );

        // Once per `BUDGET_RECONCILE_INTERVAL`, do one full-pool walk per
        // backend purely to confirm the O(1) budget-derived usage still
        // matches on-disk reality, warning if an un-instrumented mutation
        // site leaked drift (#49). The only place the removed per-tick
        // walk still happens — far less often than the eviction wakeup.
        let reconcile = last_reconcile.elapsed() >= shared_disk_evict::BUDGET_RECONCILE_INTERVAL;
        if reconcile {
            last_reconcile = std::time::Instant::now();
        }

        // Per-backend DiskCacheManagers. Each manager is scoped to one
        // named backend and evicts down to *that backend's* cap (read
        // from the per-backend PoolBudget the construction phase built,
        // possibly re-resolved above for `auto`-mode entries this
        // tick). Per-backend pools are sharded under
        // `<data_dir>/chunks/<backend>/`, so identical hashes in two
        // backends are physically distinct files — per-backend LRU is
        // also globally correct.
        let managers: Vec<DiskCacheManager> = backend_names
            .iter()
            .map(|name| {
                let cap = pool_budgets.get(name).map(|b| b.cap_bytes()).unwrap_or(0);
                let mut cm = DiskCacheManager::new(data_dir.clone(), name, cap);
                if let Some(budget) = pool_budgets.get(name) {
                    cm.set_pool_budget(budget.clone());
                }
                if let Some(gl) = ghost_lists.get(name) {
                    cm.set_ghost_list(gl.clone());
                }
                cm.set_recent_seal_pin_seconds(cfg.disk_cache.recent_seal_pin_seconds);
                cm
            })
            .collect();

        for mut cm in managers {
            let backend = cm.backend_name().to_string();
            let cap = cm.capacity();
            let budget = pool_budgets.get(&backend).cloned();
            let has_budget = budget.is_some();
            // Per-tick usage is now an O(1) budget read plus a cheap
            // staging walk, not a full O(total-chunks) pool rescan (#49).
            // The budget is exact across every pool mutation site
            // (seal / eviction / GC / read-miss refetch / prefetch /
            // migrate); the only term it omits is pre-seal `.staging/`
            // bytes, which `staging_bytes` (O(num_cartridges)) adds back.
            // Defensive fallback: if no budget is wired for this backend
            // (shouldn't happen in the daemon), fall back to the old full
            // `calculate_usage` walk so behavior is preserved exactly.
            // The blocking fs work is offloaded to a blocking thread; the
            // manager is handed back out.
            let (cm_ret, fs_res) = match tokio::task::spawn_blocking(move || {
                let r = if has_budget {
                    cm.staging_bytes()
                } else {
                    cm.calculate_usage()
                };
                (cm, r)
            })
            .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("Cache usage task for backend '{}' panicked: {e}", backend);
                    continue;
                }
            };
            cm = cm_ret;
            let fs_val = match fs_res {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        "Cache usage calculation for backend '{}' failed: {e}",
                        backend
                    );
                    continue;
                }
            };
            // With a budget: usage = budget.current_bytes() + staging.
            // Without: `fs_val` is the full-walk total already.
            let used = match budget.as_ref() {
                Some(b) => b.current_bytes().saturating_add(fs_val),
                None => fs_val,
            };
            cm.set_current_usage(used);
            // Low-cadence safety reconcile: a full walk's total includes
            // staging, so it lines up with `used` (budget + staging).
            // Warn if they diverge beyond tolerance (detection only).
            if reconcile && budget.is_some() {
                let mut cm_r = DiskCacheManager::new(data_dir.clone(), &backend, cap);
                if let Ok(Ok(actual)) =
                    tokio::task::spawn_blocking(move || cm_r.calculate_usage()).await
                {
                    shared_disk_evict::warn_on_budget_divergence(&backend, used, actual);
                }
            }
            // Within-budget log + soft-watermark alert (shared with VSA).
            let needs_evict = match budget.as_ref() {
                Some(b) => shared_disk_evict::check_usage_or_alert(&backend, used, cap, b),
                None => used > cap,
            };
            if !needs_evict {
                continue;
            }

            // Build a fresh cloud backend for the eviction pass.
            // Eviction is rare; the construction cost is negligible.
            let cloud_backend = match cfg.storage.create_backend_named(&backend).await {
                Ok(b) => Some(b),
                Err(e) => {
                    warn!(
                        "Cache eviction: backend '{}' init failed ({e}); \
                         evicting without cloud backup",
                        backend
                    );
                    None
                }
            };

            match cm.evict_lru_chunks(cloud_backend.as_deref()).await {
                Ok(freed) if freed > 0 => {
                    info!(
                        "Cache eviction freed {} bytes from backend '{}'",
                        freed, backend
                    );
                }
                Ok(_) => {}
                Err(e) => warn!("Cache eviction for backend '{}' failed: {e}", backend),
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
        // ObjectStoreConfig's `backends: BTreeMap<...>` field.
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

    fn portal(addr: &str, tpgt: u16) -> shared_iscsi::transport::Portal {
        shared_iscsi::transport::Portal {
            address: addr.to_string(),
            tpgt,
        }
    }

    #[test]
    fn iscsi_listen_scalar_form_deserializes_into_single_entry() {
        let yaml = "listen: \"10.0.0.5:3260\"\n";
        let cfg: IscsiConfig = serde_yaml::from_str(yaml).expect("scalar form parses");
        assert_eq!(cfg.listen, vec![portal("10.0.0.5:3260", 1)]);
    }

    #[test]
    fn iscsi_listen_list_form_deserializes_in_order() {
        // Bare strings auto-assign sequential TPGTs from their input
        // position (1-indexed).
        let yaml = "listen:\n  - \"10.0.0.5:3260\"\n  - \"10.0.0.6:3260\"\n";
        let cfg: IscsiConfig = serde_yaml::from_str(yaml).expect("list form parses");
        assert_eq!(
            cfg.listen,
            vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 2)]
        );
    }

    #[test]
    fn iscsi_listen_object_form_carries_explicit_tpgt() {
        let yaml = "listen:\n  - { address: \"10.0.0.5:3260\", tpgt: 5 }\n  - { address: \"10.0.0.6:3260\", tpgt: 9 }\n";
        let cfg: IscsiConfig = serde_yaml::from_str(yaml).expect("object form parses");
        assert_eq!(
            cfg.listen,
            vec![portal("10.0.0.5:3260", 5), portal("10.0.0.6:3260", 9)]
        );
    }

    #[test]
    fn iscsi_listen_object_form_allows_shared_tpgt_for_group() {
        // Multiple portals sharing one TPGT is legal (one TPG, many
        // paths) — the group surface is the prerequisite for ALUA.
        let yaml = "listen:\n  - { address: \"10.0.0.5:3260\", tpgt: 1 }\n  - { address: \"10.0.0.6:3260\", tpgt: 1 }\n";
        let cfg: IscsiConfig = serde_yaml::from_str(yaml).expect("shared-TPGT form parses");
        assert_eq!(
            cfg.listen,
            vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 1)]
        );
    }

    #[test]
    fn iscsi_listen_missing_takes_default() {
        let yaml = "target_iqn: \"iqn.test:tgt\"\n";
        let cfg: IscsiConfig = serde_yaml::from_str(yaml).expect("default fires");
        assert_eq!(cfg.listen, vec![portal("0.0.0.0:3260", 1)]);
    }

    #[test]
    fn iscsi_listen_empty_list_is_rejected() {
        let yaml = "listen: []\n";
        let err = serde_yaml::from_str::<IscsiConfig>(yaml).unwrap_err();
        assert!(
            err.to_string().contains("at least one"),
            "want 'at least one' in error, got: {err}"
        );
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
        // Both buffer sizes default to `auto` — the daemon resolves
        // them against /proc/meminfo at boot.
        assert_eq!(
            cfg.memory_buffers.write_gb_per_tape,
            memory_buffers_size::MemoryBuffersSize::Auto
        );
        assert_eq!(
            cfg.memory_buffers.read_gb_per_tape,
            memory_buffers_size::MemoryBuffersSize::Auto
        );
        assert_eq!(
            cfg.memory_buffers.auto_host_fraction_pct,
            default_auto_host_fraction_pct()
        );
        assert_eq!(
            cfg.memory_buffers.safety_max_host_fraction_pct,
            default_safety_max_host_fraction_pct()
        );
        assert_eq!(
            cfg.memory_buffers.auto_min_gb_per_tape,
            default_auto_min_gb_per_tape()
        );
        assert_eq!(
            cfg.memory_buffers.auto_max_gb_per_tape,
            default_auto_max_gb_per_tape()
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

    #[test]
    fn build_declared_topology_missing_block_names_config_path() {
        let err = build_declared_topology(None, "/etc/thurvtl/thurvtl.yaml")
            .expect_err("missing library: block must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("library: block missing"), "msg={msg}");
        assert!(msg.contains("/etc/thurvtl/thurvtl.yaml"), "msg={msg}");
    }

    #[test]
    fn build_declared_topology_collects_all_missing_fields_at_once() {
        // The whole point: operators get all three missing-field
        // names on one shot, not "fix one, re-run, see the next."
        let block = LibraryConfig {
            num_slots: None,
            num_drives: None,
            lto_generation: None,
            firmware: None,
        };
        let err = build_declared_topology(Some(&block), "/tmp/c.yaml")
            .expect_err("all-missing must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("library.num_slots"), "msg={msg}");
        assert!(msg.contains("library.num_drives"), "msg={msg}");
        assert!(msg.contains("library.lto_generation"), "msg={msg}");
    }

    #[test]
    fn build_declared_topology_reports_only_missing_field() {
        // Only num_drives missing → error mentions just that field,
        // not the two already-set ones.
        let block = LibraryConfig {
            num_slots: Some(40),
            num_drives: None,
            lto_generation: Some(8),
            firmware: None,
        };
        let err = build_declared_topology(Some(&block), "/tmp/c.yaml").expect_err("partial");
        let msg = format!("{err:#}");
        assert!(msg.contains("library.num_drives"), "msg={msg}");
        assert!(!msg.contains("library.num_slots"), "msg={msg}");
        assert!(!msg.contains("library.lto_generation"), "msg={msg}");
    }

    #[test]
    fn build_declared_topology_fully_specified_block_returns_topology() {
        let block = LibraryConfig {
            num_slots: Some(40),
            num_drives: Some(3),
            lto_generation: Some(8),
            firmware: Some("TVL8".to_string()),
        };
        let topo = build_declared_topology(Some(&block), "/tmp/c.yaml")
            .expect("fully specified must succeed");
        assert_eq!(topo.num_storage_slots, 40);
        assert_eq!(topo.num_drives, 3);
        assert_eq!(topo.lto_generation, 8);
        assert_eq!(topo.firmware.as_deref(), Some("TVL8"));
    }
}
