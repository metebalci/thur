// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! The `system set-admin-password` daemon handler.
//!
//! Daemon-routed: the CLI prompts for the new password with no echo and
//! sends it over the local peer-cred admin socket; the daemon hashes it
//! server-side (the plaintext never lands on disk and never leaves the
//! host). On success it persists `<data_dir>/admin-password.json`,
//! hot-swaps the live [`AuthState`] so the new password takes effect
//! without a restart, and appends a (secret-free) audit row.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;
use shared_admin_server::PeerCred;
use shared_audit::{AuditActor, AuditChannel, AuditResult};

use crate::hash::hash_password;
use crate::state::AuthState;
use crate::store::{AdminPasswordFile, admin_password_path};

/// Per-daemon glue for the setter: where `admin-password.json` lives,
/// where to forward audit rows, and the live [`AuthState`] to hot-swap.
/// Both `thurvtld::AdminState` and `thurvsad::AdminState` impl this.
pub trait AdminPasswordState: Clone + Send + Sync + 'static {
    fn data_dir(&self) -> &std::path::Path;
    fn audit_channel(&self) -> Option<&AuditChannel>;
    fn auth(&self) -> &AuthState;
}

#[derive(Debug, Deserialize)]
pub struct SetRequest {
    pub password: String,
}

/// Minimum admin-password length. Short enough not to annoy; the real
/// throttle on guessing is Argon2id's per-verify cost.
const MIN_PASSWORD_LEN: usize = 12;

/// `POST /api/v1/system/admin-password` — set (or replace) the shared
/// web-admin password. Replies `204 No Content`.
pub async fn set<S: AdminPasswordState>(
    State(state): State<S>,
    peer: PeerCred,
    Json(body): Json<SetRequest>,
) -> Result<StatusCode, ApiError> {
    if body.password.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::bad_request(format!(
            "password must be at least {MIN_PASSWORD_LEN} bytes"
        )));
    }

    let phc = hash_password(&body.password).map_err(ApiError::internal)?;
    let path = admin_password_path(state.data_dir());
    AdminPasswordFile {
        phc: phc.clone(),
        updated_at: chrono::Utc::now(),
    }
    .save(&path)
    .map_err(ApiError::internal)?;

    // Take effect immediately — same process, shared handle.
    state.auth().store(Some(phc));

    if let Some(c) = state.audit_channel() {
        c.try_append(
            "system.admin_password.set",
            AuditActor::cli(peer.audit_descriptor()),
            json!({}),
            AuditResult::Ok,
        );
    }
    Ok(StatusCode::NO_CONTENT)
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
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[derive(Clone)]
    struct MockState {
        dir: PathBuf,
        auth: AuthState,
    }

    impl AdminPasswordState for MockState {
        fn data_dir(&self) -> &Path {
            &self.dir
        }
        fn audit_channel(&self) -> Option<&AuditChannel> {
            None
        }
        fn auth(&self) -> &AuthState {
            &self.auth
        }
    }

    fn fresh() -> (TempDir, MockState) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = MockState {
            dir: tmp.path().to_path_buf(),
            auth: AuthState::new(None),
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

    #[tokio::test]
    async fn set_persists_hashes_and_hot_swaps() {
        let (_t, state) = fresh();
        assert!(!state.auth.is_configured());

        let code = set(
            State(state.clone()),
            peer(),
            Json(SetRequest {
                password: "a-good-password".into(),
            }),
        )
        .await
        .expect("set ok");
        assert_eq!(code, StatusCode::NO_CONTENT);

        // Live state hot-swapped...
        assert!(state.auth.is_configured());
        let phc = state.auth.current().expect("configured");
        assert!(crate::hash::verify_phc(&phc, b"a-good-password"));

        // ...and persisted to disk (PHC, never the plaintext).
        let path = admin_password_path(state.data_dir());
        let on_disk = AdminPasswordFile::load(&path).expect("ok").expect("some");
        assert!(on_disk.phc.starts_with("$argon2id$"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("a-good-password"), "plaintext must not persist");
    }

    #[tokio::test]
    async fn set_rejects_a_short_password() {
        let (_t, state) = fresh();
        let err = set(
            State(state),
            peer(),
            Json(SetRequest {
                password: "short".into(),
            }),
        )
        .await
        .expect_err("must reject");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_twice_replaces_the_password() {
        let (_t, state) = fresh();
        for pw in ["first-password-1", "second-password-2"] {
            set(
                State(state.clone()),
                peer(),
                Json(SetRequest {
                    password: pw.to_string(),
                }),
            )
            .await
            .expect("set ok");
        }
        let phc = state.auth.current().expect("configured");
        assert!(crate::hash::verify_phc(&phc, b"second-password-2"));
        assert!(!crate::hash::verify_phc(&phc, b"first-password-1"));
    }

    #[test]
    fn api_error_constructors_pick_the_right_status() {
        assert_eq!(ApiError::bad_request("x").status, StatusCode::BAD_REQUEST);
        assert_eq!(
            ApiError::internal("x").status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn set_request_deserializes() {
        let req: SetRequest = serde_json::from_value(json!({"password": "p"})).expect("parse");
        assert_eq!(req.password, "p");
    }
}
