// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Admin handlers for the mutual-CHAP target credential
//! (`iscsi-users.json` top-level `target_username` / `target_password`).
//! Singleton, not a list — verbs are `set` / `clear` / `show` rather
//! than `add` / `remove`.
//!
//! Thin trampoline over `shared_admin_iscsi::target_*` — the shared
//! crate holds the on-disk layout + audit op names so VTL and VSA
//! emit identical wire shapes.

use shared_admin_iscsi::{ApiError, TargetSetRequest, TargetShowResponse};

use super::handlers::AdminState;

pub async fn show(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
) -> Result<axum::Json<TargetShowResponse>, ApiError> {
    shared_admin_iscsi::target_show::<AdminState>(state, peer).await
}

pub async fn set(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<TargetSetRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::target_set::<AdminState>(state, peer, body).await
}

pub async fn clear(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::target_clear::<AdminState>(state, peer).await
}
