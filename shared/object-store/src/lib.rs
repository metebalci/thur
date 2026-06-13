// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared storage-backend layer — `ObjectStoreBackend` trait + S3 / GCS /
//! Azure / Local implementations + retry classification + config
//! schema. Lifted from `core-mediachanger` so both the tape product
//! (thurvtl) and the block product (thurvsa) can consume the same
//! storage abstraction without one depending on the other.
//!
//! Every backend impl, Local included, follows the object-store access
//! pattern — PUT / GET / HEAD / DELETE on opaque, immutable blobs keyed
//! by hash. The trait name reflects that contract honestly. Operator-
//! facing naming uses the broader "storage backends" umbrella so a
//! future block-shaped backend can join without another rename.
//!
//! Compression primitives (`CompressionAlgo`, `compress_data`,
//! `decompress_data`, `CompressionConfig`, `DriveCompressionState`)
//! also live here for now — the storage backends compress before
//! upload, and the chunk-store layer (still in core-mediachanger)
//! shares the same primitives.

pub mod azure;
pub mod caching;
pub mod compression;
pub mod dedup;
pub mod error;
pub mod gcs;
mod gcs_api;
pub mod local;
pub mod object_store_backend;
pub mod object_store_config;
pub mod object_store_helpers;
pub mod s3;

pub use azure::AzureBackend;
pub use caching::CachingObjectStoreBackend;
pub use compression::{
    COMPRESSION_ALGORITHM_DEFAULT, CompressionAlgo, CompressionConfig, DriveCompressionState,
    ZSTD_DEFAULT_LEVEL, compress_data, compress_data_async, decompress_data, decompress_data_async,
};
pub use dedup::DedupScope;
pub use error::{ObjectStoreError, Result};
pub use gcs::GcsBackend;
pub use local::LocalBackend;
pub use object_store_backend::{LockState, ObjectStoreBackend};
pub use object_store_config::{
    BackendEntry, FailureKind, ObjectStoreCheckStep, ObjectStoreConfig, ObjectStoreConfigError,
    RetentionMode, classify, is_retryable, validate_object_store_backend,
};
pub use object_store_helpers::{full_key, retry_async};
pub use s3::S3Backend;
