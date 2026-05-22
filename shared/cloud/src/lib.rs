// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared cloud-backend layer — `CloudBackend` trait + S3 / GCS /
//! Azure / Local implementations + retry classification + config
//! schema. Lifted from `core-mediachanger` so both the tape product
//! (thurvtl) and the block product (thurvsa) can consume the same
//! cloud abstraction without one depending on the other.
//!
//! Compression primitives (`CompressionAlgo`, `compress_data`,
//! `decompress_data`, `CompressionConfig`, `DriveCompressionState`)
//! also live here for now — the cloud backends compress before
//! upload, and the chunk-store layer (still in core-mediachanger)
//! shares the same primitives. They'll likely move to a separate
//! `shared-storage` crate when the chunk store is extracted; for
//! now they're co-located here so shared-cloud has no path-deps.

pub mod azure;
pub mod caching;
pub mod cloud_backend;
pub mod cloud_config;
pub mod cloud_helpers;
pub mod compression;
pub mod dedup;
pub mod error;
pub mod gcs;
pub mod local;
pub mod s3;

pub use azure::AzureBackend;
pub use caching::CachingCloudBackend;
pub use cloud_backend::{CloudBackend, LockState};
pub use cloud_config::{
    BackendEntry, CloudCheckStep, CloudConfig, CloudConfigError, FailureKind, RetentionMode,
    classify, is_retryable, reject_legacy_cloud_backends_json, validate_cloud_backend,
};
pub use cloud_helpers::{full_key, retry_async};
pub use compression::{
    COMPRESSION_ALGORITHM_DEFAULT, CompressionAlgo, CompressionConfig, DriveCompressionState,
    ZSTD_DEFAULT_LEVEL, compress_data, decompress_data,
};
pub use dedup::DedupScope;
pub use error::{CloudError, Result};
pub use gcs::GcsBackend;
pub use local::LocalBackend;
pub use s3::S3Backend;
