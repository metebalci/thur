// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! GCP Cloud KMS SDK seam.
//!
//! [`GcpKmsApi`] captures exactly what [`crate::GcpKmsBackend`] needs
//! from `google-cloud-kms-v1`: `encrypt`, `decrypt`, `get_crypto_key`.
//! [`RealGcpKmsApi`] is the only place SDK types appear — error
//! classification (gRPC `Code` → [`KeyStoreError`]) lives inside it.
//! Tests inject a mock impl that hands back canned outcomes directly,
//! without an HTTP wire.
//!
//! Trade-off: a malformed proto bug in the SDK adapter layer won't
//! surface from `cargo test`. Those failure modes are caught by the
//! env-gated `vsa/scripts/test-keystore.sh` rig run against a real
//! GCP project — same coverage we had before.

use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_auth::credentials::{Builder as CredsBuilder, Credentials, service_account};
use google_cloud_gax::error::rpc::Code;
use google_cloud_kms_v1::Error as KmsError;
use google_cloud_kms_v1::client::KeyManagementService;

use crate::error::KeyStoreError;
use crate::keystore_config::ResolvedGcpKmsAuth;

/// Per-operation surface `GcpKmsBackend` actually needs.
#[async_trait]
pub(crate) trait GcpKmsApi: Send + Sync + std::fmt::Debug {
    async fn encrypt(
        &self,
        key_name: &str,
        plaintext: Bytes,
        aad: Bytes,
    ) -> Result<Bytes, KeyStoreError>;
    async fn decrypt(
        &self,
        key_name: &str,
        ciphertext: Bytes,
        aad: Bytes,
    ) -> Result<Bytes, KeyStoreError>;
    async fn get_crypto_key(&self, key_name: &str) -> Result<(), KeyStoreError>;
}

/// Production `GcpKmsApi` impl.
#[derive(Clone)]
pub(crate) struct RealGcpKmsApi {
    client: KeyManagementService,
}

impl std::fmt::Debug for RealGcpKmsApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealGcpKmsApi").finish()
    }
}

impl RealGcpKmsApi {
    pub(crate) async fn from_auth(auth: Option<ResolvedGcpKmsAuth>) -> Result<Self, KeyStoreError> {
        let creds = build_credentials(auth).await?;
        let client = KeyManagementService::builder()
            .with_credentials(creds)
            .build()
            .await
            .map_err(|e| {
                KeyStoreError::Other(format!(
                    "gcpkms: KeyManagementService client build failed: {e}"
                ))
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl GcpKmsApi for RealGcpKmsApi {
    async fn encrypt(
        &self,
        key_name: &str,
        plaintext: Bytes,
        aad: Bytes,
    ) -> Result<Bytes, KeyStoreError> {
        let response = self
            .client
            .encrypt()
            .set_name(key_name.to_string())
            .set_plaintext(plaintext)
            .set_additional_authenticated_data(aad)
            .send()
            .await
            .map_err(|e| classify_gcp_kms_err("gcpkms.encrypt", e))?;
        Ok(response.ciphertext)
    }

    async fn decrypt(
        &self,
        key_name: &str,
        ciphertext: Bytes,
        aad: Bytes,
    ) -> Result<Bytes, KeyStoreError> {
        let response = self
            .client
            .decrypt()
            .set_name(key_name.to_string())
            .set_ciphertext(ciphertext)
            .set_additional_authenticated_data(aad)
            .send()
            .await
            .map_err(|e| classify_gcp_kms_err("gcpkms.decrypt", e))?;
        Ok(response.plaintext)
    }

    async fn get_crypto_key(&self, key_name: &str) -> Result<(), KeyStoreError> {
        self.client
            .get_crypto_key()
            .set_name(key_name.to_string())
            .send()
            .await
            .map_err(|e| classify_gcp_kms_err("gcpkms.get_crypto_key", e))?;
        Ok(())
    }
}

async fn build_credentials(auth: Option<ResolvedGcpKmsAuth>) -> Result<Credentials, KeyStoreError> {
    match auth {
        Some(ResolvedGcpKmsAuth::ServiceAccountKey { path }) => {
            let json = tokio::fs::read_to_string(&path).await.map_err(|e| {
                KeyStoreError::Auth(format!(
                    "gcpkms: service-account key file '{path}' could not be loaded: {e}"
                ))
            })?;
            let value: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
                KeyStoreError::Auth(format!(
                    "gcpkms: service-account key file '{path}' could not be parsed: {e}"
                ))
            })?;
            service_account::Builder::new(value).build().map_err(|e| {
                KeyStoreError::Auth(format!(
                    "gcpkms: service-account credential build from '{path}' failed: {e}"
                ))
            })
        }
        Some(ResolvedGcpKmsAuth::Adc) | None => CredsBuilder::default()
            .build()
            .map_err(|e| KeyStoreError::Auth(format!("gcpkms: ADC credential build: {e}"))),
    }
}

/// Classify a google-cloud-rust error into the keystore error
/// taxonomy. The Status code (gRPC-flavored) drives the mapping; an
/// error without a Status (transport / serialization) gets a coarser
/// classification via Error::is_* probes.
fn classify_gcp_kms_err(op: &str, err: KmsError) -> KeyStoreError {
    let label = format!("{op}: {err}");
    if let Some(status) = err.status() {
        return match status.code {
            Code::Unauthenticated => KeyStoreError::Auth(label),
            Code::PermissionDenied => KeyStoreError::Authz(label),
            Code::NotFound => KeyStoreError::NotFound(label),
            Code::DeadlineExceeded => KeyStoreError::Timeout(label),
            Code::Unavailable
            | Code::ResourceExhausted
            | Code::Internal
            | Code::Unknown
            | Code::Aborted => KeyStoreError::Other(label),
            _ => KeyStoreError::Other(label),
        };
    }
    if err.is_timeout() {
        return KeyStoreError::Timeout(label);
    }
    if err.is_serialization() || err.is_deserialization() {
        return KeyStoreError::Other(label);
    }
    // Transport / DNS / TLS surfaced through http_* fields; treat
    // them as network so the retry budget kicks in.
    if err.http_status_code().is_some() || err.http_headers().is_some() {
        return KeyStoreError::Other(label);
    }
    KeyStoreError::Network(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_credentials_reports_missing_key_file() {
        let err = build_credentials(Some(ResolvedGcpKmsAuth::ServiceAccountKey {
            path: "/no/such/key.json".into(),
        }))
        .await
        .expect_err("missing key file");
        assert!(matches!(err, KeyStoreError::Auth(_)));
    }

    #[tokio::test]
    async fn build_credentials_rejects_non_json_key_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("bad.json");
        tokio::fs::write(&path, b"not json").await.expect("seed");
        let err = build_credentials(Some(ResolvedGcpKmsAuth::ServiceAccountKey {
            path: path.to_string_lossy().into_owned(),
        }))
        .await
        .expect_err("bad json");
        assert!(matches!(err, KeyStoreError::Auth(_)));
    }
}
