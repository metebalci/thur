// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Axum handler for `GET /sessions` — JSON snapshot of every active
//! iSCSI session on the target.
//!
//! Both daemons mount this on the same TCP HTTP listener as
//! `/health`, `/metrics`, and `/license`. The body wraps the
//! existing [`SessionManager::get_session_info`] output with the
//! configured target IQN + listen address so a single curl answers
//! "what's connected, and on what coordinates?".

use std::sync::Arc;

use axum::{Json, extract::State, response::IntoResponse};
use serde_json::json;

use crate::session::SessionManager;

/// Daemon-side state injected into [`sessions_handler`]. Cheap to
/// clone — one `Arc` plus two owned strings (per-daemon config
/// values that don't change at runtime).
#[derive(Clone)]
pub struct SessionsState {
    pub sessions: Arc<SessionManager>,
    pub target_iqn: String,
    pub listen_address: String,
}

pub async fn sessions_handler(State(state): State<SessionsState>) -> impl IntoResponse {
    Json(json!({
        "status": "online",
        "target_iqn": state.target_iqn,
        "listen_address": state.listen_address,
        "sessions": state.sessions.get_session_info(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use serde_json::Value;

    #[tokio::test]
    async fn body_carries_iqn_listen_and_empty_session_list() {
        let state = SessionsState {
            sessions: Arc::new(SessionManager::new()),
            target_iqn: "iqn.2025-10.com.metebalci:thurvtl".into(),
            listen_address: "0.0.0.0:3260".into(),
        };
        let resp = sessions_handler(State(state)).await.into_response();
        let (_, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        let v: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(v["status"], "online");
        assert_eq!(v["target_iqn"], "iqn.2025-10.com.metebalci:thurvtl");
        assert_eq!(v["listen_address"], "0.0.0.0:3260");
        assert!(v["sessions"].as_array().expect("array").is_empty());
    }
}
