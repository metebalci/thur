// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! thurvsad configuration loader.
//!
//! Reads `/etc/thurvsa/thurvsa.yaml` (or `--config PATH`) and parses
//! the top-level keys thurvsad needs to boot:
//!
//! - `data_dir`: filesystem root for `<data_dir>/volumes/<name>/`
//!   manifests + page indexes and the per-backend chunk pool under
//!   `<data_dir>/chunks/<backend>/...`. Symmetric to thurvtl's
//!   `data_dir`.
//! - `cloud`: the shared `ObjectStoreConfig` schema (named `backends:`
//!   map + retry / compression knobs) consumed by `shared-cloud`'s
//!   backend constructors.
//! - `iscsi.auth`: optional CHAP block. Schema mirrors thurvtl's
//!   `iscsi.auth` (enabled / method / target_username /
//!   target_password / allowed_algorithms / users[]) minus the
//!   per-user `partition` field — thurvsa has no library topology /
//!   partition concept.
//! - `audit`: minimal JSONL audit log (daily-rotating). Default
//!   directory is `<data_dir>/audit/` when unset.
//!
//! Validation runs `ObjectStoreConfig::validate()` so an empty backend
//! map or a misconfigured Azure-WORM entry surfaces here, not later
//! when the first volume tries to open. Connectivity probes (the
//! list/write/delete dance from `validate_object_store_backend`) happen at
//! discovery time on a per-volume basis — the daemon doesn't sweep
//! every backend at boot since most runs reference only one or two.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use shared_object_store::ObjectStoreConfig;

/// Default location for thurvsad's config. Sourced from
/// [`shared_naming::DISK`] so the CLI and daemon agree on the path
/// without duplicating the literal.
pub const DEFAULT_CONFIG_PATH: &str = shared_naming::DISK.config_path;

/// Top-level thurvsad config. Extra YAML keys are ignored —
/// future fields can be added here without breaking older daemons
/// that don't yet understand them.
#[derive(Debug, Deserialize)]
pub struct DaemonConfig {
    pub data_dir: String,
    /// Host-facing wire transports. One daemon exports every listed
    /// transport concurrently against the shared volume set + shared
    /// reservation state, so a volume is reachable as a SCSI LUN and
    /// an NVMe namespace (`nsid = lun + 1`) at the same time. Accepts
    /// a bare scalar (`transports: iscsi`) or a list
    /// (`transports: [iscsi, nvmetcp]`). Defaults to `[iscsi]`; an
    /// empty list is rejected and duplicates are de-duped.
    #[serde(
        default = "default_transports",
        deserialize_with = "deserialize_transports"
    )]
    pub transports: Vec<Transport>,
    #[serde(default)]
    pub storage: ObjectStoreConfig,
    #[serde(default)]
    pub iscsi: IscsiSettings,
    /// NVMe/TCP listener settings. Only consulted when `nvmetcp` is
    /// listed in `transports`. Single tunable today (the listen
    /// address); auth + NQN overrides land alongside the protocol
    /// implementation in a follow-up.
    #[serde(default)]
    pub nvmetcp: NvmetcpSettings,
    #[serde(default)]
    pub http: HttpSettings,
    #[serde(default)]
    pub audit: AuditSettings,
    #[serde(default)]
    pub keystore: shared_keystore::KeystoreYamlConfig,
    /// Shared content-addressed chunk-pool budget on disk. See
    /// [`DiskCacheSettings`]. Mirrors thurvtl's `disk_cache:` block.
    #[serde(default)]
    pub disk_cache: DiskCacheSettings,
    /// First-party alerting (email + generic webhook). Off by
    /// default; opt in by setting `alerting.enabled: true` and
    /// listing at least one sink. Schema in `shared-alerting`.
    #[serde(default)]
    pub alerting: shared_alerting::AlertingConfig,
}

/// `disk_cache:` block. Per-backend hard cap on local pool
/// occupancy at `<data_dir>/chunks/<backend>/...`. Chunk-seal
/// applies upload backpressure (SBC-3 NOT READY + retry) when the
/// backend's pool would exceed the cap. Eviction (LRU over the
/// per-volume `lru.idx` sidecar) and successful uploads are what
/// create headroom.
///
/// Mirrors thurvtl's `disk_cache:` block byte-for-byte — both
/// products share the same `shared_pool::PoolBudget` primitive and
/// the same `disk_cache_size_gb` per-entry override on
/// `cloud-backends.json`.
#[derive(Debug, Deserialize, Clone)]
pub struct DiskCacheSettings {
    /// Default per-backend disk-cache budget. Either an explicit GB
    /// integer or the literal string `auto` (the default): under
    /// `auto`, the eviction worker statvfs's `data_dir` on every
    /// tick and pins the cap to `min(50% of free, max_size_gb)`,
    /// floored at `min_size_gb`. Multi-backend installs with several
    /// `auto` entries split the 50%-of-free share evenly so two
    /// `auto` backends can't combined commit 100% of free space.
    /// Individual `cloud-backends.json` entries may override per-
    /// entry via their own `disk_cache_size_gb` field — same shape
    /// (`auto | <gb>`).
    #[serde(default)]
    pub size_gb: core_block::DiskCacheSize,
    /// Floor (GB) for the `auto`-derived cap. Honored only when
    /// `size_gb: auto`; explicit values ignore both bounds (operator
    /// chose). Matches the pre-`auto` default.
    #[serde(default = "default_min_size_gb")]
    pub min_size_gb: u64,
    /// Ceiling (GB) for the `auto`-derived cap. Honored only when
    /// `size_gb: auto`. Bounds the eviction-worker scan cost on
    /// very large filesystems.
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: u64,
    /// Soft watermark as a percentage of `size_gb`. Crossing it
    /// fires a warn-level log + bumps the
    /// `thurvsa_pool_used_bytes` / `thurvsa_pool_cap_bytes` gauges
    /// the operator's dashboard already watches. Range 1-100.
    #[serde(default = "default_localonly_soft_watermark_pct")]
    pub localonly_soft_watermark_pct: u8,
    /// Reserve of free filesystem bytes (GB) below which chunk-seal
    /// also backpressures, regardless of pool occupancy. Catches
    /// disk-fill from sources outside the pool. Set to 0 to disable.
    #[serde(default = "default_disk_free_min_gb")]
    pub disk_free_min_gb: u64,
    /// Max time a page-seal will park on backpressure before
    /// surfacing SBC-3 NOT READY + ASC/ASCQ 0x04/0x07. Host backup
    /// software treats that as transient and retries. Tune up only
    /// if the eviction worker's recovery latency outruns the
    /// cloud's PUT-then-HEAD cadence.
    #[serde(default = "default_backpressure_max_wait_seconds")]
    pub backpressure_max_wait_seconds: u64,
    /// How often the eviction worker re-scans every backend's pool
    /// and trims down to cap. Tighter intervals catch read-miss
    /// downloads faster; looser intervals keep the per-volume
    /// `pages.idx` walks off the hot path. Default 5 minutes
    /// matches the VTL `disk_cache_eviction_interval_seconds`
    /// backstop tick.
    #[serde(default = "default_eviction_interval_seconds")]
    pub eviction_interval_seconds: u64,
    /// Pin pool chunks whose most recent `lru.idx` touch (write
    /// OR read) is within this many seconds against LRU eviction.
    /// Counters the verify-after-write pattern (Veeam / NetBackup
    /// re-read freshly-written data within seconds), at the cost
    /// of capping effective cache capacity by the volume of recent
    /// writes-plus-reads. Default 0 disables the pin and restores
    /// pure LRU — see [`ROADMAP.md`] § Pin recent sealed chunks for
    /// the RC/GA validation task.
    #[serde(default = "default_recent_seal_pin_seconds")]
    pub recent_seal_pin_seconds: u64,

    /// Per-backend ghost-list ring size. Each entry is ~100 B
    /// (32 B BLAKE3 + 8 B timestamp + HashMap overhead); the default
    /// 100,000 sets each backend's ring to roughly 10 MB. The ring
    /// drives the `cache_miss_after_eviction_seconds` histogram — on
    /// every cache miss the chunk hash is looked up against the ring
    /// to bucket "how long ago was this evicted?" Set to `0` to
    /// disable the ring (no histogram observations).
    #[serde(default = "default_ghost_ring_size")]
    pub ghost_ring_size: usize,
}

fn default_min_size_gb() -> u64 {
    core_block::DiskCacheBounds::DEFAULT.min_gb
}

fn default_max_size_gb() -> u64 {
    core_block::DiskCacheBounds::DEFAULT.max_gb
}

fn default_localonly_soft_watermark_pct() -> u8 {
    80
}

fn default_disk_free_min_gb() -> u64 {
    5
}

fn default_backpressure_max_wait_seconds() -> u64 {
    30
}

fn default_eviction_interval_seconds() -> u64 {
    300
}

fn default_recent_seal_pin_seconds() -> u64 {
    0
}

fn default_ghost_ring_size() -> usize {
    100_000
}

impl Default for DiskCacheSettings {
    fn default() -> Self {
        Self {
            size_gb: core_block::DiskCacheSize::default(),
            min_size_gb: default_min_size_gb(),
            max_size_gb: default_max_size_gb(),
            localonly_soft_watermark_pct: default_localonly_soft_watermark_pct(),
            disk_free_min_gb: default_disk_free_min_gb(),
            backpressure_max_wait_seconds: default_backpressure_max_wait_seconds(),
            eviction_interval_seconds: default_eviction_interval_seconds(),
            recent_seal_pin_seconds: default_recent_seal_pin_seconds(),
            ghost_ring_size: default_ghost_ring_size(),
        }
    }
}

impl DiskCacheSettings {
    pub fn bounds(&self) -> core_block::DiskCacheBounds {
        core_block::DiskCacheBounds {
            min_gb: self.min_size_gb,
            max_gb: self.max_size_gb,
        }
    }
}

/// Host-facing wire transport. `iscsi` binds the iSCSI listener and
/// routes through `shared-iscsi` + `scsi-sbc`. `nvmetcp` binds the
/// NVMe/TCP listener and routes through `nvme-tcp` + `nvme-nvm`. Both
/// may be listed in `transports` — they bind concurrently against the
/// shared volume set + reservation state (their default ports don't
/// collide: 3260 vs 4420).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Iscsi,
    Nvmetcp,
}

/// Default `transports` when the key is omitted: iSCSI only (the
/// historical single-transport default).
fn default_transports() -> Vec<Transport> {
    vec![Transport::Iscsi]
}

/// Accept either a bare scalar (`transports: iscsi`) or a list
/// (`transports: [iscsi, nvmetcp]`); normalize both to a
/// `Vec<Transport>`. An empty list is rejected — omit the key for the
/// default. Duplicates are de-duped, first occurrence kept (order
/// preserved). Mirrors `deserialize_listen_opt`'s scalar-or-list shape.
fn deserialize_transports<'de, D>(de: D) -> std::result::Result<Vec<Transport>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Transport),
        Many(Vec<Transport>),
    }
    let raw = match OneOrMany::deserialize(de)? {
        OneOrMany::One(t) => vec![t],
        OneOrMany::Many(v) => v,
    };
    if raw.is_empty() {
        return Err(serde::de::Error::custom(
            "transports must list at least one of [iscsi, nvmetcp]",
        ));
    }
    let mut seen = Vec::with_capacity(raw.len());
    for t in raw {
        if !seen.contains(&t) {
            seen.push(t);
        }
    }
    Ok(seen)
}

/// `nvmetcp:` block. Only consulted when `nvmetcp` is listed in
/// `transports`. Defaults are operationally safe: bind 0.0.0.0:4420
/// (the IANA-registered NVMe/TCP port; doesn't collide with iSCSI's
/// 3260, so the two bind concurrently with no override).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct NvmetcpSettings {
    /// Override the NVMe/TCP TCP listen address. Defaults to
    /// `0.0.0.0:4420` (IANA-registered nvme-tcp port) when unset.
    pub listen: Option<String>,
    /// Override the NVMe Subsystem NQN advertised to hosts. Defaults
    /// to `shared_naming::DISK.nqn` (`nqn.2025-10.com.metebalci:thurvsa`)
    /// when unset. A host's `nvme connect --nqn=` must match this
    /// exactly; the TLS-PSK derivation also binds to it, so changing
    /// it rederives every per-host PSK.
    pub subnqn: Option<String>,
    /// Wire encryption + auth. Default `disabled` keeps the existing
    /// cleartext behavior; flip to `psk` for TLS 1.3 with pre-shared
    /// keys per NVMe-TCP §3.6.1.5 (see [`docs/AUTH.md`]
    /// § NVMe/TCP TLS-PSK).
    #[serde(default)]
    pub tls: NvmetcpTlsSettings,
    /// DH-HMAC-CHAP in-band host authentication (NVMe Base §8.13).
    /// Default `none`; `dhchap` requires every host to pass a
    /// challenge/response before any command. Orthogonal to `tls`:
    /// `auth.mode: dhchap` with `tls.mode: psk` runs DH-HMAC-CHAP
    /// inside a TLS-PSK channel ("dhchap+tls"). See [`docs/AUTH.md`]
    /// § NVMe/TCP DH-HMAC-CHAP.
    #[serde(default)]
    pub auth: NvmetcpAuthSettings,
    /// Discovery controller (`nvme discover` / `nvme connect-all`).
    /// Default on: a cleartext, unauthenticated listener on
    /// `0.0.0.0:8009` answers the well-known discovery NQN and lists
    /// this subsystem.
    #[serde(default)]
    pub discovery: NvmetcpDiscoverySettings,
}

impl NvmetcpSettings {
    /// Resolved TLS-PSK identity-file path: the `tls.identity_file`
    /// override if set, else the `<data_dir>/nvmetcp-psks.json` default.
    /// Resolved once at boot so the transport listener and the
    /// `nvmetcp psks` admin handlers agree on the path (issue #69).
    pub fn psks_path(&self, data_dir: &Path) -> PathBuf {
        self.tls
            .identity_file
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("nvmetcp-psks.json"))
    }

    /// Resolved DH-HMAC-CHAP identity-file path: the `auth.identity_file`
    /// override if set, else the `<data_dir>/nvmetcp-dhchap.json`
    /// default. Resolved once at boot so the transport listener and the
    /// `nvmetcp dhchap` admin handlers agree on the path (issue #69).
    pub fn dhchap_path(&self, data_dir: &Path) -> PathBuf {
        self.auth
            .identity_file
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("nvmetcp-dhchap.json"))
    }
}

/// `nvmetcp.tls:` block. Mode-selector + identity-file path. The
/// PSK material itself lives in the identity file (default
/// `<data_dir>/nvmetcp-psks.json`, daemon-managed), not in YAML —
/// same split iSCSI uses (`iscsi.auth.method` here, CHAP creds in
/// `<data_dir>/iscsi-users.json`).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct NvmetcpTlsSettings {
    #[serde(default)]
    pub mode: NvmetcpTlsMode,
    /// Override the identity-file path. Defaults to
    /// `<data_dir>/nvmetcp-psks.json` when unset.
    pub identity_file: Option<String>,
}

/// TLS mode for the NVMe/TCP listener.
///
/// - `Disabled` (default): plain TCP. Every byte clear on the wire.
/// - `Psk`: TLS 1.3 with the two NVMe-TCP §3.6.1.5 mandated cipher
///   suites (`TLS_AES_128_GCM_SHA256` / `TLS_AES_256_GCM_SHA384`).
///   Per-host PSKs loaded from the identity file at boot — restart
///   to reload, same as `iscsi-users.json`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NvmetcpTlsMode {
    #[default]
    Disabled,
    Psk,
}

/// `nvmetcp.auth:` block. Mode-selector + identity-file path for
/// DH-HMAC-CHAP in-band authentication. The `DHHC-1:` secrets live in
/// the identity file (default `<data_dir>/nvmetcp-dhchap.json`,
/// daemon-managed via `thurvsa nvmetcp dhchap`), not in YAML — same
/// split as `iscsi.auth.method` + `iscsi-users.json`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct NvmetcpAuthSettings {
    #[serde(default)]
    pub mode: NvmetcpAuthMode,
    /// Override the identity-file path. Defaults to
    /// `<data_dir>/nvmetcp-dhchap.json` when unset.
    pub identity_file: Option<String>,
}

/// In-band authentication mode for the NVMe/TCP listener.
///
/// - `None` (default): no in-band auth. Combined with `tls.mode:
///   disabled` this is the legacy cleartext, see-everything behavior.
/// - `Dhchap`: DH-HMAC-CHAP (NVMe Base §8.13). Every Connect asserts
///   AUTHREQ and the host must complete Authentication Send/Receive
///   before any command. Per-host secrets + volume admission load from
///   the identity file on every handshake — `nvmetcp dhchap` edits take
///   effect on the next session without restart.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NvmetcpAuthMode {
    #[default]
    None,
    Dhchap,
}

/// Default Discovery controller listen address — the IANA-registered
/// NVMe discovery port. Doesn't collide with the I/O listener (4420)
/// or iSCSI (3260).
pub const DEFAULT_NVMETCP_DISCOVERY_LISTEN_ADDRESS: &str = "0.0.0.0:8009";

/// `nvmetcp.discovery:` block. A direct Discovery controller answering
/// the well-known NQN `nqn.2014-08.org.nvmexpress.discovery`, so
/// `nvme discover` / `nvme connect-all` work without out-of-band
/// distribution of the SUBNQN / address / port. The listener is
/// always cleartext + unauthenticated (the spec/industry default and
/// the analog of unauthenticated iSCSI SendTargets); the Discovery Log
/// record advertises the I/O subsystem's TLS requirement so hosts use
/// TLS for the real Connect.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct NvmetcpDiscoverySettings {
    /// Enable the Discovery controller listener. Defaults to **on**
    /// when the `nvmetcp` transport is enabled.
    pub enabled: Option<bool>,
    /// Override the Discovery listen address. Defaults to
    /// [`DEFAULT_NVMETCP_DISCOVERY_LISTEN_ADDRESS`] (`0.0.0.0:8009`).
    pub listen: Option<String>,
}

impl NvmetcpDiscoverySettings {
    /// Whether to bind the Discovery listener (default on).
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Resolved Discovery listen address (override or the 8009 default).
    pub fn listen_addr(&self) -> String {
        self.listen
            .clone()
            .unwrap_or_else(|| DEFAULT_NVMETCP_DISCOVERY_LISTEN_ADDRESS.to_string())
    }
}

/// `iscsi:` block. Carries the optional CHAP `auth` sub-block, a
/// tunable `listen` portal (or list of portals), and an optional
/// `target_iqn` override.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct IscsiSettings {
    /// Override the iSCSI TCP listen portal(s). Accepts:
    ///
    /// - a single `"ip:port"` scalar;
    /// - a list of bare `"ip:port"` strings (auto-assign sequential
    ///   TPGTs from input position, 1-indexed);
    /// - a list of `{address, tpgt}` objects (operator-controlled
    ///   Target Portal Group Tag — the prerequisite for ALUA);
    /// - or a mix (per-entry).
    ///
    /// Defaults to `[{address: "0.0.0.0:3260", tpgt: 1}]` when unset.
    /// Each entry binds its own listener; SendTargets advertises every
    /// entry as `TargetAddress=<address>,<tpgt>`, with wildcards
    /// (`0.0.0.0:*`, `[::]:*`) substituted by the connection's actual
    /// local IP. Test scripts assign an ephemeral port here so
    /// concurrent runs don't fight over a fixed bind.
    #[serde(default, deserialize_with = "deserialize_listen_opt")]
    pub listen: Option<Vec<shared_iscsi::transport::Portal>>,
    /// Override the iSCSI target IQN advertised to initiators in the
    /// Login / SendTargets response. Defaults to
    /// `shared_naming::DISK.iqn` (`iqn.2025-10.com.metebalci:thurvsa`)
    /// when unset.
    pub target_iqn: Option<String>,
    #[serde(default)]
    pub auth: AuthSettings,
    /// Persistent-reservation tuning (issue #57). `initiator_port`
    /// selects whether reservations key by the full iSCSI port
    /// (IQN + ISID, default) or by IQN alone.
    #[serde(default)]
    pub reservations: shared_iscsi::transport::ReservationSettings,
}

/// Accept either a single TCP address (scalar) or a list of portal
/// entries (sequence of either bare `"ip:port"` strings or
/// `{address, tpgt}` objects); normalizes both forms to
/// `Option<Vec<Portal>>`. Bare strings auto-assign `tpgt = position`
/// (1-indexed) so a single-string `listen: "0.0.0.0:3260"` advertises
/// TPGT=1. Empty lists are rejected — pick "unset" (omit the key)
/// for the default.
fn deserialize_listen_opt<'de, D>(
    de: D,
) -> std::result::Result<Option<Vec<shared_iscsi::transport::Portal>>, D::Error>
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
    let entries = match Option::<OneOrMany>::deserialize(de)? {
        None => return Ok(None),
        Some(OneOrMany::One(s)) => vec![Entry::Bare(s)],
        Some(OneOrMany::Many(v)) => {
            if v.is_empty() {
                return Err(serde::de::Error::custom(
                    "iscsi.listen must contain at least one address",
                ));
            }
            v
        }
    };
    Ok(Some(
        entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let position = (i as u16) + 1;
                match e {
                    Entry::Bare(address) => shared_iscsi::transport::Portal {
                        address,
                        tpgt: position,
                    },
                    Entry::Full { address, tpgt } => {
                        shared_iscsi::transport::Portal { address, tpgt }
                    }
                }
            })
            .collect(),
    ))
}

/// `http:` block. Carries the management HTTP listen address
/// (defaults to `0.0.0.0:9090` when unset) and the optional TLS
/// trio. When `tls.cert_file` + `tls.key_file` are both set and
/// both files are absent, the daemon auto-generates a self-signed
/// pair on first boot.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct HttpSettings {
    pub listen: Option<String>,
    #[serde(default)]
    pub tls: HttpTlsSettings,
}

/// TLS knobs for the admin HTTP listener. All three default to
/// empty strings (plaintext listener). Partial state (one of
/// `cert_file` / `key_file` set, the other empty) is rejected at
/// boot by [`HttpSettings::listener_config`].
#[derive(Debug, Default, Clone, Deserialize)]
pub struct HttpTlsSettings {
    #[serde(default)]
    pub cert_file: String,
    #[serde(default)]
    pub key_file: String,
    #[serde(default)]
    pub client_ca_file: String,
    #[serde(default)]
    pub extra_sans: Vec<String>,
}

impl HttpSettings {
    /// Coerce the YAML block into the `shared-admin-http` listener
    /// config. Fails fast at boot if the TLS triple is in a half-set
    /// state.
    pub fn listener_config(&self) -> anyhow::Result<shared_admin_http::HttpListenerConfig> {
        let tls = shared_admin_http::TlsConfig::from_yaml(
            &self.tls.cert_file,
            &self.tls.key_file,
            &self.tls.client_ca_file,
            &self.tls.extra_sans,
        )?;
        let listen = self
            .listen
            .clone()
            .unwrap_or_else(|| crate::http::DEFAULT_HTTP_LISTEN_ADDRESS.to_string());
        Ok(shared_admin_http::HttpListenerConfig { listen, tls })
    }
}

/// `iscsi.auth:` block. CHAP users are NOT here — they live in
/// `<data_dir>/iscsi-users.json`. This block only carries the auth
/// method enum and the allowed-algorithms list (policy bits, not
/// secrets).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthSettings {
    #[serde(default)]
    pub method: shared_iscsi::auth::AuthMethod,
    /// CHAP digest algorithms allowed by the target, in preference
    /// order (strongest first). Empty falls back to
    /// `[SHA3-256, SHA-256, SHA-1, MD5]`. Recognized aliases:
    /// `MD5` / `5`, `SHA-1` / `SHA1` / `6`, `SHA-256` / `SHA256` /
    /// `7`, `SHA3-256` / `SHA3_256` / `SHA3256` / `8`.
    #[serde(default)]
    pub allowed_algorithms: Vec<String>,
}

/// `audit:` block. thurvsa's audit subsystem is minimal today —
/// daily-rotating JSONL appends to
/// `<audit.dir or data_dir/audit>/audit-YYYY-MM-DD.jsonl`. No chain,
/// no rate limiting: those land when shared-audit lifts
/// (ROADMAP.md § shared-audit). The single emitter today is the
/// shared-iscsi login phase (CHAP success / failure) via
/// `crate::audit::IscsiDiskLoginAudit`.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditSettings {
    /// On by default. Disable only for development / ephemeral
    /// runs — auditing is a compliance signal, not an operational
    /// knob.
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,
    /// Override the audit directory. Defaults to
    /// `<data_dir>/audit/` when unset.
    pub dir: Option<String>,
}

impl Default for AuditSettings {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            dir: None,
        }
    }
}

fn default_audit_enabled() -> bool {
    true
}

impl DaemonConfig {
    /// Read + parse a thurvsa.yaml file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config file: {}", path.display()))?;
        let cfg: Self = serde_yaml::from_str(&raw)
            .with_context(|| format!("parse config file: {}", path.display()))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(yaml: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile create");
        f.write_all(yaml.as_bytes()).expect("write yaml");
        f
    }

    #[test]
    fn loads_minimal_config() {
        // Backends moved to <data_dir>/cloud-backends.json; YAML only
        // carries cloud.upload/compression/skip_retention_mode_check.
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(cfg.data_dir, "/var/lib/thurvsa");
        // Defaults: auth method None, audit enabled.
        assert!(!cfg.iscsi.auth.method.is_chap());
        assert!(cfg.audit.enabled);
        assert!(cfg.audit.dir.is_none());
    }

    #[test]
    fn missing_file_surfaces_path_in_context() {
        let err = DaemonConfig::load(Path::new("/does/not/exist/thurvsa.yaml"))
            .expect_err("missing file must error");
        assert!(format!("{err:#}").contains("/does/not/exist/thurvsa.yaml"));
    }

    #[test]
    fn unrecognized_top_level_keys_are_ignored() {
        // Forward-compat: a daemon can still load a config that
        // includes future keys it doesn't yet understand.
        // serde_yaml deserialization is non-strict by default for
        // un-tagged structs.
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
unknown_future_key:
  whatever: 1
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok despite extra key");
        assert_eq!(cfg.data_dir, "/var/lib/thurvsa");
    }

    #[test]
    fn loads_iscsi_auth_method_chap_with_policy() {
        // CHAP users + target creds now live in
        // <data_dir>/iscsi-users.json (not YAML); the YAML auth block
        // only carries the method enum and the algorithm-policy list.
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  auth:
    method: CHAP
    allowed_algorithms:
      - SHA3-256
      - SHA-256
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        let auth = &cfg.iscsi.auth;
        assert!(auth.method.is_chap());
        assert_eq!(
            auth.allowed_algorithms,
            vec!["SHA3-256".to_string(), "SHA-256".to_string()]
        );
    }

    fn portal(addr: &str, tpgt: u16) -> shared_iscsi::transport::Portal {
        shared_iscsi::transport::Portal {
            address: addr.to_string(),
            tpgt,
        }
    }

    #[test]
    fn iscsi_listen_scalar_form_deserializes_into_single_entry() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  listen: "10.0.0.5:3260"
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("scalar form parses");
        assert_eq!(cfg.iscsi.listen, Some(vec![portal("10.0.0.5:3260", 1)]));
    }

    #[test]
    fn iscsi_listen_list_form_deserializes_in_order() {
        // Bare strings auto-assign sequential TPGTs from their input
        // position (1-indexed).
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  listen:
    - "10.0.0.5:3260"
    - "10.0.0.6:3260"
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("list form parses");
        assert_eq!(
            cfg.iscsi.listen,
            Some(vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 2)])
        );
    }

    #[test]
    fn iscsi_listen_object_form_carries_explicit_tpgt() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  listen:
    - { address: "10.0.0.5:3260", tpgt: 1 }
    - { address: "10.0.0.6:3260", tpgt: 2 }
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("object form parses");
        assert_eq!(
            cfg.iscsi.listen,
            Some(vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 2)])
        );
    }

    #[test]
    fn iscsi_listen_object_form_allows_shared_tpgt_for_group() {
        // Multiple portals sharing one TPGT (one group, many paths)
        // is legal — the ALUA prerequisite this enables.
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  listen:
    - { address: "10.0.0.5:3260", tpgt: 1 }
    - { address: "10.0.0.6:3260", tpgt: 1 }
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("shared-TPGT form parses");
        assert_eq!(
            cfg.iscsi.listen,
            Some(vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 1)])
        );
    }

    #[test]
    fn iscsi_listen_missing_is_none() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  auth:
    method: None
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("absent key parses");
        assert!(cfg.iscsi.listen.is_none());
    }

    #[test]
    fn iscsi_listen_empty_list_is_rejected() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  listen: []
"#,
        );
        let err = DaemonConfig::load(f.path()).expect_err("empty list rejected");
        assert!(
            format!("{err:#}").contains("at least one"),
            "want 'at least one' in error, got: {err:#}"
        );
    }

    #[test]
    fn audit_block_overrides_dir() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
audit:
  enabled: true
  dir: /var/log/thurvsa/audit
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert!(cfg.audit.enabled);
        assert_eq!(cfg.audit.dir.as_deref(), Some("/var/log/thurvsa/audit"));
    }

    #[test]
    fn transports_default_is_iscsi() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(cfg.transports, vec![Transport::Iscsi]);
    }

    #[test]
    fn transports_scalar_form() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
transports: nvmetcp
nvmetcp:
  listen: 0.0.0.0:4420
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(cfg.transports, vec![Transport::Nvmetcp]);
        assert_eq!(cfg.nvmetcp.listen.as_deref(), Some("0.0.0.0:4420"));
    }

    #[test]
    fn transports_list_both() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
transports: [iscsi, nvmetcp]
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(cfg.transports, vec![Transport::Iscsi, Transport::Nvmetcp]);
    }

    #[test]
    fn transports_empty_rejected() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
transports: []
"#,
        );
        let err = DaemonConfig::load(f.path()).expect_err("empty transports must fail");
        assert!(
            format!("{err:#}").contains("at least one"),
            "want 'at least one' in error, got: {err:#}"
        );
    }

    #[test]
    fn transports_dedup() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
transports: [nvmetcp, iscsi, nvmetcp]
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        // First occurrence kept, order preserved.
        assert_eq!(cfg.transports, vec![Transport::Nvmetcp, Transport::Iscsi]);
    }

    #[test]
    fn iscsi_target_iqn_and_nvmetcp_subnqn_round_trip() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
iscsi:
  target_iqn: "iqn.2025-10.com.example:custom-vsa"
nvmetcp:
  subnqn: "nqn.2025-10.com.example:custom-vsa"
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(
            cfg.iscsi.target_iqn.as_deref(),
            Some("iqn.2025-10.com.example:custom-vsa")
        );
        assert_eq!(
            cfg.nvmetcp.subnqn.as_deref(),
            Some("nqn.2025-10.com.example:custom-vsa")
        );
    }

    #[test]
    fn iscsi_target_iqn_and_nvmetcp_subnqn_default_to_none() {
        let f = write_config("data_dir: /var/lib/thurvsa\n");
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert!(cfg.iscsi.target_iqn.is_none());
        assert!(cfg.nvmetcp.subnqn.is_none());
    }

    #[test]
    fn nvmetcp_auth_mode_defaults_to_none() {
        let f = write_config("data_dir: /var/lib/thurvsa\n");
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(cfg.nvmetcp.auth.mode, NvmetcpAuthMode::None);
        assert!(cfg.nvmetcp.auth.identity_file.is_none());
    }

    #[test]
    fn nvmetcp_auth_dhchap_and_dhchap_plus_tls_parse() {
        // dhchap alone (no TLS).
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
nvmetcp:
  auth:
    mode: dhchap
    identity_file: /etc/thurvsa/nvmetcp-dhchap.json
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(cfg.nvmetcp.auth.mode, NvmetcpAuthMode::Dhchap);
        assert_eq!(cfg.nvmetcp.tls.mode, NvmetcpTlsMode::Disabled);
        assert_eq!(
            cfg.nvmetcp.auth.identity_file.as_deref(),
            Some("/etc/thurvsa/nvmetcp-dhchap.json")
        );

        // dhchap + tls (the combined "dhchap+tls" mode).
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
nvmetcp:
  tls:
    mode: psk
  auth:
    mode: dhchap
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert_eq!(cfg.nvmetcp.auth.mode, NvmetcpAuthMode::Dhchap);
        assert_eq!(cfg.nvmetcp.tls.mode, NvmetcpTlsMode::Psk);
    }

    #[test]
    fn nvmetcp_identity_paths_default_under_data_dir() {
        // No override -> the daemon defaults under <data_dir>/. This is
        // the path the admin handlers must resolve to so CLI edits land
        // where the transport reads (issue #69).
        let f = write_config("data_dir: /var/lib/thurvsa\n");
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        let data_dir = Path::new("/var/lib/thurvsa");
        assert_eq!(
            cfg.nvmetcp.psks_path(data_dir),
            Path::new("/var/lib/thurvsa/nvmetcp-psks.json")
        );
        assert_eq!(
            cfg.nvmetcp.dhchap_path(data_dir),
            Path::new("/var/lib/thurvsa/nvmetcp-dhchap.json")
        );
    }

    #[test]
    fn nvmetcp_identity_paths_honor_override() {
        // With an explicit identity_file the resolved path is the
        // override verbatim, NOT <data_dir>/... (the issue #69 bug: the
        // admin handlers ignored this and wrote under <data_dir>/).
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
nvmetcp:
  tls:
    mode: psk
    identity_file: /etc/thurvsa/nvmetcp-psks.json
  auth:
    mode: dhchap
    identity_file: /etc/thurvsa/nvmetcp-dhchap.json
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        // data_dir is deliberately distinct from the override dir so a
        // resolution that fell back to <data_dir>/ would mismatch.
        let data_dir = Path::new("/var/lib/thurvsa");
        assert_eq!(
            cfg.nvmetcp.psks_path(data_dir),
            Path::new("/etc/thurvsa/nvmetcp-psks.json")
        );
        assert_eq!(
            cfg.nvmetcp.dhchap_path(data_dir),
            Path::new("/etc/thurvsa/nvmetcp-dhchap.json")
        );
    }

    #[test]
    fn audit_can_be_disabled() {
        let f = write_config(
            r#"
data_dir: /var/lib/thurvsa
storage:
  backends:
    devbox:
      type: local
      root_dir: /tmp/thurvsa-storage
audit:
  enabled: false
"#,
        );
        let cfg = DaemonConfig::load(f.path()).expect("load ok");
        assert!(!cfg.audit.enabled);
    }
}
