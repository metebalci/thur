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

/// Cheap-to-clone handle to the current Argon2id PHC string (or `None`).
#[derive(Clone)]
pub struct AuthState {
    inner: Arc<ArcSwapOption<String>>,
}

impl AuthState {
    /// Construct from an already-resolved PHC (or `None` when unset).
    pub fn new(phc: Option<String>) -> Self {
        Self {
            inner: Arc::new(ArcSwapOption::from(phc.map(Arc::new))),
        }
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
}
