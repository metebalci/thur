// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared admin HTTP listener used by both daemons.
//!
//! Owns the bind / serve / TLS plumbing so the two products don't carry
//! duplicate copies. Each daemon builds its own [`axum::Router`] (with
//! its product-specific state + handlers) and hands it to
//! [`run_http_server`].
//!
//! TLS is opt-in via [`TlsConfig`]. When enabled, a missing cert+key
//! pair is auto-generated as a self-signed pair on first boot (logged
//! at `WARN` with the SHA-256 fingerprint).

mod config;
mod selfsign;
mod tls;

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::OnceCell;
use tracing::{info, warn};

pub use config::{HttpListenerConfig, TlsConfig};
pub use selfsign::{CertGenerationOutcome, RegenerateOutcome, regenerate_cert};

/// Bind the listener and serve `router`. Returns when the listener
/// exits (either gracefully or with an error).
///
/// Plaintext path: `axum_server::bind`. TLS path:
/// `axum_server::bind_rustls` with a server config built from the
/// configured cert/key pair (auto-generated if both files are absent).
pub async fn run_http_server(cfg: HttpListenerConfig, router: axum::Router) -> Result<()> {
    ensure_crypto_provider_installed();

    let addr: std::net::SocketAddr = cfg
        .listen
        .parse()
        .with_context(|| format!("parsing HTTP listen address '{}'", cfg.listen))?;

    match cfg.tls {
        None => {
            info!("HTTP server listening on http://{addr}");
            axum_server::bind(addr)
                .serve(router.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await
                .context("HTTP server exited")?;
        }
        Some(tls_cfg) => {
            let (certs, key, outcome) = selfsign::load_or_generate_cert_key(&tls_cfg)
                .context("loading or generating TLS cert/key")?;

            if outcome.was_generated {
                warn!(
                    cert_file = %tls_cfg.cert_file.display(),
                    key_file = %tls_cfg.key_file.display(),
                    fingerprint_sha256 = %outcome.cert_fingerprint_sha256,
                    sans = %outcome.sans.join(","),
                    "admin HTTP TLS: self-signed cert generated; replace with CA-issued cert for production",
                );
            } else {
                info!(
                    cert_file = %tls_cfg.cert_file.display(),
                    fingerprint_sha256 = %outcome.cert_fingerprint_sha256,
                    "admin HTTP TLS: loaded existing cert/key",
                );
            }

            let server_config =
                tls::build_rustls_server_config(certs, key, tls_cfg.client_ca_file.as_deref())
                    .context("building rustls ServerConfig")?;
            let mtls = tls_cfg.client_ca_file.is_some();
            info!("HTTPS server listening on https://{addr} (mtls={mtls})");

            let rustls_cfg =
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
            axum_server::bind_rustls(addr, rustls_cfg)
                .serve(router.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await
                .context("HTTPS server exited")?;
        }
    }
    Ok(())
}

/// Install the default rustls `CryptoProvider` exactly once per process.
///
/// rustls 0.23 requires a provider to be selected before any
/// `ServerConfig` / `ClientConfig` is built. Other crates in the
/// workspace (`shared-keystore::kmip`, `shared-object-store::gcs`) also call
/// `install_default()`; whichever runs first wins, and the rest become
/// no-ops. Both `ring` and `aws-lc-rs` providers support the cert
/// algorithms this crate emits (ECDSA P-256), so the race is benign.
fn ensure_crypto_provider_installed() {
    static ONCE: OnceCell<()> = OnceCell::const_new();
    ONCE.set(()).ok();
    // `install_default` returns Err if a provider is already installed
    // — that's fine, we accept whichever ran first.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
