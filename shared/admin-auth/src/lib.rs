// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Single shared "admin password" gate for the network-facing TCP HTTP
//! listener — the hard prerequisite for the Web UI (issue #4).
//!
//! The admin surface is split: mutations + sensitive reads on the TCP
//! listener (`http.listen`) have no peer-credential to lean on the way
//! the Unix admin socket does, so they need an authentication check.
//! This crate is that check: a single shared password (printer-admin
//! model — one synthetic [`WEBADMIN_USER`], no per-user accounts),
//! presented over HTTP Basic on the existing TLS listener.
//!
//! Pieces:
//! - [`AdminPasswordFile`] / [`admin_password_path`] — the on-disk
//!   `<data_dir>/admin-password.json` store (Argon2id PHC only, never
//!   the plaintext; mode 0640, atomic rename).
//! - [`AuthState`] — the live verifier handle, a cheap `Arc` clone
//!   shared between the admin-socket setter and the HTTP middleware in
//!   the same process. Hot-swaps on a password change, no restart.
//! - [`require_admin_password`] — the axum middleware the daemons hang
//!   on their *protected* route group. `/metrics` + `/health` stay open.
//! - [`set`] — the `system set-admin-password` daemon handler (the CLI
//!   prompts no-echo and the daemon hashes server-side).
//!
//! Both daemons impl [`AdminPasswordState`] for their `AdminState`;
//! everything else is shared so the two surfaces can't drift. The
//! middleware stamps an [`shared_audit::AuditActor::rest`] into request
//! extensions on success, ready for the Web UI v2 mutating handlers.

#![forbid(unsafe_code)]

mod basic;
mod hash;
mod middleware;
mod set;
mod state;
mod store;

pub use basic::parse_basic;
pub use hash::{hash_password, verify_phc};
pub use middleware::require_admin_password;
pub use set::{AdminPasswordState, ApiError, SetRequest, set};
pub use state::AuthState;
pub use store::{AdminPasswordFile, admin_password_path};

/// Synthetic single-user identity for the shared admin password
/// (printer-admin-interface model — no per-user accounts). HTTP Basic
/// still carries a username field, so we pin one rather than accept any.
pub const WEBADMIN_USER: &str = "webadmin";

/// HTTP Basic realm presented in the `WWW-Authenticate` challenge.
pub const REALM: &str = "thur admin";
