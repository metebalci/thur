// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `<product> system set-admin-password` — set (or replace) the single
//! shared web-admin password that gates the TCP HTTP listener's
//! protected routes (issue #4).
//!
//! Daemon-routed: the password is prompted with no echo and sent over
//! the local peer-cred admin socket; the daemon hashes it server-side
//! (Argon2id) and writes `<data_dir>/admin-password.json`. The
//! plaintext never touches disk and never leaves the host.

use anyhow::{Context, Result, bail};
use shared_admin_client::AdminClient;
use shared_naming::ProductIdentity;

/// Synthetic web-admin username the daemon accepts over HTTP Basic.
/// Mirrors `shared_admin_auth::WEBADMIN_USER` — kept as a literal here
/// so the CLI doesn't pull the daemon-side auth crate (argon2 / axum)
/// into its binary just for a help string.
const WEBADMIN_USER: &str = "webadmin";

/// `system set-admin-password`. Prompts twice (no echo) — or reads the
/// per-product `<PREFIX>_ADMIN_PASSWORD` env var for non-interactive
/// provisioning — and refuses if the daemon isn't running (it owns
/// `<data_dir>/admin-password.json`).
pub async fn cmd_set_admin_password(product: &'static ProductIdentity) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    if !admin.ping().await {
        bail!(
            "{}d is not running; `system set-admin-password` is daemon-routed \
             (the daemon owns <data_dir>/admin-password.json). Start the daemon and re-run.",
            product.metric_prefix
        );
    }

    let password = resolve_new_password(product)?;

    admin
        .post_unit(
            "/api/v1/system/admin-password",
            &serde_json::json!({ "password": password }),
        )
        .await?;

    println!("OK: web-admin password set (effective immediately, no restart).");
    println!("  Log in to the HTTP listener as user '{WEBADMIN_USER}'.");
    Ok(())
}

/// Resolve the new password from the per-product
/// `<METRIC_PREFIX>_ADMIN_PASSWORD` env var (e.g. `THURVTL_ADMIN_PASSWORD`)
/// for non-interactive automation, otherwise prompt twice on the tty
/// with no echo and confirm they match. The daemon enforces the length
/// floor and surfaces a clear error if it's too short.
fn resolve_new_password(product: &ProductIdentity) -> Result<String> {
    let env_name = format!("{}_ADMIN_PASSWORD", product.metric_prefix.to_uppercase());
    if let Ok(env_pw) = std::env::var(&env_name) {
        if env_pw.is_empty() {
            bail!("{env_name} is set but empty");
        }
        return Ok(env_pw);
    }

    let p1 = rpassword::prompt_password("New web-admin password: ")
        .context("reading password from tty")?;
    if p1.is_empty() {
        bail!("empty password rejected");
    }
    let p2 = rpassword::prompt_password("Confirm password: ")
        .context("reading confirmation from tty")?;
    if p1 != p2 {
        bail!("passwords do not match");
    }
    Ok(p1)
}
