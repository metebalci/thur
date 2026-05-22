// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Admin handlers for NVMe-TCP TLS-PSK lifecycle verbs:
//! `add` / `remove` / `disable` / `enable` / `rotate` /
//! `rotate cancel` and `list`. Mirror of the iSCSI users surface
//! ([`super::iscsi_users`]) but the entry key is host NQN instead of
//! username, and `interchange_key` replaces `password`.
//!
//! VSA-only — VTL doesn't carry an NVMe-TCP transport.

use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use nvme_tcp::identity::{NvmetcpPsksFile, PskEntry};
use nvme_tcp::tls::parse_interchange_key;
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared_admin_server::PeerCred;
use shared_audit::{AuditActor, AuditResult};
use std::path::{Path, PathBuf};

use super::handlers::AdminState;
use super::iscsi_users::ApiError;

// ---------- request/response types ----------

#[derive(Debug, Deserialize)]
pub struct AddRequest {
    pub host_nqn: String,
    pub interchange_key: String,
}

#[derive(Debug, Deserialize)]
pub struct HostNqnOnlyRequest {
    pub host_nqn: String,
}

#[derive(Debug, Deserialize)]
pub struct RotateRequest {
    pub host_nqn: String,
    pub interchange_key: String,
    pub grace_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct PskRow {
    pub host_nqn: String,
    pub disabled: bool,
    pub in_grace: bool,
    pub previous_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub psks: Vec<PskRow>,
}

// ---------- handlers ----------

pub async fn list(
    State(state): State<AdminState>,
    _peer: PeerCred,
) -> Result<Json<ListResponse>, ApiError> {
    let (_, file) = load_psks(&state)?;
    let now = Utc::now();
    let psks = file
        .psks
        .into_iter()
        .map(|p| PskRow {
            host_nqn: p.host_nqn,
            disabled: p.disabled,
            in_grace: p.previous_expires_at.map(|t| t > now).unwrap_or(false),
            previous_expires_at: p.previous_expires_at,
        })
        .collect();
    Ok(Json(ListResponse { psks }))
}

pub async fn add(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<AddRequest>,
) -> Result<(StatusCode, Json<PskRow>), ApiError> {
    validate_host_nqn(&body.host_nqn)?;
    validate_interchange_key(&body.interchange_key)?;

    let (path, mut file) = load_psks(&state)?;
    let swept = sweep_expired_previous(&mut file);

    if file.psks.iter().any(|p| p.host_nqn == body.host_nqn) {
        return Err(ApiError::conflict(format!(
            "PSK for host_nqn '{}' already exists",
            body.host_nqn
        )));
    }

    let row = PskRow {
        host_nqn: body.host_nqn.clone(),
        disabled: false,
        in_grace: false,
        previous_expires_at: None,
    };
    file.psks.push(PskEntry {
        host_nqn: body.host_nqn.clone(),
        interchange_key: body.interchange_key,
        disabled: false,
        previous_interchange_key: None,
        previous_expires_at: None,
    });
    save_psks(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.psks.add",
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn remove(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_psks(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let before = file.psks.len();
    file.psks.retain(|p| p.host_nqn != body.host_nqn);
    if file.psks.len() == before {
        return Err(ApiError::not_found(format!(
            "PSK for host_nqn '{}'",
            body.host_nqn
        )));
    }
    save_psks(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.psks.remove",
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn disable(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    toggle_disabled(state, peer, body.host_nqn, true).await
}

pub async fn enable(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    toggle_disabled(state, peer, body.host_nqn, false).await
}

async fn toggle_disabled(
    state: AdminState,
    peer: PeerCred,
    host_nqn: String,
    disabled: bool,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_psks(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .psks
        .iter_mut()
        .find(|p| p.host_nqn == host_nqn)
        .ok_or_else(|| ApiError::not_found(format!("PSK for host_nqn '{}'", host_nqn)))?;
    entry.disabled = disabled;
    save_psks(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        if disabled {
            "nvmetcp.psks.disable"
        } else {
            "nvmetcp.psks.enable"
        },
        json!({ "host_nqn": host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rotate(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<RotateRequest>,
) -> Result<(StatusCode, Json<PskRow>), ApiError> {
    validate_interchange_key(&body.interchange_key)?;
    if body.grace_seconds == 0 {
        return Err(ApiError::bad_request(
            "grace_seconds must be > 0; use add + remove for an immediate cutover",
        ));
    }

    let (path, mut file) = load_psks(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .psks
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| ApiError::not_found(format!("PSK for host_nqn '{}'", body.host_nqn)))?;

    if entry.previous_interchange_key.is_some() && entry.previous_expires_at.is_some() {
        return Err(ApiError::conflict(format!(
            "PSK for '{}' already has a pending rotation; cancel it first",
            body.host_nqn
        )));
    }

    let expires = Utc::now() + ChronoDuration::seconds(body.grace_seconds as i64);
    let old_key = std::mem::replace(&mut entry.interchange_key, body.interchange_key);
    entry.previous_interchange_key = Some(old_key);
    entry.previous_expires_at = Some(expires);

    let row = PskRow {
        host_nqn: entry.host_nqn.clone(),
        disabled: entry.disabled,
        in_grace: true,
        previous_expires_at: Some(expires),
    };
    save_psks(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.psks.rotate.start",
        json!({
            "host_nqn": body.host_nqn,
            "grace_seconds": body.grace_seconds,
            "previous_expires_at": expires,
        }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn rotate_cancel(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_psks(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .psks
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| ApiError::not_found(format!("PSK for host_nqn '{}'", body.host_nqn)))?;
    let prev = entry.previous_interchange_key.take().ok_or_else(|| {
        ApiError::conflict(format!(
            "no rotation in progress for PSK '{}'",
            body.host_nqn
        ))
    })?;
    entry.previous_expires_at = None;
    entry.interchange_key = prev;
    save_psks(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.psks.rotate.cancel",
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------- helpers ----------

fn psks_path(state: &AdminState) -> PathBuf {
    state.data_dir.join("nvmetcp-psks.json")
}

fn load_psks(state: &AdminState) -> Result<(PathBuf, NvmetcpPsksFile), ApiError> {
    let path = psks_path(state);
    let file = NvmetcpPsksFile::load(&path)
        .map_err(|e| ApiError::internal(format!("loading {}: {}", path.display(), e)))?;
    Ok((path, file))
}

fn save_psks(path: &Path, file: &NvmetcpPsksFile) -> Result<(), ApiError> {
    file.save(path)
        .map_err(|e| ApiError::internal(format!("saving {}: {}", path.display(), e)))
}

fn sweep_expired_previous(file: &mut NvmetcpPsksFile) -> Vec<String> {
    let now = Utc::now();
    let mut swept = Vec::new();
    for p in &mut file.psks {
        if let Some(expires) = p.previous_expires_at
            && expires <= now
        {
            p.previous_interchange_key = None;
            p.previous_expires_at = None;
            swept.push(p.host_nqn.clone());
        }
    }
    swept
}

fn emit_sweep_audits(state: &AdminState, peer: &PeerCred, swept: Vec<String>) {
    for host_nqn in swept {
        audit(
            state,
            peer,
            "nvmetcp.psks.rotate.commit",
            json!({ "host_nqn": host_nqn, "reason": "grace_expired" }),
        );
    }
}

fn audit(state: &AdminState, peer: &PeerCred, op: &str, params: serde_json::Value) {
    if let Some(chan) = state.audit.as_ref() {
        chan.try_append(
            op,
            AuditActor::cli(peer.audit_descriptor()),
            params,
            AuditResult::Ok,
        );
    }
}

fn validate_host_nqn(s: &str) -> Result<(), ApiError> {
    if s.is_empty() {
        return Err(ApiError::bad_request("host_nqn must not be empty"));
    }
    if s.len() > 223 {
        // NVMe Base §7.9: NQN field width.
        return Err(ApiError::bad_request(
            "host_nqn exceeds 223 bytes (NVMe §7.9)",
        ));
    }
    Ok(())
}

fn validate_interchange_key(s: &str) -> Result<(), ApiError> {
    parse_interchange_key(s)
        .map(|_| ())
        .map_err(|e| ApiError::bad_request(format!("invalid interchange_key: {}", e)))
}
