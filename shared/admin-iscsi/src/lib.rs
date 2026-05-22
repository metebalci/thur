// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Admin handlers for iSCSI CHAP user lifecycle (`add` / `remove` /
//! `disable` / `enable` / `rotate` / `rotate cancel` / `list`) and
//! the mutual-CHAP target credential (`target {set, clear, show}`).
//!
//! Cross-product: both `thurvtld` and `thurvsad` mount
//! the same axum routes on the same wire shapes. Each daemon's
//! `AdminState` impls [`IscsiUsersState`] to plumb `data_dir` and
//! the optional [`AuditChannel`]; everything else is shared. Audit
//! op names are pinned (`iscsi.users.add` / `iscsi.users.rotate.start` /
//! `iscsi.target.set` / etc.) so a multi-product audit chain reads
//! uniformly.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared_admin_server::PeerCred;
use shared_audit::{AuditActor, AuditChannel, AuditResult};
use shared_iscsi::auth::{IscsiUsersFile, UserEntry};

/// Per-daemon glue: where the on-disk `iscsi-users.json` lives, and
/// where to forward audit rows. Both `thurvtld::AdminState`
/// and `thurvsad::AdminState` impl this so the handlers below
/// are product-agnostic.
pub trait IscsiUsersState: Clone + Send + Sync + 'static {
    fn data_dir(&self) -> &Path;
    fn audit_channel(&self) -> Option<&AuditChannel>;
}

// ---------- request/response types ----------

#[derive(Debug, Deserialize)]
pub struct AddRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub mutual_chap: bool,
    #[serde(default)]
    pub partition: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NameOnlyRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct RotateRequest {
    pub name: String,
    pub password: String,
    pub grace_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct UserRow {
    pub username: String,
    pub mutual_chap: bool,
    pub partition: Option<String>,
    pub disabled: bool,
    pub in_grace: bool,
    pub previous_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub users: Vec<UserRow>,
}

#[derive(Debug, Deserialize)]
pub struct TargetSetRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct TargetShowResponse {
    pub username: Option<String>,
    pub password_set: bool,
}

// ---------- iSCSI user handlers ----------

pub async fn list<S: IscsiUsersState>(
    State(state): State<S>,
    _peer: PeerCred,
) -> Result<Json<ListResponse>, ApiError> {
    let (_, file) = load_users(&state)?;
    let now = Utc::now();
    let users = file
        .users
        .into_iter()
        .map(|u| UserRow {
            username: u.username,
            mutual_chap: u.mutual_chap,
            partition: u.partition,
            disabled: u.disabled,
            in_grace: u.previous_expires_at.map(|t| t > now).unwrap_or(false),
            previous_expires_at: u.previous_expires_at,
        })
        .collect();
    Ok(Json(ListResponse { users }))
}

pub async fn add<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<AddRequest>,
) -> Result<(StatusCode, Json<UserRow>), ApiError> {
    validate_username(&body.username)?;
    validate_password(&body.password)?;

    let (path, mut file) = load_users(&state)?;
    let swept = sweep_expired_previous(&mut file);

    if file.users.iter().any(|u| u.username == body.username) {
        return Err(ApiError::conflict(format!(
            "user '{}' already exists",
            body.username
        )));
    }

    let row = UserRow {
        username: body.username.clone(),
        mutual_chap: body.mutual_chap,
        partition: body.partition.clone(),
        disabled: false,
        in_grace: false,
        previous_expires_at: None,
    };
    file.users.push(UserEntry {
        username: body.username.clone(),
        password: body.password,
        mutual_chap: body.mutual_chap,
        partition: body.partition,
        disabled: false,
        previous_password: None,
        previous_expires_at: None,
    });
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "iscsi.users.add",
        json!({ "username": body.username }),
    );
    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn remove<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<NameOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_users(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let before = file.users.len();
    file.users.retain(|u| u.username != body.name);
    if file.users.len() == before {
        return Err(ApiError::not_found(format!("user '{}'", body.name)));
    }
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "iscsi.users.remove",
        json!({ "username": body.name }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn disable<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<NameOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    toggle_disabled(state, peer, body.name, true).await
}

pub async fn enable<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<NameOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    toggle_disabled(state, peer, body.name, false).await
}

async fn toggle_disabled<S: IscsiUsersState>(
    state: S,
    peer: PeerCred,
    name: String,
    disabled: bool,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_users(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let user = file
        .users
        .iter_mut()
        .find(|u| u.username == name)
        .ok_or_else(|| ApiError::not_found(format!("user '{}'", name)))?;
    user.disabled = disabled;
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        if disabled {
            "iscsi.users.disable"
        } else {
            "iscsi.users.enable"
        },
        json!({ "username": name }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rotate<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<RotateRequest>,
) -> Result<(StatusCode, Json<UserRow>), ApiError> {
    validate_password(&body.password)?;
    if body.grace_seconds == 0 {
        return Err(ApiError::bad_request(
            "grace_seconds must be > 0; use add + remove for an immediate cutover",
        ));
    }

    let (path, mut file) = load_users(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let user = file
        .users
        .iter_mut()
        .find(|u| u.username == body.name)
        .ok_or_else(|| ApiError::not_found(format!("user '{}'", body.name)))?;

    if user.previous_password.is_some() && user.previous_expires_at.is_some() {
        return Err(ApiError::conflict(format!(
            "user '{}' already has a pending rotation; cancel it first",
            body.name
        )));
    }

    let expires = Utc::now() + ChronoDuration::seconds(body.grace_seconds as i64);
    let old_password = std::mem::replace(&mut user.password, body.password);
    user.previous_password = Some(old_password);
    user.previous_expires_at = Some(expires);

    let row = UserRow {
        username: user.username.clone(),
        mutual_chap: user.mutual_chap,
        partition: user.partition.clone(),
        disabled: user.disabled,
        in_grace: true,
        previous_expires_at: Some(expires),
    };
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "iscsi.users.rotate.start",
        json!({
            "username": body.name,
            "grace_seconds": body.grace_seconds,
            "previous_expires_at": expires,
        }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn rotate_cancel<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<NameOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_users(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let user = file
        .users
        .iter_mut()
        .find(|u| u.username == body.name)
        .ok_or_else(|| ApiError::not_found(format!("user '{}'", body.name)))?;
    let prev = user.previous_password.take().ok_or_else(|| {
        ApiError::conflict(format!("no rotation in progress for user '{}'", body.name))
    })?;
    user.previous_expires_at = None;
    user.password = prev;
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "iscsi.users.rotate.cancel",
        json!({ "username": body.name }),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------- target (mutual-CHAP credential) handlers ----------

pub async fn target_show<S: IscsiUsersState>(
    State(state): State<S>,
    _peer: PeerCred,
) -> Result<Json<TargetShowResponse>, ApiError> {
    let (_, file) = load_users(&state)?;
    Ok(Json(TargetShowResponse {
        username: file.target_username,
        password_set: file.target_password.is_some(),
    }))
}

pub async fn target_set<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<TargetSetRequest>,
) -> Result<StatusCode, ApiError> {
    if body.username.is_empty() {
        return Err(ApiError::bad_request("username must not be empty"));
    }
    if body.password.len() < 12 {
        return Err(ApiError::bad_request(
            "password must be at least 12 bytes (RFC 3720 §11.1.4)",
        ));
    }
    let (path, mut file) = load_users(&state)?;
    file.target_username = Some(body.username.clone());
    file.target_password = Some(body.password);
    save_users(&path, &file)?;
    audit(
        &state,
        &peer,
        "iscsi.target.set",
        json!({ "username": body.username }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn target_clear<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_users(&state)?;
    file.target_username = None;
    file.target_password = None;
    save_users(&path, &file)?;
    audit(&state, &peer, "iscsi.target.clear", json!({}));
    Ok(StatusCode::NO_CONTENT)
}

// ---------- helpers ----------

/// On-disk path of `iscsi-users.json` for the given state. Public so
/// daemons that split target handlers into their own module can share
/// the path resolution.
pub fn users_path<S: IscsiUsersState>(state: &S) -> PathBuf {
    state.data_dir().join("iscsi-users.json")
}

fn load_users<S: IscsiUsersState>(state: &S) -> Result<(PathBuf, IscsiUsersFile), ApiError> {
    let path = users_path(state);
    let file = IscsiUsersFile::load(&path)
        .map_err(|e| ApiError::internal(format!("loading {}: {}", path.display(), e)))?;
    Ok((path, file))
}

fn save_users(path: &Path, file: &IscsiUsersFile) -> Result<(), ApiError> {
    file.save(path)
        .map_err(|e| ApiError::internal(format!("saving {}: {}", path.display(), e)))
}

fn sweep_expired_previous(file: &mut IscsiUsersFile) -> Vec<String> {
    let now = Utc::now();
    let mut swept = Vec::new();
    for u in &mut file.users {
        if let Some(expires) = u.previous_expires_at
            && expires <= now
        {
            u.previous_password = None;
            u.previous_expires_at = None;
            swept.push(u.username.clone());
        }
    }
    swept
}

fn emit_sweep_audits<S: IscsiUsersState>(state: &S, peer: &PeerCred, swept: Vec<String>) {
    for username in swept {
        audit(
            state,
            peer,
            "iscsi.users.rotate.commit",
            json!({ "username": username, "reason": "grace_expired" }),
        );
    }
}

fn audit<S: IscsiUsersState>(state: &S, peer: &PeerCred, op: &str, params: serde_json::Value) {
    if let Some(c) = state.audit_channel() {
        c.try_append(
            op,
            AuditActor::cli(peer.audit_descriptor()),
            params,
            AuditResult::Ok,
        );
    }
}

fn validate_username(s: &str) -> Result<(), ApiError> {
    if s.is_empty() {
        return Err(ApiError::bad_request("username must not be empty"));
    }
    if s.len() > 256 {
        return Err(ApiError::bad_request("username exceeds 256 bytes"));
    }
    Ok(())
}

fn validate_password(s: &str) -> Result<(), ApiError> {
    if s.len() < 12 {
        return Err(ApiError::bad_request(
            "password must be at least 12 bytes (RFC 3720 §11.1.4)",
        ));
    }
    Ok(())
}

// ---------- error type ----------

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
