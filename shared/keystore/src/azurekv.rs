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
//! All SDK-touching code lives in [`crate::azurekv_api`]; this module
//! composes the backend on top of the [`AzureKvApi`] seam — envelope
//! shape, context binding, fingerprint, [`KeyStoreBackend`] wiring.
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
//! the vault. Documented in `docs/admin/ENCRYPTION.md` § VSA keystore
//! backends.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::azurekv_api::{AzureKvApi, RealAzureKvApi};
use crate::error::KeyStoreError;
use crate::keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
use crate::keystore_config::ResolvedAzureKvAuth;

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
    api: Arc<dyn AzureKvApi>,
    vault_uri: String,
    key_name: String,
    /// Empty string = "latest" — KV's wrap/unwrap APIs accept the
    /// empty path segment for the version slot and pick the current
    /// active version. Pinned versions live here verbatim.
    key_version: String,
}

impl std::fmt::Debug for AzureKvBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
    /// construction matches `shared_object_store::azure::AzureBackend`'s AAD
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
        let api = RealAzureKvApi::from_auth(&vault_uri, auth)?;
        Ok(Self {
            api: Arc::new(api),
            vault_uri,
            key_name,
            key_version: key_version.unwrap_or_default(),
        })
    }

    /// Compose an `AzureKvBackend` from an already-built `AzureKvApi`.
    /// Test constructor for the mock-injected coverage.
    #[cfg(test)]
    pub(crate) fn with_api(
        api: Arc<dyn AzureKvApi>,
        vault_uri: String,
        key_name: String,
        key_version: Option<String>,
    ) -> Self {
        Self {
            api,
            vault_uri,
            key_name,
            key_version: key_version.unwrap_or_default(),
        }
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
        let ct = self
            .api
            .wrap_key(
                &self.key_name,
                &self.key_version,
                plaintext.as_bytes().to_vec(),
            )
            .await?;
        Ok(Self::build_envelope(wrap_context, &ct))
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        let ct = Self::parse_envelope(wrap_context, wrapped)?;
        let plain = self
            .api
            .unwrap_key(&self.key_name, &self.key_version, ct)
            .await?;
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
        self.api.get_key(&self.key_name, &self.key_version).await
    }

    fn clone_box(&self) -> Box<dyn KeyStoreBackend> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::KeyStoreFailureKind;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

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
    fn envelope_refuses_garbage() {
        let err = AzureKvBackend::parse_envelope(&fixture_uuid(), b"not-json").expect_err("");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[test]
    fn envelope_refuses_invalid_base64() {
        let bad = serde_json::to_vec(&WrappedEnvelope {
            v: ENVELOPE_VERSION,
            uuid: hex::encode(fixture_uuid()),
            ct: "!!!not base64!!!".into(),
        })
        .expect("encode");
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
    fn classify_failure_kinds_route_correctly() {
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

    // ---- Mock-injected backend tests ---------------------------------

    #[derive(Default, Debug)]
    struct MockAzureKvApi {
        wrap_outcomes: Mutex<Vec<Result<Vec<u8>, KeyStoreError>>>,
        unwrap_outcomes: Mutex<Vec<Result<Vec<u8>, KeyStoreError>>>,
        get_outcomes: Mutex<Vec<Result<(), KeyStoreError>>>,

        wrap_calls: AtomicU32,
        unwrap_calls: AtomicU32,
        get_calls: AtomicU32,

        captured_wrap: Mutex<Vec<(String, String, Vec<u8>)>>,
        captured_unwrap: Mutex<Vec<(String, String, Vec<u8>)>>,
    }

    impl MockAzureKvApi {
        fn pop_or<T>(
            q: &Mutex<Vec<Result<T, KeyStoreError>>>,
            default: impl FnOnce() -> Result<T, KeyStoreError>,
        ) -> Result<T, KeyStoreError> {
            let mut g = match q.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.is_empty() { default() } else { g.remove(0) }
        }
    }

    #[async_trait]
    impl AzureKvApi for MockAzureKvApi {
        async fn wrap_key(
            &self,
            key_name: &str,
            key_version: &str,
            plaintext: Vec<u8>,
        ) -> Result<Vec<u8>, KeyStoreError> {
            self.wrap_calls.fetch_add(1, Ordering::SeqCst);
            self.captured_wrap.lock().expect("cap").push((
                key_name.to_string(),
                key_version.to_string(),
                plaintext,
            ));
            Self::pop_or(&self.wrap_outcomes, || Ok(b"kv-ciphertext".to_vec()))
        }
        async fn unwrap_key(
            &self,
            key_name: &str,
            key_version: &str,
            ciphertext: Vec<u8>,
        ) -> Result<Vec<u8>, KeyStoreError> {
            self.unwrap_calls.fetch_add(1, Ordering::SeqCst);
            self.captured_unwrap.lock().expect("cap").push((
                key_name.to_string(),
                key_version.to_string(),
                ciphertext,
            ));
            Self::pop_or(&self.unwrap_outcomes, || Ok(vec![0u8; DEK_LEN]))
        }
        async fn get_key(&self, _key_name: &str, _key_version: &str) -> Result<(), KeyStoreError> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            Self::pop_or(&self.get_outcomes, || Ok(()))
        }
    }

    const FIXTURE_VAULT: &str = "https://kv.example.vault.azure.net";
    const FIXTURE_KEY: &str = "thurvsa-kek";

    fn backend_latest(api: Arc<dyn AzureKvApi>) -> AzureKvBackend {
        AzureKvBackend::with_api(
            api,
            FIXTURE_VAULT.to_string(),
            FIXTURE_KEY.to_string(),
            None,
        )
    }

    fn backend_pinned(api: Arc<dyn AzureKvApi>, version: &str) -> AzureKvBackend {
        AzureKvBackend::with_api(
            api,
            FIXTURE_VAULT.to_string(),
            FIXTURE_KEY.to_string(),
            Some(version.to_string()),
        )
    }

    #[test]
    fn backend_type_and_fingerprint_latest() {
        let b = backend_latest(Arc::new(MockAzureKvApi::default()));
        assert_eq!(b.backend_type(), "azurekv");
        assert_eq!(
            b.wrap_target_fingerprint(),
            format!("azurekv:{FIXTURE_VAULT}/{FIXTURE_KEY}/<latest>")
        );
    }

    #[test]
    fn backend_fingerprint_pinned() {
        let b = backend_pinned(Arc::new(MockAzureKvApi::default()), "v9");
        assert_eq!(
            b.wrap_target_fingerprint(),
            format!("azurekv:{FIXTURE_VAULT}/{FIXTURE_KEY}/v9")
        );
    }

    #[test]
    fn debug_shows_latest_marker() {
        let b = backend_latest(Arc::new(MockAzureKvApi::default()));
        let s = format!("{:?}", b);
        assert!(s.contains("<latest>"));
        assert!(s.contains(FIXTURE_VAULT));
    }

    #[test]
    fn debug_shows_pinned_version() {
        let b = backend_pinned(Arc::new(MockAzureKvApi::default()), "v7");
        let s = format!("{:?}", b);
        assert!(s.contains("v7"));
    }

    #[test]
    fn clone_box_yields_independent_handle() {
        let b = backend_latest(Arc::new(MockAzureKvApi::default()));
        let boxed: Box<dyn KeyStoreBackend> = Box::new(b);
        let cloned = boxed.clone();
        assert_eq!(cloned.backend_type(), "azurekv");
    }

    #[tokio::test]
    async fn wrap_round_trips_envelope_through_mock() {
        let api = Arc::new(MockAzureKvApi::default());
        let b = backend_latest(api.clone());
        let ctx = fixture_uuid();
        let plain = SecretBytes::new([0x33u8; DEK_LEN]);
        let envelope = b.wrap(&ctx, &plain).await.expect("wrap");
        // Envelope decodes back to the canned KV ciphertext.
        let inner = AzureKvBackend::parse_envelope(&ctx, &envelope).expect("parse");
        assert_eq!(inner, b"kv-ciphertext");
        let captured = api.captured_wrap.lock().expect("cap").clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, FIXTURE_KEY);
        assert_eq!(captured[0].1, "");
        assert_eq!(captured[0].2, vec![0x33u8; DEK_LEN]);
    }

    #[tokio::test]
    async fn unwrap_round_trips_through_envelope_and_mock() {
        // Seed the wrap path to produce a real envelope, then unwrap it.
        let api = Arc::new(MockAzureKvApi::default());
        {
            let mut g = api.unwrap_outcomes.lock().expect("queue");
            g.push(Ok(vec![0x55u8; DEK_LEN]));
        }
        let b = backend_latest(api.clone());
        let ctx = fixture_uuid();
        let envelope = AzureKvBackend::build_envelope(&ctx, b"opaque-kv-ciphertext");
        let plain = b.unwrap(&ctx, &envelope).await.expect("unwrap");
        assert_eq!(plain.as_bytes(), &[0x55u8; DEK_LEN][..]);
        let captured = api.captured_unwrap.lock().expect("cap").clone();
        assert_eq!(captured[0].0, FIXTURE_KEY);
        assert_eq!(captured[0].2, b"opaque-kv-ciphertext");
    }

    #[tokio::test]
    async fn unwrap_refuses_when_envelope_context_does_not_match() {
        // No KV call must happen — the envelope's UUID mismatch should
        // be caught locally.
        let api = Arc::new(MockAzureKvApi::default());
        let b = backend_latest(api.clone());
        let envelope = AzureKvBackend::build_envelope(&[0x01u8; 16], b"ct");
        let err = b
            .unwrap(&[0x02u8; 16], &envelope)
            .await
            .expect_err("must reject");
        assert!(matches!(err, KeyStoreError::Authz(_)));
        assert_eq!(api.unwrap_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unwrap_rejects_wrong_length_plaintext() {
        let api = Arc::new(MockAzureKvApi::default());
        {
            let mut g = api.unwrap_outcomes.lock().expect("queue");
            g.push(Ok(vec![0u8; DEK_LEN - 1]));
        }
        let b = backend_latest(api);
        let ctx = fixture_uuid();
        let envelope = AzureKvBackend::build_envelope(&ctx, b"ct");
        let err = b.unwrap(&ctx, &envelope).await.expect_err("must reject");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[tokio::test]
    async fn wrap_surfaces_authz_failure() {
        let api = Arc::new(MockAzureKvApi::default());
        {
            let mut g = api.wrap_outcomes.lock().expect("queue");
            g.push(Err(KeyStoreError::Authz("denied".into())));
        }
        let b = backend_latest(api);
        let err = b
            .wrap(&fixture_uuid(), &SecretBytes::new([0u8; DEK_LEN]))
            .await
            .expect_err("must surface");
        assert_eq!(err.kind(), KeyStoreFailureKind::Authz);
    }

    #[tokio::test]
    async fn generate_and_wrap_returns_matching_envelope() {
        let api = Arc::new(MockAzureKvApi::default());
        let b = backend_latest(api.clone());
        let ctx = fixture_uuid();
        let (plain, wrapped) = b
            .generate_and_wrap(&ctx, DekSource::Daemon)
            .await
            .expect("gen+wrap");
        assert_eq!(plain.as_bytes().len(), DEK_LEN);
        // Wrapped is a JSON envelope; can be parsed back with the same ctx.
        let inner = AzureKvBackend::parse_envelope(&ctx, &wrapped).expect("parse");
        assert_eq!(inner, b"kv-ciphertext");
        assert_eq!(api.wrap_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn generate_and_wrap_collapses_backend_source_to_daemon() {
        let api = Arc::new(MockAzureKvApi::default());
        let b = backend_latest(api);
        let (_, _) = b
            .generate_and_wrap(&fixture_uuid(), DekSource::Backend)
            .await
            .expect("collapses without error");
    }

    #[tokio::test]
    async fn forget_is_a_noop() {
        let b = backend_latest(Arc::new(MockAzureKvApi::default()));
        b.forget(&fixture_uuid()).await.expect("forget");
    }

    #[tokio::test]
    async fn health_check_calls_get_key_with_pinned_version() {
        let api = Arc::new(MockAzureKvApi::default());
        let b = backend_pinned(api.clone(), "v3");
        b.health_check().await.expect("health");
        assert_eq!(api.get_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn health_check_surfaces_not_found() {
        let api = Arc::new(MockAzureKvApi::default());
        {
            let mut g = api.get_outcomes.lock().expect("queue");
            g.push(Err(KeyStoreError::NotFound("missing".into())));
        }
        let b = backend_latest(api);
        let err = b.health_check().await.expect_err("must error");
        assert_eq!(err.kind(), KeyStoreFailureKind::NotFound);
    }
}
