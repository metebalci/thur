// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Privilege drop for daemon-down CLI write paths.
//!
//! Daemon-down verbs (`library init` / `library modify` on the VTL
//! side, `volume key migrate` on the VSA side, etc.) write files
//! under `data_dir`. The daemon
//! later reads and rewrites those files as its service user. If the
//! operator runs the CLI under plain `sudo`, the resulting files
//! end up owned by root and the daemon can no longer modify them.
//!
//! Rather than make operators remember `sudo -u <product> …`, the
//! CLI detects euid 0 and drops to the configured target user
//! (`--user`, default = the product's system user) before opening
//! any file. Running as a non-root, non-target user is left alone —
//! the subsequent file I/O will surface EACCES naturally if the
//! perms are wrong.

use anyhow::{Context, Result, anyhow};
use nix::unistd::{User, geteuid, initgroups, setgid, setuid};
use std::ffi::CString;

/// Drop privileges to `target_user` if currently running as root.
///
/// * euid == target user's uid → no-op (already there, e.g.
///   `sudo -u <product>`).
/// * euid == 0 → setgid → initgroups → setuid; logs the transition
///   to stderr.
/// * any other euid → no-op; later file I/O surfaces permission
///   errors itself.
pub fn drop_to_user_if_root(target_user: &str) -> Result<()> {
    let euid = geteuid();

    // Non-root → no-op. Skip the passwd lookup entirely so a dev box
    // without the daemon's service user provisioned still runs the
    // CLI (the contract is "file I/O surfaces EACCES on its own").
    if euid.as_raw() != 0 {
        return Ok(());
    }

    let user = User::from_name(target_user)
        .with_context(|| format!("looking up user '{target_user}' in passwd"))?
        .ok_or_else(|| anyhow!("user '{target_user}' not found in passwd database"))?;

    let name_cstr = CString::new(user.name.as_bytes())
        .with_context(|| format!("user name '{}' contains a NUL byte", user.name))?;

    eprintln!(
        "Running as root; dropping privileges to {}({}:{}) before writing data_dir.",
        user.name,
        user.uid.as_raw(),
        user.gid.as_raw(),
    );

    // Order matters: drop primary group, then supplementary groups,
    // then user. Once setuid succeeds we're no longer privileged and
    // can't change groups anymore.
    setgid(user.gid).context("setgid to target group failed")?;
    initgroups(&name_cstr, user.gid).context("initgroups for target user failed")?;
    setuid(user.uid).context("setuid to target user failed")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The test suite runs as an unprivileged user (euid != 0), so
    // `drop_to_user_if_root` must hit the non-root early return — a
    // no-op `Ok(())` that never touches the passwd database. This is
    // the only branch reachable without root; the setgid/setuid path
    // is exercised by the daemon-down integration scripts.

    #[test]
    fn non_root_is_a_noop_for_any_target() {
        if geteuid().as_raw() == 0 {
            // Skip if the suite is somehow running as root — calling
            // the function would actually drop privileges.
            return;
        }
        // A user that almost certainly does not exist: the early
        // return must fire before any passwd lookup, so this still
        // succeeds.
        assert!(drop_to_user_if_root("definitely-not-a-real-user-xyz").is_ok());
    }

    #[test]
    fn non_root_noop_for_realistic_product_users() {
        if geteuid().as_raw() == 0 {
            return;
        }
        // The default targets the products pass — still a no-op when
        // not root, regardless of whether they exist on the box.
        assert!(drop_to_user_if_root("thurvtl").is_ok());
        assert!(drop_to_user_if_root("thurvsa").is_ok());
    }
}
