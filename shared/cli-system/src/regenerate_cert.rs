// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `<product> system regenerate-cert` — regenerate the admin HTTP
//! self-signed TLS cert+key. Daemon-down only: the rewritten cert is
//! only served after a daemon restart.
//!
//! Refuses to overwrite a cert the daemon did not auto-generate. The
//! detection — a `.autogen` fingerprint marker sidecar — lives in
//! [`shared_admin_http::regenerate_cert`].

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use shared_admin_client::AdminClient;
use shared_admin_http::TlsConfig;
use shared_naming::ProductIdentity;

/// `system regenerate-cert`. Refuses if the daemon is running (admin
/// socket reachable) or if `http.tls` is not configured for TLS.
pub async fn cmd_regenerate_cert(
    product: &'static ProductIdentity,
    config_path: &Path,
) -> Result<()> {
    let admin = AdminClient::auto_discover(product);
    if admin.ping().await {
        bail!(
            "{}d is running; `system regenerate-cert` can only run \
             while the daemon is stopped. Stop the daemon, re-run, then restart.",
            product.metric_prefix
        );
    }

    let tls = TlsConfig::load_from_conffile(config_path)
        .with_context(|| format!("reading http.tls from {}", config_path.display()))?
        .ok_or_else(|| {
            anyhow!(
                "http.tls is not configured (cert_file/key_file empty in {}); nothing to \
                 regenerate. Set http.tls.cert_file + http.tls.key_file first.",
                config_path.display()
            )
        })?;

    let outcome = shared_admin_http::regenerate_cert(&tls)?;

    match &outcome.prior_fingerprint {
        None => println!(
            "OK: self-signed cert generated: {}",
            tls.cert_file.display()
        ),
        Some(prior) => {
            println!(
                "OK: self-signed cert regenerated: {}",
                tls.cert_file.display()
            );
            println!("  previous fingerprint: {prior}");
        }
    }
    println!(
        "  new fingerprint:      {}",
        outcome.cert_fingerprint_sha256
    );
    println!("  SANs: {}", outcome.sans.join(", "));
    println!(
        "  Restart the {}d to serve the new cert.",
        product.metric_prefix
    );
    Ok(())
}
