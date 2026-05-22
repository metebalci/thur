// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration smoke against a local PyKMIP server.
//!
//! All cases here are `#[ignore]` so the default `cargo test` run skips
//! them. They drive a real `wrap` / `unwrap` round-trip against a
//! locally-running PyKMIP — set up via the companion scripts in
//! `vsa/scripts/`:
//!
//! ```bash
//! # Terminal 1: start the server (foreground).
//! ~/kmip/bin/python vsa/scripts/pykmip-server.py
//!
//! # Terminal 2: provision an AES-256 KEK + run the tests.
//! KEK_UID=$(~/kmip/bin/python vsa/scripts/pykmip-create-kek.py)
//! THURVSA_KMIP_KEK_UID=$KEK_UID cargo test -p shared-keystore \
//!     --test kmip_pykmip -- --ignored --nocapture
//! ```
//!
//! The server's cert + key + CA bundle live at /tmp/thurvsa-kmip/ —
//! the same path the server script writes them to.

use shared_keystore::{
    DekSource, KeyStoreBackend, KeyStoreError, KmipBackend, ResolvedKmipCaBundle, ResolvedKmipMtls,
};

const CERT_DIR: &str = "/tmp/thurvsa-kmip";
const ENDPOINT: &str = "127.0.0.1:5696";

fn kek_uid() -> String {
    std::env::var("THURVSA_KMIP_KEK_UID")
        .expect("set THURVSA_KMIP_KEK_UID — run vsa/scripts/pykmip-create-kek.py to provision one")
}

async fn make_backend() -> KmipBackend {
    let mtls = ResolvedKmipMtls::ClientCert {
        cert_path: format!("{CERT_DIR}/client.crt"),
        key_path: format!("{CERT_DIR}/client.key"),
    };
    let ca = ResolvedKmipCaBundle::Path {
        path: format!("{CERT_DIR}/ca.crt"),
    };
    // The server cert SAN lists both `localhost` and `127.0.0.1` —
    // override SNI to the DNS form, the more widely-tested rustls
    // path. Endpoint stays as the IP literal so we don't depend on
    // /etc/hosts.
    KmipBackend::new(
        ENDPOINT.to_string(),
        kek_uid(),
        Some("localhost".to_string()),
        Some(ca),
        mtls,
        None,
    )
    .await
    .expect("KmipBackend::new failed — is the PyKMIP server running?")
}

#[tokio::test]
#[ignore = "requires local PyKMIP — see vsa/scripts/pykmip-server.py"]
async fn pykmip_health_check_advertises_encrypt_decrypt() {
    let backend = make_backend().await;
    backend
        .health_check()
        .await
        .expect("Query Operations failed; server should advertise both Encrypt and Decrypt");
}

#[tokio::test]
#[ignore = "requires local PyKMIP — see vsa/scripts/pykmip-server.py"]
async fn pykmip_wrap_unwrap_round_trip() {
    let backend = make_backend().await;
    let uuid = [0xABu8; 16];
    let (plaintext, wrapped) = backend
        .generate_and_wrap(&uuid, DekSource::Daemon)
        .await
        .expect("generate_and_wrap");
    eprintln!("[smoke] wrapped envelope is {} bytes", wrapped.len());
    let unwrapped = backend.unwrap(&uuid, &wrapped).await.expect("unwrap");
    assert_eq!(
        plaintext.as_bytes(),
        unwrapped.as_bytes(),
        "round-trip DEK did not match"
    );
}

#[tokio::test]
#[ignore = "requires local PyKMIP — see vsa/scripts/pykmip-server.py"]
async fn pykmip_unwrap_rejects_uuid_mismatch_before_server_call() {
    // Belt-and-suspenders: KMIP's AEAD-AAD binding would also catch
    // this server-side, but our envelope check refuses *before* any
    // network IO. Verifies that path against a real envelope minted
    // by the production wrap call.
    let backend = make_backend().await;
    let uuid_a = [0xAAu8; 16];
    let uuid_b = [0xBBu8; 16];
    let (_plain, wrapped) = backend
        .generate_and_wrap(&uuid_a, DekSource::Daemon)
        .await
        .expect("generate_and_wrap");
    let err = backend
        .unwrap(&uuid_b, &wrapped)
        .await
        .expect_err("must refuse mismatched UUID");
    assert!(
        matches!(err, KeyStoreError::Authz(_)),
        "expected Authz, got {err:?}"
    );
}
