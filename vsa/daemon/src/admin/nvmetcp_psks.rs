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
    /// Volumes this host is admitted to. `None` or omitted = no
    /// admission fence (see-everything). Each name must currently
    /// resolve to a volume — VSA's admin handler rejects unknown
    /// names before persisting.
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
    pub interchange_key: String,
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

#[derive(Debug, Serialize)]
pub struct PskRow {
    pub host_nqn: String,
    pub volumes: Option<Vec<String>>,
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
            volumes: p.volumes,
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
    // VSA-mandatory: every PSK entry must declare an admission set.
    let names = body
        .volumes
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("at least one --volume required"))?;
    if names.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_volumes_exist(&state, names)?;

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
        volumes: body.volumes.clone(),
        disabled: false,
        in_grace: false,
        previous_expires_at: None,
    };
    file.psks.push(PskEntry {
        host_nqn: body.host_nqn.clone(),
        interchange_key: body.interchange_key,
        disabled: false,
        volumes: body.volumes,
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
        volumes: entry.volumes.clone(),
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

pub async fn grant(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<GrantRequest>,
) -> Result<(StatusCode, Json<PskRow>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    validate_volumes_exist(&state, &body.volumes)?;
    let (path, mut file) = load_psks(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .psks
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| ApiError::not_found(format!("PSK for host_nqn '{}'", body.host_nqn)))?;

    let mut current = entry.volumes.clone().unwrap_or_default();
    let added: Vec<String> = body
        .volumes
        .iter()
        .filter(|v| !current.iter().any(|c| c == *v))
        .cloned()
        .collect();
    current.extend(added.iter().cloned());
    entry.volumes = Some(current);

    let row = PskRow {
        host_nqn: entry.host_nqn.clone(),
        volumes: entry.volumes.clone(),
        disabled: entry.disabled,
        in_grace: entry
            .previous_expires_at
            .map(|t| t > Utc::now())
            .unwrap_or(false),
        previous_expires_at: entry.previous_expires_at,
    };
    save_psks(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.psks.grant",
        json!({ "host_nqn": body.host_nqn, "volumes_added": added }),
    );
    Ok((StatusCode::OK, Json(row)))
}

pub async fn revoke(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<RevokeRequest>,
) -> Result<(StatusCode, Json<PskRow>), ApiError> {
    if body.volumes.is_empty() {
        return Err(ApiError::bad_request("at least one --volume required"));
    }
    let (path, mut file) = load_psks(&state)?;
    let swept = sweep_expired_previous(&mut file);
    let entry = file
        .psks
        .iter_mut()
        .find(|p| p.host_nqn == body.host_nqn)
        .ok_or_else(|| ApiError::not_found(format!("PSK for host_nqn '{}'", body.host_nqn)))?;

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
            "revoke would leave host '{}' with no volumes; use `nvmetcp psks disable` or `remove` instead",
            body.host_nqn
        )));
    }
    entry.volumes = Some(next);

    let row = PskRow {
        host_nqn: entry.host_nqn.clone(),
        volumes: entry.volumes.clone(),
        disabled: entry.disabled,
        in_grace: entry
            .previous_expires_at
            .map(|t| t > Utc::now())
            .unwrap_or(false),
        previous_expires_at: entry.previous_expires_at,
    };
    save_psks(&path, &file)?;
    emit_sweep_audits(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.psks.revoke",
        json!({ "host_nqn": body.host_nqn, "volumes_removed": removed }),
    );
    Ok((StatusCode::OK, Json(row)))
}

// ---------- helpers ----------

fn psks_path(state: &AdminState) -> PathBuf {
    // Resolved once at boot — honors `nvmetcp.tls.identity_file` so the
    // CLI writes where the TLS-PSK acceptor reads (issue #69).
    state.nvmetcp_psks_path.clone()
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

    fn psk(host: &str, prev_key: Option<&str>, prev_expires: Option<DateTime<Utc>>) -> PskEntry {
        PskEntry {
            host_nqn: host.into(),
            interchange_key: "NVMeTLSkey-1:01:00000000000000000000000000000000000000000000000000000000000000000000:".into(),
            disabled: false,
            volumes: None,
            previous_interchange_key: prev_key.map(|s| s.into()),
            previous_expires_at: prev_expires,
        }
    }

    fn file_with(entries: Vec<PskEntry>) -> NvmetcpPsksFile {
        NvmetcpPsksFile {
            version: 1,
            psks: entries,
        }
    }

    #[test]
    fn sweep_clears_expired_previous_and_reports_host() {
        let past = Utc::now() - ChronoDuration::seconds(60);
        let mut f = file_with(vec![psk("nqn.host.a", Some("old-key"), Some(past))]);
        let swept = sweep_expired_previous(&mut f);
        assert_eq!(swept, vec!["nqn.host.a".to_string()]);
        let e = &f.psks[0];
        assert!(e.previous_interchange_key.is_none());
        assert!(e.previous_expires_at.is_none());
    }

    #[test]
    fn sweep_preserves_unexpired_previous() {
        let future = Utc::now() + ChronoDuration::seconds(3600);
        let mut f = file_with(vec![psk("nqn.host.a", Some("old-key"), Some(future))]);
        let swept = sweep_expired_previous(&mut f);
        assert!(swept.is_empty());
        let e = &f.psks[0];
        assert_eq!(e.previous_interchange_key.as_deref(), Some("old-key"));
        assert_eq!(e.previous_expires_at, Some(future));
    }

    #[test]
    fn sweep_clears_at_exact_boundary() {
        // The state machine uses `expires <= now` so a row whose
        // expiry has just landed in the current tick is swept on that
        // same call. Without this guard a row that hit its deadline
        // mid-microsecond could linger one whole sweep interval.
        let now_boundary = Utc::now() - ChronoDuration::milliseconds(1);
        let mut f = file_with(vec![psk("nqn.host.a", Some("old-key"), Some(now_boundary))]);
        let swept = sweep_expired_previous(&mut f);
        assert_eq!(swept.len(), 1);
        assert!(f.psks[0].previous_interchange_key.is_none());
    }

    #[test]
    fn sweep_only_touches_rows_with_previous_key_set() {
        // No previous key staged → sweep is a no-op on the row.
        let mut f = file_with(vec![psk("nqn.host.a", None, None)]);
        let swept = sweep_expired_previous(&mut f);
        assert!(swept.is_empty());
    }

    #[test]
    fn sweep_handles_mixed_population() {
        let past = Utc::now() - ChronoDuration::seconds(10);
        let future = Utc::now() + ChronoDuration::seconds(600);
        let mut f = file_with(vec![
            psk("nqn.expired", Some("k1"), Some(past)),
            psk("nqn.in-grace", Some("k2"), Some(future)),
            psk("nqn.no-previous", None, None),
        ]);
        let swept = sweep_expired_previous(&mut f);
        assert_eq!(swept, vec!["nqn.expired".to_string()]);
        assert!(f.psks[0].previous_interchange_key.is_none());
        assert!(f.psks[1].previous_interchange_key.is_some());
    }

    #[test]
    fn validate_host_nqn_rejects_empty_and_overlong() {
        assert!(validate_host_nqn("").is_err());
        let long = "a".repeat(224);
        assert!(validate_host_nqn(&long).is_err());
        assert!(validate_host_nqn("nqn.2025-01.example:host").is_ok());
    }
}
