// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Re-export shell for the shared chunk-pool primitives.
//!
//! The actual implementation moved to `shared-pool` in Step 5
//! Milestone 5.A.3 (2026-05-09). Existing call sites
//! `crate::chunk_pool::ChunkPool` etc. resolve via the `pub use`
//! re-export here so the dispatcher arms compile unchanged.

pub use shared_pool::{ChunkPool, ChunkPoolError};
