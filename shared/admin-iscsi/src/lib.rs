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

    /// Notify that user `username`'s admitted-volume set changed
    /// (`add` / `grant` / `revoke` → `Some(new_set)`; `remove` →
    /// `None`). Default no-op — VTL has no admission concept.
    ///
    /// VSA overrides it to keep its in-memory [`AdmissionView`] in
    /// lockstep with the just-saved `iscsi-users.json` and to fan a
    /// REPORTED LUNS DATA HAS CHANGED Unit Attention to that user's
    /// live sessions, so an already-connected initiator re-reads
    /// REPORT LUNS and picks up a newly-granted volume (dynamic
    /// admission — the Kubernetes CSI per-node CHAP model).
    ///
    /// [`AdmissionView`]: shared_iscsi::AdmissionView
    fn on_admission_changed(&self, _username: &str, _volumes: Option<&[String]>) {}
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
    /// Volumes this user is admitted to, by name (VSA only). VTL
    /// ignores. Opaque pass-through at this layer; VSA's
    /// daemon-side admin wrapper validates that each name resolves
    /// to a current volume before persisting.
    #[serde(default)]
    pub volumes: Option<Vec<String>>,
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

/// `iscsi users grant USER --volume V [--volume V ...]` body.
/// Set-union: volumes already in the user's allow-list are no-ops.
/// VSA-only verb; VTL has no admission concept on this field.
#[derive(Debug, Deserialize)]
pub struct GrantRequest {
    pub name: String,
    pub volumes: Vec<String>,
}

/// `iscsi users revoke USER --volume V [--volume V ...]` body.
/// Set-difference: volumes not in the user's allow-list are no-ops.
/// Refuses if the resulting set is empty (use `remove` / `disable`
/// for that). VSA-only verb.
#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub name: String,
    pub volumes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UserRow {
    pub username: String,
    pub mutual_chap: bool,
    pub partition: Option<String>,
    pub volumes: Option<Vec<String>>,
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
            volumes: u.volumes,
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
        volumes: body.volumes.clone(),
        disabled: false,
        in_grace: false,
        previous_expires_at: None,
    };
    file.users.push(UserEntry {
        username: body.username.clone(),
        password: body.password,
        mutual_chap: body.mutual_chap,
        partition: body.partition,
        volumes: body.volumes,
        disabled: false,
        previous_password: None,
        previous_expires_at: None,
    });
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    state.on_admission_changed(&body.username, row.volumes.as_deref());
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
    state.on_admission_changed(&body.name, None);
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
        volumes: user.volumes.clone(),
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

pub async fn grant<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<GrantRequest>,
) -> Result<(StatusCode, Json<UserRow>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    let (path, mut file) = load_users(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let user = file
        .users
        .iter_mut()
        .find(|u| u.username == body.name)
        .ok_or_else(|| ApiError::not_found(format!("user '{}'", body.name)))?;

    let mut current = user.volumes.clone().unwrap_or_default();
    let added: Vec<String> = body
        .volumes
        .iter()
        .filter(|v| !current.iter().any(|c| c == *v))
        .cloned()
        .collect();
    current.extend(added.iter().cloned());
    user.volumes = Some(current);

    let row = UserRow {
        username: user.username.clone(),
        mutual_chap: user.mutual_chap,
        partition: user.partition.clone(),
        volumes: user.volumes.clone(),
        disabled: user.disabled,
        in_grace: user
            .previous_expires_at
            .map(|t| t > Utc::now())
            .unwrap_or(false),
        previous_expires_at: user.previous_expires_at,
    };
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    state.on_admission_changed(&body.name, row.volumes.as_deref());
    audit(
        &state,
        &peer,
        "iscsi.users.grant",
        json!({ "username": body.name, "volumes_added": added }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn revoke<S: IscsiUsersState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<RevokeRequest>,
) -> Result<(StatusCode, Json<UserRow>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    let (path, mut file) = load_users(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let user = file
        .users
        .iter_mut()
        .find(|u| u.username == body.name)
        .ok_or_else(|| ApiError::not_found(format!("user '{}'", body.name)))?;

    let current = user.volumes.clone().unwrap_or_default();
    let removed: Vec<String> = body
        .volumes
        .iter()
        .filter(|v| current.iter().any(|c| c == *v))
        .cloned()
        .collect();
    let next: Vec<String> = current
        .into_iter()
        .filter(|c| !body.volumes.iter().any(|v| v == c))
        .collect();
    if next.is_empty() {
        return Err(ApiError::bad_request(format!(
            "revoke would leave user '{}' with no volumes; use `iscsi users disable` or `remove` instead",
            body.name
        )));
    }
    user.volumes = Some(next);

    let row = UserRow {
        username: user.username.clone(),
        mutual_chap: user.mutual_chap,
        partition: user.partition.clone(),
        volumes: user.volumes.clone(),
        disabled: user.disabled,
        in_grace: user
            .previous_expires_at
            .map(|t| t > Utc::now())
            .unwrap_or(false),
        previous_expires_at: user.previous_expires_at,
    };
    save_users(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    state.on_admission_changed(&body.name, row.volumes.as_deref());
    audit(
        &state,
        &peer,
        "iscsi.users.revoke",
        json!({ "username": body.name, "volumes_removed": removed }),
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

#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use shared_admin_server::PeerCred;
    use shared_iscsi::auth::UserEntry;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[derive(Clone)]
    struct MockState {
        dir: PathBuf,
    }

    impl IscsiUsersState for MockState {
        fn data_dir(&self) -> &Path {
            &self.dir
        }
        fn audit_channel(&self) -> Option<&AuditChannel> {
            None
        }
    }

    fn fresh_state() -> (TempDir, MockState) {
        let tmp = tempfile::tempdir().expect("temp dir");
        let state = MockState {
            dir: tmp.path().to_path_buf(),
        };
        (tmp, state)
    }

    fn peer() -> PeerCred {
        PeerCred {
            uid: 0,
            gid: 0,
            pid: Some(1),
        }
    }

    #[test]
    fn validate_username_rejects_empty_and_too_long() {
        assert!(validate_username("alice").is_ok());
        let err = validate_username("").expect_err("empty");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("empty"));
        let long = "a".repeat(257);
        let err = validate_username(&long).expect_err("too long");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("256"));
    }

    #[test]
    fn validate_password_enforces_the_rfc_3720_floor() {
        assert!(validate_password("123456789012").is_ok()); // exactly 12
        let err = validate_password("short").expect_err("under 12");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("12 bytes"));
    }

    #[test]
    fn api_error_constructors_pick_the_right_status() {
        assert_eq!(ApiError::bad_request("x").status, StatusCode::BAD_REQUEST,);
        assert_eq!(ApiError::not_found("x").status, StatusCode::NOT_FOUND);
        assert_eq!(ApiError::conflict("x").status, StatusCode::CONFLICT);
        assert_eq!(
            ApiError::internal("x").status,
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    #[test]
    fn sweep_expired_previous_clears_expired_grace_passwords() {
        let mut file = IscsiUsersFile::default();
        // No previous_password -> not swept.
        file.users.push(UserEntry {
            username: "fresh".into(),
            password: "long-enough-12".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
            disabled: false,
            previous_password: None,
            previous_expires_at: None,
        });
        // Expired grace -> swept.
        file.users.push(UserEntry {
            username: "expired".into(),
            password: "current-password-1".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
            disabled: false,
            previous_password: Some("old".into()),
            previous_expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
        });
        // Future expiry -> not swept yet.
        file.users.push(UserEntry {
            username: "active".into(),
            password: "current-password-2".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
            disabled: false,
            previous_password: Some("still-valid".into()),
            previous_expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
        });

        let swept = sweep_expired_previous(&mut file);
        assert_eq!(swept, vec!["expired".to_string()]);

        // The expired entry's grace fields are zeroed.
        let expired = file.users.iter().find(|u| u.username == "expired").unwrap();
        assert!(expired.previous_password.is_none());
        assert!(expired.previous_expires_at.is_none());

        // The still-active entry is untouched.
        let active = file.users.iter().find(|u| u.username == "active").unwrap();
        assert_eq!(active.previous_password.as_deref(), Some("still-valid"));
    }

    #[test]
    fn add_request_serde_round_trip() {
        let json =
            r#"{"username":"u","password":"123456789012","mutual_chap":true,"partition":"backup"}"#;
        let req: AddRequest = serde_json::from_str(json).expect("parse");
        assert_eq!(req.username, "u");
        assert_eq!(req.password, "123456789012");
        assert!(req.mutual_chap);
        assert_eq!(req.partition.as_deref(), Some("backup"));
        assert!(req.volumes.is_none());
    }

    #[test]
    fn add_request_with_volumes_serde_round_trip() {
        let json = r#"{"username":"u","password":"123456789012","volumes":["v1","v2"]}"#;
        let req: AddRequest = serde_json::from_str(json).expect("parse");
        assert_eq!(
            req.volumes.as_deref(),
            Some(&["v1".to_string(), "v2".to_string()][..])
        );
    }

    #[test]
    fn name_only_request_round_trip() {
        let req: NameOnlyRequest =
            serde_json::from_value(serde_json::json!({"name": "alice"})).expect("parse");
        assert_eq!(req.name, "alice");
    }

    #[test]
    fn rotate_request_round_trip() {
        let req: RotateRequest = serde_json::from_value(serde_json::json!({
            "name": "u",
            "password": "newpassword-1234",
            "grace_seconds": 600,
        }))
        .expect("parse");
        assert_eq!(req.name, "u");
        assert_eq!(req.password, "newpassword-1234");
        assert_eq!(req.grace_seconds, 600);
    }

    #[test]
    fn target_set_request_round_trip() {
        let req: TargetSetRequest = serde_json::from_value(serde_json::json!({
            "username": "tgt",
            "password": "target-pw-12345",
        }))
        .expect("parse");
        assert_eq!(req.username, "tgt");
        assert_eq!(req.password, "target-pw-12345");
    }

    #[test]
    fn users_path_joins_the_data_dir() {
        let (_t, state) = fresh_state();
        let p = users_path(&state);
        assert!(p.ends_with("iscsi-users.json"));
        assert!(p.starts_with(&state.dir));
    }

    #[test]
    fn save_then_load_round_trips_the_users_file() {
        let (_t, state) = fresh_state();
        let path = users_path(&state);
        let mut file = IscsiUsersFile::default();
        file.users.push(UserEntry {
            username: "alice".into(),
            password: "password-1234".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
            disabled: false,
            previous_password: None,
            previous_expires_at: None,
        });
        save_users(&path, &file).expect("save");

        let (_p, loaded) = load_users(&state).expect("load");
        assert_eq!(loaded.users.len(), 1);
        assert_eq!(loaded.users[0].username, "alice");
    }

    #[tokio::test]
    async fn list_on_an_empty_data_dir_returns_an_empty_user_list() {
        let (_t, state) = fresh_state();
        let resp = list(State(state), peer()).await.expect("list ok");
        assert!(resp.0.users.is_empty());
    }

    #[tokio::test]
    async fn add_then_list_surfaces_the_new_user() {
        let (_t, state) = fresh_state();
        let req = AddRequest {
            username: "alice".into(),
            password: "password-1234".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
        };
        let _ = add(State(state.clone()), peer(), axum::Json(req))
            .await
            .expect("add ok");

        let resp = list(State(state), peer()).await.expect("list ok");
        assert_eq!(resp.0.users.len(), 1);
        assert_eq!(resp.0.users[0].username, "alice");
        assert!(!resp.0.users[0].disabled);
    }

    #[tokio::test]
    async fn add_rejects_a_short_password() {
        let (_t, state) = fresh_state();
        let req = AddRequest {
            username: "alice".into(),
            password: "short".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
        };
        let err = add(State(state), peer(), axum::Json(req))
            .await
            .expect_err("must reject");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    fn add_req(username: &str, password: &str) -> AddRequest {
        AddRequest {
            username: username.to_string(),
            password: password.to_string(),
            mutual_chap: false,
            partition: None,
            volumes: None,
        }
    }

    #[tokio::test]
    async fn add_conflicts_on_a_duplicate_username() {
        let (_t, state) = fresh_state();
        let _ = add(
            State(state.clone()),
            peer(),
            axum::Json(add_req("alice", "password-1234")),
        )
        .await
        .expect("first add ok");
        let err = add(
            State(state),
            peer(),
            axum::Json(add_req("alice", "password-1234")),
        )
        .await
        .expect_err("duplicate");
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn remove_succeeds_for_an_existing_user_and_404s_otherwise() {
        let (_t, state) = fresh_state();
        let add_req = AddRequest {
            username: "alice".into(),
            password: "password-1234".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
        };
        let _ = add(State(state.clone()), peer(), axum::Json(add_req))
            .await
            .expect("add ok");

        // Existing user removes cleanly.
        let _ = remove(
            State(state.clone()),
            peer(),
            axum::Json(NameOnlyRequest {
                name: "alice".into(),
            }),
        )
        .await
        .expect("remove ok");

        // Repeating the remove returns 404.
        let err = remove(
            State(state),
            peer(),
            axum::Json(NameOnlyRequest {
                name: "alice".into(),
            }),
        )
        .await
        .expect_err("repeat");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rotate_cancel_with_no_rotation_in_progress_returns_conflict() {
        // Cancelling a rotation that doesn't exist is a no-op from the
        // operator's POV, but the daemon needs to surface it as 409
        // CONFLICT so the CLI prints the right "no rotation pending"
        // message rather than reporting success.
        let (_t, state) = fresh_state();
        let _ = add(
            State(state.clone()),
            peer(),
            axum::Json(add_req("alice", "password-1234")),
        )
        .await
        .expect("add ok");

        // alice has no previous_password — rotate_cancel must refuse.
        let err = rotate_cancel(
            State(state),
            peer(),
            axum::Json(NameOnlyRequest {
                name: "alice".into(),
            }),
        )
        .await
        .expect_err("must conflict when no rotation pending");
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn rotate_cancel_on_missing_user_returns_404() {
        let (_t, state) = fresh_state();
        let err = rotate_cancel(
            State(state),
            peer(),
            axum::Json(NameOnlyRequest {
                name: "ghost".into(),
            }),
        )
        .await
        .expect_err("must 404 on missing user");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn sweep_expired_previous_clears_at_exact_now_boundary() {
        // The state machine uses `expires <= now`; a row whose
        // expiry has just landed in the current tick must be swept
        // on this same call, not the next one.
        let mut file = IscsiUsersFile::default();
        file.users.push(UserEntry {
            username: "boundary".into(),
            password: "current-password-1".into(),
            mutual_chap: false,
            partition: None,
            volumes: None,
            disabled: false,
            previous_password: Some("old".into()),
            // 1 ms in the past — effectively "now" for the sweep.
            previous_expires_at: Some(Utc::now() - chrono::Duration::milliseconds(1)),
        });
        let swept = sweep_expired_previous(&mut file);
        assert_eq!(swept, vec!["boundary".to_string()]);
        assert!(file.users[0].previous_password.is_none());
    }

    #[test]
    fn load_users_on_malformed_json_surfaces_internal_error() {
        // A corrupted iscsi-users.json on disk must produce an
        // INTERNAL_SERVER_ERROR with a clear message, not a panic.
        let (_t, state) = fresh_state();
        let path = users_path(&state);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not valid json").expect("seed corrupted file");
        let err = load_users(&state).expect_err("must fail on garbage");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn target_set_show_clear_round_trips() {
        let (_t, state) = fresh_state();
        // Nothing set yet -> show returns username=None.
        let shown = target_show(State(state.clone()), peer())
            .await
            .expect("show ok");
        assert!(shown.0.username.is_none());

        target_set(
            State(state.clone()),
            peer(),
            axum::Json(TargetSetRequest {
                username: "tgt".into(),
                password: "target-pw-12345".into(),
            }),
        )
        .await
        .expect("set ok");

        let shown = target_show(State(state.clone()), peer())
            .await
            .expect("show ok");
        assert_eq!(shown.0.username.as_deref(), Some("tgt"));

        target_clear(State(state.clone()), peer())
            .await
            .expect("clear ok");
        let shown = target_show(State(state), peer()).await.expect("show ok");
        assert!(shown.0.username.is_none());
    }
}
