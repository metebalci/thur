// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe-TCP DH-HMAC-CHAP admin surface — the in-band-auth analog of
//! [`super::nvmetcp_psks`]. Host NQN is the entry key, the secret is a
//! `DHHC-1:...` string, and an optional controller secret enables
//! mutual auth.
//!
//! The shared `add` / `remove` / `disable` / `enable` / `rotate` /
//! `rotate cancel` / `grant` / `revoke` verbs come from
//! [`super::nvmetcp_host_file`] via the [`Surface`] impl below. The
//! `set-ctrl-key` / `clear-ctrl-key` verbs and the `list` envelope are
//! DH-HMAC-CHAP-specific and stay here.
//!
//! VSA-only — VTL doesn't carry an NVMe-TCP transport.

use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use nvme_tcp::auth::parse_dhchap_secret;
use nvme_tcp::identity::{DhchapEntry, NvmetcpDhchapFile};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared_admin_server::PeerCred;
use std::path::PathBuf;

use super::handlers::AdminState;
use super::iscsi_users::ApiError;
use super::nvmetcp_host_file::{
    AddRequest, Surface, audit, collect_rows, emit_sweep_audits, load, save, sweep_all,
};

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

#[derive(Debug, Deserialize)]
pub struct SetCtrlKeyRequest {
    pub host_nqn: String,
    pub ctrl_key: String,
}

/// DH-HMAC-CHAP surface wiring for the generic host-credential handlers.
pub struct DhchapSurface;

impl Surface for DhchapSurface {
    type File = NvmetcpDhchapFile;
    type Row = DhchapRow;

    const AUDIT_PREFIX: &'static str = "nvmetcp.dhchap";
    const ENTITY: &'static str = "DH-HMAC-CHAP entry";
    const VERB: &'static str = "dhchap";

    fn path(state: &AdminState) -> PathBuf {
        // Resolved once at boot — honors `nvmetcp.auth.identity_file` so
        // the CLI writes where the DH-HMAC-CHAP handshake reads (#69).
        state.nvmetcp_dhchap_path.clone()
    }

    fn validate_key(s: &str) -> Result<(), ApiError> {
        parse_dhchap_secret(s)
            .map(|_| ())
            .map_err(|e| ApiError::bad_request(format!("invalid DH-HMAC-CHAP key: {}", e)))
    }

    fn validate_add(req: &AddRequest) -> Result<(), ApiError> {
        Self::validate_key(&req.key)?;
        if let Some(ctrl) = &req.ctrl_key {
            Self::validate_key(ctrl)?;
        }
        Ok(())
    }

    fn row(entry: &DhchapEntry) -> DhchapRow {
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

    fn build_entry(req: AddRequest) -> DhchapEntry {
        DhchapEntry {
            host_nqn: req.host_nqn,
            dhchap_key: req.key,
            dhchap_ctrl_key: req.ctrl_key,
            disabled: false,
            volumes: req.volumes,
            previous_dhchap_key: None,
            previous_expires_at: None,
        }
    }

    fn add_audit_params(entry: &DhchapEntry) -> serde_json::Value {
        json!({ "host_nqn": entry.host_nqn, "mutual": entry.dhchap_ctrl_key.is_some() })
    }
}

pub async fn list(
    State(state): State<AdminState>,
    _peer: PeerCred,
) -> Result<Json<ListResponse>, ApiError> {
    let dhchap = collect_rows::<DhchapSurface>(&state)?;
    Ok(Json(ListResponse { dhchap }))
}

// ---------- DH-HMAC-CHAP-only: controller secret (mutual auth) ----------

pub async fn set_ctrl_key(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<SetCtrlKeyRequest>,
) -> Result<(StatusCode, Json<DhchapRow>), ApiError> {
    DhchapSurface::validate_key(&body.ctrl_key)?;
    let (path, mut file) = load::<DhchapSurface>(&state)?;
    let swept = sweep_all::<DhchapSurface>(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|e| e.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "{} for host '{}'",
                DhchapSurface::ENTITY,
                body.host_nqn
            ))
        })?;
    entry.dhchap_ctrl_key = Some(body.ctrl_key);
    let row = DhchapSurface::row(entry);
    save::<DhchapSurface>(&path, &file)?;
    emit_sweep_audits::<DhchapSurface>(&state, &peer, swept);
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
    Json(body): Json<super::nvmetcp_host_file::HostNqnOnlyRequest>,
) -> Result<StatusCode, ApiError> {
    let (path, mut file) = load::<DhchapSurface>(&state)?;
    let swept = sweep_all::<DhchapSurface>(&mut file);
    let entry = file
        .dhchap
        .iter_mut()
        .find(|e| e.host_nqn == body.host_nqn)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "{} for host '{}'",
                DhchapSurface::ENTITY,
                body.host_nqn
            ))
        })?;
    entry.dhchap_ctrl_key = None;
    save::<DhchapSurface>(&path, &file)?;
    emit_sweep_audits::<DhchapSurface>(&state, &peer, swept);
    audit(
        &state,
        &peer,
        "nvmetcp.dhchap.ctrl_key.clear",
        json!({ "host_nqn": body.host_nqn }),
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_key_round_trips_dhchap_and_rejects_interchange() {
        let good = nvme_tcp::auth::encode_dhchap_secret(&[0xAB; 32], 0);
        assert!(DhchapSurface::validate_key(&good).is_ok());
        assert!(DhchapSurface::validate_key("NVMeTLSkey-1:01:abc:").is_err());
    }

    #[test]
    fn build_entry_carries_ctrl_key_and_row_reports_mutual() {
        let req = AddRequest {
            host_nqn: "nqn.host".into(),
            key: "DHHC-1:00:x:".into(),
            ctrl_key: Some("DHHC-1:00:y:".into()),
            volumes: Some(vec!["v1".into()]),
        };
        let e = DhchapSurface::build_entry(req);
        assert_eq!(e.dhchap_key, "DHHC-1:00:x:");
        assert_eq!(e.dhchap_ctrl_key.as_deref(), Some("DHHC-1:00:y:"));
        assert!(DhchapSurface::row(&e).mutual);
    }

    #[test]
    fn validate_add_rejects_bad_ctrl_key() {
        let good = nvme_tcp::auth::encode_dhchap_secret(&[0xAB; 32], 0);
        // Valid key, no ctrl -> ok.
        assert!(
            DhchapSurface::validate_add(&AddRequest {
                host_nqn: "nqn.host".into(),
                key: good.clone(),
                ctrl_key: None,
                volumes: Some(vec!["v1".into()]),
            })
            .is_ok()
        );
        // Valid key, garbage ctrl_key -> 400 (the DH-HMAC-CHAP surface
        // validates the controller secret too).
        assert!(
            DhchapSurface::validate_add(&AddRequest {
                host_nqn: "nqn.host".into(),
                key: good,
                ctrl_key: Some("not-a-secret".into()),
                volumes: Some(vec!["v1".into()]),
            })
            .is_err()
        );
    }

    #[test]
    fn add_audit_params_flags_mutual() {
        let e = DhchapSurface::build_entry(AddRequest {
            host_nqn: "nqn.host".into(),
            key: "DHHC-1:00:x:".into(),
            ctrl_key: None,
            volumes: Some(vec!["v1".into()]),
        });
        assert_eq!(DhchapSurface::add_audit_params(&e)["mutual"], false);
    }
}
