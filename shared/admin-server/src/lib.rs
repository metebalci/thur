// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Admin Unix-socket transport for daemon management APIs.
//!
//! Carries the byte-identical pieces both `thurvtld` and
//! `thurvsad` need:
//!
//! - [`run_admin_server`] — binds the listener, sets chmod 0660,
//!   captures SO_PEERCRED per connection, and serves a caller-built
//!   `axum::Router`. Stateless: every product owns its own router
//!   and state, hands them in, and the server transports them.
//! - [`PeerCred`] — axum extractor for the SO_PEERCRED uid/gid/pid
//!   the accept loop injects into request extensions.
//! - [`JobRegistry`] / [`JobEmitter`] / [`JobHandle`] — long-running
//!   admin job machinery with NDJSON event streaming. Currently
//!   exercised by VTL's `system gc / verify / stats / …`; VSA
//!   inherits the type and can wire it the day it grows long jobs.
//! - [`jobs_router`] — pre-built `Router<S>` with
//!   `POST /api/v1/jobs/:kind` and `GET /api/v1/jobs/:id/events`,
//!   parameterized on a product state type that impls [`HasJobs`]
//!   and a `dispatch` closure that routes by kind.

#![forbid(unsafe_code)]

pub mod jobs;
pub mod peer;
pub mod server;

pub use jobs::{JobEmitter, JobHandle, JobRegistry, JobSummary, SubscriberGuard};
pub use peer::PeerCred;
pub use server::{HasJobs, jobs_router, run_admin_server};

// Re-export the shared wire types so consumers only need to pull
// `shared-admin-server` (the daemon never needs to depend on
// `shared-admin-proto` directly).
pub use shared_admin_proto::{JobAccepted, JobEvent};
