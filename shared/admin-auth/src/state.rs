// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Live admin-password verifier handle.
//!
//! The setter (admin Unix socket) and the verifier (TCP HTTP listener)
//! run in one process / one state graph, so a single `Arc`-shared
//! [`AuthState`] lets the middleware verify against the current hash
//! with no per-request disk read, and a password change takes effect
//! immediately (the setter `store`s the new PHC). `None` = no password
//! configured, which the middleware turns into a fail-closed 503.

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::store::AdminPasswordFile;

/// Whether the protected TCP route group requires the shared web-admin
/// password. Mirrors the opt-out shape of `iscsi.auth.method`: `None`
/// is the default (no authentication — the trusted / isolated-network
/// posture unauthenticated iSCSI also defaults to), `Password` turns
/// the gate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
pub enum AuthMethod {
    /// No authentication: the protected routes (`/sessions`, `/info`,
    /// `/ui`, read-only `/api/v1`) are served open. `/health` +
    /// `/metrics` are unauthenticated regardless.
    #[default]
    None,
    /// Require the single shared web-admin password over HTTP Basic
    /// (synthetic `webadmin` user). No password configured =>
    /// fail-closed 503.
    Password,
}

/// Cheap-to-clone handle to the current Argon2id PHC string (or `None`),
/// plus the configured [`AuthMethod`].
#[derive(Clone)]
pub struct AuthState {
    inner: Arc<ArcSwapOption<String>>,
    method: AuthMethod,
}

impl AuthState {
    /// Construct from an already-resolved PHC (or `None` when unset).
    /// The auth method defaults to [`AuthMethod::Password`]; the daemon
    /// overrides it from `http.auth.method` via `with_method`.
    pub fn new(phc: Option<String>) -> Self {
        Self {
            inner: Arc::new(ArcSwapOption::from(phc.map(Arc::new))),
            method: AuthMethod::Password,
        }
    }

    /// Set the auth method (from the resolved `http.auth.method`).
    pub fn with_method(mut self, method: AuthMethod) -> Self {
        self.method = method;
        self
    }

    /// The configured auth method.
    pub fn method(&self) -> AuthMethod {
        self.method
    }

    /// Seed from the on-disk store at boot. A missing file yields an
    /// unconfigured (`None`) state; a present-but-malformed file is an
    /// error the daemon surfaces at startup.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        let phc = AdminPasswordFile::load(path)?.map(|f| f.phc);
        Ok(Self::new(phc))
    }

    /// The current PHC, or `None` when no password is configured.
    pub fn current(&self) -> Option<Arc<String>> {
        self.inner.load_full()
    }

    /// True when a password is configured.
    pub fn is_configured(&self) -> bool {
        self.inner.load().is_some()
    }

    /// Hot-swap the live PHC. Called by the setter after a successful
    /// write so the change takes effect without a daemon restart.
    pub fn store(&self, phc: Option<String>) {
        self.inner.store(phc.map(Arc::new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::admin_password_path;

    #[test]
    fn new_none_is_unconfigured() {
        let s = AuthState::new(None);
        assert!(s.current().is_none());
        assert!(!s.is_configured());
    }

    #[test]
    fn new_some_then_current_reflects_it() {
        let s = AuthState::new(Some("phc-1".to_string()));
        assert_eq!(s.current().as_deref().map(String::as_str), Some("phc-1"));
        assert!(s.is_configured());
    }

    #[test]
    fn store_hot_swaps_the_value() {
        let s = AuthState::new(None);
        s.store(Some("phc-2".to_string()));
        assert_eq!(s.current().as_deref().map(String::as_str), Some("phc-2"));
        s.store(None);
        assert!(s.current().is_none());
    }

    #[test]
    fn load_from_missing_file_is_unconfigured() {
        let tmp = tempfile::tempdir().unwrap();
        let s = AuthState::load_from(&admin_password_path(tmp.path())).expect("ok");
        assert!(!s.is_configured());
    }

    #[test]
    fn load_from_saved_file_is_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let path = admin_password_path(tmp.path());
        AdminPasswordFile {
            phc: "$argon2id$v=19$seeded".to_string(),
            updated_at: chrono::Utc::now(),
        }
        .save(&path)
        .expect("save");
        let s = AuthState::load_from(&path).expect("ok");
        assert_eq!(
            s.current().as_deref().map(String::as_str),
            Some("$argon2id$v=19$seeded")
        );
    }

    #[test]
    fn auth_method_default_is_none() {
        assert_eq!(AuthMethod::default(), AuthMethod::None);
    }

    #[test]
    fn method_defaults_to_password_and_with_method_overrides() {
        let s = AuthState::new(None);
        assert_eq!(s.method(), AuthMethod::Password);
        let s = s.with_method(AuthMethod::None);
        assert_eq!(s.method(), AuthMethod::None);
    }
}
