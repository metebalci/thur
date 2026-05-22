// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cloud-layer error type.
//!
//! Decoupled from `core-mediachanger::errors::SmcError` so that
//! consumers (`core-block`, future shared-storage, third-party
//! callers) can use the cloud backends without inheriting the
//! whole `SmcError` hierarchy. core-mediachanger provides
//! `From<CloudError> for SmcError` so existing call sites
//! that propagate via `?` continue to work unchanged.
//!
//! Variants intentionally match the four cloud-shaped variants
//! the original `SmcError` carried (`CloudError`,
//! `CloudPreconditionFailed`, `CloudConflict`, `NotSupported`)
//! plus an `Io` passthrough — this keeps the conversion table
//! at the boundary one-to-one.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum CloudError {
    /// Generic cloud failure carrying the raw provider message.
    /// Equivalent to the legacy `SmcError::CloudError(String)`.
    #[error("cloud: {0}")]
    Other(String),

    /// HTTP 412 / structured precondition failure — provider said
    /// the request's preconditions weren't met. Permanent for the
    /// current attempt; the retry classifier maps this to "no point
    /// retrying" (PERMISSION-shaped).
    #[error("cloud precondition failed: {0}")]
    PreconditionFailed(String),

    /// HTTP 409 / structured conflict — concurrent state change
    /// invalidated the request. Often transient; the retry
    /// classifier treats this as retry-eligible.
    #[error("cloud conflict: {0}")]
    Conflict(String),

    /// Backend-supported feature is not implemented for this
    /// concrete backend (e.g. legal-hold ops on the Local backend).
    #[error("cloud op not supported: {0}")]
    NotSupported(String),

    /// Local I/O during cloud operations (e.g. reading the file to
    /// upload, writing the downloaded bytes).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Compression / decompression failure raised by `compress_data`
    /// / `decompress_data`. Carried here because compression lives
    /// in shared-cloud (cloud backends compress before upload).
    #[error("compression: {0}")]
    Compression(String),
}

/// Convenience alias used throughout shared-cloud.
pub type Result<T> = std::result::Result<T, CloudError>;
