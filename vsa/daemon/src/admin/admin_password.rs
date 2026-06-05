// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Thin adapter wiring VSA's [`AdminState`] into the shared
//! `system set-admin-password` handler (`shared_admin_auth::set`). All
//! logic lives in `shared-admin-auth`; this module exposes
//! `AdminState`'s `data_dir` / `audit` / `auth` through the
//! [`AdminPasswordState`] trait and re-exports the handler.

use std::path::Path;

use shared_admin_auth::{AdminPasswordState, AuthState};
use shared_audit::AuditChannel;

use super::handlers::AdminState;

impl AdminPasswordState for AdminState {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn audit_channel(&self) -> Option<&AuditChannel> {
        self.audit.as_ref()
    }

    fn auth(&self) -> &AuthState {
        &self.auth
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
