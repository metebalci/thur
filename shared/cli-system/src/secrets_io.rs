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
