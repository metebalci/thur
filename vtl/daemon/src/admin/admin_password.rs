// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Thin adapter wiring VTL's [`AdminState`] into the shared
//! `system set-admin-password` handler (`shared_admin_auth::set`). All
//! logic — hashing, the on-disk store, the live `AuthState` hot-swap,
//! the audit row — lives in `shared-admin-auth`; this module just
//! exposes `AdminState`'s `data_dir` / `audit_channel` / `auth` through
//! the [`AdminPasswordState`] trait and re-exports the handler so the
//! router wiring stays uniform with `iscsi_users`.

use std::path::Path;

use core_mediachanger::AuditChannel;
use shared_admin_auth::{AdminPasswordState, AuthState};

use super::handlers::AdminState;

impl AdminPasswordState for AdminState {
    fn data_dir(&self) -> &Path {
        &self.daemon.data_dir
    }

    fn audit_channel(&self) -> Option<&AuditChannel> {
        self.daemon.audit_log.as_ref()
    }

    fn auth(&self) -> &AuthState {
        &self.daemon.auth
    }
}

pub use shared_admin_auth::{ApiError, SetRequest};

pub async fn set(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<SetRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_auth::set::<AdminState>(state, peer, body).await
}
