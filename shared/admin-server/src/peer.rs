// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-connection peer credentials for the admin Unix socket.
//!
//! The kernel hands us the uid/gid/pid of the connecting process
//! via SO_PEERCRED. The accept loop extracts them and injects them
//! into the per-request extension map; handlers pull them out via
//! [`PeerCred`] (an axum `FromRequestParts` implementor). Mutating
//! endpoints record this on the audit entry so post-hoc review can
//! tell which operator (uid) issued each command.
//!
//! The type lives in this crate (not `shared-admin-proto`) because
//! Rust's orphan rule forbids impl-ing the foreign
//! `FromRequestParts` trait on a foreign type — and the CLI side
//! has no use for the type.

use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use std::convert::Infallible;

#[derive(Debug, Clone)]
pub struct PeerCred {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<i32>,
}

impl PeerCred {
    /// Render as `unix:<uid>:<pid>` for the audit `actor.user` field.
    pub fn audit_descriptor(&self) -> String {
        match self.pid {
            Some(pid) => format!("unix:{}:{}", self.uid, pid),
            None => format!("unix:{}", self.uid),
        }
    }
}

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for PeerCred {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Anonymous fallback if the accept loop didn't inject one
        // (shouldn't happen in practice — present in every socket
        // request — but keeping the type infallible avoids handler-
        // level error plumbing for a near-impossible case).
        Ok(parts
            .extensions
            .get::<PeerCred>()
            .cloned()
            .unwrap_or(PeerCred {
                uid: u32::MAX,
                gid: u32::MAX,
                pid: None,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    fn audit_descriptor_includes_pid_when_present() {
        let cred = PeerCred {
            uid: 1000,
            gid: 1000,
            pid: Some(12345),
        };
        assert_eq!(cred.audit_descriptor(), "unix:1000:12345");
    }

    #[test]
    fn audit_descriptor_omits_pid_when_absent() {
        let cred = PeerCred {
            uid: 1000,
            gid: 1000,
            pid: None,
        };
        assert_eq!(cred.audit_descriptor(), "unix:1000");
    }

    #[tokio::test]
    async fn from_request_parts_returns_the_injected_cred() {
        let (mut parts, _) = Request::builder().body(()).expect("build").into_parts();
        let injected = PeerCred {
            uid: 42,
            gid: 7,
            pid: Some(99),
        };
        parts.extensions.insert(injected.clone());
        let got = PeerCred::from_request_parts(&mut parts, &())
            .await
            .expect("infallible");
        assert_eq!(got.uid, 42);
        assert_eq!(got.gid, 7);
        assert_eq!(got.pid, Some(99));
    }

    #[tokio::test]
    async fn from_request_parts_falls_back_to_anonymous_when_no_extension() {
        let (mut parts, _) = Request::builder().body(()).expect("build").into_parts();
        let got = PeerCred::from_request_parts(&mut parts, &())
            .await
            .expect("infallible");
        assert_eq!(got.uid, u32::MAX);
        assert_eq!(got.gid, u32::MAX);
        assert_eq!(got.pid, None);
    }
}
