// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `keystore:` section of `thurvtl.yaml` / `thurvsa.yaml` parsing +
//! dispatch.
//!
//! Holds the named `keystore.backends:` map. Per-backend auth enums (`AwsKmsAuth`,
//! `VaultAuth`, …) follow the same Env-vs-Static shape `shared-cloud`
//! uses for S3 / Azure — secrets stay out of YAML via `_env` variants
//! populated from `/etc/{thurvtl,thurvsa}/{product}.env`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::awskms::AwsKmsBackend;
use crate::azurekv::AzureKvBackend;
use crate::error::{KeyStoreConfigError, KeyStoreConfigResult};
use crate::gcpkms::GcpKmsBackend;
use crate::keystore_backend::KeyStoreBackend;
use crate::kmip::KmipBackend;
use crate::local::LocalBackend;
use crate::vault::VaultBackend;

/// One named entry under `keystore.backends:` in the YAML conffile.
/// The discriminant is the YAML `type:` field — exactly one of `local`
/// / `awskms` / `vault` / `azurekv` / `gcpkms` / `kmip` per entry,
/// enforced by the enum shape.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum KeystoreBackendEntry {
    Local(LocalBackendConfig),
    Awskms(AwsKmsBackendConfig),
    Vault(VaultBackendConfig),
    Azurekv(AzureKvBackendConfig),
    Gcpkms(GcpKmsBackendConfig),
    Kmip(KmipBackendConfig),
}

impl KeystoreBackendEntry {
    pub fn backend_type(&self) -> &'static str {
        match self {
            KeystoreBackendEntry::Local(_) => "local",
            KeystoreBackendEntry::Awskms(_) => "awskms",
            KeystoreBackendEntry::Vault(_) => "vault",
            KeystoreBackendEntry::Azurekv(_) => "azurekv",
            KeystoreBackendEntry::Gcpkms(_) => "gcpkms",
            KeystoreBackendEntry::Kmip(_) => "kmip",
        }
    }
}

/// `local` backend config. The on-disk side state lives at
/// `<data_dir>/keys/`; this struct is intentionally empty so a
/// minimal entry `{"type": "local"}` parses cleanly.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct LocalBackendConfig {}

/// `awskms` backend config. `key_id` is the CMK identifier the
/// daemon hands to `kms.Encrypt` / `kms.Decrypt` / `kms.DescribeKey`
/// — alias (`alias/foo`), key id, or ARN, whatever the operator
/// prefers. `endpoint_url` overrides the SDK-default endpoint
/// (LocalStack, VPC endpoints).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AwsKmsBackendConfig {
    pub key_id: String,
    pub region: String,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub auth: Option<AwsKmsAuth>,
}

/// AWS KMS auth. Mirrors `shared_cloud::S3Auth` shape — Env variants
/// are the production posture; `Static` is for dev / single-host
/// installs where putting the secret in the JSON is acceptable;
/// `Profile` picks a named profile from `~/.aws/credentials`.
/// When `None` (no `auth` block) the AWS SDK default credential
/// chain is used (env vars → IRSA / web identity → SSO → ECS task
/// role → EC2 IMDS → `~/.aws/credentials`).
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AwsKmsAuth {
    Static {
        access_key_id: String,
        secret_access_key: String,
        #[serde(default)]
        session_token: Option<String>,
    },
    Env {
        access_key_id_env: String,
        secret_access_key_env: String,
        #[serde(default)]
        session_token_env: Option<String>,
    },
    Profile {
        name: String,
    },
}

/// Resolved KMS credentials, post-env-var-lookup.
#[derive(Debug, Clone)]
pub enum ResolvedAwsKmsAuth {
    Static {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    Profile {
        name: String,
    },
}

impl AwsKmsAuth {
    /// Read any `_env` variants and return a [`ResolvedAwsKmsAuth`].
    pub fn resolve(&self) -> KeyStoreConfigResult<ResolvedAwsKmsAuth> {
        match self {
            AwsKmsAuth::Static {
                access_key_id,
                secret_access_key,
                session_token,
            } => Ok(ResolvedAwsKmsAuth::Static {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
            }),
            AwsKmsAuth::Env {
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
                Ok(ResolvedAwsKmsAuth::Static {
                    access_key_id,
                    secret_access_key,
                    session_token,
                })
            }
            AwsKmsAuth::Profile { name } => Ok(ResolvedAwsKmsAuth::Profile { name: name.clone() }),
        }
    }
}

/// `vault` backend config. Talks to a HashiCorp Vault Transit secrets
/// engine at `<address>/v1/<transit_mount>/{encrypt,decrypt,datakey}/<transit_key>`.
/// `namespace` is the Vault Enterprise namespace header (omit for
/// open-source Vault). `tls_skip_verify` is dev-only and refused in
/// release builds at health-check time.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct VaultBackendConfig {
    pub address: String,
    #[serde(default = "default_transit_mount")]
    pub transit_mount: String,
    pub transit_key: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub tls_skip_verify: bool,
    pub auth: VaultAuth,
}

fn default_transit_mount() -> String {
    "transit".to_string()
}

/// Vault auth. `Token` / `TokenEnv` plug in a long-lived static token
/// (root in dev, periodic in prod). `AppRole` / `AppRoleEnv` exchange
/// `(role_id, secret_id)` for a token at backend construction time
/// and lazily refresh on 401/403.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VaultAuth {
    Token {
        value: String,
    },
    TokenEnv {
        env: String,
    },
    AppRole {
        role_id: String,
        secret_id: String,
    },
    AppRoleEnv {
        role_id_env: String,
        secret_id_env: String,
    },
}

/// Resolved Vault credentials, post-env-var-lookup. The backend
/// branches on the variant: static-token modes fail fast on auth
/// errors; AppRole exchanges credentials for a Vault token at
/// construction and lazily re-logs in on 401/403.
#[derive(Debug, Clone)]
pub enum ResolvedVaultAuth {
    Token(String),
    AppRole { role_id: String, secret_id: String },
}

impl VaultAuth {
    pub fn resolve(&self) -> KeyStoreConfigResult<ResolvedVaultAuth> {
        match self {
            VaultAuth::Token { value } => Ok(ResolvedVaultAuth::Token(value.clone())),
            VaultAuth::TokenEnv { env } => Ok(ResolvedVaultAuth::Token(read_env(env)?)),
            VaultAuth::AppRole { role_id, secret_id } => Ok(ResolvedVaultAuth::AppRole {
                role_id: role_id.clone(),
                secret_id: secret_id.clone(),
            }),
            VaultAuth::AppRoleEnv {
                role_id_env,
                secret_id_env,
            } => Ok(ResolvedVaultAuth::AppRole {
                role_id: read_env(role_id_env)?,
                secret_id: read_env(secret_id_env)?,
            }),
        }
    }
}

fn read_env(name: &str) -> KeyStoreConfigResult<String> {
    std::env::var(name).map_err(|_| KeyStoreConfigError::AuthEnvVarMissing(name.to_string()))
}

/// `azurekv` backend config. Targets one RSA key in an Azure Key
/// Vault — the key is identified by `vault_uri` (the data-plane FQDN,
/// `https://<vault>.vault.azure.net`) and a key name. `key_version` is
/// optional; absent = "always use the latest version of the key".
/// Pinning to a version protects future-you against an operator
/// rotating the KEK mid-flight; not pinning lets a rotation pick up
/// without an operator touch.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AzureKvBackendConfig {
    pub vault_uri: String,
    pub key_name: String,
    #[serde(default)]
    pub key_version: Option<String>,
    pub auth: AzureKvAuth,
}

/// Azure Key Vault auth. AAD-only (KV does not support SAS or
/// account-key auth — it always sits behind Entra ID). `Env` variants
/// keep the secret out of JSON; `Static` is for dev / single-host
/// installs where putting the secret in the file is acceptable.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AzureKvAuth {
    /// Service principal inline (tenant + client id + client secret).
    ServicePrincipal {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
    /// Service principal from named env vars (typically populated via
    /// `/etc/thurvsa/thurvsa.env` loaded by the systemd unit).
    ServicePrincipalEnv {
        tenant_id_env: String,
        client_id_env: String,
        client_secret_env: String,
    },
}

/// Resolved Azure KV credentials, post-env-var-lookup.
#[derive(Debug, Clone)]
pub enum ResolvedAzureKvAuth {
    ServicePrincipal {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
}

impl AzureKvAuth {
    pub fn resolve(&self) -> KeyStoreConfigResult<ResolvedAzureKvAuth> {
        match self {
            AzureKvAuth::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => Ok(ResolvedAzureKvAuth::ServicePrincipal {
                tenant_id: tenant_id.clone(),
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
            }),
            AzureKvAuth::ServicePrincipalEnv {
                tenant_id_env,
                client_id_env,
                client_secret_env,
            } => Ok(ResolvedAzureKvAuth::ServicePrincipal {
                tenant_id: read_env(tenant_id_env)?,
                client_id: read_env(client_id_env)?,
                client_secret: read_env(client_secret_env)?,
            }),
        }
    }
}

/// `gcpkms` backend config. Targets one symmetric CryptoKey in Cloud
/// KMS. `key_name` is the full resource name
/// (`projects/P/locations/L/keyRings/R/cryptoKeys/K`); the backend
/// passes it verbatim to `EncryptRequest::name` /
/// `DecryptRequest::name`. The wrap path binds AAD =
/// `hex(volume_uuid)` natively (no envelope wrapper) — KMS rejects
/// mismatching AAD on decrypt.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GcpKmsBackendConfig {
    pub key_name: String,
    #[serde(default)]
    pub auth: Option<GcpKmsAuth>,
}

/// GCP KMS auth. Mirrors shared-cloud's GCS auth posture: either a
/// service-account JSON key file (inline path or env-var-named path)
/// or the Application Default Credentials chain
/// (`GOOGLE_APPLICATION_CREDENTIALS` → `gcloud auth
/// application-default login` → GCE/GKE metadata server). `Adc` is
/// also what `auth = None` falls through to; the explicit variant
/// exists so operators can document the intent in the JSON.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GcpKmsAuth {
    /// Path to a service-account JSON key file inline.
    ServiceAccountKey { path: String },
    /// Path to a service-account JSON key file from a named env var.
    ServiceAccountKeyEnv { env: String },
    /// Application Default Credentials chain. Equivalent to leaving
    /// `auth` unset.
    Adc,
}

/// Resolved GCP KMS credentials, post-env-var-lookup.
#[derive(Debug, Clone)]
pub enum ResolvedGcpKmsAuth {
    ServiceAccountKey { path: String },
    Adc,
}

impl GcpKmsAuth {
    pub fn resolve(&self) -> KeyStoreConfigResult<ResolvedGcpKmsAuth> {
        match self {
            GcpKmsAuth::ServiceAccountKey { path } => {
                Ok(ResolvedGcpKmsAuth::ServiceAccountKey { path: path.clone() })
            }
            GcpKmsAuth::ServiceAccountKeyEnv { env } => Ok(ResolvedGcpKmsAuth::ServiceAccountKey {
                path: read_env(env)?,
            }),
            GcpKmsAuth::Adc => Ok(ResolvedGcpKmsAuth::Adc),
        }
    }
}

/// `kmip` backend config. Wraps the per-volume AES-256 DEK against
/// a long-lived AES KEK held by a KMIP 1.4+ server (Thales
/// CipherTrust, Entrust nShield / KeyControl, Fortanix DSM, Utimaco,
/// HashiCorp Vault Enterprise's KMIP endpoint, IBM SKLM, PyKMIP).
/// `endpoint` is `host:port` (KMIP default port 5696); `kek_uid` is
/// the KMIP Unique Identifier of the long-lived AES KEK provisioned
/// out-of-band by the operator. `server_name` overrides the SNI
/// hostname (defaults to the host portion of `endpoint`); useful
/// when the KMIP server's certificate CN differs from the routable
/// address. `ca_bundle` selects the trust store — defaults to the
/// host's system roots; pin to an explicit bundle for private CAs.
/// `mtls` is mandatory (cert + key); `credential` adds an optional
/// KMIP `Authentication` (UsernameAndPassword) header for servers
/// that require both transport + application auth (Cosmian KMS, some
/// Thales configs).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct KmipBackendConfig {
    pub endpoint: String,
    pub kek_uid: String,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub ca_bundle: Option<KmipCaBundle>,
    pub mtls: KmipMtls,
    #[serde(default)]
    pub credential: Option<KmipCredential>,
}

/// KMIP mTLS transport credentials. `ClientCert` puts paths inline
/// (dev / single-host installs where the JSON already lives next to
/// the cert files); `ClientCertEnv` indirects through env vars so the
/// paths can come from `/etc/thurvsa/thurvsa.env`. The cert + key
/// content stay on disk — we never inline PEM blobs into the JSON.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum KmipMtls {
    ClientCert {
        cert_path: String,
        key_path: String,
    },
    ClientCertEnv {
        cert_path_env: String,
        key_path_env: String,
    },
}

/// Resolved KMIP mTLS credentials, post-env-var-lookup.
#[derive(Debug, Clone)]
pub enum ResolvedKmipMtls {
    ClientCert { cert_path: String, key_path: String },
}

impl KmipMtls {
    pub fn resolve(&self) -> KeyStoreConfigResult<ResolvedKmipMtls> {
        match self {
            KmipMtls::ClientCert {
                cert_path,
                key_path,
            } => Ok(ResolvedKmipMtls::ClientCert {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
            }),
            KmipMtls::ClientCertEnv {
                cert_path_env,
                key_path_env,
            } => Ok(ResolvedKmipMtls::ClientCert {
                cert_path: read_env(cert_path_env)?,
                key_path: read_env(key_path_env)?,
            }),
        }
    }
}

/// Optional KMIP application-layer auth, layered on top of the
/// mandatory mTLS transport. Only `UsernameAndPassword` is supported
/// — the most-deployed KMIP `Credential` type. Other credential
/// types (Device, Attestation, HashedPassword, Ticket) are out of
/// scope for now.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum KmipCredential {
    UsernamePassword {
        username: String,
        password: String,
    },
    UsernamePasswordEnv {
        username_env: String,
        password_env: String,
    },
}

/// Resolved KMIP application credentials, post-env-var-lookup.
#[derive(Debug, Clone)]
pub enum ResolvedKmipCredential {
    UsernamePassword { username: String, password: String },
}

impl KmipCredential {
    pub fn resolve(&self) -> KeyStoreConfigResult<ResolvedKmipCredential> {
        match self {
            KmipCredential::UsernamePassword { username, password } => {
                Ok(ResolvedKmipCredential::UsernamePassword {
                    username: username.clone(),
                    password: password.clone(),
                })
            }
            KmipCredential::UsernamePasswordEnv {
                username_env,
                password_env,
            } => Ok(ResolvedKmipCredential::UsernamePassword {
                username: read_env(username_env)?,
                password: read_env(password_env)?,
            }),
        }
    }
}

/// Trust-store selection for KMIP server cert verification. Defaults
/// to `SystemRoots` (the host's `rustls-native-certs` view); pin to
/// an explicit `Path` for private / corporate CAs.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum KmipCaBundle {
    Path { path: String },
    PathEnv { env: String },
    SystemRoots,
}

#[derive(Debug, Clone)]
pub enum ResolvedKmipCaBundle {
    Path { path: String },
    SystemRoots,
}

impl KmipCaBundle {
    pub fn resolve(&self) -> KeyStoreConfigResult<ResolvedKmipCaBundle> {
        match self {
            KmipCaBundle::Path { path } => Ok(ResolvedKmipCaBundle::Path { path: path.clone() }),
            KmipCaBundle::PathEnv { env } => Ok(ResolvedKmipCaBundle::Path {
                path: read_env(env)?,
            }),
            KmipCaBundle::SystemRoots => Ok(ResolvedKmipCaBundle::SystemRoots),
        }
    }
}

/// Top-level `keystore:` section of `thurvtl.yaml` / `thurvsa.yaml`.
/// Holds the named backend map.
///
/// ```yaml
/// keystore:
///   backends:
///     my-vault: { type: vault, address: ..., transit_key: ..., auth: { ... } }
/// ```
///
/// Empty `backends:` at boot is fine — volume / cartridge ops that
/// reference a backend fail at op time with `UnknownBackend`.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct KeystoreYamlConfig {
    /// Named backend entries. Keyed by operator-chosen name.
    #[serde(default)]
    pub backends: BTreeMap<String, KeystoreBackendEntry>,
}

impl KeystoreYamlConfig {
    /// Parse the YAML conffile at `config_path` and return its
    /// `keystore:` block. Tolerates a missing file (returns
    /// `KeystoreYamlConfig::default()` — empty `backends`, no
    /// default), letting the caller's error message point operators
    /// at the right path. Bubbles I/O and YAML-parse errors up.
    ///
    /// Used by daemon-down CLI paths (`volume key {migrate,export,
    /// import}`) that need the same `keystore.backends:` view the
    /// daemon has but without a running daemon.
    pub fn load_from_conffile(config_path: &Path) -> KeyStoreConfigResult<Self> {
        #[derive(serde::Deserialize)]
        struct KeystoreOnly {
            #[serde(default)]
            keystore: KeystoreYamlConfig,
        }

        let raw = match std::fs::read_to_string(config_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(KeyStoreConfigError::ConffileIo {
                    path: config_path.to_path_buf(),
                    message: e.to_string(),
                });
            }
        };
        let parsed: KeystoreOnly =
            serde_yaml::from_str(&raw).map_err(|e| KeyStoreConfigError::ConffileParse {
                path: config_path.to_path_buf(),
                message: e.to_string(),
            })?;
        Ok(parsed.keystore)
    }

    pub fn backend_names(&self) -> Vec<String> {
        self.backends.keys().cloned().collect()
    }

    /// True iff exactly one backend is configured. Mirrors
    /// `CloudConfig::is_single_backend` — the daemon uses this
    /// to decide whether `--keystore NAME` is optional.
    pub fn is_single_backend(&self) -> bool {
        self.backends.len() == 1
    }

    /// Single backend name when exactly one is configured. Used by
    /// the daemon's selection-inference path when `--keystore` was
    /// not provided.
    pub fn single_backend_name(&self) -> Option<&str> {
        if self.is_single_backend() {
            self.backends.keys().next().map(String::as_str)
        } else {
            None
        }
    }

    pub fn backend_entry(&self, name: &str) -> KeyStoreConfigResult<&KeystoreBackendEntry> {
        self.backends
            .get(name)
            .ok_or_else(|| KeyStoreConfigError::UnknownBackend(name.to_string()))
    }

    /// Build the `KeyStoreBackend` trait object for the named entry.
    /// `data_dir` is needed by [`LocalBackend`] to anchor its on-disk
    /// sidecar; other backends ignore it.
    pub async fn create_backend_named(
        &self,
        name: &str,
        data_dir: &Path,
    ) -> KeyStoreConfigResult<Box<dyn KeyStoreBackend>> {
        let entry = self.backend_entry(name)?;
        match entry {
            KeystoreBackendEntry::Local(_) => {
                Ok(Box::new(LocalBackend::new(data_dir.to_path_buf())))
            }
            KeystoreBackendEntry::Awskms(cfg) => {
                let auth = cfg.auth.as_ref().map(|a| a.resolve()).transpose()?;
                let backend = AwsKmsBackend::new(
                    cfg.key_id.clone(),
                    cfg.region.clone(),
                    cfg.endpoint_url.clone(),
                    auth,
                )
                .await
                .map_err(|source| KeyStoreConfigError::BackendInit {
                    backend: "awskms",
                    name: name.to_string(),
                    source,
                })?;
                Ok(Box::new(backend))
            }
            KeystoreBackendEntry::Vault(cfg) => {
                let auth = cfg.auth.resolve()?;
                let backend = VaultBackend::new(
                    cfg.address.clone(),
                    cfg.transit_mount.clone(),
                    cfg.transit_key.clone(),
                    cfg.namespace.clone(),
                    cfg.tls_skip_verify,
                    auth,
                )
                .await
                .map_err(|source| KeyStoreConfigError::BackendInit {
                    backend: "vault",
                    name: name.to_string(),
                    source,
                })?;
                Ok(Box::new(backend))
            }
            KeystoreBackendEntry::Azurekv(cfg) => {
                let auth = cfg.auth.resolve()?;
                let backend = AzureKvBackend::new(
                    cfg.vault_uri.clone(),
                    cfg.key_name.clone(),
                    cfg.key_version.clone(),
                    auth,
                )
                .await
                .map_err(|source| KeyStoreConfigError::BackendInit {
                    backend: "azurekv",
                    name: name.to_string(),
                    source,
                })?;
                Ok(Box::new(backend))
            }
            KeystoreBackendEntry::Gcpkms(cfg) => {
                let auth = cfg.auth.as_ref().map(|a| a.resolve()).transpose()?;
                let backend = GcpKmsBackend::new(cfg.key_name.clone(), auth)
                    .await
                    .map_err(|source| KeyStoreConfigError::BackendInit {
                        backend: "gcpkms",
                        name: name.to_string(),
                        source,
                    })?;
                Ok(Box::new(backend))
            }
            KeystoreBackendEntry::Kmip(cfg) => {
                let mtls = cfg.mtls.resolve()?;
                let credential = cfg.credential.as_ref().map(|c| c.resolve()).transpose()?;
                let ca_bundle = cfg.ca_bundle.as_ref().map(|c| c.resolve()).transpose()?;
                let backend = KmipBackend::new(
                    cfg.endpoint.clone(),
                    cfg.kek_uid.clone(),
                    cfg.server_name.clone(),
                    ca_bundle,
                    mtls,
                    credential,
                )
                .await
                .map_err(|source| KeyStoreConfigError::BackendInit {
                    backend: "kmip",
                    name: name.to_string(),
                    source,
                })?;
                Ok(Box::new(backend))
            }
        }
    }

    /// Resolve a per-volume keystore selection. Precedence:
    ///   1. explicit `--keystore NAME` (parsed CLI flag),
    ///   2. single-backend inference.
    ///
    /// Returns the resolved name on success.
    pub fn resolve_name<'a>(&'a self, explicit: Option<&'a str>) -> KeyStoreConfigResult<&'a str> {
        if let Some(name) = explicit {
            if !self.backends.contains_key(name) {
                return Err(KeyStoreConfigError::UnknownBackend(name.to_string()));
            }
            return Ok(name);
        }
        if let Some(name) = self.single_backend_name() {
            return Ok(name);
        }
        if self.backends.is_empty() {
            return Err(KeyStoreConfigError::NoBackends);
        }
        Err(KeyStoreConfigError::SelectionAmbiguous {
            choices: self.backends.len(),
            names: self.backends.keys().cloned().collect::<Vec<_>>().join(", "),
        })
    }
}

/// Refuse to start if a legacy `<data_dir>/keystore-backends.json` is
/// still present — pre-alpha.3 installs kept keystore-backend
/// definitions in that file, which has since moved into the YAML
/// conffile under `keystore.backends:`. The daemon halts rather than
/// silently ignore the stale state.
pub fn reject_legacy_keystore_backends_json(
    data_dir: &Path,
    config_path: &Path,
) -> std::result::Result<(), String> {
    let legacy = data_dir.join("keystore-backends.json");
    if !legacy.exists() {
        return Ok(());
    }
    Err(format!(
        "refusing to start: {legacy} exists.\n\
         \n\
         Keystore-backend definitions now live in the YAML conffile.\n\
         Copy each entry from {legacy} into the `keystore.backends:`\n\
         block of {config_path}, then remove {legacy}. The JSON\n\
         shape maps 1:1 to YAML — keys and field names are unchanged.\n\
         \n\
         See /usr/share/doc/<product>/AUTH.md (or docs/AUTH.md\n\
         in source) for the YAML shape per provider.",
        legacy = legacy.display(),
        config_path = config_path.display(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn local_only_cfg() -> KeystoreYamlConfig {
        let mut backends = BTreeMap::new();
        backends.insert(
            "local".to_string(),
            KeystoreBackendEntry::Local(LocalBackendConfig::default()),
        );
        KeystoreYamlConfig { backends }
    }

    #[test]
    fn yaml_round_trip_local_only() {
        let yaml = "\
backends:
  local:
    type: local
";
        let cfg: KeystoreYamlConfig = serde_yaml::from_str(yaml).expect("decode");
        assert_eq!(cfg.backends.len(), 1);
        assert!(matches!(
            cfg.backends.get("local"),
            Some(KeystoreBackendEntry::Local(_))
        ));
        assert!(cfg.is_single_backend());
        assert_eq!(cfg.single_backend_name(), Some("local"));
    }

    #[test]
    fn empty_yaml_decodes_to_default() {
        let cfg: KeystoreYamlConfig = serde_yaml::from_str("").expect("decode");
        assert!(cfg.backends.is_empty());
    }

    #[test]
    fn reject_legacy_no_file_ok() {
        let dir = TempDir::new().unwrap();
        let cfg_path = dir.path().join("thurvsa.yaml");
        reject_legacy_keystore_backends_json(dir.path(), &cfg_path).unwrap();
    }

    #[test]
    fn reject_legacy_with_file_errors() {
        let dir = TempDir::new().unwrap();
        let cfg_path = dir.path().join("thurvsa.yaml");
        let legacy = dir.path().join("keystore-backends.json");
        std::fs::write(&legacy, b"{}").unwrap();
        let err = reject_legacy_keystore_backends_json(dir.path(), &cfg_path).unwrap_err();
        assert!(err.contains("keystore.backends:"));
    }

    #[test]
    fn resolve_name_explicit_wins() {
        let cfg = local_only_cfg();
        assert_eq!(cfg.resolve_name(Some("local")).unwrap(), "local");
    }

    #[test]
    fn resolve_name_single_backend_inference() {
        // Single-backend inference picks `local` when `--keystore`
        // is not given.
        let cfg = local_only_cfg();
        assert_eq!(cfg.resolve_name(None).unwrap(), "local");
    }

    #[test]
    fn resolve_name_unknown_explicit_errors() {
        let cfg = local_only_cfg();
        let err = cfg.resolve_name(Some("nope")).unwrap_err();
        assert!(matches!(err, KeyStoreConfigError::UnknownBackend(_)));
    }

    #[test]
    fn resolve_name_ambiguous_when_multi() {
        let mut cfg = local_only_cfg();
        cfg.backends.insert(
            "second".to_string(),
            KeystoreBackendEntry::Local(LocalBackendConfig::default()),
        );
        let err = cfg.resolve_name(None).unwrap_err();
        assert!(matches!(
            err,
            KeyStoreConfigError::SelectionAmbiguous { .. }
        ));
    }

    #[tokio::test]
    async fn build_local_backend() {
        let dir = TempDir::new().unwrap();
        let cfg = local_only_cfg();
        let backend = cfg.create_backend_named("local", dir.path()).await.unwrap();
        assert_eq!(backend.backend_type(), "local");
        assert!(backend.manages_local_blob());
    }

    #[tokio::test]
    async fn two_local_entries_same_data_dir_share_fingerprint() {
        // The collision the migrate fingerprint check is designed to
        // catch: two differently-named `local` entries that resolve
        // to the same on-disk sidecar location. Backends built from
        // distinct entry names but anchored at the same `data_dir`
        // must compare equal.
        let dir = TempDir::new().unwrap();
        let mut cfg = local_only_cfg();
        cfg.backends.insert(
            "alias".to_string(),
            KeystoreBackendEntry::Local(LocalBackendConfig::default()),
        );
        let a = cfg.create_backend_named("local", dir.path()).await.unwrap();
        let b = cfg.create_backend_named("alias", dir.path()).await.unwrap();
        assert_eq!(a.wrap_target_fingerprint(), b.wrap_target_fingerprint());
    }

    #[test]
    fn awskms_static_resolve_round_trips() {
        let auth = AwsKmsAuth::Static {
            access_key_id: "AKID".into(),
            secret_access_key: "SAK".into(),
            session_token: Some("TOK".into()),
        };
        match auth.resolve().unwrap() {
            ResolvedAwsKmsAuth::Static {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                assert_eq!(access_key_id, "AKID");
                assert_eq!(secret_access_key, "SAK");
                assert_eq!(session_token.as_deref(), Some("TOK"));
            }
            other => panic!("expected Static, got {other:?}"),
        }
    }

    #[test]
    fn vault_token_static_resolves() {
        let auth = VaultAuth::Token {
            value: "s.deadbeef".into(),
        };
        match auth.resolve().unwrap() {
            ResolvedVaultAuth::Token(t) => assert_eq!(t, "s.deadbeef"),
            other => panic!("expected Token, got {other:?}"),
        }
    }

    #[test]
    fn awskms_env_missing_errors() {
        let auth = AwsKmsAuth::Env {
            access_key_id_env: "KEYSTORE_TEST_NEVER_SET".into(),
            secret_access_key_env: "KEYSTORE_TEST_NEVER_SET_2".into(),
            session_token_env: None,
        };
        let err = auth.resolve().unwrap_err();
        assert!(matches!(err, KeyStoreConfigError::AuthEnvVarMissing(_)));
    }

    #[test]
    fn azurekv_service_principal_resolves() {
        let auth = AzureKvAuth::ServicePrincipal {
            tenant_id: "T".into(),
            client_id: "C".into(),
            client_secret: "S".into(),
        };
        match auth.resolve().unwrap() {
            ResolvedAzureKvAuth::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => {
                assert_eq!(tenant_id, "T");
                assert_eq!(client_id, "C");
                assert_eq!(client_secret, "S");
            }
        }
    }

    #[test]
    fn azurekv_env_missing_errors() {
        let auth = AzureKvAuth::ServicePrincipalEnv {
            tenant_id_env: "AKV_TEST_NEVER_SET".into(),
            client_id_env: "AKV_TEST_NEVER_SET_2".into(),
            client_secret_env: "AKV_TEST_NEVER_SET_3".into(),
        };
        let err = auth.resolve().unwrap_err();
        assert!(matches!(err, KeyStoreConfigError::AuthEnvVarMissing(_)));
    }

    #[test]
    fn gcpkms_adc_default_resolves() {
        let auth = GcpKmsAuth::Adc;
        match auth.resolve().unwrap() {
            ResolvedGcpKmsAuth::Adc => {}
            other => panic!("expected Adc, got {other:?}"),
        }
    }

    #[test]
    fn gcpkms_service_account_key_env_missing_errors() {
        let auth = GcpKmsAuth::ServiceAccountKeyEnv {
            env: "GCP_KMS_TEST_NEVER_SET".into(),
        };
        let err = auth.resolve().unwrap_err();
        assert!(matches!(err, KeyStoreConfigError::AuthEnvVarMissing(_)));
    }

    #[test]
    fn yaml_decodes_all_six_backend_types() {
        // Verify every per-type tag round-trips through serde_yaml.
        // (JSON-is-valid-YAML, so this covers operator JSON-paste too.)
        let yaml = r#"
default: local
backends:
  local:
    type: local
  kms:
    type: awskms
    key_id: alias/foo
    region: us-east-1
  vlt:
    type: vault
    address: https://vault.example.com
    transit_key: thurvsa
    auth:
      type: token
      value: s.devroot
  akv:
    type: azurekv
    vault_uri: https://kv.example.vault.azure.net
    key_name: thurvsa-kek
    auth:
      type: service_principal
      tenant_id: T
      client_id: C
      client_secret: S
  gcp:
    type: gcpkms
    key_name: projects/p/locations/global/keyRings/thur/cryptoKeys/thurvsa
    auth:
      type: adc
  kmip:
    type: kmip
    endpoint: kms.corp.example:5696
    kek_uid: kek-1
    ca_bundle:
      type: path
      path: /etc/thurvsa/kmip-ca.crt
    mtls:
      type: client_cert
      cert_path: /etc/thurvsa/kmip-client.crt
      key_path: /etc/thurvsa/kmip-client.key
    credential:
      type: username_password
      username: thurvsa
      password: s3cr3t
"#;
        let cfg: KeystoreYamlConfig = serde_yaml::from_str(yaml).expect("decode");
        assert_eq!(cfg.backends.len(), 6);
        assert_eq!(cfg.backend_entry("local").unwrap().backend_type(), "local");
        assert_eq!(cfg.backend_entry("kms").unwrap().backend_type(), "awskms");
        assert_eq!(cfg.backend_entry("vlt").unwrap().backend_type(), "vault");
        assert_eq!(cfg.backend_entry("akv").unwrap().backend_type(), "azurekv");
        assert_eq!(cfg.backend_entry("gcp").unwrap().backend_type(), "gcpkms");
        assert_eq!(cfg.backend_entry("kmip").unwrap().backend_type(), "kmip");
    }

    #[test]
    fn kmip_mtls_client_cert_resolves() {
        let mtls = KmipMtls::ClientCert {
            cert_path: "/tmp/c.crt".into(),
            key_path: "/tmp/c.key".into(),
        };
        match mtls.resolve().unwrap() {
            ResolvedKmipMtls::ClientCert {
                cert_path,
                key_path,
            } => {
                assert_eq!(cert_path, "/tmp/c.crt");
                assert_eq!(key_path, "/tmp/c.key");
            }
        }
    }

    #[test]
    fn kmip_mtls_env_missing_errors() {
        let mtls = KmipMtls::ClientCertEnv {
            cert_path_env: "KMIP_TEST_NEVER_SET".into(),
            key_path_env: "KMIP_TEST_NEVER_SET_2".into(),
        };
        let err = mtls.resolve().unwrap_err();
        assert!(matches!(err, KeyStoreConfigError::AuthEnvVarMissing(_)));
    }

    #[test]
    fn kmip_credential_username_password_resolves() {
        let cred = KmipCredential::UsernamePassword {
            username: "alice".into(),
            password: "s3cr3t".into(),
        };
        match cred.resolve().unwrap() {
            ResolvedKmipCredential::UsernamePassword { username, password } => {
                assert_eq!(username, "alice");
                assert_eq!(password, "s3cr3t");
            }
        }
    }

    #[test]
    fn kmip_ca_bundle_system_roots_resolves() {
        match KmipCaBundle::SystemRoots.resolve().unwrap() {
            ResolvedKmipCaBundle::SystemRoots => {}
            other => panic!("expected SystemRoots, got {other:?}"),
        }
    }

    #[test]
    fn kmip_ca_bundle_path_resolves() {
        let b = KmipCaBundle::Path {
            path: "/etc/ca.crt".into(),
        };
        match b.resolve().unwrap() {
            ResolvedKmipCaBundle::Path { path } => assert_eq!(path, "/etc/ca.crt"),
            other => panic!("expected Path, got {other:?}"),
        }
    }
}
