// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Thin adapter wiring VSA's [`AdminState`] into the shared
//! `iscsi-users.json` admin handlers (`shared_admin_iscsi`). All
//! business logic, wire types, audit op names and the `ApiError`
//! type live in `shared-admin-iscsi`; this module just plumbs
//! `data_dir` + `audit` through the [`IscsiUsersState`] trait and
//! re-exports the handlers so the router wiring in
//! `crate::admin::mod` stays unchanged.
//!
//! The mutual-CHAP target verbs live in [`super::iscsi_target`] for
//! VSA (historical split); they call into the same shared
//! `users_path` helper so the wire shape stays in sync.

use std::path::Path;

use shared_admin_iscsi::IscsiUsersState;
use shared_audit::AuditChannel;

use super::handlers::AdminState;

impl IscsiUsersState for AdminState {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn audit_channel(&self) -> Option<&AuditChannel> {
        self.audit.as_ref()
    }
}

pub use shared_admin_iscsi::{
    AddRequest, ApiError, ListResponse, NameOnlyRequest, RotateRequest, UserRow,
};

pub async fn list(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
) -> Result<axum::Json<ListResponse>, ApiError> {
    shared_admin_iscsi::list::<AdminState>(state, peer).await
}

pub async fn add(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<AddRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    shared_admin_iscsi::add::<AdminState>(state, peer, body).await
}

pub async fn remove(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::remove::<AdminState>(state, peer, body).await
}

pub async fn disable(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::disable::<AdminState>(state, peer, body).await
}

pub async fn enable(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::enable::<AdminState>(state, peer, body).await
}

pub async fn rotate(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<RotateRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    shared_admin_iscsi::rotate::<AdminState>(state, peer, body).await
}

pub async fn rotate_cancel(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<NameOnlyRequest>,
) -> Result<axum::http::StatusCode, ApiError> {
    shared_admin_iscsi::rotate_cancel::<AdminState>(state, peer, body).await
}
