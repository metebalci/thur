// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe-TCP TLS-PSK admin surface (`add` / `remove` / `disable` /
//! `enable` / `rotate` / `rotate cancel` / `grant` / `revoke` /
//! `list`). The entry key is host NQN and the secret is an
//! `NVMeTLSkey-...` interchange string.
//!
//! All the rotatable-host-entry lifecycle lives in
//! [`super::nvmetcp_host_file`]; this module supplies only the
//! TLS-PSK specifics via the [`Surface`] impl, plus the `list`
//! envelope. The DH-HMAC-CHAP sibling is [`super::nvmetcp_dhchap`].
//!
//! VSA-only — VTL doesn't carry an NVMe-TCP transport.

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use nvme_tcp::identity::{NvmetcpPsksFile, PskEntry};
use nvme_tcp::tls::parse_interchange_key;
use serde::Serialize;
use shared_admin_server::PeerCred;
use std::path::PathBuf;

use super::handlers::AdminState;
use super::iscsi_users::ApiError;
use super::nvmetcp_host_file::{AddRequest, Surface, collect_rows};

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

/// TLS-PSK surface wiring for the generic host-credential handlers.
pub struct PsksSurface;

impl Surface for PsksSurface {
    type File = NvmetcpPsksFile;
    type Row = PskRow;

    const AUDIT_PREFIX: &'static str = "nvmetcp.psks";
    const ENTITY: &'static str = "PSK";
    const VERB: &'static str = "psks";

    fn path(state: &AdminState) -> PathBuf {
        // Resolved once at boot — honors `nvmetcp.tls.identity_file` so
        // the CLI writes where the TLS-PSK acceptor reads (issue #69).
        state.nvmetcp_psks_path.clone()
    }

    fn validate_key(s: &str) -> Result<(), ApiError> {
        parse_interchange_key(s)
            .map(|_| ())
            .map_err(|e| ApiError::bad_request(format!("invalid interchange_key: {}", e)))
    }

    fn row(entry: &PskEntry) -> PskRow {
        PskRow {
            host_nqn: entry.host_nqn.clone(),
            volumes: entry.volumes.clone(),
            disabled: entry.disabled,
            in_grace: entry
                .previous_expires_at
                .map(|t| t > Utc::now())
                .unwrap_or(false),
            previous_expires_at: entry.previous_expires_at,
        }
    }

    fn build_entry(req: AddRequest) -> PskEntry {
        PskEntry {
            host_nqn: req.host_nqn,
            interchange_key: req.key,
            disabled: false,
            volumes: req.volumes,
            previous_interchange_key: None,
            previous_expires_at: None,
        }
    }

    fn add_audit_params(entry: &PskEntry) -> serde_json::Value {
        serde_json::json!({ "host_nqn": entry.host_nqn })
    }
}

pub async fn list(
    State(state): State<AdminState>,
    _peer: PeerCred,
) -> Result<Json<ListResponse>, ApiError> {
    let psks = collect_rows::<PsksSurface>(&state)?;
    Ok(Json(ListResponse { psks }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_key_rejects_non_interchange_strings() {
        assert!(PsksSurface::validate_key("not-a-key").is_err());
        // A DH-HMAC-CHAP secret is not a valid interchange key.
        assert!(PsksSurface::validate_key("DHHC-1:00:abc:").is_err());
    }

    #[test]
    fn validate_add_ignores_ctrl_key_on_psk_surface() {
        // The PSK surface has no controller secret; a bogus `ctrl_key`
        // in the (shared) request body must be ignored, not 400'd —
        // matching the pre-dedup behaviour where the field didn't exist.
        // A valid interchange `key` (32 bytes of 0xAB + CRC) plus garbage
        // ctrl_key must pass validate_add.
        let req = AddRequest {
            host_nqn: "nqn.host".into(),
            key: "NVMeTLSkey-1:01:q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6sIy4rZ:".into(),
            ctrl_key: Some("total-garbage".into()),
            volumes: Some(vec!["v1".into()]),
        };
        assert!(PsksSurface::validate_add(&req).is_ok());
    }

    #[test]
    fn build_entry_maps_neutral_key_to_interchange_field() {
        let req = AddRequest {
            host_nqn: "nqn.host".into(),
            key: "NVMeTLSkey-1:01:abc:".into(),
            ctrl_key: Some("ignored".into()),
            volumes: Some(vec!["v1".into()]),
        };
        let e = PsksSurface::build_entry(req);
        assert_eq!(e.host_nqn, "nqn.host");
        assert_eq!(e.interchange_key, "NVMeTLSkey-1:01:abc:");
        assert_eq!(e.volumes.as_deref(), Some(&["v1".to_string()][..]));
        assert!(!e.disabled);
        assert!(e.previous_interchange_key.is_none());
    }

    #[test]
    fn row_reports_in_grace_only_when_previous_unexpired() {
        let mut e = PsksSurface::build_entry(AddRequest {
            host_nqn: "nqn.host".into(),
            key: "NVMeTLSkey-1:01:abc:".into(),
            ctrl_key: None,
            volumes: Some(vec!["v1".into()]),
        });
        assert!(!PsksSurface::row(&e).in_grace);
        e.previous_expires_at = Some(Utc::now() + chrono::Duration::hours(1));
        assert!(PsksSurface::row(&e).in_grace);
    }
}
