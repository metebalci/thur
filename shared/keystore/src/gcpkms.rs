// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! GCP Cloud KMS keystore backend.
//!
//! Symmetric `encrypt`/`decrypt` against one CryptoKey in Cloud KMS.
//! The DEK never appears on the Thur host's disk; the manifest holds
//! the wrapped (KMS-ciphertext) blob.
//!
//! **Encryption-context binding.** GCP KMS accepts a native
//! `additional_authenticated_data` field on `EncryptRequest` /
//! `DecryptRequest`. We pass `hex(wrap_context)` (as bytes) on every
//! call; KMS validates the AAD byte-for-byte and rejects mismatching
//! decrypts. A stolen wrapped blob plus KMS access cannot decrypt to
//! the right key without also presenting the matching context bytes
//! — same protection profile AWS KMS gives us via encryption context.
//!
//! Threat model + auth flow: see `docs/AUTH.md` § VSA keystore
//! backends.

use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_auth::credentials::{Builder as CredsBuilder, Credentials, service_account};
use google_cloud_gax::error::rpc::Code;
use google_cloud_kms_v1::Error as KmsError;
use google_cloud_kms_v1::client::KeyManagementService;
use tracing::debug;

use crate::error::KeyStoreError;
use crate::keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
use crate::keystore_config::ResolvedGcpKmsAuth;

/// GCP Cloud KMS-backed keystore. The DEK never appears on the Thur
/// host's disk; the manifest holds the wrapped (KMS-ciphertext)
/// blob.
#[derive(Clone, Debug)]
pub struct GcpKmsBackend {
    client: KeyManagementService,
    /// Full resource name:
    /// `projects/P/locations/L/keyRings/R/cryptoKeys/K`. The backend
    /// passes it verbatim to `encrypt().set_name()` /
    /// `decrypt().set_name()` / `get_crypto_key().set_name()`.
    key_name: String,
}

impl GcpKmsBackend {
    /// Construct a KMS client + handle bound to one CryptoKey.
    /// Credential loading mirrors `shared_cloud::gcs::GcsBackend::new`
    /// — service-account JSON key file when configured, ADC chain
    /// otherwise (`GOOGLE_APPLICATION_CREDENTIALS` env →
    /// `gcloud auth application-default login` → GCE/GKE metadata
    /// server).
    pub async fn new(
        key_name: String,
        auth: Option<ResolvedGcpKmsAuth>,
    ) -> Result<Self, KeyStoreError> {
        debug!("Initializing GCP KMS backend: key_name={}", key_name);

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
        Ok(Self { client, key_name })
    }

    fn aad(wrap_context: &[u8; 16]) -> Bytes {
        Bytes::from(hex::encode(wrap_context).into_bytes())
    }
}

async fn build_credentials(auth: Option<ResolvedGcpKmsAuth>) -> Result<Credentials, KeyStoreError> {
    match auth {
        Some(ResolvedGcpKmsAuth::ServiceAccountKey { path }) => {
            debug!(
                "gcpkms: using service-account key file {} (ADC bypassed)",
                path
            );
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
        Some(ResolvedGcpKmsAuth::Adc) | None => {
            debug!("gcpkms: using Application Default Credentials chain");
            CredsBuilder::default()
                .build()
                .map_err(|e| KeyStoreError::Auth(format!("gcpkms: ADC credential build: {e}")))
        }
    }
}

#[async_trait]
impl KeyStoreBackend for GcpKmsBackend {
    async fn generate_and_wrap(
        &self,
        wrap_context: &[u8; 16],
        source: DekSource,
    ) -> Result<(SecretBytes, Vec<u8>), KeyStoreError> {
        // Cloud KMS has `generate_random_bytes` but it requires an HSM
        // CryptoKeyVersion and the extra `cloudkms.locations.generateRandomBytes`
        // permission. Collapse Backend → Daemon so a software CryptoKey
        // works out of the box.
        if matches!(source, DekSource::Backend) {
            debug!(
                "gcpkms: DekSource::Backend requested; collapsing to Daemon (cloudkms \
                 generate_random_bytes requires HSM keys + extra IAM)"
            );
        }
        use shared_crypto::{OsRng, RngCore};
        let mut plain = [0u8; DEK_LEN];
        OsRng.fill_bytes(&mut plain);
        let wrapped = self.wrap(wrap_context, &SecretBytes::new(plain)).await?;
        Ok((SecretBytes::new(plain), wrapped))
    }

    async fn wrap(
        &self,
        wrap_context: &[u8; 16],
        plaintext: &SecretBytes,
    ) -> Result<Vec<u8>, KeyStoreError> {
        let response = self
            .client
            .encrypt()
            .set_name(self.key_name.clone())
            .set_plaintext(Bytes::copy_from_slice(plaintext.as_bytes()))
            .set_additional_authenticated_data(Self::aad(wrap_context))
            .send()
            .await
            .map_err(|e| classify_gcp_kms_err("gcpkms.encrypt", e))?;
        Ok(response.ciphertext.to_vec())
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        let response = self
            .client
            .decrypt()
            .set_name(self.key_name.clone())
            .set_ciphertext(Bytes::copy_from_slice(wrapped))
            .set_additional_authenticated_data(Self::aad(wrap_context))
            .send()
            .await
            .map_err(|e| classify_gcp_kms_err("gcpkms.decrypt", e))?;
        let plain = response.plaintext;
        if plain.len() != DEK_LEN {
            return Err(KeyStoreError::Other(format!(
                "gcpkms.decrypt returned {} bytes, expected {}",
                plain.len(),
                DEK_LEN
            )));
        }
        let mut out = [0u8; DEK_LEN];
        out.copy_from_slice(&plain);
        Ok(SecretBytes::new(out))
    }

    async fn forget(&self, _wrap_context: &[u8; 16]) -> Result<(), KeyStoreError> {
        // KMS holds no per-context state. The wrapped blob at the
        // caller's persistence layer is the only thing tied to this
        // call site; the CryptoKey itself serves every call site
        // bound to this backend.
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "gcpkms"
    }

    fn wrap_target_fingerprint(&self) -> String {
        // `key_name` is the full GCP resource name
        // (`projects/P/locations/L/keyRings/R/cryptoKeys/K`) — already
        // globally unique by GCP construction, so no extra fields.
        format!("gcpkms:{}", self.key_name)
    }

    async fn health_check(&self) -> Result<(), KeyStoreError> {
        self.client
            .get_crypto_key()
            .set_name(self.key_name.clone())
            .send()
            .await
            .map_err(|e| classify_gcp_kms_err("gcpkms.get_crypto_key", e))?;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn KeyStoreBackend> {
        Box::new(self.clone())
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
    use crate::error::KeyStoreFailureKind;

    #[test]
    fn context_carries_volume_uuid_hex() {
        let uuid = [0xABu8; 16];
        let aad = GcpKmsBackend::aad(&uuid);
        assert_eq!(aad.as_ref(), b"ab".repeat(16).as_slice());
        assert_eq!(aad.len(), 32);
    }

    #[test]
    fn classify_failure_kinds_route_correctly() {
        // Synthesized KeyStoreError values → expected
        // KeyStoreFailureKind. The real service-error path is
        // exercised against a real project by the env-gated row in
        // the integration script.
        let auth = KeyStoreError::Auth("test".into());
        assert_eq!(auth.kind(), KeyStoreFailureKind::Auth);
        assert!(!auth.is_retryable());

        let authz = KeyStoreError::Authz("test".into());
        assert_eq!(authz.kind(), KeyStoreFailureKind::Authz);
        assert!(!authz.is_retryable());

        let nf = KeyStoreError::NotFound("test".into());
        assert_eq!(nf.kind(), KeyStoreFailureKind::NotFound);
        assert!(!nf.is_retryable());

        let to = KeyStoreError::Timeout("test".into());
        assert_eq!(to.kind(), KeyStoreFailureKind::Timeout);
        assert!(to.is_retryable());
    }
}
