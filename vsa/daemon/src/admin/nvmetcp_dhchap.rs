// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Admin handlers for NVMe-TCP DH-HMAC-CHAP lifecycle verbs:
//! `add` / `remove` / `disable` / `enable` / `rotate` /
//! `rotate cancel` / `grant` / `revoke` / `set-ctrl-key` /
//! `clear-ctrl-key` and `list`. The in-band-auth analog of
//! [`super::nvmetcp_psks`] — host NQN is the entry key, `dhchap_key`
//! (a `DHHC-1:...` secret) replaces `interchange_key`, and an optional
//! `dhchap_ctrl_key` enables mutual auth.
//!
//! VSA-only — VTL doesn't carry an NVMe-TCP transport.

use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use nvme_tcp::auth::parse_dhchap_secret;
use nvme_tcp::identity::{DhchapEntry, NvmetcpDhchapFile};
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
    pub dhchap_key: String,
    /// Optional controller secret enabling mutual auth from creation.
    #[serde(default)]
    pub dhchap_ctrl_key: Option<String>,
    /// Volumes this host is admitted to. Mandatory (>= 1) — VSA's
    /// admin handler rejects unknown names before persisting.
    #[serde(default)]
    pub volumes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct HostNqnOnlyRequest {
    pub host_nqn: String,
}

#[derive(Debug, Deserialize)]
pub struct RotateRequest {
    pub host_nqn: String,
    pub dhchap_key: String,
    pub grace_seconds: u64,
}

#[derive(Debug, Deserialize)]
pub struct GrantRequest {
    pub host_nqn: String,
    pub volumes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub host_nqn: String,
    pub volumes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetCtrlKeyRequest {
    pub host_nqn: String,
    pub dhchap_ctrl_key: String,
}

#[derive(Debug, Serialize)]
pub struct DhchapRow {
    pub host_nqn: String,
    pub volumes: Option<Vec<String>>,
    /// Whether a controller secret is configured (mutual auth) — the
    /// secret itself is never returned.
    pub mutual: bool,
    pub disabled: bool,
    pub in_grace: bool,
    pub previous_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub dhchap: Vec<DhchapRow>,
}

fn row_of(entry: &DhchapEntry) -> DhchapRow {
    DhchapRow {
        host_nqn: entry.host_nqn.clone(),
        volumes: entry.volumes.clone(),
        mutual: entry.dhchap_ctrl_key.is_some(),
        disabled: entry.disabled,
        in_grace: entry
            .previous_expires_at
            .map(|t| t > Utc::now())
            .unwrap_or(false),
        previous_expires_at: entry.previous_expires_at,
    }
}

// ---------- handlers ----------

pub async fn list(
    State(state): State<AdminState>,
    _peer: PeerCred,
) -> Result<Json<ListResponse>, ApiError> {
    let (_, file) = load_dhchap(&state)?;
    let dhchap = file.dhchap.iter().map(row_of).collect();
    Ok(Json(ListResponse { dhchap }))
}

pub async fn add(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<AddRequest>,
) -> Result<(StatusCode, Json<DhchapRow>), ApiError> {
    validate_host_nqn(&body.host_nqn)?;
    validate_dhchap_key(&body.dhchap_key)?;
    if let Some(ctrl) = &body.dhchap_ctrl_key {
        validate_dhchap_key(ctrl)?;
    }
    // VSA-mandatory: every entry must declare an admission set.
    let names = body
        .volumes
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("at least one --volume required"))?;
    if names.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_volumes_exist(&state, names)?;

    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);

    if file.dhchap.iter().any(|p| p.host_nqn == body.host_nqn) {
        return Err(ApiError::conflict(format!(
            "DH-HMAC-CHAP entry for host_nqn '{}' already exists",
            body.host_nqn
        )));
    }

    let entry = DhchapEntry {
        host_nqn: body.host_nqn.clone(),
        dhchap_key: body.dhchap_key,
        dhchap_ctrl_key: body.dhchap_ctrl_key,
        disabled: false,
        volumes: body.volumes,
        previous_dhchap_key: None,
        previous_expires_at: None,
    };
    let row = row_of(&entry);
    file.dhchap.push(entry);
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.add",
        json!({ "host_nqn": body.host_nqn, "mutual": row.mutual }),
    );
    Ok((StatusCode::CREATED, Json(row)))
}

pub async fn remove(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let before = file.dhchap.len();
    file.dhchap.retain(|p| p.host_nqn != body.host_nqn);
    if file.dhchap.len() == before {
        return Err(ApiError::not_found(format!(
            "DH-HMAC-CHAP entry for host_nqn '{}'",
            body.host_nqn
        )));
    }
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.remove",
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
    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|p| p.host_nqn == host_nqn)
        .ok_or_else(|| ApiError::not_found(format!("DH-HMAC-CHAP entry for '{}'", host_nqn)))?;
    entry.disabled = disabled;
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        if disabled {
            "nvmetcp.dhchap.disable"
        } else {
            "nvmetcp.dhchap.enable"
        },
        json!({ "host_nqn": host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rotate(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<RotateRequest>,
) -> Result<(StatusCode, Json<DhchapRow>), ApiError> {
    validate_dhchap_key(&body.dhchap_key)?;
    if body.grace_seconds == 0 {
        return Err(ApiError::bad_request(
            "grace_seconds must be > 0; use add + remove for an immediate cutover",
        ));
    }

    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!("DH-HMAC-CHAP entry for '{}'", body.host_nqn))
        })?;

    if entry.previous_dhchap_key.is_some() && entry.previous_expires_at.is_some() {
        return Err(ApiError::conflict(format!(
            "entry for '{}' already has a pending rotation; cancel it first",
            body.host_nqn
        )));
    }

    let expires = Utc::now() + ChronoDuration::seconds(body.grace_seconds as i64);
    let old_key = std::mem::replace(&mut entry.dhchap_key, body.dhchap_key);
    entry.previous_dhchap_key = Some(old_key);
    entry.previous_expires_at = Some(expires);
    let row = row_of(entry);
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.rotate.start",
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
    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!("DH-HMAC-CHAP entry for '{}'", body.host_nqn))
        })?;
    let prev = entry.previous_dhchap_key.take().ok_or_else(|| {
        ApiError::conflict(format!("no rotation in progress for '{}'", body.host_nqn))
    })?;
    entry.previous_expires_at = None;
    entry.dhchap_key = prev;
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.rotate.cancel",
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn grant(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<GrantRequest>,
) -> Result<(StatusCode, Json<DhchapRow>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_volumes_exist(&state, &body.volumes)?;
    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!("DH-HMAC-CHAP entry for '{}'", body.host_nqn))
        })?;

    let mut current = entry.volumes.clone().unwrap_or_default();
    let added: Vec<String> = body
        .volumes
        .iter()
        .filter(|v| !current.iter().any(|c| c == *v))
        .cloned()
        .collect();
    current.extend(added.iter().cloned());
    entry.volumes = Some(current);
    let row = row_of(entry);
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.grant",
        json!({ "host_nqn": body.host_nqn, "volumes_added": added }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn revoke(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<RevokeRequest>,
) -> Result<(StatusCode, Json<DhchapRow>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!("DH-HMAC-CHAP entry for '{}'", body.host_nqn))
        })?;

    let current = entry.volumes.clone().unwrap_or_default();
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
            "revoke would leave host '{}' with no volumes; use `nvmetcp dhchap disable` or `remove` instead",
            body.host_nqn
        )));
    }
    entry.volumes = Some(next);
    let row = row_of(entry);
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.revoke",
        json!({ "host_nqn": body.host_nqn, "volumes_removed": removed }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn set_ctrl_key(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<SetCtrlKeyRequest>,
) -> Result<(StatusCode, Json<DhchapRow>), ApiError> {
    validate_dhchap_key(&body.dhchap_ctrl_key)?;
    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!("DH-HMAC-CHAP entry for '{}'", body.host_nqn))
        })?;
    entry.dhchap_ctrl_key = Some(body.dhchap_ctrl_key);
    let row = row_of(entry);
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.ctrl_key.set",
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn clear_ctrl_key(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load_dhchap(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!("DH-HMAC-CHAP entry for '{}'", body.host_nqn))
        })?;
    entry.dhchap_ctrl_key = None;
    save_dhchap(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.ctrl_key.clear",
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------- helpers ----------

fn dhchap_path(state: &AdminState) -> PathBuf {
    state.data_dir.join("nvmetcp-dhchap.json")
}

fn load_dhchap(state: &AdminState) -> Result<(PathBuf, NvmetcpDhchapFile), ApiError> {
    let path = dhchap_path(state);
    let file = NvmetcpDhchapFile::load(&path)
        .map_err(|e| ApiError::internal(format!("loading {}: {}", path.display(), e)))?;
    Ok((path, file))
}

fn save_dhchap(path: &Path, file: &NvmetcpDhchapFile) -> Result<(), ApiError> {
    file.save(path)
        .map_err(|e| ApiError::internal(format!("saving {}: {}", path.display(), e)))
}

fn sweep_expired_previous(file: &mut NvmetcpDhchapFile) -> Vec<String> {
    let now = Utc::now();
    let mut swept = Vec::new();
    for p in &mut file.dhchap {
        if let Some(expires) = p.previous_expires_at
            && expires <= now
        {
            p.previous_dhchap_key = None;
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
            "nvmetcp.dhchap.rotate.commit",
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

fn validate_dhchap_key(s: &str) -> Result<(), ApiError> {
    parse_dhchap_secret(s)
        .map(|_| ())
        .map_err(|e| ApiError::bad_request(format!("invalid DH-HMAC-CHAP key: {}", e)))
}

/// Reject unknown volume names at add / grant time so we don't
/// accumulate dangling admission entries.
fn validate_volumes_exist(state: &AdminState, names: &[String]) -> Result<(), ApiError> {
    let mut unknown = Vec::new();
    for n in names {
        if state.registry.get_by_name(n).is_none() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        host: &str,
        prev_key: Option<&str>,
        prev_expires: Option<DateTime<Utc>>,
    ) -> DhchapEntry {
        DhchapEntry {
            host_nqn: host.into(),
            dhchap_key: "DHHC-1:00:dGVzdC1rZXktMzItYnl0ZXMtZm9yLWRoY2hhcA==:".into(),
            dhchap_ctrl_key: None,
            disabled: false,
            volumes: None,
            previous_dhchap_key: prev_key.map(|s| s.into()),
            previous_expires_at: prev_expires,
        }
    }

    fn file_with(entries: Vec<DhchapEntry>) -> NvmetcpDhchapFile {
        NvmetcpDhchapFile {
            version: 1,
            dhchap: entries,
        }
    }

    #[test]
    fn sweep_clears_expired_previous_and_reports_host() {
        let past = Utc::now() - ChronoDuration::seconds(60);
        let mut f = file_with(vec![entry("nqn.host.a", Some("old-key"), Some(past))]);
        let swept = sweep_expired_previous(&mut f);
        assert_eq!(swept, vec!["nqn.host.a".to_string()]);
        assert!(f.dhchap[0].previous_dhchap_key.is_none());
        assert!(f.dhchap[0].previous_expires_at.is_none());
    }

    #[test]
    fn sweep_preserves_unexpired_previous() {
        let future = Utc::now() + ChronoDuration::seconds(3600);
        let mut f = file_with(vec![entry("nqn.host.a", Some("old-key"), Some(future))]);
        assert!(sweep_expired_previous(&mut f).is_empty());
        assert_eq!(f.dhchap[0].previous_dhchap_key.as_deref(), Some("old-key"));
    }

    #[test]
    fn sweep_handles_mixed_population() {
        let past = Utc::now() - ChronoDuration::seconds(10);
        let future = Utc::now() + ChronoDuration::seconds(600);
        let mut f = file_with(vec![
            entry("nqn.expired", Some("k1"), Some(past)),
            entry("nqn.in-grace", Some("k2"), Some(future)),
            entry("nqn.no-previous", None, None),
        ]);
        let swept = sweep_expired_previous(&mut f);
        assert_eq!(swept, vec!["nqn.expired".to_string()]);
        assert!(f.dhchap[0].previous_dhchap_key.is_none());
        assert!(f.dhchap[1].previous_dhchap_key.is_some());
    }

    #[test]
    fn validate_host_nqn_rejects_empty_and_overlong() {
        assert!(validate_host_nqn("").is_err());
        assert!(validate_host_nqn(&"a".repeat(224)).is_err());
        assert!(validate_host_nqn("nqn.2025-01.example:host").is_ok());
    }

    #[test]
    fn validate_dhchap_key_round_trips() {
        let good = nvme_tcp::auth::encode_dhchap_secret(&[0xAB; 32], 0);
        assert!(validate_dhchap_key(&good).is_ok());
        assert!(validate_dhchap_key("NVMeTLSkey-1:01:abc:").is_err());
    }

    #[test]
    fn row_reports_mutual_when_ctrl_key_present() {
        let mut e = entry("nqn.host.a", None, None);
        assert!(!row_of(&e).mutual);
        e.dhchap_ctrl_key = Some("DHHC-1:00:x:".into());
        assert!(row_of(&e).mutual);
    }
}
