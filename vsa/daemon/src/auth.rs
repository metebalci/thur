// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! CHAP authenticator construction for thurvsad.
//!
//! Build a [`ChapAuthFactory`] closure that loads
//! `<data_dir>/iscsi-users.json` and runs
//! [`ChapAuthenticator::from_file`] over it on every iSCSI login.
//! The YAML `iscsi.auth.method` + `iscsi.auth.allowed_algorithms`
//! policy is parsed once at startup and captured in the closure —
//! per-login work is just file read + parse + map build, which the
//! page cache makes effectively free for the sub-KB files we deal
//! with. The `parse_chap_algorithms` helper recognizes the same name
//! aliases the VTL daemon does (`MD5` / `SHA-1` / `SHA1` /
//! `SHA-256` / `SHA256` / `SHA3-256` / `SHA3_256` / `SHA3256` plus the
//! integer IDs `5` / `6` / `7` / `8`).
//!
//! No partition fencing — thurvsa has no library-topology /
//! `library.json::partitions` concept. The shared-iscsi
//! `UserEntry::partition` field is parsed but ignored at build time.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use shared_iscsi::auth::{AuthMethod, ChapAuthenticator, IscsiUsersFile, parse_chap_algorithms};
use shared_iscsi::transport::ChapAuthFactory;

use crate::config::AuthSettings;

/// Build a [`ChapAuthFactory`] when the YAML method is `CHAP`.
/// Returns `Ok(None)` for `method: None` — the
/// `shared_iscsi::ServerConfig::auth` field is `Option<_>` so an
/// absent factory means the daemon accepts unauthenticated logins.
///
/// The returned closure reads `iscsi-users.json` fresh on every
/// invocation, so the daemon picks up CLI verb edits (or hand-edits)
/// on the next session without restart or reload. `allowed` is
/// parsed once here and moved into the closure.
pub fn build(
    auth: &AuthSettings,
    users_path: PathBuf,
    initial_user_count: usize,
) -> Result<Option<ChapAuthFactory>> {
    if !auth.method.is_chap() {
        tracing::info!("CHAP authentication disabled (method=None)");
        return Ok(None);
    }
    let allowed = parse_chap_algorithms(&auth.allowed_algorithms).map_err(|e| anyhow!("{e}"))?;
    let algo_names: Vec<&str> = allowed.iter().map(|a| a.name()).collect();
    tracing::info!(
        "CHAP authentication enabled with {} user(s) at boot, algorithms={:?}, parse-on-login",
        initial_user_count,
        algo_names
    );

    let factory: ChapAuthFactory = Arc::new(move || -> Result<ChapAuthenticator> {
        let file = IscsiUsersFile::load(&users_path)
            .map_err(|e| anyhow!("loading {}: {}", users_path.display(), e))?;
        ChapAuthenticator::from_file(&file, AuthMethod::Chap, allowed.clone()).ok_or_else(|| {
            anyhow!(
                "CHAP method active but ChapAuthenticator::from_file returned None ({})",
                users_path.display()
            )
        })
    });
    Ok(Some(factory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_iscsi::auth::{ChapAlgorithm, UserEntry};

    fn write_users(path: &std::path::Path, users: Vec<UserEntry>) {
        let file = IscsiUsersFile {
            users,
            ..IscsiUsersFile::default()
        };
        file.save(path).unwrap();
    }

    #[test]
    fn method_none_returns_none() {
        let tmp =
            std::env::temp_dir().join(format!("vsa-auth-test-none-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write_users(&tmp, vec![]);
        let auth = AuthSettings {
            method: AuthMethod::None,
            ..AuthSettings::default()
        };
        assert!(build(&auth, tmp.clone(), 0).unwrap().is_none());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn method_chap_factory_reads_file_each_call() {
        let tmp =
            std::env::temp_dir().join(format!("vsa-auth-test-chap-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write_users(
            &tmp,
            vec![UserEntry {
                username: "alice".to_string(),
                password: "alice-pw-1234567890".to_string(),
                mutual_chap: false,
                partition: None,
                disabled: false,
                previous_password: None,
                previous_expires_at: None,
            }],
        );
        let auth = AuthSettings {
            method: AuthMethod::Chap,
            ..AuthSettings::default()
        };
        let factory = build(&auth, tmp.clone(), 1).unwrap().expect("factory");

        // First call sees alice.
        let a = factory().unwrap();
        assert!(a.get_user("alice").is_some());
        assert!(a.get_user("bob").is_none());
        // Empty allowed_algorithms → strongest-first default.
        assert_eq!(a.allowed_algorithms()[0], ChapAlgorithm::Sha3_256);

        // Edit the file; second call picks up bob without rebuilding the factory.
        write_users(
            &tmp,
            vec![UserEntry {
                username: "bob".to_string(),
                password: "bob-pw-1234567890".to_string(),
                mutual_chap: false,
                partition: None,
                disabled: false,
                previous_password: None,
                previous_expires_at: None,
            }],
        );
        let b = factory().unwrap();
        assert!(b.get_user("alice").is_none());
        assert!(b.get_user("bob").is_some());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn factory_errors_on_corrupt_file() {
        let tmp =
            std::env::temp_dir().join(format!("vsa-auth-test-bad-{}.json", std::process::id()));
        std::fs::write(&tmp, b"{not valid json").unwrap();
        let auth = AuthSettings {
            method: AuthMethod::Chap,
            ..AuthSettings::default()
        };
        let factory = build(&auth, tmp.clone(), 0).unwrap().expect("factory");
        assert!(factory().is_err());
        let _ = std::fs::remove_file(&tmp);
    }
}
