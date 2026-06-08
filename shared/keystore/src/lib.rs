// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Pluggable keystore backends for Thur VSA at-rest volume encryption.
//!
//! Six backends today — `local` (on-disk daemon-side keyfile,
//! preserves the pre-trait shape), `awskms` (AWS KMS envelope
//! encryption), `vault` (HashiCorp Vault Transit), `azurekv` (Azure
//! Key Vault RSA wrap/unwrap), `gcpkms` (GCP Cloud KMS symmetric
//! encrypt/decrypt), `kmip` (KMIP 1.4+ AES-GCM Encrypt/Decrypt against
//! an on-prem HSM / enterprise KMS). All six are reached through the
//! [`KeyStoreBackend`] trait; the daemon picks one per volume at
//! create time and stamps the choice in the manifest's
//! `encryption.keystore_backend`. The wrapped DEK lives in
//! `encryption.wrapped_dek` for non-local backends; `local` leaves
//! that field absent and keeps its sidecar at
//! `<data_dir>/keys/<uuid>.key`.
//!
//! Design / threat model: see `docs/admin/ENCRYPTION.md` § VSA keystore
//! backends.

#![forbid(unsafe_code)]

mod awskms;
mod azurekv;
mod azurekv_api;
mod error;
mod gcpkms;
mod gcpkms_api;
mod keystore_backend;
mod keystore_config;
mod kmip;
mod kmip_wire;
mod local;
pub mod passphrase_envelope;
mod vault;

pub use awskms::AwsKmsBackend;
pub use azurekv::AzureKvBackend;
pub use error::{KeyStoreConfigError, KeyStoreConfigResult, KeyStoreError, KeyStoreFailureKind};
pub use gcpkms::GcpKmsBackend;
pub use keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
pub use keystore_config::{
    AwsKmsAuth, AwsKmsBackendConfig, AzureKvAuth, AzureKvBackendConfig, GcpKmsAuth,
    GcpKmsBackendConfig, KeystoreBackendEntry, KeystoreYamlConfig, KmipBackendConfig, KmipCaBundle,
    KmipCredential, KmipMtls, LocalBackendConfig, ResolvedAwsKmsAuth, ResolvedAzureKvAuth,
    ResolvedGcpKmsAuth, ResolvedKmipCaBundle, ResolvedKmipCredential, ResolvedKmipMtls,
    ResolvedVaultAuth, VaultAuth, VaultBackendConfig,
};
pub use kmip::KmipBackend;
pub use local::{KEYS_SUBDIR, LocalBackend};
pub use vault::VaultBackend;
