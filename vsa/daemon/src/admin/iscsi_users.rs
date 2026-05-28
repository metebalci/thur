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
    AddRequest, ApiError, GrantRequest, ListResponse, NameOnlyRequest, RevokeRequest,
    RotateRequest, UserRow,
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
    // VSA-mandatory: every CHAP user must declare an admission set.
    // Empty / missing `volumes` is refused at the wire — pairs with
    // clap's `required=true` on the CLI side and the daemon-startup
    // filter that drops legacy entries without volumes.
    let names = body
        .volumes
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("at least one --volume required"))?;
    if names.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_admitted_volumes(&state.registry, names)?;
    shared_admin_iscsi::add::<AdminState>(state, peer, body).await
}

pub async fn grant(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<GrantRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    validate_admitted_volumes(&state.registry, &body.volumes)?;
    shared_admin_iscsi::grant::<AdminState>(state, peer, body).await
}

pub async fn revoke(
    state: axum::extract::State<AdminState>,
    peer: shared_admin_server::PeerCred,
    body: axum::Json<RevokeRequest>,
) -> Result<(axum::http::StatusCode, axum::Json<UserRow>), ApiError> {
    // No volume-exists check on revoke — we want operators to be
    // able to revoke names of volumes that have since been destroyed
    // (dangling admission entries). The shared handler validates
    // that the resulting set is non-empty.
    shared_admin_iscsi::revoke::<AdminState>(state, peer, body).await
}

/// VSA-only pre-flight check: every volume name in the admission
/// list must currently resolve to a registered volume. Rejecting
/// unknown names at add / grant time keeps `iscsi-users.json` from
/// accumulating dead admission entries that point at typos. The
/// daemon's VolumeRegistry is the source of truth, so volumes
/// created later that match a previously-rejected name simply
/// require the operator to re-issue the verb.
fn validate_admitted_volumes(
    registry: &std::sync::Arc<crate::registry::VolumeRegistry>,
    names: &[String],
) -> Result<(), ApiError> {
    let mut unknown = Vec::new();
    for n in names {
        if registry.get_by_name(n).is_none() {
            unknown.push(n.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(ApiError::bad_request(format!(
            "unknown volume name(s): {}",
            unknown.join(", ")
        )));
    }
    Ok(())
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
