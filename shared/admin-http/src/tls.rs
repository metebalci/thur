// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Build a `rustls::ServerConfig` from cert + key + optional mTLS
//! trust anchor.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

pub(crate) fn build_rustls_server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    client_ca_file: Option<&Path>,
) -> Result<ServerConfig> {
    let builder = ServerConfig::builder();
    let builder = match client_ca_file {
        None => builder.with_no_client_auth(),
        Some(ca_path) => {
            let roots = load_ca_roots(ca_path)?;
            let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .context("building WebPkiClientVerifier from client_ca_file")?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    builder
        .with_single_cert(certs, key)
        .context("installing TLS cert/key into rustls ServerConfig")
}

fn load_ca_roots(path: &Path) -> Result<RootCertStore> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading mTLS client CA file {path:?}"))?;
    let certs = CertificateDer::pem_slice_iter(&bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parsing mTLS client CA PEM at {path:?}"))?;
    if certs.is_empty() {
        bail!("mTLS client CA file {path:?} contains no CERTIFICATE blocks");
    }
    let mut roots = RootCertStore::empty();
    for c in certs {
        roots
            .add(c)
            .with_context(|| format!("adding cert from {path:?} to RootCertStore"))?;
    }
    Ok(roots)
}
