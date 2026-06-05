// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! The `require_admin_password` axum middleware: the gate the daemons
//! hang on their *protected* TCP route group.
//!
//! Verdicts:
//! - No password configured -> 503 + `WWW-Authenticate` challenge, so
//!   operators tell "unset" apart from "wrong creds".
//! - Missing / malformed / wrong creds -> 401 + challenge.
//! - Valid creds -> stamp an [`AuditActor::rest`] (synthetic
//!   [`WEBADMIN_USER`] + peer `ip:port`) into request extensions for
//!   downstream mutating handlers (Web UI v2), then run the inner route.
//!
//! To avoid a username-timing oracle the password hash is always
//! verified even when the username is wrong, and the two checks are
//! `AND`ed; the username compare itself is constant-time.

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use shared_audit::AuditActor;
use subtle::ConstantTimeEq;

use crate::basic::parse_basic;
use crate::hash::verify_phc;
use crate::state::AuthState;
use crate::{REALM, WEBADMIN_USER};

pub async fn require_admin_password(
    State(auth): State<AuthState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(phc) = auth.current() else {
        return challenge(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin password not configured; run `system set-admin-password`",
        );
    };

    let Some((user, pass)) = parse_basic(&req) else {
        return challenge(StatusCode::UNAUTHORIZED, "authentication required");
    };

    let user_ok = user.as_bytes().ct_eq(WEBADMIN_USER.as_bytes()).into();
    let pass_ok = verify_phc(&phc, pass.as_bytes());
    if user_ok && pass_ok {
        req.extensions_mut()
            .insert(AuditActor::rest(WEBADMIN_USER, peer.to_string()));
        next.run(req).await
    } else {
        challenge(StatusCode::UNAUTHORIZED, "invalid credentials")
    }
}

/// Build a JSON error response carrying the Basic `WWW-Authenticate`
/// challenge. Falls back to a bare status if the body can't be built
/// (it always can — the inputs are static-ish — but no `unwrap` in a
/// non-test path).
fn challenge(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::WWW_AUTHENTICATE, format!("Basic realm=\"{REALM}\""))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!("{{\"error\":\"{msg}\"}}")))
        .unwrap_or_else(|_| status.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use base64::Engine;
    use tower::ServiceExt; // oneshot

    fn app(auth: AuthState) -> Router {
        Router::new()
            .route("/protected", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(
                auth,
                require_admin_password,
            ))
            // Inject a synthetic peer addr so `ConnectInfo` resolves
            // under `oneshot` (no real socket).
            .layer(axum::extract::connect_info::MockConnectInfo(
                "127.0.0.1:54321".parse::<SocketAddr>().unwrap(),
            ))
    }

    fn basic(user: &str, pass: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        )
    }

    async fn status_for(auth: AuthState, header_val: Option<&str>) -> StatusCode {
        let mut builder = Request::builder().uri("/protected");
        if let Some(v) = header_val {
            builder = builder.header(header::AUTHORIZATION, v);
        }
        let req = builder.body(Body::empty()).unwrap();
        app(auth).oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn unconfigured_password_yields_503() {
        let st = status_for(AuthState::new(None), None).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn missing_credentials_yields_401() {
        let auth = AuthState::new(Some(crate::hash::hash_password("the-password-12").unwrap()));
        assert_eq!(status_for(auth, None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_credentials_pass_through_200() {
        let auth = AuthState::new(Some(crate::hash::hash_password("the-password-12").unwrap()));
        let st = status_for(auth, Some(&basic(WEBADMIN_USER, "the-password-12"))).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_password_yields_401() {
        let auth = AuthState::new(Some(crate::hash::hash_password("the-password-12").unwrap()));
        let st = status_for(auth, Some(&basic(WEBADMIN_USER, "wrong-password"))).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_username_yields_401() {
        let auth = AuthState::new(Some(crate::hash::hash_password("the-password-12").unwrap()));
        let st = status_for(auth, Some(&basic("root", "the-password-12"))).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn challenge_carries_a_www_authenticate_header() {
        let auth = AuthState::new(Some(crate::hash::hash_password("the-password-12").unwrap()));
        let req = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        let resp = app(auth).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let challenge = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(challenge.contains("Basic"), "got: {challenge}");
        assert!(challenge.contains(REALM), "got: {challenge}");
    }
}
