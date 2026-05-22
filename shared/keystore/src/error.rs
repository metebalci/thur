// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Keystore-backend error model.
//!
//! Mirrors `shared_cloud::FailureKind` / `is_retryable` shape so the
//! daemon can apply the same fail-fast posture against permanent
//! errors (`Auth` / `Authz` / `NotFound`) and only burn retry budget
//! on transient classes (`Network` / `Timeout` / `Other`).

use std::path::PathBuf;

use thiserror::Error;

/// Errors returned by any [`super::KeyStoreBackend`] implementation.
///
/// The first three variants (`BadPermissions` / `Malformed` /
/// `InvalidHex`) preserve the local on-disk keystore's diagnostic
/// surface from the pre-trait days. The transport variants
/// (`Network` / `Timeout` / `Auth` / `Authz` / `NotFound`) parallel
/// `shared_cloud::FailureKind` so admin handlers can classify and
/// retry uniformly across keystore + cloud paths.
#[derive(Error, Debug)]
pub enum KeyStoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "key file '{path}' has mode {mode:o}, expected {expected:o} \
         (owner read+write only)"
    )]
    BadPermissions {
        path: PathBuf,
        mode: u32,
        expected: u32,
    },

    #[error(
        "key file '{path}' is malformed: expected 64 hex chars + \
         optional newline, got {got} bytes"
    )]
    Malformed { path: PathBuf, got: usize },

    #[error("key file '{0}' has invalid hex: {1}")]
    InvalidHex(PathBuf, hex::FromHexError),

    /// Credentials missing / expired / signature wrong. Permanent —
    /// retrying without operator intervention won't help.
    #[error("keystore auth: {0}")]
    Auth(String),

    /// Credentials valid but no permission for the operation
    /// (KMS `AccessDeniedException`, Vault 403). Permanent.
    #[error("keystore authz: {0}")]
    Authz(String),

    /// Key id / Vault key name not found at the backend. Permanent —
    /// the operator has to create it or fix the config.
    #[error("keystore not_found: {0}")]
    NotFound(String),

    /// Transport / DNS / TLS / connection error. Transient — worth
    /// retrying after backoff.
    #[error("keystore network: {0}")]
    Network(String),

    /// Request timed out. Transient.
    #[error("keystore timeout: {0}")]
    Timeout(String),

    /// Anything else: 5xx from the backend, unclassified SDK noise,
    /// JSON decode failures on a backend response. Treated as
    /// transient to keep the retry budget aligned with cloud.
    #[error("keystore: {0}")]
    Other(String),
}

/// Coarse classification of a [`KeyStoreError`]. Mirrors
/// `shared_cloud::FailureKind` semantically so daemon handlers can
/// route both error families through the same retry / hint logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStoreFailureKind {
    Network,
    Auth,
    Authz,
    NotFound,
    Timeout,
    Other,
}

impl KeyStoreError {
    /// Bucket this error into a [`KeyStoreFailureKind`].
    pub fn kind(&self) -> KeyStoreFailureKind {
        match self {
            KeyStoreError::Auth(_) => KeyStoreFailureKind::Auth,
            KeyStoreError::Authz(_) => KeyStoreFailureKind::Authz,
            KeyStoreError::NotFound(_) => KeyStoreFailureKind::NotFound,
            KeyStoreError::Network(_) => KeyStoreFailureKind::Network,
            KeyStoreError::Timeout(_) => KeyStoreFailureKind::Timeout,
            KeyStoreError::Io(_)
            | KeyStoreError::BadPermissions { .. }
            | KeyStoreError::Malformed { .. }
            | KeyStoreError::InvalidHex(..)
            | KeyStoreError::Other(_) => KeyStoreFailureKind::Other,
        }
    }

    /// Whether the daemon should retry this error or fail fast.
    /// `Auth` / `Authz` / `NotFound` are permanent; everything else
    /// is worth at least one retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind(),
            KeyStoreFailureKind::Network
                | KeyStoreFailureKind::Timeout
                | KeyStoreFailureKind::Other
        )
    }
}

/// Errors from parsing / dispatching the `keystore.backends:` block of
/// the YAML conffile.
#[derive(Debug, Error)]
pub enum KeyStoreConfigError {
    #[error("backend name '{0}' not defined under `keystore.backends:` in the YAML conffile")]
    UnknownBackend(String),

    #[error(
        "auth env var '{0}' is not set. Define it in /etc/thurvsa/thurvsa.env \
         (loaded by the systemd unit) or as a systemd `Environment=` override."
    )]
    AuthEnvVarMissing(String),

    #[error("failed to initialize {backend} keystore backend '{name}': {source}")]
    BackendInit {
        backend: &'static str,
        name: String,
        #[source]
        source: KeyStoreError,
    },

    #[error(
        "keystore selection ambiguous: `keystore.backends:` defines \
         {choices} ({names}) but --keystore NAME was not provided"
    )]
    SelectionAmbiguous { choices: usize, names: String },

    #[error("`keystore.backends:` is empty — at least one entry is required")]
    NoBackends,

    #[error("I/O error reading {path}: {message}")]
    ConffileIo { path: PathBuf, message: String },

    #[error("failed to parse {path}: {message}")]
    ConffileParse { path: PathBuf, message: String },
}

pub type KeyStoreConfigResult<T> = std::result::Result<T, KeyStoreConfigError>;
