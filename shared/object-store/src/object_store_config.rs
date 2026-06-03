// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared cloud backend configuration parsing and validation.
//!
//! Used by both `thurvtld` (on startup) and `thurvtl`
//! (`cloud check` command) so they parse `thurvtl.yaml` identically
//! and exercise the same validation steps.

use crate::azure::AzureBackend;
use crate::compression::{
    CompressionAlgo, CompressionConfig as CoreCompressionConfig, ZSTD_DEFAULT_LEVEL,
};
use crate::gcs::GcsBackend;
use crate::local::LocalBackend;
use crate::object_store_backend::ObjectStoreBackend;
use crate::s3::S3Backend;
use serde::Deserialize;
use shared_pool::DiskCacheSize;
use std::collections::BTreeMap;

/// Top-level `storage:` section of `thurvtl.yaml` / `thurvsa.yaml`.
/// Holds the workspace-wide storage-backend tuning knobs that apply
/// uniformly across every backend, plus the named backend definitions
/// themselves under `storage.backends:`. Each cartridge (VTL) or
/// volume (VSA) picks one entry at create time and is bound to it for
/// life.
///
/// ```yaml
/// storage:
///   upload: { max_concurrent: 0, retry_max_attempts: 10, ... }
///   compression: { algorithm: zstd, level: 3 }
///   skip_retention_mode_check: false
///   backends:
///     primary: { type: s3, bucket: ..., region: ..., auth: { ... } }
/// ```
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ObjectStoreConfig {
    #[serde(default)]
    pub upload: UploadConfig,
    #[serde(default)]
    pub compression: CompressionConfigYaml,
    /// Named backend entries. Empty at boot is fine — cartridge / volume
    /// ops that reference a backend fail at op time with `UnknownBackend`.
    #[serde(default)]
    pub backends: BTreeMap<String, BackendEntry>,
    /// Opt out of bucket immutability validation at startup and in
    /// `storage check`. When `true`:
    ///   - `lock_state()` is never called against any backend, in
    ///     `validate_object_store_backend` or anywhere else.
    ///   - Azure backends with `retention_mode != none` no longer
    ///     require `subscription_id` and `resource_group` (those
    ///     fields exist solely to address the management-plane
    ///     immutability-policy resource).
    ///   - The `retention_mode` field still parses and is still used
    ///     to gate `cartridge create --worm` (refuses against
    ///     `retention_mode: none` backends), but the daemon does
    ///     not verify the bucket actually has retention configured.
    ///
    /// Trade-off: skipping the check loses the safety net that
    /// catches "operator declared WORM but bucket has no Object
    /// Lock" misconfigurations at boot. Useful when the principal
    /// Thur VTL runs as cannot be granted the management-plane
    /// IAM (`s3:GetBucketObjectLockConfiguration` /
    /// `storage.buckets.get` / `Storage Account Contributor`).
    /// Defaults to `false` (the check runs); operators with a real
    /// compliance story should leave it default.
    #[serde(default)]
    pub skip_retention_mode_check: bool,

    /// Periodic backend-reachability ticker interval, in seconds. `0`
    /// (the default) disables it — reachability is then only checked on
    /// operator-invoked `system storage check`. When non-zero, each
    /// daemon spawns a background task that runs the same reachability
    /// probe (a small list/write/read/delete per backend) every
    /// `check_interval_seconds` and fires `backend_reachability`
    /// failure/recovery alerts, so a backend that goes unreachable
    /// overnight (revoked credential, quota, network partition) is
    /// caught without an operator at the console. Each tick does real
    /// backend I/O, so set it conservatively (e.g. 300+); `local`
    /// backends are construct-only (no network round-trip).
    #[serde(default)]
    pub check_interval_seconds: u64,
}

/// One named entry in `cloud.backends`. The discriminant is the YAML
/// `type:` field — exactly one of S3 / GCS / Azure / Local per entry,
/// enforced by the enum shape.
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackendEntry {
    S3(S3BackendConfig),
    Gcs(GcsBackendConfig),
    Azure(AzureBackendConfig),
    Local(LocalBackendConfig),
}

impl BackendEntry {
    /// Type discriminant string ("s3", "gcs", "azure", "local").
    pub fn backend_type(&self) -> &'static str {
        match self {
            BackendEntry::S3(_) => "s3",
            BackendEntry::Gcs(_) => "gcs",
            BackendEntry::Azure(_) => "azure",
            BackendEntry::Local(_) => "local",
        }
    }

    /// Per-backend override of the YAML `disk_cache.size_gb` default,
    /// if the operator set one on this entry. `None` → fall back to
    /// the shared default. The value carries the same `auto | <gb>`
    /// shape as the YAML default; resolution against `min_size_gb` /
    /// `max_size_gb` happens on every eviction tick in the daemon.
    pub fn disk_cache_size_gb(&self) -> Option<DiskCacheSize> {
        match self {
            BackendEntry::S3(c) => c.disk_cache_size_gb,
            BackendEntry::Gcs(c) => c.disk_cache_size_gb,
            BackendEntry::Azure(c) => c.disk_cache_size_gb,
            BackendEntry::Local(c) => c.disk_cache_size_gb,
        }
    }
}

/// Operator-declared bucket immutability mode for a cloud backend.
/// Validated against the bucket's actual `lock_state()` at startup;
/// any mismatch is a hard fail. The bucket is the contract — this
/// field tells Thur VTL what to expect, not what to configure.
#[derive(Debug, Deserialize, serde::Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetentionMode {
    /// No bucket-level immutability. WORM cartridges cannot be created
    /// against this backend.
    #[default]
    None,
    /// S3 GOVERNANCE / GCS unlocked retention policy / Azure unlocked
    /// time-based policy. Privileged users can shorten retention.
    Governance,
    /// S3 COMPLIANCE / GCS locked retention policy / Azure locked
    /// time-based policy. Retention is irrevocable until expiry.
    Compliance,
}

impl RetentionMode {
    /// True if the operator declared any kind of immutability for the
    /// backend (governance or compliance, but not none).
    pub fn requires_lock(self) -> bool {
        !matches!(self, RetentionMode::None)
    }

    /// Short label suitable for log/error output.
    pub fn label(self) -> &'static str {
        match self {
            RetentionMode::None => "none",
            RetentionMode::Governance => "governance",
            RetentionMode::Compliance => "compliance",
        }
    }
}

/// Per-backend S3 auth. Optional in YAML — when omitted, the AWS
/// SDK's default credential chain is used (env vars → IRSA / web
/// identity → SSO → ECS task role → EC2 IMDS → `~/.aws/credentials`).
/// When present, it is a **strict override**: the daemon ignores the
/// chain entirely for that backend, so two S3-compatible providers
/// (e.g. AWS S3 + MinIO) can coexist with their own credentials.
///
/// `_env` variants name an environment variable (typically populated
/// from `/etc/thurvtl/thurvtl.env`); the daemon reads the var at
/// startup. Inline `static` puts the secret in the YAML directly —
/// fine for dev, but secrets in YAML are visible to anyone with read
/// access on the file. Prefer `env` for production.
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum S3Auth {
    /// Inline credentials. Secret lives in the YAML.
    Static {
        access_key_id: String,
        secret_access_key: String,
        #[serde(default)]
        session_token: Option<String>,
    },
    /// Credentials read from environment variables at daemon
    /// startup. Lets the YAML stay version-controllable while the
    /// secret lives in `thurvtl.env` (or a vault-managed env injector).
    Env {
        access_key_id_env: String,
        secret_access_key_env: String,
        #[serde(default)]
        session_token_env: Option<String>,
    },
    /// Pick a named profile from `~/.aws/credentials` /
    /// `~/.aws/config`. `aws sso login --profile <name>` works here.
    Profile { name: String },
}

/// Resolved S3 credentials, post-env-var-lookup. Either explicit
/// access-key/secret material or a profile name to feed into the
/// AWS SDK profile provider. Constructed from [`S3Auth`] via
/// [`S3Auth::resolve`]; the backend uses this to build the SDK
/// client without any further env lookup.
#[derive(Debug, Clone)]
pub enum ResolvedS3Auth {
    Static {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    Profile {
        name: String,
    },
}

impl S3Auth {
    /// Read any `_env` variants and return a [`ResolvedS3Auth`]. The
    /// caller should resolve once at config time; the resolved struct
    /// is then passed to [`crate::s3::S3Backend::new`].
    pub fn resolve(&self) -> ObjectStoreConfigResult<ResolvedS3Auth> {
        match self {
            S3Auth::Static {
                access_key_id,
                secret_access_key,
                session_token,
            } => Ok(ResolvedS3Auth::Static {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
            }),
            S3Auth::Env {
                access_key_id_env,
                secret_access_key_env,
                session_token_env,
            } => {
                let access_key_id = read_env(access_key_id_env)?;
                let secret_access_key = read_env(secret_access_key_env)?;
                let session_token = match session_token_env {
                    Some(name) => Some(read_env(name)?),
                    None => None,
                };
                Ok(ResolvedS3Auth::Static {
                    access_key_id,
                    secret_access_key,
                    session_token,
                })
            }
            S3Auth::Profile { name } => Ok(ResolvedS3Auth::Profile { name: name.clone() }),
        }
    }
}

/// Per-backend Azure auth. Same override semantics as [`S3Auth`]:
/// when present, the daemon uses **only** what's specified for this
/// backend, ignoring `AZURE_*` env vars that would otherwise drive
/// the chain.
///
/// Variants mirror the auth methods Microsoft's official
/// `azure_storage_blob` SDK supports. Storage-account shared-key
/// auth was dropped in the 2026-05 SDK migration: the new line is
/// bearer-token-only (`Arc<dyn TokenCredential>`), with SAS handled
/// by encoding the token into the URL itself. SAS is data-plane-only
/// and cannot mint AAD tokens, so backends with `retention_mode !=
/// none` (which need management-plane access for the
/// immutability-policy query) must use `service_principal`.
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AzureAuth {
    /// SAS URL inline. Full container or account URL with `?sv=...`
    /// query string. Anyone holding the URL has the SAS-granted
    /// access until expiry — treat like a password.
    SasUrl { value: String },
    /// SAS URL from named env var.
    SasUrlEnv { env: String },
    /// AAD service principal inline. Required for WORM containers.
    ServicePrincipal {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
    /// AAD service principal from named env vars.
    ServicePrincipalEnv {
        tenant_id_env: String,
        client_id_env: String,
        client_secret_env: String,
    },
}

/// Resolved Azure credentials, post-env-var-lookup. The backend
/// branches on this to build either a SAS-URL data-plane endpoint or
/// an `Arc<dyn TokenCredential>` AAD bearer-token credential.
#[derive(Debug, Clone)]
pub enum ResolvedAzureAuth {
    SasUrl(String),
    ServicePrincipal {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
}

impl AzureAuth {
    pub fn resolve(&self) -> ObjectStoreConfigResult<ResolvedAzureAuth> {
        match self {
            AzureAuth::SasUrl { value } => Ok(ResolvedAzureAuth::SasUrl(value.clone())),
            AzureAuth::SasUrlEnv { env } => Ok(ResolvedAzureAuth::SasUrl(read_env(env)?)),
            AzureAuth::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => Ok(ResolvedAzureAuth::ServicePrincipal {
                tenant_id: tenant_id.clone(),
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
            }),
            AzureAuth::ServicePrincipalEnv {
                tenant_id_env,
                client_id_env,
                client_secret_env,
            } => Ok(ResolvedAzureAuth::ServicePrincipal {
                tenant_id: read_env(tenant_id_env)?,
                client_id: read_env(client_id_env)?,
                client_secret: read_env(client_secret_env)?,
            }),
        }
    }
}

fn read_env(name: &str) -> ObjectStoreConfigResult<String> {
    std::env::var(name).map_err(|_| ObjectStoreConfigError::AuthEnvVarMissing(name.to_string()))
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct S3BackendConfig {
    pub bucket: String,
    /// Object-key prefix prepended to every chunk / manifest under
    /// this backend. Empty (the default) puts objects at the bucket
    /// root; set a trailing-slash prefix like `tapes/` to namespace
    /// the daemon's traffic inside a shared bucket.
    #[serde(default)]
    pub prefix: String,
    pub region: String,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// Force path-style request URLs (`/<bucket>/<key>` vs the
    /// virtual-host-style `<bucket>.<endpoint>/<key>`). Most
    /// S3-compatible services (MinIO, Ceph RGW, AIStor, …) only
    /// support path-style because they don't control DNS for
    /// arbitrary bucket names. `None` → infer from `endpoint_url`:
    /// path-style is forced whenever a custom endpoint is set (the
    /// safe default for non-AWS endpoints), and left at the SDK
    /// default (virtual-host) when talking to real AWS. `Some(true)`
    /// / `Some(false)` is an explicit override that takes precedence
    /// over the inference.
    #[serde(default)]
    pub path_style: Option<bool>,
    #[serde(default)]
    pub retention_mode: RetentionMode,
    /// Per-backend credentials. `None` falls through to the AWS
    /// default credential chain (env vars / IRSA / instance profile /
    /// `~/.aws/credentials`). When `Some`, the chain is bypassed for
    /// this backend — useful when running multiple S3-flavored
    /// providers (AWS + MinIO + Wasabi) in one daemon, since the
    /// chain is process-global and can only carry one set of creds.
    #[serde(default)]
    pub auth: Option<S3Auth>,
    /// Per-backend override of the YAML `disk_cache.size_gb` default.
    /// `None` → use the YAML default. Set when this backend warrants
    /// a different chunk-pool cap from the rest (e.g. a hot S3 tier
    /// that needs more headroom than a cold Azure mirror).
    #[serde(default)]
    pub disk_cache_size_gb: Option<DiskCacheSize>,
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct GcsBackendConfig {
    pub bucket: String,
    /// Object-key prefix. See `S3BackendConfig::prefix`.
    #[serde(default)]
    pub prefix: String,
    pub project_id: String,
    /// Path to a service-account JSON key file. `None` falls
    /// through to Application Default Credentials
    /// (`GOOGLE_APPLICATION_CREDENTIALS` env var, `gcloud auth
    /// application-default login` user creds, GCE/GKE metadata
    /// server). When `Some`, that file is used instead — bypassing
    /// the chain so multiple GCS backends in the same daemon can
    /// authenticate with different service accounts.
    #[serde(default)]
    pub service_account_key_file: Option<String>,
    #[serde(default)]
    pub retention_mode: RetentionMode,
    /// Per-backend override of the YAML `disk_cache.size_gb` default.
    /// See `S3BackendConfig::disk_cache_size_gb` for semantics.
    #[serde(default)]
    pub disk_cache_size_gb: Option<DiskCacheSize>,
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct AzureBackendConfig {
    /// Azure storage account name. Globally unique; becomes the
    /// left-of-`.` part of `https://<storage_account>.blob.core.windows.net/`.
    /// Disambiguated from `subscription_id` and AAD account concepts
    /// by the explicit `storage_account` name.
    pub storage_account: String,
    pub container: String,
    /// Object-key prefix. See `S3BackendConfig::prefix`.
    #[serde(default)]
    pub prefix: String,
    /// Optional custom endpoint URL. Use `http://127.0.0.1:10000/<account>`
    /// for Azurite, or a sovereign-cloud endpoint. When omitted the SDK
    /// defaults to `https://<account>.blob.core.windows.net`.
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub retention_mode: RetentionMode,
    /// Azure subscription ID hosting the storage account. Required when
    /// `retention_mode != none`: container immutability policies live
    /// on the ARM management plane at `/subscriptions/<sub>/resourceGroups/...`,
    /// and resource-group names are scoped to subscriptions, so both
    /// fields are needed to address the policy resource. Plain blob
    /// ops (the data-plane endpoint) don't need either field.
    #[serde(default)]
    pub subscription_id: Option<String>,
    /// Azure resource group hosting the storage account. Required when
    /// `retention_mode != none` (paired with `subscription_id`).
    #[serde(default)]
    pub resource_group: Option<String>,
    /// Per-backend auth. `None` falls through to the env-var
    /// auto-detect chain in `azure.rs::AzureBackend::new`
    /// (`AZURE_STORAGE_SAS_URL` → AAD env service principal
    /// (`AZURE_TENANT_ID` + `_CLIENT_ID` + `_CLIENT_SECRET`) → AAD
    /// fallback chain (managed identity → `az` CLI)). When `Some`,
    /// the chain is bypassed for this backend.
    #[serde(default)]
    pub auth: Option<AzureAuth>,
    /// Per-backend override of the YAML `disk_cache.size_gb` default.
    /// See `S3BackendConfig::disk_cache_size_gb` for semantics.
    #[serde(default)]
    pub disk_cache_size_gb: Option<DiskCacheSize>,
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct LocalBackendConfig {
    pub root_dir: String,
    /// Per-backend override of the YAML `disk_cache.size_gb` default.
    /// See `S3BackendConfig::disk_cache_size_gb` for semantics.
    #[serde(default)]
    pub disk_cache_size_gb: Option<DiskCacheSize>,
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct UploadConfig {
    /// Per-backend upload concurrency ceiling. `0` (the default)
    /// means "auto-scale to host capacity" — resolved at startup to
    /// `min(16, available_parallelism * 4)`. Explicit `>=1` honored
    /// as-is. Bigger values trade memory for throughput: each
    /// in-flight upload pins one chunk in RAM, so `N × chunk_size`
    /// memory per backend per cartridge. The auto cap of 16 keeps
    /// the in-flight footprint at 128 MiB per backend per cartridge
    /// for 8 MiB chunks while capturing most of the available
    /// throughput on cloud backends (S3 / GCS / Azure all saturate
    /// well before c=32 on a single 10 Gbps link).
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_retry_max_attempts")]
    pub retry_max_attempts: u32,
    /// Maximum time a chunk-seal blocks on the per-backend
    /// `PoolBudget` before surfacing `Backpressured` (and ultimately
    /// SCSI NOT READY) to the host. Most backup software retries on
    /// NOT READY, so this is "how long do we let one write stall
    /// before nudging the host to retry." Range 1-600 s.
    #[serde(default = "default_backpressure_max_wait_seconds")]
    pub backpressure_max_wait_seconds: u32,
}

fn default_max_concurrent() -> usize {
    0
}
fn default_retry_max_attempts() -> u32 {
    10
}
fn default_backpressure_max_wait_seconds() -> u32 {
    60
}

impl UploadConfig {
    /// Resolve `max_concurrent` to a concrete value at daemon
    /// startup. Returns `(resolved, source)` where `source` is a
    /// short human-readable label the caller logs alongside the
    /// resolved value so operators see the effective concurrency
    /// (and how it was chosen) in the boot log.
    ///
    /// - `max_concurrent == 0` → auto-scale to
    ///   `min(16, available_parallelism * 4)`. On a 4+ core box the
    ///   cap binds (16). On a 1-core CI VM it resolves to 4. If
    ///   `available_parallelism` errors (no hint available), falls
    ///   back to `num_cpus = 4`.
    /// - `max_concurrent >= 1` → operator override, returned as-is.
    pub fn resolve_max_concurrent(&self) -> (usize, String) {
        if self.max_concurrent == 0 {
            let num_cpus = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(4);
            let resolved = (num_cpus * 4).min(16);
            (resolved, format!("auto-detected from num_cpus={num_cpus}"))
        } else {
            (self.max_concurrent, "operator override".to_string())
        }
    }
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_concurrent(),
            retry_max_attempts: default_retry_max_attempts(),
            backpressure_max_wait_seconds: default_backpressure_max_wait_seconds(),
        }
    }
}

/// YAML schema for `cloud.compression`. `algorithm` accepts
/// `none` / `lz4` / `zstd` (lowercase strings); when `none`, the
/// upload worker uploads chunks uncompressed. `level` only applies
/// to `zstd` and is ignored for `lz4` and `none`.
#[derive(Debug, Deserialize, Clone)]
pub struct CompressionConfigYaml {
    #[serde(default = "default_cloud_algorithm")]
    pub algorithm: CompressionAlgoYaml,
    #[serde(default = "default_compression_level")]
    pub level: i32,
}

/// Algorithm choice as it appears in YAML — adds an explicit `none`
/// variant on top of the core `CompressionAlgo` enum so operators can
/// disable cloud-side compression with a single key.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompressionAlgoYaml {
    None,
    Lz4,
    Zstd,
}

fn default_cloud_algorithm() -> CompressionAlgoYaml {
    CompressionAlgoYaml::Zstd
}
fn default_compression_level() -> i32 {
    ZSTD_DEFAULT_LEVEL
}

impl Default for CompressionConfigYaml {
    fn default() -> Self {
        Self {
            algorithm: CompressionAlgoYaml::Zstd,
            level: ZSTD_DEFAULT_LEVEL,
        }
    }
}

impl ObjectStoreConfig {
    /// Per-backend invariants. Azure-with-retention requires both
    /// `subscription_id` and `resource_group` so the management-plane
    /// query can address the immutability policy. Skipped when
    /// `self.skip_retention_mode_check` is set — those fields only
    /// exist for the management-plane query.
    pub fn validate_backends(&self) -> ObjectStoreConfigResult<()> {
        if self.skip_retention_mode_check {
            return Ok(());
        }
        for (name, entry) in &self.backends {
            if let BackendEntry::Azure(a) = entry
                && a.retention_mode.requires_lock()
                && (a.subscription_id.is_none() || a.resource_group.is_none())
            {
                return Err(ObjectStoreConfigError::AzureRetentionFieldsMissing {
                    name: name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Names of every backend in the configured map, in lexicographic
    /// order. May be empty if no backends are configured yet.
    pub fn backend_names(&self) -> Vec<String> {
        self.backends.keys().cloned().collect()
    }

    /// True iff the configured map has exactly one entry. Used by the
    /// CLI/daemon to decide whether `--backend NAME` is optional
    /// (single backend → optional/inferred; ≥2 → required).
    pub fn is_single_backend(&self) -> bool {
        self.backends.len() == 1
    }

    /// Look up a backend entry by name; structured error if the name
    /// isn't in the configured map.
    pub fn backend_entry(&self, name: &str) -> ObjectStoreConfigResult<&BackendEntry> {
        self.backends
            .get(name)
            .ok_or_else(|| ObjectStoreConfigError::UnknownBackend(name.to_string()))
    }

    /// Build the `ObjectStoreBackend` trait object for the named backend.
    /// Loading credentials and contacting the provider happens here.
    /// Compression settings come from `self.compression`.
    pub async fn create_backend_named(
        &self,
        name: &str,
    ) -> ObjectStoreConfigResult<Box<dyn ObjectStoreBackend>> {
        let compression = self.compression.to_core();
        let entry = self.backend_entry(name)?;
        let inner: Box<dyn ObjectStoreBackend> = match entry {
            BackendEntry::S3(s3) => {
                let auth = s3.auth.as_ref().map(|a| a.resolve()).transpose()?;
                let backend = S3Backend::new(
                    s3.bucket.clone(),
                    s3.prefix.clone(),
                    s3.region.clone(),
                    s3.endpoint_url.clone(),
                    s3.path_style,
                    auth,
                    compression,
                )
                .await
                .map_err(|source| ObjectStoreConfigError::BackendInit {
                    backend: "S3",
                    source,
                })?;
                Box::new(backend)
            }
            BackendEntry::Gcs(gcs) => {
                let backend = GcsBackend::new(
                    gcs.bucket.clone(),
                    gcs.prefix.clone(),
                    gcs.project_id.clone(),
                    gcs.service_account_key_file.clone(),
                    compression,
                )
                .await
                .map_err(|source| ObjectStoreConfigError::BackendInit {
                    backend: "GCS",
                    source,
                })?;
                Box::new(backend)
            }
            BackendEntry::Azure(azure) => {
                let auth = azure.auth.as_ref().map(|a| a.resolve()).transpose()?;
                let backend = AzureBackend::new(
                    azure.storage_account.clone(),
                    azure.container.clone(),
                    azure.prefix.clone(),
                    azure.endpoint_url.clone(),
                    azure.subscription_id.clone(),
                    azure.resource_group.clone(),
                    auth,
                    compression,
                )
                .await
                .map_err(|source| ObjectStoreConfigError::BackendInit {
                    backend: "Azure",
                    source,
                })?;
                Box::new(backend)
            }
            BackendEntry::Local(local) => {
                let backend =
                    LocalBackend::new(local.root_dir.clone())
                        .await
                        .map_err(|source| ObjectStoreConfigError::BackendInit {
                            backend: "Local",
                            source,
                        })?;
                Box::new(backend)
            }
        };
        // Wrap every constructed backend in the meta-cache. Call sites
        // can't bypass — once the registry hands a Box<dyn ObjectStoreBackend>
        // back, every cloud op rides through the cache. Memoizes
        // upload + positive HEAD facts; singleflights concurrent PUTs
        // to the same key (the GCS-mkfs zero-page collapse). Versioned
        // writes use upload_versioned, which the wrapper passes through
        // and invalidates.
        Ok(Box::new(crate::caching::CachingObjectStoreBackend::new(
            inner, name,
        )))
    }

    /// Bucket / container name (S3/GCS/Azure) or root directory (Local)
    /// for the named backend, suitable for log/error messages.
    pub fn target_label_named(&self, name: &str) -> Option<String> {
        let entry = self.backends.get(name)?;
        Some(match entry {
            BackendEntry::S3(s3) => s3.bucket.clone(),
            BackendEntry::Gcs(gcs) => gcs.bucket.clone(),
            BackendEntry::Azure(a) => format!("{}/{}", a.storage_account, a.container),
            BackendEntry::Local(l) => l.root_dir.clone(),
        })
    }

    /// Object-key prefix used for chunks/manifests under the named
    /// backend (S3/GCS/Azure); empty for Local.
    pub fn prefix_named(&self, name: &str) -> &str {
        match self.backends.get(name) {
            Some(BackendEntry::S3(s3)) => s3.prefix.as_str(),
            Some(BackendEntry::Gcs(gcs)) => gcs.prefix.as_str(),
            Some(BackendEntry::Azure(a)) => a.prefix.as_str(),
            _ => "",
        }
    }

    /// Operator-declared retention mode for the named backend. Local
    /// backends always report `None` (the filesystem has no
    /// immutability concept).
    pub fn retention_mode_named(&self, name: &str) -> RetentionMode {
        match self.backends.get(name) {
            Some(BackendEntry::S3(s3)) => s3.retention_mode,
            Some(BackendEntry::Gcs(gcs)) => gcs.retention_mode,
            Some(BackendEntry::Azure(a)) => a.retention_mode,
            _ => RetentionMode::None,
        }
    }
}

/// Refuse to start if a legacy `<data_dir>/cloud-backends.json` is
/// still present — pre-alpha.2 installs kept backend definitions in
/// that file, which has since moved into the YAML conffile under
/// `cloud.backends:`. The daemon halts rather than silently ignore the
/// stale state.
pub fn reject_legacy_cloud_backends_json(
    data_dir: &std::path::Path,
    config_path: &std::path::Path,
) -> std::result::Result<(), String> {
    let legacy = data_dir.join("cloud-backends.json");
    if !legacy.exists() {
        return Ok(());
    }
    Err(format!(
        "refusing to start: {legacy} exists.\n\
         \n\
         Cloud backend definitions now live in the YAML conffile.\n\
         Copy each entry from {legacy} into the `cloud.backends:`\n\
         block of {config_path}, then remove {legacy}. The JSON\n\
         shape maps 1:1 to YAML — keys and field names are unchanged.\n\
         \n\
         See /usr/share/doc/<product>/AUTH.md (or docs/AUTH.md\n\
         in source) for the YAML shape per provider.",
        legacy = legacy.display(),
        config_path = config_path.display(),
    ))
}

impl CompressionConfigYaml {
    pub fn to_core(&self) -> CoreCompressionConfig {
        let algo = match self.algorithm {
            CompressionAlgoYaml::None => None,
            CompressionAlgoYaml::Lz4 => Some(CompressionAlgo::Lz4),
            CompressionAlgoYaml::Zstd => Some(CompressionAlgo::Zstd),
        };
        CoreCompressionConfig::new(algo, self.level)
    }
}

/// Coarse classification of a cloud-check failure, so callers can render
/// a short, human-readable diagnosis and tailored hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Could not reach the cloud provider at all (DNS, TCP, TLS, wrong endpoint URL).
    Network,
    /// Provider rejected the credentials (missing, expired, signature mismatch).
    Auth,
    /// Credentials are valid but lack the required permission.
    Authz,
    /// The bucket / project does not exist.
    NotFound,
    /// Bucket is in a different region than configured.
    RegionMismatch,
    /// The request timed out.
    Timeout,
    /// Could not classify; show the raw error.
    Other,
}

impl FailureKind {
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::Network => "NETWORK",
            FailureKind::Auth => "AUTH",
            FailureKind::Authz => "PERMISSION",
            FailureKind::NotFound => "NOT_FOUND",
            FailureKind::RegionMismatch => "REGION",
            FailureKind::Timeout => "TIMEOUT",
            FailureKind::Other => "OTHER",
        }
    }

    /// One short sentence describing the likely root cause.
    pub fn diagnosis(self) -> &'static str {
        match self {
            FailureKind::Network => {
                "Cannot reach the cloud provider — DNS, network, TLS, or endpoint URL."
            }
            FailureKind::Auth => {
                "Credentials were rejected — missing, expired, or wrong access key / secret."
            }
            FailureKind::Authz => {
                "Authentication succeeded but the credentials lack the required permission."
            }
            FailureKind::NotFound => "The bucket (or project) does not exist.",
            FailureKind::RegionMismatch => {
                "The bucket exists in a different region than configured."
            }
            FailureKind::Timeout => {
                "Request timed out — slow link, blocked port, or provider degraded."
            }
            FailureKind::Other => "Unclassified error — see the raw message below.",
        }
    }

    /// Multi-line bullet list of things to check.
    pub fn hints(self) -> &'static str {
        match self {
            FailureKind::Network => {
                "\
              - DNS: can the host resolve the provider hostname?\n\
              - Outbound HTTPS (TCP 443) reachable? Try: curl -v https://<bucket>.s3.<region>.amazonaws.com/\n\
              - For MinIO/Wasabi: is cloud.s3.endpoint_url correct and reachable?\n\
              - Proxy / firewall blocking the connection?"
            }
            FailureKind::Auth => {
                "\
              - Are AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY set in the environment?\n\
              - For GCS: is GOOGLE_APPLICATION_CREDENTIALS set, or have you run `gcloud auth application-default login`?\n\
              - Have the credentials been rotated or revoked?\n\
              - Is the system clock skewed (causes signature mismatch)?"
            }
            FailureKind::Authz => {
                "\
              Cloud permissions for Thur VTL split across two surfaces:\n\
              data plane (chunks/manifests) AND, only for WORM backends with\n\
              retention_mode != none, the management plane (lock_state query).\n\
              \n\
              S3:\n\
                - data plane:        s3:ListBucket, s3:GetObject, s3:PutObject, s3:DeleteObject\n\
                - management plane:  s3:GetBucketObjectLockConfiguration  (WORM only)\n\
              \n\
              GCS:\n\
                - data plane:        roles/storage.objectAdmin\n\
                - management plane:  storage.buckets.get  (WORM only;\n\
                                     roles/storage.legacyBucketReader is the minimal grant)\n\
                - single-role shortcut: roles/storage.admin covers both planes.\n\
                  Broader than necessary but simpler if least-privilege isn't a hard\n\
                  requirement.\n\
              \n\
              Azure (assign at the storage-account scope):\n\
                - data plane:        'Storage Blob Data Contributor'\n\
                                     (the 'Storage Blob Data *' family — NOT plain\n\
                                      'Contributor', which is mgmt-plane and doesn't\n\
                                      grant blob ops)\n\
                - management plane:  'Storage Account Contributor'  (WORM only)\n\
                Also check the storage account's Networking blade — if\n\
                'Selected networks' is on, your client IP / VNet must be in\n\
                the allow list.\n\
              \n\
              Verify the policy / role is attached to the actual identity\n\
              being used (IAM user, service principal, managed identity)."
            }
            FailureKind::NotFound => {
                "\
              - Does the bucket exist? (`aws s3 ls` or `gsutil ls`)\n\
              - Is cloud.<backend>.bucket spelled correctly?\n\
              - For GCS: is project_id correct?"
            }
            FailureKind::RegionMismatch => {
                "\
              - Set cloud.s3.region to the bucket's actual region.\n\
              - Check with: aws s3api get-bucket-location --bucket <name>"
            }
            FailureKind::Timeout => {
                "\
              - Try again — provider may be transiently slow.\n\
              - Check outbound connectivity and any rate-limiting proxy."
            }
            FailureKind::Other => {
                "\
              - See the raw error message above for provider-specific details."
            }
        }
    }
}

/// Map a [`crate::ObjectStoreError`] to a coarse [`FailureKind`].
///
/// Each backend classifies its error at the SDK boundary — where the
/// typed SDK error / HTTP status / gRPC code is still in hand — and mints
/// the matching carrier variant via [`crate::ObjectStoreError::classified`].
/// This function is just the inverse of that constructor, so the retry
/// loop's fail-fast decision never depends on substring-matching a
/// rendered message (an SDK wording change can no longer silently flip a
/// permanent error to transient or vice-versa).
///
/// `PreconditionFailed` (412) and `Conflict` (409) are the two structured
/// variants callers also match on directly; they fold into `Authz`
/// (permanent — policy says no) and `Other` (transient — concurrent
/// change) respectively. `NotSupported` / `Compression` are local,
/// non-retryable-in-practice failures that fall to `Other`; `Io` is a
/// local filesystem blip during an upload/download and is retry-eligible
/// (`Other`).
pub fn classify(err: &crate::ObjectStoreError) -> FailureKind {
    match err {
        crate::ObjectStoreError::Auth(_) => FailureKind::Auth,
        crate::ObjectStoreError::Authz(_) => FailureKind::Authz,
        crate::ObjectStoreError::NotFound(_) => FailureKind::NotFound,
        crate::ObjectStoreError::RegionMismatch(_) => FailureKind::RegionMismatch,
        crate::ObjectStoreError::Network(_) => FailureKind::Network,
        crate::ObjectStoreError::Timeout(_) => FailureKind::Timeout,
        crate::ObjectStoreError::PreconditionFailed(_) => FailureKind::Authz,
        crate::ObjectStoreError::Conflict(_) => FailureKind::Other,
        crate::ObjectStoreError::Other(_)
        | crate::ObjectStoreError::NotSupported(_)
        | crate::ObjectStoreError::Io(_)
        | crate::ObjectStoreError::Compression(_) => FailureKind::Other,
    }
}

/// Whether a classified failure is worth retrying. Permanent errors
/// (`Auth`, `Authz`, `NotFound`, `RegionMismatch`) burn through the
/// backoff budget without ever succeeding — fail fast instead.
/// Transient classes (`Network`, `Timeout`) plus `Other` (typically
/// 5xx, throttling, or unclassified SDK noise) keep retrying.
pub fn is_retryable(kind: FailureKind) -> bool {
    match kind {
        FailureKind::Network | FailureKind::Timeout | FailureKind::Other => true,
        FailureKind::Auth
        | FailureKind::Authz
        | FailureKind::NotFound
        | FailureKind::RegionMismatch => false,
    }
}

/// Map an HTTP status code to a coarse [`FailureKind`].
///
/// Shared by the GCS and Azure backends, which both surface a numeric
/// HTTP status on their SDK errors. 4xx that point at a fixable
/// credential / permission / target problem fail fast; everything else
/// (409 conflict, 429 throttling, 5xx provider hiccups, and any
/// unrecognized status) is retry-eligible `Other`.
pub(crate) fn http_status_to_failure_kind(status: u16) -> FailureKind {
    match status {
        401 => FailureKind::Auth,
        403 => FailureKind::Authz,
        404 => FailureKind::NotFound,
        408 => FailureKind::Timeout,
        // 412 Precondition Failed: an immutability / conditional-write
        // policy refused the request — permanent for this attempt.
        412 => FailureKind::Authz,
        _ => FailureKind::Other,
    }
}

/// Errors returned by cloud config / validation operations.
#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreConfigError {
    #[error("backend name '{0}' not defined under `cloud.backends:` in the YAML conffile")]
    UnknownBackend(String),
    #[error(
        "auth env var '{0}' is not set. The backend's `auth` block names \
         this env var, but it isn't present in the daemon's environment. \
         Define it in `/etc/thurvtl/thurvtl.env` (loaded by the systemd \
         unit) or systemd `Environment=` overrides."
    )]
    AuthEnvVarMissing(String),
    #[error(
        "Azure backend '{name}' has retention_mode != none but is missing `subscription_id` and/or `resource_group`. \
         Both are required so the daemon can query the container's immutability policy on the ARM management plane \
         (the data-plane endpoint doesn't expose it)."
    )]
    AzureRetentionFieldsMissing { name: String },
    #[error("failed to initialize {backend} backend: {source}")]
    BackendInit {
        backend: &'static str,
        #[source]
        source: crate::ObjectStoreError,
    },
    #[error(
        "backend '{name}': configured retention_mode is '{configured}' but bucket lock state is '{actual}'. \
         The bucket is the contract — either change retention_mode to match, or reconfigure the bucket. \
         {hint}"
    )]
    RetentionMismatch {
        name: String,
        configured: &'static str,
        actual: &'static str,
        hint: &'static str,
    },
    #[error("list failed on bucket '{bucket}' (prefix '{prefix}'): {source}")]
    ListFailed {
        bucket: String,
        prefix: String,
        #[source]
        source: crate::ObjectStoreError,
    },
    #[error("write of test object failed on bucket '{bucket}': {source}")]
    WriteFailed {
        bucket: String,
        #[source]
        source: crate::ObjectStoreError,
    },
    #[error("delete of test object failed on bucket '{bucket}': {source}")]
    DeleteFailed {
        bucket: String,
        #[source]
        source: crate::ObjectStoreError,
    },
    #[error("lock_state query failed for backend '{name}': {source}")]
    LockStateQueryFailed {
        name: String,
        #[source]
        source: crate::ObjectStoreError,
    },
}

impl ObjectStoreConfigError {
    /// Which validation step did this failure happen on?
    pub fn step(&self) -> &'static str {
        match self {
            ObjectStoreConfigError::UnknownBackend(_)
            | ObjectStoreConfigError::AuthEnvVarMissing(_)
            | ObjectStoreConfigError::AzureRetentionFieldsMissing { .. } => "config",
            ObjectStoreConfigError::BackendInit { .. } => "init",
            ObjectStoreConfigError::ListFailed { .. } => "list",
            ObjectStoreConfigError::WriteFailed { .. } => "write",
            ObjectStoreConfigError::DeleteFailed { .. } => "delete",
            ObjectStoreConfigError::LockStateQueryFailed { .. }
            | ObjectStoreConfigError::RetentionMismatch { .. } => "lock_state",
        }
    }

    /// Coarse classification suitable for tailored hints.
    pub fn kind(&self) -> FailureKind {
        match self {
            ObjectStoreConfigError::UnknownBackend(_)
            | ObjectStoreConfigError::AuthEnvVarMissing(_)
            | ObjectStoreConfigError::AzureRetentionFieldsMissing { .. }
            | ObjectStoreConfigError::RetentionMismatch { .. } => FailureKind::Other,
            ObjectStoreConfigError::BackendInit { source, .. }
            | ObjectStoreConfigError::ListFailed { source, .. }
            | ObjectStoreConfigError::WriteFailed { source, .. }
            | ObjectStoreConfigError::DeleteFailed { source, .. }
            | ObjectStoreConfigError::LockStateQueryFailed { source, .. } => classify(source),
        }
    }
}

pub type ObjectStoreConfigResult<T> = std::result::Result<T, ObjectStoreConfigError>;

/// Result of one validation step, used so callers can stream pretty output.
#[derive(Debug, Clone)]
pub struct ObjectStoreCheckStep {
    pub name: &'static str,
    pub detail: String,
}

/// Validate that a single named cloud backend is reachable and has
/// the permissions Thur VTL needs (list, write, delete).
///
/// For the `local` backend this only verifies that the backend can be
/// constructed (the root directory is writable), since there is no cloud
/// authentication or network round-trip to test.
///
/// `step_cb` is called after each successful step so callers can render
/// progress (e.g. CLI prints, daemon `info!` logs).
pub async fn validate_object_store_backend<F: FnMut(ObjectStoreCheckStep)>(
    cfg: &ObjectStoreConfig,
    name: &str,
    mut step_cb: F,
) -> ObjectStoreConfigResult<()> {
    let backend = cfg.create_backend_named(name).await?;
    step_cb(ObjectStoreCheckStep {
        name: "init",
        detail: format!("backend type: {}", backend.backend_type()),
    });

    if validate_local_short_circuit(cfg, name)? {
        return Ok(());
    }

    let configured = cfg.retention_mode_named(name);
    check_retention_state(backend.as_ref(), cfg, name, configured, &mut step_cb).await?;
    probe_data_plane(backend.as_ref(), cfg, name, configured, &mut step_cb).await
}

/// Local-backend fast-path. Returns `Ok(true)` when the backend is
/// local and validation is complete (no remote endpoint, no auth, no
/// probe). Returns `Ok(false)` when the backend isn't local and the
/// caller should continue with the retention check + data-plane probe.
///
/// Local has no immutability concept, and any non-`none`
/// retention_mode would have been rejected by the lock-state check
/// (LocalBackend returns `LockState::Off`). Apply the same check
/// here for symmetry and a clear error. Skipped under
/// `skip_retention_mode_check`.
fn validate_local_short_circuit(
    cfg: &ObjectStoreConfig,
    name: &str,
) -> ObjectStoreConfigResult<bool> {
    if cfg.backend_entry(name)?.backend_type() != "local" {
        return Ok(false);
    }
    let configured = cfg.retention_mode_named(name);
    if !cfg.skip_retention_mode_check && configured.requires_lock() {
        return Err(ObjectStoreConfigError::RetentionMismatch {
            name: name.to_string(),
            configured: configured.label(),
            actual: "off",
            hint: "Local backend has no immutability — retention_mode must be `none`.",
        });
    }
    Ok(true)
}

/// Bidirectional retention check for a remote backend. Compares the
/// operator-declared `retention_mode` against the bucket's actual
/// `lock_state()` and refuses to start on mismatch.
///
/// Honours `cfg.skip_retention_mode_check`: when set, the
/// management-plane query is skipped entirely (only a step note is
/// emitted). The `retention_mode` field still parses and is still
/// consulted by the CLI for `cartridge create --worm` gating; we just
/// don't issue the management-plane query.
///
/// Without the skip flag, queries the bucket's lock state for every
/// backend — including `retention_mode: none` — so misconfigurations
/// are caught in both directions:
///   - configured none + bucket actually locked: catches a backup
///     bucket that's been switched to retention-protected out from
///     under us.
///   - configured governance/compliance + bucket off: catches the
///     opposite, where the operator declared WORM but never set up
///     the bucket policy.
///
/// Permission failures on the management-plane query (the principal
/// doesn't have `s3:GetBucketObjectLockConfiguration` /
/// `storage.buckets.get` / Azure 'Storage Account Contributor') are
/// downgraded to a warning rather than fail-to-start. Non-WORM
/// operators who don't grant management-plane IAM keep working; WORM
/// operators who care about the safety net grant the IAM and get
/// hard verification. Mismatches that DO get verified are still
/// fatal.
async fn check_retention_state<F: FnMut(ObjectStoreCheckStep)>(
    backend: &dyn crate::object_store_backend::ObjectStoreBackend,
    cfg: &ObjectStoreConfig,
    name: &str,
    configured: RetentionMode,
    step_cb: &mut F,
) -> ObjectStoreConfigResult<()> {
    if cfg.skip_retention_mode_check {
        step_cb(ObjectStoreCheckStep {
            name: "lock_state",
            detail: format!(
                "retention_mode '{}' — bucket lock state check disabled \
                 (skip_retention_mode_check: true)",
                configured.label()
            ),
        });
        return Ok(());
    }
    match backend.lock_state().await {
        Ok(actual) => {
            let actual_label = actual.label();
            let configured_label = configured.label();
            let mismatch = !matches!(
                (configured, actual),
                (
                    RetentionMode::None,
                    crate::object_store_backend::LockState::Off
                ) | (
                    RetentionMode::Governance,
                    crate::object_store_backend::LockState::Governance { .. }
                ) | (
                    RetentionMode::Compliance,
                    crate::object_store_backend::LockState::Compliance { .. }
                )
            );
            if mismatch {
                // Provider-aware vocabulary. "Object Lock" on AWS,
                // "retention policy" on GCS, "immutability policy"
                // on Azure — the operator-facing message uses the
                // term they'll find in their cloud's own UI.
                let bt = cfg
                    .backend_entry(name)
                    .ok()
                    .map(|e| e.backend_type())
                    .unwrap_or("");
                let policy_term = match bt {
                    "s3" => "Object Lock",
                    "gcs" => "retention policy",
                    "azure" => "immutability policy",
                    _ => "retention policy",
                };
                let hint_owned: String = match (configured, actual) {
                    (RetentionMode::None, _) => format!(
                        "Bucket has a {} configured but the backend declares retention_mode: \
                         none. Either set retention_mode to match the bucket, or point this \
                         backend at a bucket without {}.",
                        policy_term, policy_term
                    ),
                    (RetentionMode::Governance, crate::object_store_backend::LockState::Off) => {
                        format!(
                            "Bucket has no {}. Configure a default retention period in mutable / \
                         GOVERNANCE / unlocked mode, or remove retention_mode from this backend.",
                            policy_term
                        )
                    }
                    (RetentionMode::Compliance, crate::object_store_backend::LockState::Off) => {
                        format!(
                            "Bucket has no {}. Configure a default retention period in irrevocable / \
                         COMPLIANCE / locked mode, or remove retention_mode.",
                            policy_term
                        )
                    }
                    (
                        RetentionMode::Governance,
                        crate::object_store_backend::LockState::Compliance { .. },
                    ) => format!(
                        "Bucket {} is irrevocable / locked / COMPLIANCE. Update retention_mode \
                         to 'compliance', or reconfigure the bucket.",
                        policy_term
                    ),
                    (
                        RetentionMode::Compliance,
                        crate::object_store_backend::LockState::Governance { .. },
                    ) => format!(
                        "Bucket {} is mutable / unlocked / GOVERNANCE. For compliance you need \
                         an irrevocable lock — reconfigure the bucket, or downgrade \
                         retention_mode to 'governance'.",
                        policy_term
                    ),
                    _ => format!(
                        "Reconfigure the bucket {} or update retention_mode.",
                        policy_term
                    ),
                };
                // ObjectStoreConfigError::RetentionMismatch carries `hint` as
                // `&'static str`. Leak the formatted string — this is
                // a fatal startup error and the process is about to
                // terminate, so the leak is bounded.
                let hint: &'static str = Box::leak(hint_owned.into_boxed_str());
                return Err(ObjectStoreConfigError::RetentionMismatch {
                    name: name.to_string(),
                    configured: configured_label,
                    actual: actual_label,
                    hint,
                });
            }
            step_cb(ObjectStoreCheckStep {
                name: "lock_state",
                detail: format!(
                    "retention_mode '{}' matches bucket lock state '{}'",
                    configured_label, actual_label
                ),
            });
        }
        Err(source) => {
            tracing::warn!(
                "backend '{}': lock_state query failed (configured retention_mode: {}); \
                 cannot verify bucket immutability state. Cause: {}",
                name,
                configured.label(),
                source
            );
            step_cb(ObjectStoreCheckStep {
                name: "lock_state",
                detail: format!(
                    "retention_mode '{}' — bucket lock state could not be queried (insufficient \
                     permissions or provider error); proceeding without verification",
                    configured.label()
                ),
            });
        }
    }
    Ok(())
}

/// Data-plane probe: list / write / delete a sentinel object to
/// prove auth + bucket existence + list/write/delete permissions.
///
/// Locked buckets inherit the bucket's default retention on every
/// PUT, which makes our test object undeletable until retention
/// expires. The write/delete steps are skipped (with a `skip_probe`
/// step note) for any backend whose `configured` retention requires
/// a lock; list-only validation is sufficient there.
///
/// Note: locked-bucket write/delete probe-skip applies based on the
/// configured retention_mode even under `skip_retention_mode_check`,
/// since we can't tell whether the bucket is actually locked without
/// querying — better to assume it might be and skip the probe than
/// to leave undeletable test objects behind.
async fn probe_data_plane<F: FnMut(ObjectStoreCheckStep)>(
    backend: &dyn crate::object_store_backend::ObjectStoreBackend,
    cfg: &ObjectStoreConfig,
    name: &str,
    configured: RetentionMode,
    step_cb: &mut F,
) -> ObjectStoreConfigResult<()> {
    let bucket = cfg.target_label_named(name).unwrap_or_default();
    // S3Backend / GcsBackend prepend their configured prefix internally,
    // so we must pass RELATIVE paths here, not the full key with prefix.
    let configured_prefix = cfg.prefix_named(name);
    let test_key_rel = ".thurvtl-cloud-check";
    // For human-readable messages only:
    let display_prefix = configured_prefix.to_string();
    let display_key = format!("{}{}", configured_prefix, test_key_rel);

    // 1. List — proves auth + bucket existence + list permission + reachability.
    // List under the configured prefix itself (no extra subdir).
    let listed =
        backend
            .list_objects("")
            .await
            .map_err(|source| ObjectStoreConfigError::ListFailed {
                bucket: bucket.clone(),
                prefix: display_prefix.clone(),
                source,
            })?;
    step_cb(ObjectStoreCheckStep {
        name: "list",
        detail: format!(
            "bucket '{}' reachable ({} objects under '{}')",
            bucket,
            listed.len(),
            display_prefix
        ),
    });

    if configured.requires_lock() {
        step_cb(ObjectStoreCheckStep {
            name: "skip_probe",
            detail: format!(
                "bucket '{}' is locked (retention_mode={}); skipping write/delete probe",
                bucket,
                configured.label()
            ),
        });
        return Ok(());
    }

    // 2. Write — proves write permission.
    backend
        .upload_manifest(test_key_rel, "{\"test\": true}")
        .await
        .map_err(|source| ObjectStoreConfigError::WriteFailed {
            bucket: bucket.clone(),
            source,
        })?;
    step_cb(ObjectStoreCheckStep {
        name: "write",
        detail: format!("uploaded test object '{}'", display_key),
    });

    // 3. Delete — proves delete permission and cleans up.
    backend
        .delete_object(test_key_rel)
        .await
        .map_err(|source| ObjectStoreConfigError::DeleteFailed {
            bucket: bucket.clone(),
            source,
        })?;
    step_cb(ObjectStoreCheckStep {
        name: "delete",
        detail: format!("deleted test object '{}'", display_key),
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a YAML snippet into the full ObjectStoreConfig.
    fn parse(s: &str) -> ObjectStoreConfig {
        serde_yaml::from_str(s).expect("cloud config parses")
    }

    /// Alias retained for tests that only exercise the
    /// upload/compression/skip-flag side without a backends block.
    fn parse_cfg(yaml: &str) -> ObjectStoreConfig {
        serde_yaml::from_str(yaml).expect("cloud config parses")
    }

    #[test]
    fn multi_backend_yaml_parses_and_validates() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: thurvtl-data
    prefix: ""
    region: us-east-1
  archive:
    type: gcs
    bucket: thurvtl-cold
    prefix: ""
    project_id: my-project
"#,
        );
        cfg.validate_backends().expect("validate ok");
        let mut names = cfg.backend_names();
        names.sort();
        assert_eq!(names, vec!["archive".to_string(), "primary".to_string()]);
        assert!(!cfg.is_single_backend());
    }

    #[test]
    fn single_entry_is_recognized() {
        let cfg = parse(
            r#"
backends:
  only:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
"#,
        );
        cfg.validate_backends().expect("validate ok");
        assert!(cfg.is_single_backend());
        assert_eq!(cfg.backend_names(), vec!["only".to_string()]);
    }

    #[test]
    fn s3_auth_static_parses_and_resolves_inline() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
    auth: { type: static, access_key_id: AKIA1234, secret_access_key: secret-value }
"#,
        );
        cfg.validate_backends().unwrap();
        let entry = cfg.backend_entry("primary").unwrap();
        let BackendEntry::S3(s3) = entry else {
            panic!("expected S3")
        };
        let resolved = s3.auth.as_ref().unwrap().resolve().unwrap();
        match resolved {
            ResolvedS3Auth::Static {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                assert_eq!(access_key_id, "AKIA1234");
                assert_eq!(secret_access_key, "secret-value");
                assert!(session_token.is_none());
            }
            _ => panic!("expected Static"),
        }
    }

    #[test]
    fn s3_auth_env_parses_and_records_var_names() {
        // We can't set process env vars from a test (workspace
        // forbids unsafe), so we verify the parser captures the
        // names — the resolve()-from-env path is exercised by the
        // missing-var test below (which is enough to prove
        // resolve() reads the named var from the environment).
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
    auth:
      type: env
      access_key_id_env: THUR_TEST_KEY
      secret_access_key_env: THUR_TEST_SECRET
      session_token_env: THUR_TEST_TOKEN
"#,
        );
        let entry = cfg.backend_entry("primary").unwrap();
        let BackendEntry::S3(s3) = entry else {
            panic!()
        };
        match s3.auth.as_ref().unwrap() {
            S3Auth::Env {
                access_key_id_env,
                secret_access_key_env,
                session_token_env,
            } => {
                assert_eq!(access_key_id_env, "THUR_TEST_KEY");
                assert_eq!(secret_access_key_env, "THUR_TEST_SECRET");
                assert_eq!(session_token_env.as_deref(), Some("THUR_TEST_TOKEN"));
            }
            _ => panic!("expected Env"),
        }
    }

    #[test]
    fn s3_auth_env_missing_var_surfaces_structured_error() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
    auth:
      type: env
      access_key_id_env: THUR_TEST_DEFINITELY_UNSET_X1
      secret_access_key_env: THUR_TEST_DEFINITELY_UNSET_X2
"#,
        );
        let entry = cfg.backend_entry("primary").unwrap();
        let BackendEntry::S3(s3) = entry else {
            panic!()
        };
        let err = s3.auth.as_ref().unwrap().resolve().unwrap_err();
        assert!(
            matches!(err, ObjectStoreConfigError::AuthEnvVarMissing(ref n) if n == "THUR_TEST_DEFINITELY_UNSET_X1")
        );
    }

    #[test]
    fn s3_auth_profile_round_trips() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
    auth: { type: profile, name: production }
"#,
        );
        let entry = cfg.backend_entry("primary").unwrap();
        let BackendEntry::S3(s3) = entry else {
            panic!()
        };
        match s3.auth.as_ref().unwrap().resolve().unwrap() {
            ResolvedS3Auth::Profile { name } => assert_eq!(name, "production"),
            _ => panic!("expected Profile"),
        }
    }

    #[test]
    fn gcs_service_account_key_file_round_trips() {
        let cfg = parse(
            r#"
backends:
  archive:
    type: gcs
    bucket: b
    prefix: ""
    project_id: p
    service_account_key_file: /etc/thurvtl/gcs.json
"#,
        );
        let entry = cfg.backend_entry("archive").unwrap();
        let BackendEntry::Gcs(gcs) = entry else {
            panic!()
        };
        assert_eq!(
            gcs.service_account_key_file.as_deref(),
            Some("/etc/thurvtl/gcs.json")
        );
    }

    #[test]
    fn azure_auth_sas_url_inline_parses() {
        let cfg = parse(
            r#"
backends:
  cold:
    type: azure
    storage_account: a
    container: c
    prefix: ""
    auth: { type: sas_url, value: "https://a.blob.core.windows.net/c?sv=foo" }
"#,
        );
        cfg.validate_backends().unwrap();
        let entry = cfg.backend_entry("cold").unwrap();
        let BackendEntry::Azure(az) = entry else {
            panic!()
        };
        match az.auth.as_ref().unwrap().resolve().unwrap() {
            ResolvedAzureAuth::SasUrl(s) => assert!(s.contains("?sv=foo")),
            _ => panic!("expected SasUrl"),
        }
    }

    #[test]
    fn azure_auth_service_principal_inline_parses() {
        let cfg = parse(
            r#"
backends:
  cold:
    type: azure
    storage_account: a
    container: c
    prefix: ""
    auth:
      type: service_principal
      tenant_id: t
      client_id: c
      client_secret: s
"#,
        );
        let entry = cfg.backend_entry("cold").unwrap();
        let BackendEntry::Azure(az) = entry else {
            panic!()
        };
        match az.auth.as_ref().unwrap().resolve().unwrap() {
            ResolvedAzureAuth::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => {
                assert_eq!(tenant_id, "t");
                assert_eq!(client_id, "c");
                assert_eq!(client_secret, "s");
            }
            _ => panic!("expected ServicePrincipal"),
        }
    }

    #[test]
    fn s3_auth_unknown_field_is_rejected() {
        // `deny_unknown_fields` on the auth enum means typos (e.g.
        // `access_key` vs `access_key_id`) fail at parse time
        // instead of silently failing later.
        let err = serde_yaml::from_str::<ObjectStoreConfig>(
            r#"
backends:
  primary:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
    auth: { type: static, access_key: AKIA, secret_access_key: s }
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown") || msg.contains("missing"),
            "expected typo to surface as parse error, got: {msg}"
        );
    }

    #[test]
    fn empty_backends_map_validates_ok() {
        // An empty `cloud.backends:` map is fine at boot — cartridge /
        // volume ops that reference a backend fail at op time with
        // `UnknownBackend`. The daemon itself starts cleanly.
        let cfg = parse("backends: {}");
        cfg.validate_backends().expect("empty backends OK at boot");
        assert!(cfg.backends.is_empty());
        assert!(cfg.backend_names().is_empty());
    }

    #[test]
    fn unknown_backend_lookup_errors() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
"#,
        );
        cfg.validate_backends().unwrap();
        let err = cfg.backend_entry("does-not-exist").unwrap_err();
        assert!(matches!(err, ObjectStoreConfigError::UnknownBackend(_)));
    }

    #[test]
    fn retention_mode_default_is_none() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
"#,
        );
        cfg.validate_backends().unwrap();
        assert_eq!(cfg.retention_mode_named("primary"), RetentionMode::None);
    }

    #[test]
    fn retention_mode_governance_and_compliance_parse() {
        let cfg = parse(
            r#"
backends:
  worm:
    type: s3
    bucket: thurvtl-worm
    prefix: ""
    region: us-east-1
    retention_mode: governance
  locked:
    type: s3
    bucket: thurvtl-locked
    prefix: ""
    region: us-east-1
    retention_mode: compliance
"#,
        );
        cfg.validate_backends().unwrap();
        assert_eq!(cfg.retention_mode_named("worm"), RetentionMode::Governance);
        assert_eq!(
            cfg.retention_mode_named("locked"),
            RetentionMode::Compliance
        );
        assert!(RetentionMode::Governance.requires_lock());
        assert!(RetentionMode::Compliance.requires_lock());
        assert!(!RetentionMode::None.requires_lock());
    }

    #[test]
    fn skip_retention_mode_check_default_is_false() {
        let cfg = parse_cfg("");
        assert!(!cfg.skip_retention_mode_check);
    }

    #[test]
    fn skip_retention_mode_check_relaxes_azure_field_requirement() {
        // With the flag off (default), Azure + retention_mode != none
        // requires subscription_id + resource_group → AzureRetentionFieldsMissing.
        let cfg = parse(
            r#"
backends:
  cold:
    type: azure
    storage_account: thurvtl
    container: worm
    prefix: ""
    retention_mode: governance
"#,
        );
        let err = cfg
            .validate_backends()
            .expect_err("should require Azure fields");
        assert!(matches!(
            err,
            ObjectStoreConfigError::AzureRetentionFieldsMissing { .. }
        ));

        // With the flag on, the same backends validate — the fields
        // only matter for the management-plane query, which is now
        // disabled.
        let cfg_skipped = ObjectStoreConfig {
            skip_retention_mode_check: true,
            ..cfg.clone()
        };
        cfg_skipped
            .validate_backends()
            .expect("Azure WORM without sub/rg validates when check is skipped");
        let cfg_yaml = parse_cfg("skip_retention_mode_check: true");
        assert!(cfg_yaml.skip_retention_mode_check);
    }

    #[test]
    fn azure_retention_governance_requires_subscription_and_rg() {
        let cfg = parse(
            r#"
backends:
  cold:
    type: azure
    storage_account: thurvtl
    container: worm
    prefix: ""
    retention_mode: governance
"#,
        );
        let err = cfg
            .validate_backends()
            .expect_err("Azure retention without sub/rg must error");
        assert!(matches!(
            err,
            ObjectStoreConfigError::AzureRetentionFieldsMissing { .. }
        ));
    }

    #[test]
    fn azure_retention_governance_with_subscription_and_rg_validates() {
        let cfg = parse(
            r#"
backends:
  cold:
    type: azure
    storage_account: thurvtl
    container: worm
    prefix: ""
    retention_mode: governance
    subscription_id: "00000000-0000-0000-0000-000000000000"
    resource_group: thurvtl-rg
"#,
        );
        cfg.validate_backends()
            .expect("Azure WORM with both fields validates");
        assert_eq!(cfg.retention_mode_named("cold"), RetentionMode::Governance);
    }

    #[test]
    fn azure_retention_none_does_not_require_subscription_or_rg() {
        let cfg = parse(
            r#"
backends:
  cold:
    type: azure
    storage_account: thurvtl
    container: cold
    prefix: ""
"#,
        );
        cfg.validate_backends().unwrap();
        assert_eq!(cfg.retention_mode_named("cold"), RetentionMode::None);
    }

    #[test]
    fn retention_mode_named_is_none_for_local_and_unknown() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: local
    root_dir: /tmp/x
"#,
        );
        cfg.validate_backends().unwrap();
        assert_eq!(cfg.retention_mode_named("primary"), RetentionMode::None);
        assert_eq!(
            cfg.retention_mode_named("doesnotexist"),
            RetentionMode::None
        );
    }

    #[test]
    fn target_label_and_prefix_resolve_per_backend() {
        let cfg = parse(
            r#"
backends:
  primary:
    type: s3
    bucket: thurvtl-data
    prefix: "tapes/"
    region: us-east-1
  cold:
    type: azure
    storage_account: thurvtl
    container: cold
    prefix: ""
"#,
        );
        cfg.validate_backends().unwrap();
        assert_eq!(cfg.target_label_named("primary").unwrap(), "thurvtl-data");
        assert_eq!(cfg.prefix_named("primary"), "tapes/");
        assert_eq!(cfg.target_label_named("cold").unwrap(), "thurvtl/cold");
        assert_eq!(cfg.prefix_named("cold"), "");
    }

    #[test]
    fn prefix_defaults_to_empty_when_omitted() {
        // Omitting `prefix:` on each of S3 / GCS / Azure must
        // deserialize cleanly and resolve to the empty string —
        // equivalent to writing `prefix: ""` explicitly.
        let cfg = parse(
            r#"
backends:
  s3-no-prefix:
    type: s3
    bucket: b
    region: us-east-1
  gcs-no-prefix:
    type: gcs
    bucket: b
    project_id: p
  azure-no-prefix:
    type: azure
    storage_account: a
    container: c
"#,
        );
        cfg.validate_backends().unwrap();
        assert_eq!(cfg.prefix_named("s3-no-prefix"), "");
        assert_eq!(cfg.prefix_named("gcs-no-prefix"), "");
        assert_eq!(cfg.prefix_named("azure-no-prefix"), "");
    }

    // ===== classify() + is_retryable() coverage =====
    //
    // Classification now happens at the SDK boundary inside each backend
    // (s3::classify_s3_*, gcs_api::classify_gcs_*, azure::classify_azure_*),
    // which mint the typed carrier variant via `ObjectStoreError::classified`.
    // These tests pin that `classify` is the exact inverse of `classified`
    // and that each kind pairs with the right `is_retryable` verdict — the
    // fail-fast contract the retry loop depends on.

    /// Every FailureKind round-trips through `classified` → `classify`.
    #[test]
    fn classified_and_classify_are_inverses() {
        for kind in [
            FailureKind::Auth,
            FailureKind::Authz,
            FailureKind::NotFound,
            FailureKind::RegionMismatch,
            FailureKind::Network,
            FailureKind::Timeout,
            FailureKind::Other,
        ] {
            let err = crate::ObjectStoreError::classified(kind, "x".to_string());
            assert_eq!(classify(&err), kind, "round-trip failed for {kind:?}");
        }
    }

    /// The permanent classes fail fast; the transient classes keep retrying.
    #[test]
    fn permanent_kinds_fail_fast_transient_kinds_retry() {
        for kind in [
            FailureKind::Auth,
            FailureKind::Authz,
            FailureKind::NotFound,
            FailureKind::RegionMismatch,
        ] {
            assert!(!is_retryable(kind), "{kind:?} must be permanent");
        }
        for kind in [
            FailureKind::Network,
            FailureKind::Timeout,
            FailureKind::Other,
        ] {
            assert!(is_retryable(kind), "{kind:?} must be retryable");
        }
    }

    #[test]
    fn classify_precondition_failed_is_authz_permanent() {
        // 412 from Azure's legal-hold path: policy says no — permanent.
        let err = crate::ObjectStoreError::PreconditionFailed("Object locked".to_string());
        assert_eq!(classify(&err), FailureKind::Authz);
        assert!(!is_retryable(FailureKind::Authz));
    }

    #[test]
    fn classify_conflict_is_retryable_other() {
        // 409 from concurrent metadata mutation: another writer
        // changed state under us — retry helps.
        let err = crate::ObjectStoreError::Conflict("concurrent update".to_string());
        assert_eq!(classify(&err), FailureKind::Other);
        assert!(is_retryable(FailureKind::Other));
    }

    /// Local-shaped failures (catch-all, unsupported op, io, compression)
    /// all bucket into the retryable `Other` class.
    #[test]
    fn classify_local_shaped_failures_are_other() {
        for err in [
            crate::ObjectStoreError::Other("brand new sdk error nobody mapped".to_string()),
            crate::ObjectStoreError::NotSupported("legal hold on local".to_string()),
            crate::ObjectStoreError::Io(std::io::Error::other("disk blip")),
            crate::ObjectStoreError::Compression("zstd failed".to_string()),
        ] {
            assert_eq!(classify(&err), FailureKind::Other);
            assert!(is_retryable(classify(&err)));
        }
    }

    // ----- validate_object_store_backend pinning tests --------------------
    //
    // These exercise the externally-observable shape of
    // validate_object_store_backend against a LocalBackend; the
    // bidirectional retention check + data-plane probe (the bulk of
    // the function) require a real S3/GCS/Azure target and are
    // covered by integration scripts. The local fast-path is exactly
    // what the upcoming split has to preserve verbatim.

    fn local_store_config(root_dir: &std::path::Path) -> ObjectStoreConfig {
        let mut backends_map = std::collections::BTreeMap::new();
        backends_map.insert(
            "archive".to_string(),
            BackendEntry::Local(LocalBackendConfig {
                root_dir: root_dir.to_string_lossy().into_owned(),
                disk_cache_size_gb: None,
            }),
        );
        ObjectStoreConfig {
            backends: backends_map,
            ..ObjectStoreConfig::default()
        }
    }

    #[tokio::test]
    async fn validate_local_backend_emits_only_init_step() {
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(temp.path());
        let mut steps: Vec<String> = Vec::new();
        validate_object_store_backend(&cfg, "archive", |s| {
            steps.push(s.name.to_string());
        })
        .await
        .expect("local backend validates");
        // Only the backend init step is emitted — local has no remote
        // endpoint, no auth, and no list/write/delete probe.
        assert_eq!(steps, vec!["init".to_string()]);
    }

    #[tokio::test]
    async fn validate_local_backend_with_skip_retention_check_still_oks() {
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = ObjectStoreConfig {
            skip_retention_mode_check: true,
            ..local_store_config(temp.path())
        };
        let mut steps: Vec<String> = Vec::new();
        validate_object_store_backend(&cfg, "archive", |s| {
            steps.push(s.name.to_string());
        })
        .await
        .expect("local backend validates with skip flag");
        assert_eq!(steps, vec!["init".to_string()]);
    }

    #[test]
    fn upload_default_is_auto_scale_sentinel() {
        let cfg = UploadConfig::default();
        assert_eq!(
            cfg.max_concurrent, 0,
            "default must be 0 (auto-scale sentinel)"
        );
        let (resolved, source) = cfg.resolve_max_concurrent();
        assert!(
            (1..=16).contains(&resolved),
            "auto-scale must resolve into 1..=16, got {resolved}"
        );
        assert!(
            source.starts_with("auto-detected"),
            "source label must say auto-detected, got {source:?}"
        );
    }

    #[test]
    fn upload_explicit_value_honored_as_override() {
        let cfg = UploadConfig {
            max_concurrent: 32,
            ..UploadConfig::default()
        };
        let (resolved, source) = cfg.resolve_max_concurrent();
        assert_eq!(resolved, 32, "explicit value must round-trip");
        assert_eq!(source, "operator override");
    }

    #[test]
    fn upload_serde_default_yields_auto_scale() {
        let cfg = parse_cfg("upload: {}");
        assert_eq!(cfg.upload.max_concurrent, 0);
    }

    #[tokio::test]
    async fn validate_unknown_backend_name_errors() {
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(temp.path());
        let err = validate_object_store_backend(&cfg, "does-not-exist", |_s| {})
            .await
            .expect_err("unknown backend name must error");
        assert!(
            matches!(err, ObjectStoreConfigError::UnknownBackend(ref n) if n == "does-not-exist"),
            "expected UnknownBackend, got: {:?}",
            err
        );
    }

    // ----- FailureKind diagnostic strings --------------------------
    //
    // `label` / `diagnosis` / `hints` are pure lookup tables wired
    // into the CLI's `cloud check` output; pin every variant so a
    // missing arm fails the build.

    #[test]
    fn failure_kind_label_diagnosis_hints_cover_every_variant() {
        for kind in [
            FailureKind::Network,
            FailureKind::Auth,
            FailureKind::Authz,
            FailureKind::NotFound,
            FailureKind::RegionMismatch,
            FailureKind::Timeout,
            FailureKind::Other,
        ] {
            assert!(!kind.label().is_empty(), "label empty for {kind:?}");
            assert!(!kind.diagnosis().is_empty(), "diagnosis empty for {kind:?}");
            assert!(!kind.hints().is_empty(), "hints empty for {kind:?}");
        }
        assert_eq!(FailureKind::Auth.label(), "AUTH");
        assert_eq!(FailureKind::Authz.label(), "PERMISSION");
        assert_eq!(FailureKind::NotFound.label(), "NOT_FOUND");
        assert_eq!(FailureKind::RegionMismatch.label(), "REGION");
    }

    // ----- ObjectStoreConfigError::step / kind ---------------------------

    #[test]
    fn object_store_config_error_step_and_kind() {
        let unknown = ObjectStoreConfigError::UnknownBackend("x".to_string());
        assert_eq!(unknown.step(), "config");
        assert_eq!(unknown.kind(), FailureKind::Other);

        let auth_missing = ObjectStoreConfigError::AuthEnvVarMissing("VAR".to_string());
        assert_eq!(auth_missing.step(), "config");
        assert_eq!(auth_missing.kind(), FailureKind::Other);

        let init = ObjectStoreConfigError::BackendInit {
            backend: "S3",
            source: crate::ObjectStoreError::Authz("AccessDenied".to_string()),
        };
        assert_eq!(init.step(), "init");
        assert_eq!(init.kind(), FailureKind::Authz);

        let listed = ObjectStoreConfigError::ListFailed {
            bucket: "b".to_string(),
            prefix: "p".to_string(),
            source: crate::ObjectStoreError::NotFound("NoSuchBucket".to_string()),
        };
        assert_eq!(listed.step(), "list");
        assert_eq!(listed.kind(), FailureKind::NotFound);

        let wrote = ObjectStoreConfigError::WriteFailed {
            bucket: "b".to_string(),
            source: crate::ObjectStoreError::Other("boom".to_string()),
        };
        assert_eq!(wrote.step(), "write");

        let deleted = ObjectStoreConfigError::DeleteFailed {
            bucket: "b".to_string(),
            source: crate::ObjectStoreError::Other("boom".to_string()),
        };
        assert_eq!(deleted.step(), "delete");

        let lock = ObjectStoreConfigError::LockStateQueryFailed {
            name: "n".to_string(),
            source: crate::ObjectStoreError::Auth("InvalidAccessKeyId".to_string()),
        };
        assert_eq!(lock.step(), "lock_state");
        assert_eq!(lock.kind(), FailureKind::Auth);

        let mismatch = ObjectStoreConfigError::RetentionMismatch {
            name: "n".to_string(),
            configured: "compliance",
            actual: "off",
            hint: "h",
        };
        assert_eq!(mismatch.step(), "lock_state");
        assert_eq!(mismatch.kind(), FailureKind::Other);
        // Display formatting carries the backend name.
        assert!(format!("{mismatch}").contains("'n'"));
    }

    // ----- reject_legacy_cloud_backends_json -----------------------

    #[test]
    fn reject_legacy_json_passes_when_absent() {
        let temp = tempfile::TempDir::new().unwrap();
        reject_legacy_cloud_backends_json(
            temp.path(),
            std::path::Path::new("/etc/thurvtl/thurvtl.yaml"),
        )
        .expect("absent legacy file is OK");
    }

    #[test]
    fn reject_legacy_json_refuses_when_present() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("cloud-backends.json"), "{}").unwrap();
        let err = reject_legacy_cloud_backends_json(
            temp.path(),
            std::path::Path::new("/etc/thurvtl/thurvtl.yaml"),
        )
        .expect_err("legacy file must refuse start");
        assert!(err.contains("cloud-backends.json"));
        assert!(err.contains("cloud.backends:"));
    }

    // ----- CompressionConfigYaml::to_core --------------------------

    #[test]
    fn compression_yaml_to_core_maps_each_algorithm() {
        let none = CompressionConfigYaml {
            algorithm: CompressionAlgoYaml::None,
            level: 3,
        };
        assert!(!none.to_core().enabled());

        let lz4 = CompressionConfigYaml {
            algorithm: CompressionAlgoYaml::Lz4,
            level: 3,
        };
        assert_eq!(lz4.to_core().algorithm, Some(CompressionAlgo::Lz4));

        let zstd = CompressionConfigYaml {
            algorithm: CompressionAlgoYaml::Zstd,
            level: 9,
        };
        let core = zstd.to_core();
        assert_eq!(core.algorithm, Some(CompressionAlgo::Zstd));
        assert_eq!(core.level, 9);

        // Default is zstd at the standard level.
        let def = CompressionConfigYaml::default();
        assert_eq!(def.algorithm, CompressionAlgoYaml::Zstd);
    }

    // ----- create_backend_named for a local backend ----------------

    #[tokio::test]
    async fn create_backend_named_builds_local_backend() {
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(temp.path());
        let backend = cfg
            .create_backend_named("archive")
            .await
            .expect("local backend builds");
        assert_eq!(backend.backend_type(), "local");
    }

    #[tokio::test]
    async fn create_backend_named_unknown_errors() {
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(temp.path());
        let err = cfg
            .create_backend_named("nope")
            .await
            .expect_err("unknown backend must error");
        assert!(matches!(err, ObjectStoreConfigError::UnknownBackend(_)));
    }

    #[test]
    fn target_label_for_local_is_root_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(temp.path());
        assert_eq!(
            cfg.target_label_named("archive").unwrap(),
            temp.path().to_string_lossy()
        );
        // Local has no object-key prefix.
        assert_eq!(cfg.prefix_named("archive"), "");
        // Unknown name yields no label.
        assert!(cfg.target_label_named("missing").is_none());
    }

    // ----- check_retention_state + probe_data_plane ---------------
    //
    // These are private to the module but reachable from the test
    // submodule. The public `validate_object_store_backend` short-circuits
    // for Local backends, so end-to-end calls don't exercise either
    // function — these tests drive them directly with a real
    // `LocalBackend` (success path: `RetentionMode::None` matches
    // `LockState::Off`; data-plane probe: list / write / delete on
    // disk) and with a fake-lock-state wrapper for the mismatch
    // arms `LocalBackend` can't produce on its own.

    use crate::object_store_backend::LockState as LS;
    use std::path::Path;

    /// Wraps `LocalBackend` but returns a caller-supplied `LockState`
    /// from `lock_state()`. Every other trait method delegates
    /// straight to the inner backend.
    #[derive(Debug)]
    struct FakeLockBackend {
        inner: LocalBackend,
        fake: LS,
    }
    #[async_trait::async_trait]
    impl ObjectStoreBackend for FakeLockBackend {
        async fn upload_chunk(
            &self,
            k: &str,
            d: &[u8],
        ) -> crate::Result<(
            u64,
            Option<u64>,
            Option<crate::compression::CompressionAlgo>,
        )> {
            self.inner.upload_chunk(k, d).await
        }
        async fn upload_chunk_zerocopy(&self, k: &str, p: &Path) -> crate::Result<u64> {
            self.inner.upload_chunk_zerocopy(k, p).await
        }
        async fn download_chunk(&self, k: &str) -> crate::Result<Vec<u8>> {
            self.inner.download_chunk(k).await
        }
        async fn download_chunks_parallel(&self, k: &[String]) -> crate::Result<Vec<Vec<u8>>> {
            self.inner.download_chunks_parallel(k).await
        }
        async fn upload_manifest(&self, k: &str, j: &str) -> crate::Result<()> {
            self.inner.upload_manifest(k, j).await
        }
        async fn download_manifest(&self, k: &str) -> crate::Result<String> {
            self.inner.download_manifest(k).await
        }
        async fn chunk_exists(&self, k: &str) -> crate::Result<bool> {
            self.inner.chunk_exists(k).await
        }
        async fn list_objects(&self, p: &str) -> crate::Result<Vec<String>> {
            self.inner.list_objects(p).await
        }
        async fn delete_object(&self, k: &str) -> crate::Result<()> {
            self.inner.delete_object(k).await
        }
        fn backend_type(&self) -> &'static str {
            "fake-lock"
        }
        async fn lock_state(&self) -> crate::Result<LS> {
            Ok(self.fake)
        }
        async fn set_object_legal_hold(&self, k: &str, h: bool) -> crate::Result<()> {
            self.inner.set_object_legal_hold(k, h).await
        }
        async fn get_object_legal_hold(&self, k: &str) -> crate::Result<bool> {
            self.inner.get_object_legal_hold(k).await
        }
        fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
            Box::new(Self {
                inner: self.inner.clone(),
                fake: self.fake,
            })
        }
    }

    async fn local_backend(dir: &Path) -> LocalBackend {
        LocalBackend::new(dir.to_string_lossy().into_owned())
            .await
            .expect("LocalBackend constructs")
    }

    fn s3_store_config_for_hint_term() -> ObjectStoreConfig {
        // An S3 entry is enough for `backend_entry(name).backend_type()`
        // to return "s3", which steers the hint formatter into the
        // "Object Lock" branch. We never actually touch this backend.
        parse_cfg(
            r#"
backends:
  archive:
    type: s3
    bucket: b
    prefix: ""
    region: us-east-1
"#,
        )
    }

    fn azure_store_config_for_hint_term() -> ObjectStoreConfig {
        parse_cfg(
            r#"
backends:
  archive:
    type: azure
    storage_account: a
    container: c
    prefix: ""
"#,
        )
    }

    #[tokio::test]
    async fn check_retention_state_none_matches_off_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(tmp.path());
        let backend = local_backend(tmp.path()).await;
        let mut steps: Vec<String> = Vec::new();
        check_retention_state(&backend, &cfg, "archive", RetentionMode::None, &mut |s| {
            steps.push(s.name.to_string())
        })
        .await
        .expect("None matches Off");
        assert_eq!(steps, vec!["lock_state".to_string()]);
    }

    #[tokio::test]
    async fn check_retention_state_skip_flag_emits_only_note() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = ObjectStoreConfig {
            skip_retention_mode_check: true,
            ..local_store_config(tmp.path())
        };
        let backend = local_backend(tmp.path()).await;
        let mut details: Vec<String> = Vec::new();
        check_retention_state(
            &backend,
            &cfg,
            "archive",
            RetentionMode::Governance,
            &mut |s| {
                details.push(s.detail);
            },
        )
        .await
        .expect("skip flag bypasses the lock_state comparison");
        assert_eq!(details.len(), 1);
        assert!(details[0].contains("skip_retention_mode_check"));
    }

    #[tokio::test]
    async fn check_retention_state_governance_vs_off_local_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(tmp.path());
        let backend = local_backend(tmp.path()).await;
        let err = check_retention_state(
            &backend,
            &cfg,
            "archive",
            RetentionMode::Governance,
            &mut |_| {},
        )
        .await
        .expect_err("Governance requires a locked bucket");
        assert!(matches!(
            err,
            ObjectStoreConfigError::RetentionMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn check_retention_state_compliance_vs_off_with_s3_hint_term() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = s3_store_config_for_hint_term();
        let backend = local_backend(tmp.path()).await;
        let err = check_retention_state(
            &backend,
            &cfg,
            "archive",
            RetentionMode::Compliance,
            &mut |_| {},
        )
        .await
        .expect_err("Compliance requires a locked bucket");
        if let ObjectStoreConfigError::RetentionMismatch { hint, .. } = err {
            assert!(hint.contains("Object Lock"), "hint = {hint}");
        } else {
            panic!("expected RetentionMismatch");
        }
    }

    #[tokio::test]
    async fn check_retention_state_none_vs_locked_azure_hint_term() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = azure_store_config_for_hint_term();
        let backend = FakeLockBackend {
            inner: local_backend(tmp.path()).await,
            fake: LS::Compliance { default_days: 30 },
        };
        let err =
            check_retention_state(&backend, &cfg, "archive", RetentionMode::None, &mut |_| {})
                .await
                .expect_err("None vs locked bucket is a mismatch");
        if let ObjectStoreConfigError::RetentionMismatch { hint, .. } = err {
            assert!(
                hint.contains("immutability policy"),
                "azure hint term, got {hint}",
            );
        } else {
            panic!("expected RetentionMismatch");
        }
    }

    #[tokio::test]
    async fn check_retention_state_governance_vs_compliance_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = s3_store_config_for_hint_term();
        let backend = FakeLockBackend {
            inner: local_backend(tmp.path()).await,
            fake: LS::Compliance { default_days: 7 },
        };
        let err = check_retention_state(
            &backend,
            &cfg,
            "archive",
            RetentionMode::Governance,
            &mut |_| {},
        )
        .await
        .expect_err("Governance vs Compliance must mismatch");
        if let ObjectStoreConfigError::RetentionMismatch { hint, .. } = err {
            assert!(hint.contains("compliance"), "hint = {hint}");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn check_retention_state_compliance_vs_governance_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = s3_store_config_for_hint_term();
        let backend = FakeLockBackend {
            inner: local_backend(tmp.path()).await,
            fake: LS::Governance { default_days: 7 },
        };
        let err = check_retention_state(
            &backend,
            &cfg,
            "archive",
            RetentionMode::Compliance,
            &mut |_| {},
        )
        .await
        .expect_err("Compliance vs Governance must mismatch");
        assert!(matches!(
            err,
            ObjectStoreConfigError::RetentionMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn check_retention_state_governance_matches_governance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = s3_store_config_for_hint_term();
        let backend = FakeLockBackend {
            inner: local_backend(tmp.path()).await,
            fake: LS::Governance { default_days: 7 },
        };
        let mut steps: Vec<String> = Vec::new();
        check_retention_state(
            &backend,
            &cfg,
            "archive",
            RetentionMode::Governance,
            &mut |s| steps.push(s.detail.clone()),
        )
        .await
        .expect("Governance matches Governance");
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("matches"));
    }

    #[tokio::test]
    async fn probe_data_plane_local_round_trip_lists_writes_deletes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(tmp.path());
        let backend = local_backend(tmp.path()).await;
        let mut steps: Vec<String> = Vec::new();
        probe_data_plane(&backend, &cfg, "archive", RetentionMode::None, &mut |s| {
            steps.push(s.name.to_string());
        })
        .await
        .expect("local probe succeeds");
        assert_eq!(
            steps,
            vec![
                "list".to_string(),
                "write".to_string(),
                "delete".to_string()
            ],
        );
    }

    #[tokio::test]
    async fn probe_data_plane_skips_write_delete_for_locked_bucket() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = local_store_config(tmp.path());
        let backend = local_backend(tmp.path()).await;
        let mut steps: Vec<String> = Vec::new();
        probe_data_plane(
            &backend,
            &cfg,
            "archive",
            RetentionMode::Governance,
            &mut |s| {
                steps.push(s.name.to_string());
            },
        )
        .await
        .expect("locked-mode probe skips write/delete");
        assert_eq!(steps, vec!["list".to_string(), "skip_probe".to_string()]);
    }
}
