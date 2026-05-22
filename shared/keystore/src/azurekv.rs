// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Azure Key Vault keystore backend.
//!
//! Wrap/unwrap against one RSA key in a customer-owned vault. The DEK
//! never appears on the Thur host's disk; the manifest holds the
//! wrapped (KV-ciphertext, wrapped in our envelope) blob. Azure KV
//! decrypts on `volume open` to produce the plaintext the daemon
//! hands to `VolumeWriter::open_with_key`.
//!
//! **Encryption-context binding.** Azure KV's `wrapKey`/`unwrapKey`
//! on RSA keys does NOT accept service-side additional authenticated
//! data (only the symmetric `encrypt`/`decrypt` ops on AES keys do).
//! To match the cross-blob protection AWS KMS and Vault Transit give
//! us via their native context fields, we bind the 16-byte
//! `wrap_context` into a JSON envelope around the ciphertext:
//!
//! ```text
//! { "v": 1, "uuid": "<hex>", "ct": "<base64>" }
//! ```
//!
//! The JSON field name `uuid` is wire format and stays — every
//! envelope produced by this backend uses it. The value is the
//! hex-encoded `wrap_context` (a volume UUID for per-volume DEKs or
//! a product-bound binding constant for daemon-identity seeds).
//!
//! On unwrap we parse the envelope, refuse `KeyStoreError::Authz` if
//! the embedded context doesn't match the call's `wrap_context`, then
//! pass `ct` to KV. A stolen `wrapped_dek` repurposed against a
//! different call site fails this check before the bytes ever reach
//! the vault. Documented in `docs/AUTH.md` § VSA keystore
//! backends.

use std::sync::Arc;

use async_trait::async_trait;
use azure_core::credentials::{Secret, TokenCredential};
use azure_core::error::ErrorKind;
use azure_core::http::RequestContent;
use azure_identity::ClientSecretCredential;
use azure_security_keyvault_keys::clients::KeyClient;
use azure_security_keyvault_keys::models::{
    EncryptionAlgorithm, KeyClientGetKeyOptions, KeyClientUnwrapKeyOptions,
    KeyClientWrapKeyOptions, KeyOperationParameters,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::KeyStoreError;
use crate::keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
use crate::keystore_config::ResolvedAzureKvAuth;

/// Wrapping algorithm we use against an RSA key. RSA-OAEP-256 is the
/// modern Microsoft-recommended option; older `RSA-OAEP` (SHA-1) is
/// still accepted by KV but flagged by Microsoft's own security
/// guidance. RSA-1.5 is refused.
const RSA_OAEP_256: EncryptionAlgorithm = EncryptionAlgorithm::RsaOaep256;

/// Envelope version. Bump if the envelope shape changes; current
/// readers refuse anything they don't recognize so a future writer
/// must also bump.
const ENVELOPE_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct WrappedEnvelope {
    v: u8,
    uuid: String,
    ct: String,
}

/// Azure Key Vault-backed keystore. The DEK never appears on the Thur
/// host's disk; the manifest holds our envelope around the wrapped
/// (KV-ciphertext) blob.
#[derive(Clone)]
pub struct AzureKvBackend {
    client: Arc<KeyClient>,
    vault_uri: String,
    key_name: String,
    /// Empty string = "latest" — KV's wrap/unwrap APIs accept the
    /// empty path segment for the version slot and pick the current
    /// active version. Pinned versions live here verbatim.
    key_version: String,
}

impl std::fmt::Debug for AzureKvBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // KeyClient doesn't impl Debug; surface the operator-meaningful
        // fields manually.
        f.debug_struct("AzureKvBackend")
            .field("vault_uri", &self.vault_uri)
            .field("key_name", &self.key_name)
            .field(
                "key_version",
                &if self.key_version.is_empty() {
                    "<latest>"
                } else {
                    self.key_version.as_str()
                },
            )
            .finish()
    }
}

impl AzureKvBackend {
    /// Construct a KV client + handle bound to one RSA key. Credential
    /// construction matches `shared_cloud::azure::AzureBackend`'s AAD
    /// service-principal path so the same operator-facing env-var
    /// shape works (`AZURE_TENANT_ID` / `_CLIENT_ID` / `_CLIENT_SECRET`).
    pub async fn new(
        vault_uri: String,
        key_name: String,
        key_version: Option<String>,
        auth: ResolvedAzureKvAuth,
    ) -> Result<Self, KeyStoreError> {
        debug!(
            "Initializing Azure KV backend: vault_uri={}, key_name={}, key_version={:?}",
            vault_uri, key_name, key_version
        );

        let credential: Arc<dyn TokenCredential> = match auth {
            ResolvedAzureKvAuth::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => ClientSecretCredential::new(
                &tenant_id,
                client_id,
                Secret::from(client_secret),
                None,
            )
            .map_err(|e| {
                KeyStoreError::Auth(format!("azurekv: ClientSecretCredential build failed: {e}"))
            })?,
        };

        let client = KeyClient::new(&vault_uri, credential, None)
            .map_err(|e| KeyStoreError::Other(format!("azurekv: KeyClient::new failed: {e}")))?;

        Ok(Self {
            client: Arc::new(client),
            vault_uri,
            key_name,
            key_version: key_version.unwrap_or_default(),
        })
    }

    /// Build the wrap-side options carrying the pinned version (if
    /// any). KV reads the version from `KeyClientWrapKeyOptions`; the
    /// empty default value picks "latest" by virtue of `key_version:
    /// None`.
    fn wrap_options(&self) -> Option<KeyClientWrapKeyOptions<'_>> {
        if self.key_version.is_empty() {
            None
        } else {
            Some(KeyClientWrapKeyOptions {
                key_version: Some(self.key_version.clone()),
                ..Default::default()
            })
        }
    }

    fn get_options(&self) -> Option<KeyClientGetKeyOptions<'_>> {
        if self.key_version.is_empty() {
            None
        } else {
            Some(KeyClientGetKeyOptions {
                key_version: Some(self.key_version.clone()),
                ..Default::default()
            })
        }
    }

    /// Wrap one buffer via Azure KV `wrapKey`. Returns the raw
    /// KV-side ciphertext; envelope wrapping is handled in
    /// [`KeyStoreBackend::wrap`] below so this stays a thin API
    /// adapter.
    async fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, KeyStoreError> {
        let params = KeyOperationParameters {
            algorithm: Some(RSA_OAEP_256),
            value: Some(plaintext.to_vec()),
            ..Default::default()
        };
        let body: RequestContent<KeyOperationParameters> = params
            .try_into()
            .map_err(|e| KeyStoreError::Other(format!("azurekv.wrapKey body encode: {e}")))?;
        let response = self
            .client
            .wrap_key(&self.key_name, body, self.wrap_options())
            .await
            .map_err(|e| classify_azure_kv_err("azurekv.wrapKey", e))?;
        let result = response
            .into_model()
            .map_err(|e| classify_azure_kv_err("azurekv.wrapKey parse", e))?;
        result.result.ok_or_else(|| {
            KeyStoreError::Other("azurekv.wrapKey returned no `result` field".into())
        })
    }

    /// Unwrap one buffer via Azure KV `unwrapKey`.
    async fn decrypt(&self, wrapped: &[u8]) -> Result<Vec<u8>, KeyStoreError> {
        let params = KeyOperationParameters {
            algorithm: Some(RSA_OAEP_256),
            value: Some(wrapped.to_vec()),
            ..Default::default()
        };
        let body: RequestContent<KeyOperationParameters> = params
            .try_into()
            .map_err(|e| KeyStoreError::Other(format!("azurekv.unwrapKey body encode: {e}")))?;
        let response = self
            .client
            .unwrap_key(
                &self.key_name,
                &self.key_version,
                body,
                None::<KeyClientUnwrapKeyOptions<'_>>,
            )
            .await
            .map_err(|e| classify_azure_kv_err("azurekv.unwrapKey", e))?;
        let result = response
            .into_model()
            .map_err(|e| classify_azure_kv_err("azurekv.unwrapKey parse", e))?;
        result.result.ok_or_else(|| {
            KeyStoreError::Other("azurekv.unwrapKey returned no `result` field".into())
        })
    }

    /// Build the JSON envelope that binds the wrapped ciphertext to
    /// `wrap_context`. Crate-internal so the unit tests can
    /// hand-craft envelopes for the context-mismatch check.
    pub(crate) fn build_envelope(wrap_context: &[u8; 16], ct: &[u8]) -> Vec<u8> {
        let envelope = WrappedEnvelope {
            v: ENVELOPE_VERSION,
            uuid: hex::encode(wrap_context),
            ct: B64.encode(ct),
        };
        // `serde_json::to_vec` only fails on a `Serializer` IO error;
        // a `Vec<u8>` sink can't fail. Fall back to an empty buffer if
        // it ever does — `unwrap_key` will refuse on the next pass.
        serde_json::to_vec(&envelope).unwrap_or_default()
    }

    /// Canonical wrap-target fingerprint. Empty version =
    /// "latest" — surfaced explicitly so two entries pinning latest
    /// fold together, and one pinned vs one floating-latest do NOT
    /// alias (they may diverge at the next key rotation).
    pub(crate) fn fingerprint_str(vault_uri: &str, key_name: &str, key_version: &str) -> String {
        let v = if key_version.is_empty() {
            "<latest>"
        } else {
            key_version
        };
        format!("azurekv:{vault_uri}/{key_name}/{v}")
    }

    /// Parse an envelope and verify it was bound to `wrap_context`.
    /// Returns the inner ciphertext bytes (the raw KV wrap output).
    pub(crate) fn parse_envelope(
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<Vec<u8>, KeyStoreError> {
        let envelope: WrappedEnvelope = serde_json::from_slice(wrapped).map_err(|e| {
            KeyStoreError::Other(format!(
                "azurekv: wrapped_dek does not parse as a v1 JSON envelope: {e}"
            ))
        })?;
        if envelope.v != ENVELOPE_VERSION {
            return Err(KeyStoreError::Other(format!(
                "azurekv: envelope version {} not understood (expected {})",
                envelope.v, ENVELOPE_VERSION
            )));
        }
        let expected = hex::encode(wrap_context);
        if envelope.uuid != expected {
            return Err(KeyStoreError::Authz(format!(
                "azurekv: envelope wrap_context mismatch (envelope='{}', call='{}'); refusing \
                 to unwrap — wrapped blob does not belong to this call site",
                envelope.uuid, expected
            )));
        }
        B64.decode(envelope.ct.as_bytes())
            .map_err(|e| KeyStoreError::Other(format!("azurekv: envelope ct base64 decode: {e}")))
    }
}

#[async_trait]
impl KeyStoreBackend for AzureKvBackend {
    async fn generate_and_wrap(
        &self,
        wrap_context: &[u8; 16],
        source: DekSource,
    ) -> Result<(SecretBytes, Vec<u8>), KeyStoreError> {
        // Azure KV exposes `getRandomBytes` (HSM-only) but no
        // first-class data-key primitive that returns plaintext +
        // ciphertext in one call. Collapse Backend → Daemon so the
        // call site doesn't have to special-case (and so this path
        // doesn't quietly require an HSM-tier vault).
        if matches!(source, DekSource::Backend) {
            debug!(
                "azurekv: DekSource::Backend requested; collapsing to Daemon (KV has no \
                 first-class data-key primitive on software keys)"
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
        let ct = self.encrypt(plaintext.as_bytes()).await?;
        Ok(Self::build_envelope(wrap_context, &ct))
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        let ct = Self::parse_envelope(wrap_context, wrapped)?;
        let plain = self.decrypt(&ct).await?;
        if plain.len() != DEK_LEN {
            return Err(KeyStoreError::Other(format!(
                "azurekv.unwrapKey returned {} bytes, expected {}",
                plain.len(),
                DEK_LEN
            )));
        }
        let mut out = [0u8; DEK_LEN];
        out.copy_from_slice(&plain);
        Ok(SecretBytes::new(out))
    }

    async fn forget(&self, _wrap_context: &[u8; 16]) -> Result<(), KeyStoreError> {
        // KV holds no per-context state. The envelope at the caller's
        // persistence layer is the only thing tied to this call site.
        // (Deleting the KEK itself is out of scope — it serves every
        // call site bound to this backend.)
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "azurekv"
    }

    fn wrap_target_fingerprint(&self) -> String {
        Self::fingerprint_str(&self.vault_uri, &self.key_name, &self.key_version)
    }

    async fn health_check(&self) -> Result<(), KeyStoreError> {
        self.client
            .get_key(&self.key_name, self.get_options())
            .await
            .map_err(|e| classify_azure_kv_err("azurekv.getKey", e))?;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn KeyStoreBackend> {
        Box::new(self.clone())
    }
}

/// Classify an azure_core error into the keystore error taxonomy.
/// `HttpResponse { status, .. }` maps via the same 401/403/404/408/5xx
/// table the Vault backend uses; `Credential` → `Auth`; `Io` →
/// `Network`. Everything else goes to `Other`.
fn classify_azure_kv_err(op: &str, err: azure_core::Error) -> KeyStoreError {
    let label = format!("{op}: {err}");
    match err.kind() {
        ErrorKind::HttpResponse { status, .. } => {
            let code: u16 = (*status).into();
            match code {
                401 => KeyStoreError::Auth(label),
                403 => KeyStoreError::Authz(label),
                404 => KeyStoreError::NotFound(label),
                408 => KeyStoreError::Timeout(label),
                _ => KeyStoreError::Other(label),
            }
        }
        ErrorKind::Credential => KeyStoreError::Auth(label),
        ErrorKind::Io => KeyStoreError::Network(label),
        _ => KeyStoreError::Other(label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::KeyStoreFailureKind;

    fn fixture_uuid() -> [u8; 16] {
        [0xABu8; 16]
    }

    #[test]
    fn envelope_round_trips_with_correct_uuid() {
        let uuid = fixture_uuid();
        let ct = b"\x01\x02\x03kv-wrapped-blob";
        let env = AzureKvBackend::build_envelope(&uuid, ct);
        let back = AzureKvBackend::parse_envelope(&uuid, &env).expect("round-trip");
        assert_eq!(back, ct);
    }

    #[test]
    fn envelope_carries_volume_uuid_hex() {
        let uuid = fixture_uuid();
        let env = AzureKvBackend::build_envelope(&uuid, b"x");
        let parsed: WrappedEnvelope = serde_json::from_slice(&env).expect("parse");
        assert_eq!(parsed.v, ENVELOPE_VERSION);
        assert_eq!(parsed.uuid, "ab".repeat(16));
    }

    #[test]
    fn envelope_refuses_uuid_mismatch() {
        // The critical Azure-RSA AAD-binding guard: an envelope minted
        // against uuid_a must not unwrap when handed to a call
        // referencing uuid_b. Without this check, a stolen wrapped
        // blob could be tacked onto an unrelated volume manifest and
        // KV would happily unwrap it (KV can't bind context to RSA
        // ops itself).
        let uuid_a = [0x01u8; 16];
        let uuid_b = [0x02u8; 16];
        let env = AzureKvBackend::build_envelope(&uuid_a, b"x");
        let err = AzureKvBackend::parse_envelope(&uuid_b, &env).expect_err("must reject");
        match err {
            KeyStoreError::Authz(_) => {}
            other => panic!("expected Authz, got {other:?}"),
        }
    }

    #[test]
    fn envelope_refuses_unknown_version() {
        let bad = serde_json::to_vec(&WrappedEnvelope {
            v: ENVELOPE_VERSION + 1,
            uuid: hex::encode(fixture_uuid()),
            ct: B64.encode(b"x"),
        })
        .expect("encode bad-version envelope");
        let err = AzureKvBackend::parse_envelope(&fixture_uuid(), &bad).expect_err("must reject");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[test]
    fn fingerprint_latest_marker_when_version_empty() {
        let fp = AzureKvBackend::fingerprint_str(
            "https://kv.example.vault.azure.net",
            "thurvsa-kek",
            "",
        );
        assert_eq!(
            fp,
            "azurekv:https://kv.example.vault.azure.net/thurvsa-kek/<latest>"
        );
    }

    #[test]
    fn fingerprint_carries_pinned_version() {
        let fp = AzureKvBackend::fingerprint_str(
            "https://kv.example.vault.azure.net",
            "thurvsa-kek",
            "abc123",
        );
        assert_eq!(
            fp,
            "azurekv:https://kv.example.vault.azure.net/thurvsa-kek/abc123"
        );
    }

    #[test]
    fn fingerprint_pinned_vs_latest_diverge() {
        let pinned = AzureKvBackend::fingerprint_str("https://kv", "k", "v1");
        let latest = AzureKvBackend::fingerprint_str("https://kv", "k", "");
        assert_ne!(pinned, latest);
    }

    #[test]
    fn envelope_refuses_garbage() {
        let err = AzureKvBackend::parse_envelope(&fixture_uuid(), b"not-json").expect_err("");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[test]
    fn classify_failure_kinds_route_correctly() {
        // Synthesized KeyStoreError values → expected
        // KeyStoreFailureKind. The real service-error path is
        // exercised against a real tenant by the env-gated row in the
        // integration script.
        let auth = KeyStoreError::Auth("test".into());
        assert_eq!(auth.kind(), KeyStoreFailureKind::Auth);
        assert!(!auth.is_retryable());

        let authz = KeyStoreError::Authz("test".into());
        assert_eq!(authz.kind(), KeyStoreFailureKind::Authz);
        assert!(!authz.is_retryable());

        let nf = KeyStoreError::NotFound("test".into());
        assert_eq!(nf.kind(), KeyStoreFailureKind::NotFound);
        assert!(!nf.is_retryable());

        let net = KeyStoreError::Network("test".into());
        assert_eq!(net.kind(), KeyStoreFailureKind::Network);
        assert!(net.is_retryable());
    }
}
