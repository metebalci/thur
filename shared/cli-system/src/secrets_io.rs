// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Small operator-facing helpers shared by the passphrase-sealed
//! export / import verbs (`volume key {export,import}`).
//!
//! Kept here so each verb gets the same tty UX and the same
//! mode-0600 / refuse-clobber write discipline without per-verb
//! drift.

use std::path::Path;

use anyhow::{Context, Result, bail};
use shared_naming::ProductIdentity;

/// Prompt the operator for a passphrase on the tty, or read it from
/// the per-product `<METRIC_PREFIX>_PASSPHRASE` env var (e.g.
/// `THURVSA_PASSPHRASE`, `THURVTL_PASSPHRASE`). When `confirm` is
/// true, ask twice and bail on mismatch — appropriate for export
/// flows where a typo would seal an unrecoverable envelope.
pub fn prompt_passphrase(product: &'static ProductIdentity, confirm: bool) -> Result<String> {
    let env_name = format!("{}_PASSPHRASE", product.metric_prefix.to_uppercase());
    if let Ok(env_pass) = std::env::var(&env_name) {
        if env_pass.is_empty() {
            bail!("{env_name} is set but empty");
        }
        return Ok(env_pass);
    }
    let p1 = rpassword::prompt_password("Passphrase: ").context("reading passphrase from tty")?;
    if p1.is_empty() {
        bail!("empty passphrase rejected");
    }
    if confirm {
        let p2 = rpassword::prompt_password("Passphrase (confirm): ")
            .context("reading passphrase confirmation from tty")?;
        if p1 != p2 {
            bail!("passphrases do not match");
        }
    }
    Ok(p1)
}

/// Write `bytes` to `path` with mode 0600, refusing to clobber. The
/// `create_new` flag fails the open syscall if the target exists, so
/// the refusal is enforced even when two callers race.
pub fn write_mode_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_mode_0600_creates_file_with_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sealed.env");
        let payload = b"sealed-envelope-bytes";
        write_mode_0600(&path, payload).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn write_mode_0600_sets_owner_only_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("perms.env");
        write_mode_0600(&path, b"x").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        // Only the low 12 permission bits are meaningful; group/other
        // must be zero, owner read+write set.
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn write_mode_0600_refuses_to_clobber() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("once.env");
        write_mode_0600(&path, b"first").unwrap();
        // create_new makes the second open fail with AlreadyExists;
        // the original bytes must be untouched.
        let err = write_mode_0600(&path, b"second").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn write_mode_0600_accepts_empty_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.env");
        write_mode_0600(&path, b"").unwrap();
        assert!(std::fs::read(&path).unwrap().is_empty());
    }

    #[test]
    fn write_mode_0600_errors_on_missing_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("no-such-dir").join("child.env");
        // The parent directory does not exist, so the open fails.
        assert!(write_mode_0600(&path, b"x").is_err());
    }
}
