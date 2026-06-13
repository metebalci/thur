// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Azure Key Vault SDK seam.
//!
//! [`AzureKvApi`] captures exactly what [`crate::AzureKvBackend`] needs
//! from `azure_security_keyvault_keys`: `wrap_key`, `unwrap_key`,
//! `get_key`. [`RealAzureKvApi`] is the only place SDK types appear —
//! HTTP-status classification and the `KeyOperationParameters` body
//! shaping live inside it. Tests inject a mock impl that hands back
//! canned outcomes directly, without an HTTP wire or AAD round-trip.
//!
//! Trade-off: a malformed payload or KV-side header bug in the SDK
//! adapter layer won't surface from `cargo test`. Those failure
//! modes are caught by the env-gated `vsa/scripts/test-keystore.sh`
//! rig run against a real KV tenant — same coverage we had before.

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

use crate::error::KeyStoreError;
use crate::keystore_config::ResolvedAzureKvAuth;

/// Wrapping algorithm we use against an RSA key. RSA-OAEP-256 is the
/// modern Microsoft-recommended option; older `RSA-OAEP` (SHA-1) is
/// still accepted by KV but flagged by Microsoft's own security
/// guidance. RSA-1.5 is refused.
const RSA_OAEP_256: EncryptionAlgorithm = EncryptionAlgorithm::RsaOaep256;

/// Per-operation surface `AzureKvBackend` actually needs from the
/// `azure_security_keyvault_keys` SDK.
#[async_trait]
pub(crate) trait AzureKvApi: Send + Sync + std::fmt::Debug {
    /// Wrap `plaintext` and return `(ciphertext, resolved_version)`.
    /// `resolved_version` is the version segment of the `kid` the vault
    /// actually used (KV resolves an empty/`latest` request to a concrete
    /// version); the caller persists it so a later KEK rotation can't
    /// strand the ciphertext against the wrong private key (issue #137).
    async fn wrap_key(
        &self,
        key_name: &str,
        key_version: &str,
        plaintext: Vec<u8>,
    ) -> Result<(Vec<u8>, Option<String>), KeyStoreError>;
    async fn unwrap_key(
        &self,
        key_name: &str,
        key_version: &str,
        ciphertext: Vec<u8>,
    ) -> Result<Vec<u8>, KeyStoreError>;
    async fn get_key(&self, key_name: &str, key_version: &str) -> Result<(), KeyStoreError>;
}

/// Production `AzureKvApi` impl.
#[derive(Clone)]
pub(crate) struct RealAzureKvApi {
    client: Arc<KeyClient>,
}

impl std::fmt::Debug for RealAzureKvApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealAzureKvApi").finish()
    }
}

impl RealAzureKvApi {
    pub(crate) fn from_auth(
        vault_uri: &str,
        auth: ResolvedAzureKvAuth,
    ) -> Result<Self, KeyStoreError> {
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
        let client = KeyClient::new(vault_uri, credential, None)
            .map_err(|e| KeyStoreError::Other(format!("azurekv: KeyClient::new failed: {e}")))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    fn wrap_options(key_version: &str) -> Option<KeyClientWrapKeyOptions<'_>> {
        if key_version.is_empty() {
            None
        } else {
            Some(KeyClientWrapKeyOptions {
                key_version: Some(key_version.to_string()),
                ..Default::default()
            })
        }
    }

    fn get_options(key_version: &str) -> Option<KeyClientGetKeyOptions<'_>> {
        if key_version.is_empty() {
            None
        } else {
            Some(KeyClientGetKeyOptions {
                key_version: Some(key_version.to_string()),
                ..Default::default()
            })
        }
    }
}

#[async_trait]
impl AzureKvApi for RealAzureKvApi {
    async fn wrap_key(
        &self,
        key_name: &str,
        key_version: &str,
        plaintext: Vec<u8>,
    ) -> Result<(Vec<u8>, Option<String>), KeyStoreError> {
        let params = KeyOperationParameters {
            algorithm: Some(RSA_OAEP_256),
            value: Some(plaintext),
            ..Default::default()
        };
        let body: RequestContent<KeyOperationParameters> = params
            .try_into()
            .map_err(|e| KeyStoreError::Other(format!("azurekv.wrapKey body encode: {e}")))?;
        let response = self
            .client
            .wrap_key(key_name, body, Self::wrap_options(key_version))
            .await
            .map_err(|e| classify_azure_kv_err("azurekv.wrapKey", e))?;
        let result = response
            .into_model()
            .map_err(|e| classify_azure_kv_err("azurekv.wrapKey parse", e))?;
        // The `kid` is the full versioned key URL
        // (https://vault/keys/<name>/<version>); keep its trailing
        // version segment so unwrap can target this exact key version
        // even after a KEK rotation moved "latest" (issue #137).
        let version = result
            .kid
            .as_deref()
            .and_then(|kid| kid.rsplit('/').next())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string());
        let ct = result.result.ok_or_else(|| {
            KeyStoreError::Other("azurekv.wrapKey returned no `result` field".into())
        })?;
        Ok((ct, version))
    }

    async fn unwrap_key(
        &self,
        key_name: &str,
        key_version: &str,
        ciphertext: Vec<u8>,
    ) -> Result<Vec<u8>, KeyStoreError> {
        let params = KeyOperationParameters {
            algorithm: Some(RSA_OAEP_256),
            value: Some(ciphertext),
            ..Default::default()
        };
        let body: RequestContent<KeyOperationParameters> = params
            .try_into()
            .map_err(|e| KeyStoreError::Other(format!("azurekv.unwrapKey body encode: {e}")))?;
        let response = self
            .client
            .unwrap_key(
                key_name,
                key_version,
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

    async fn get_key(&self, key_name: &str, key_version: &str) -> Result<(), KeyStoreError> {
        self.client
            .get_key(key_name, Self::get_options(key_version))
            .await
            .map_err(|e| classify_azure_kv_err("azurekv.getKey", e))?;
        Ok(())
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
    use azure_core::http::StatusCode;

    #[test]
    fn wrap_options_empty_version_returns_none() {
        assert!(RealAzureKvApi::wrap_options("").is_none());
    }

    #[test]
    fn wrap_options_pinned_version_populated() {
        let opts = RealAzureKvApi::wrap_options("v1").expect("options");
        assert_eq!(opts.key_version.as_deref(), Some("v1"));
    }

    #[test]
    fn get_options_empty_version_returns_none() {
        assert!(RealAzureKvApi::get_options("").is_none());
    }

    #[test]
    fn get_options_pinned_version_populated() {
        let opts = RealAzureKvApi::get_options("v2").expect("options");
        assert_eq!(opts.key_version.as_deref(), Some("v2"));
    }

    fn http_err(code: u16) -> azure_core::Error {
        azure_core::Error::with_message(
            ErrorKind::HttpResponse {
                status: StatusCode::from(code),
                error_code: None,
                raw_response: None,
            },
            "synthetic",
        )
    }

    #[test]
    fn classify_routes_http_401_to_auth() {
        let err = classify_azure_kv_err("op", http_err(401));
        assert!(matches!(err, KeyStoreError::Auth(_)));
    }

    #[test]
    fn classify_routes_http_403_to_authz() {
        let err = classify_azure_kv_err("op", http_err(403));
        assert!(matches!(err, KeyStoreError::Authz(_)));
    }

    #[test]
    fn classify_routes_http_404_to_not_found() {
        let err = classify_azure_kv_err("op", http_err(404));
        assert!(matches!(err, KeyStoreError::NotFound(_)));
    }

    #[test]
    fn classify_routes_http_408_to_timeout() {
        let err = classify_azure_kv_err("op", http_err(408));
        assert!(matches!(err, KeyStoreError::Timeout(_)));
    }

    #[test]
    fn classify_routes_http_5xx_to_other() {
        let err = classify_azure_kv_err("op", http_err(503));
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[test]
    fn classify_routes_credential_error_to_auth() {
        let err = classify_azure_kv_err(
            "op",
            azure_core::Error::with_message(ErrorKind::Credential, "denied"),
        );
        assert!(matches!(err, KeyStoreError::Auth(_)));
    }

    #[test]
    fn classify_routes_io_to_network() {
        let err = classify_azure_kv_err(
            "op",
            azure_core::Error::with_message(ErrorKind::Io, "connection refused"),
        );
        assert!(matches!(err, KeyStoreError::Network(_)));
    }

    #[test]
    fn classify_routes_other_to_other() {
        let err = classify_azure_kv_err(
            "op",
            azure_core::Error::with_message(ErrorKind::Other, "weird"),
        );
        assert!(matches!(err, KeyStoreError::Other(_)));
    }
}
