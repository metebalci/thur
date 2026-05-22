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
