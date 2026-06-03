// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Object-store-layer error type.
//!
//! Decoupled from `core-mediachanger::errors::SmcError` so that
//! consumers (`core-block`, `shared-upload-worker`, third-party
//! callers) can use the storage backends without inheriting the
//! whole `SmcError` hierarchy. core-mediachanger provides
//! `From<ObjectStoreError> for SmcError` so existing call sites
//! that propagate via `?` continue to work unchanged.
//!
//! Variants intentionally match the four storage-shaped variants
//! the original `SmcError` carried (`ObjectStoreError`,
//! `ObjectStorePreconditionFailed`, `ObjectStoreConflict`,
//! `NotSupported`) plus an `Io` passthrough — this keeps the
//! conversion table at the boundary one-to-one.
//!
//! The six retry-classification variants (`Auth` / `Authz` /
//! `NotFound` / `RegionMismatch` / `Network` / `Timeout`) are minted by
//! each backend at the SDK-error boundary, where the typed SDK error /
//! HTTP status is still in hand — see each backend's `classify_*` helper.
//! `crate::object_store_config::classify` maps these straight back to a
//! [`crate::object_store_config::FailureKind`] without re-parsing any
//! rendered message string, and [`ObjectStoreError::classified`] is its
//! inverse. `Other` remains the catch-all for genuinely unclassifiable
//! failures (5xx, throttling, local IO that bubbled up as a message).

use crate::object_store_config::FailureKind;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ObjectStoreError {
    /// Generic storage-backend failure carrying the raw provider message.
    /// The catch-all class: retry-eligible, since 5xx / throttling / and
    /// unclassified SDK noise all land here.
    #[error("object store: {0}")]
    Other(String),

    /// Provider rejected the credentials (missing / expired / signature
    /// mismatch). Permanent — fail fast.
    #[error("object store auth: {0}")]
    Auth(String),

    /// Credentials are valid but lack the required permission. Permanent.
    #[error("object store permission: {0}")]
    Authz(String),

    /// The bucket / container / object does not exist. Permanent.
    #[error("object store not found: {0}")]
    NotFound(String),

    /// Bucket is in a different region than configured. Permanent.
    #[error("object store region mismatch: {0}")]
    RegionMismatch(String),

    /// Could not reach the provider at all (DNS / TCP / TLS). Retry-eligible.
    #[error("object store network: {0}")]
    Network(String),

    /// The request timed out. Retry-eligible.
    #[error("object store timeout: {0}")]
    Timeout(String),

    /// HTTP 412 / structured precondition failure — provider said
    /// the request's preconditions weren't met. Permanent for the
    /// current attempt; the retry classifier maps this to "no point
    /// retrying" (PERMISSION-shaped).
    #[error("object store precondition failed: {0}")]
    PreconditionFailed(String),

    /// HTTP 409 / structured conflict — concurrent state change
    /// invalidated the request. Often transient; the retry
    /// classifier treats this as retry-eligible.
    #[error("object store conflict: {0}")]
    Conflict(String),

    /// Backend-supported feature is not implemented for this
    /// concrete backend (e.g. legal-hold ops on the Local backend).
    #[error("object store op not supported: {0}")]
    NotSupported(String),

    /// Local I/O during storage-backend operations (e.g. reading the
    /// file to upload, writing the downloaded bytes).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Compression / decompression failure raised by `compress_data`
    /// / `decompress_data`. Carried here because compression lives
    /// in shared-object-store (storage backends compress before upload).
    #[error("compression: {0}")]
    Compression(String),
}

impl ObjectStoreError {
    /// Build the typed variant matching a backend-computed [`FailureKind`].
    ///
    /// The inverse of [`crate::object_store_config::classify`]: a backend
    /// classifies its structured SDK error into a `FailureKind`, then mints
    /// the carrier variant here, so the retry loop's fail-fast decision
    /// never depends on re-parsing a rendered message.
    pub fn classified(kind: FailureKind, msg: String) -> Self {
        match kind {
            FailureKind::Auth => Self::Auth(msg),
            FailureKind::Authz => Self::Authz(msg),
            FailureKind::NotFound => Self::NotFound(msg),
            FailureKind::RegionMismatch => Self::RegionMismatch(msg),
            FailureKind::Network => Self::Network(msg),
            FailureKind::Timeout => Self::Timeout(msg),
            FailureKind::Other => Self::Other(msg),
        }
    }
}

/// Convenience alias used throughout shared-object-store.
pub type Result<T> = std::result::Result<T, ObjectStoreError>;
