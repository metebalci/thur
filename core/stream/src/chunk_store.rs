// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Re-export shell for the shared chunk-pool primitives.
//!
//! The actual implementation moved to `shared-pool` in Step 5
//! Milestone 5.A.3 (2026-05-09) — see `shared/pool/src/lib.rs`.
//! `ChunkStore` is now a type alias for `shared_pool::ChunkPool`,
//! so existing internal call sites
//! (`crate::chunk_store::ChunkStore::new`,
//! `core_mediachanger::ChunkStore::object_key_for`, …) resolve unchanged.
//!
//! `From<ChunkPoolError> for SmcError` lives in
//! [`crate::errors`] so the historical `?` propagation through
//! tape-side handlers keeps working.

pub use shared_pool::{ChunkPool as ChunkStore, ChunkPoolError};
