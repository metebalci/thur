// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Object-store-layer error type.
//!
//! Decoupled from `core-mediachanger::errors::SmcError` so that
//! consumers (`core-block`, future shared-storage layers, third-party
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

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ObjectStoreError {
    /// Generic storage-backend failure carrying the raw provider message.
    #[error("object store: {0}")]
    Other(String),

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

/// Convenience alias used throughout shared-object-store.
pub type Result<T> = std::result::Result<T, ObjectStoreError>;
