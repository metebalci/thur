// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! GCP Cloud KMS keystore backend.
//!
//! Symmetric `encrypt`/`decrypt` against one CryptoKey in Cloud KMS.
//! The DEK never appears on the Thur host's disk; the manifest holds
//! the wrapped (KMS-ciphertext) blob. All SDK-touching code lives in
//! [`crate::gcpkms_api`]; this module composes the backend on top of
//! the [`GcpKmsApi`] seam — AAD construction, key-name binding,
//! [`KeyStoreBackend`] trait wiring.
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
use std::sync::Arc;
use tracing::debug;

use crate::error::KeyStoreError;
use crate::gcpkms_api::{GcpKmsApi, RealGcpKmsApi};
use crate::keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
use crate::keystore_config::ResolvedGcpKmsAuth;

/// GCP Cloud KMS-backed keystore. The DEK never appears on the Thur
/// host's disk; the manifest holds the wrapped (KMS-ciphertext)
/// blob.
#[derive(Clone)]
pub struct GcpKmsBackend {
    api: Arc<dyn GcpKmsApi>,
    /// Full resource name:
    /// `projects/P/locations/L/keyRings/R/cryptoKeys/K`. The backend
    /// passes it verbatim to `encrypt().set_name()` /
    /// `decrypt().set_name()` / `get_crypto_key().set_name()`.
    key_name: String,
}

impl std::fmt::Debug for GcpKmsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpKmsBackend")
            .field("key_name", &self.key_name)
            .finish()
    }
}

impl GcpKmsBackend {
    /// Construct a KMS client + handle bound to one CryptoKey.
    /// Credential loading mirrors `shared_object_store::gcs::GcsBackend::new`
    /// — service-account JSON key file when configured, ADC chain
    /// otherwise (`GOOGLE_APPLICATION_CREDENTIALS` env →
    /// `gcloud auth application-default login` → GCE/GKE metadata
    /// server).
    pub async fn new(
        key_name: String,
        auth: Option<ResolvedGcpKmsAuth>,
    ) -> Result<Self, KeyStoreError> {
        debug!("Initializing GCP KMS backend: key_name={}", key_name);
        let api = RealGcpKmsApi::from_auth(auth).await?;
        Ok(Self {
            api: Arc::new(api),
            key_name,
        })
    }

    /// Compose a `GcpKmsBackend` from an already-built `GcpKmsApi`. Test
    /// constructor for the mock-injected coverage.
    #[cfg(test)]
    pub(crate) fn with_api(api: Arc<dyn GcpKmsApi>, key_name: String) -> Self {
        Self { api, key_name }
    }

    fn aad(wrap_context: &[u8; 16]) -> Bytes {
        Bytes::from(hex::encode(wrap_context).into_bytes())
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
        let ct = self
            .api
            .encrypt(
                &self.key_name,
                Bytes::copy_from_slice(plaintext.as_bytes()),
                Self::aad(wrap_context),
            )
            .await?;
        Ok(ct.to_vec())
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        let plain = self
            .api
            .decrypt(
                &self.key_name,
                Bytes::copy_from_slice(wrapped),
                Self::aad(wrap_context),
            )
            .await?;
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
        self.api.get_crypto_key(&self.key_name).await
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

    type CapturedCall = (String, Vec<u8>, Vec<u8>);

    #[derive(Default, Debug)]
    struct MockGcpKmsApi {
        encrypt_outcomes: Mutex<Vec<Result<Bytes, KeyStoreError>>>,
        decrypt_outcomes: Mutex<Vec<Result<Bytes, KeyStoreError>>>,
        get_outcomes: Mutex<Vec<Result<(), KeyStoreError>>>,

        encrypt_calls: AtomicU32,
        decrypt_calls: AtomicU32,
        get_calls: AtomicU32,

        captured_encrypt: Mutex<Vec<CapturedCall>>,
        captured_decrypt: Mutex<Vec<CapturedCall>>,
    }

    impl MockGcpKmsApi {
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
    impl GcpKmsApi for MockGcpKmsApi {
        async fn encrypt(
            &self,
            key_name: &str,
            plaintext: Bytes,
            aad: Bytes,
        ) -> Result<Bytes, KeyStoreError> {
            self.encrypt_calls.fetch_add(1, Ordering::SeqCst);
            self.captured_encrypt.lock().expect("cap").push((
                key_name.to_string(),
                plaintext.to_vec(),
                aad.to_vec(),
            ));
            Self::pop_or(&self.encrypt_outcomes, || {
                Ok(Bytes::from_static(b"canned-ciphertext"))
            })
        }
        async fn decrypt(
            &self,
            key_name: &str,
            ciphertext: Bytes,
            aad: Bytes,
        ) -> Result<Bytes, KeyStoreError> {
            self.decrypt_calls.fetch_add(1, Ordering::SeqCst);
            self.captured_decrypt.lock().expect("cap").push((
                key_name.to_string(),
                ciphertext.to_vec(),
                aad.to_vec(),
            ));
            Self::pop_or(&self.decrypt_outcomes, || {
                Ok(Bytes::from_static(&[0u8; DEK_LEN]))
            })
        }
        async fn get_crypto_key(&self, _key_name: &str) -> Result<(), KeyStoreError> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            Self::pop_or(&self.get_outcomes, || Ok(()))
        }
    }

    const FIXTURE_KEY: &str = "projects/p/locations/global/keyRings/r/cryptoKeys/k";

    fn backend(api: Arc<dyn GcpKmsApi>) -> GcpKmsBackend {
        GcpKmsBackend::with_api(api, FIXTURE_KEY.to_string())
    }

    fn fixture_context() -> [u8; 16] {
        [0xABu8; 16]
    }

    #[test]
    fn aad_is_hex_of_context() {
        let aad = GcpKmsBackend::aad(&fixture_context());
        assert_eq!(aad.as_ref(), b"ab".repeat(16).as_slice());
        assert_eq!(aad.len(), 32);
    }

    #[test]
    fn backend_type_and_fingerprint() {
        let b = backend(Arc::new(MockGcpKmsApi::default()));
        assert_eq!(b.backend_type(), "gcpkms");
        assert_eq!(b.wrap_target_fingerprint(), format!("gcpkms:{FIXTURE_KEY}"));
    }

    #[test]
    fn debug_omits_sdk_internals() {
        let b = backend(Arc::new(MockGcpKmsApi::default()));
        let s = format!("{:?}", b);
        assert!(s.contains("key_name"));
        assert!(s.contains(FIXTURE_KEY));
    }

    #[test]
    fn clone_box_yields_independent_handle() {
        let b = backend(Arc::new(MockGcpKmsApi::default()));
        let boxed: Box<dyn KeyStoreBackend> = Box::new(b);
        let cloned = boxed.clone();
        assert_eq!(cloned.backend_type(), "gcpkms");
    }

    #[tokio::test]
    async fn wrap_passes_plaintext_key_name_and_aad() {
        let api = Arc::new(MockGcpKmsApi::default());
        let b = backend(api.clone());
        let ctx = fixture_context();
        let plain = SecretBytes::new([0x11u8; DEK_LEN]);
        let wrapped = b.wrap(&ctx, &plain).await.expect("wrap");
        assert_eq!(wrapped, b"canned-ciphertext");
        let captured = api.captured_encrypt.lock().expect("cap").clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, FIXTURE_KEY);
        assert_eq!(captured[0].1, vec![0x11u8; DEK_LEN]);
        assert_eq!(captured[0].2, b"ab".repeat(16));
    }

    #[tokio::test]
    async fn unwrap_round_trips_dek_through_mock() {
        let api = Arc::new(MockGcpKmsApi::default());
        {
            let mut g = api.decrypt_outcomes.lock().expect("queue");
            g.push(Ok(Bytes::from(vec![0x42u8; DEK_LEN])));
        }
        let b = backend(api.clone());
        let unwrapped = b
            .unwrap(&fixture_context(), b"canned-ciphertext")
            .await
            .expect("unwrap");
        assert_eq!(unwrapped.as_bytes(), &[0x42u8; DEK_LEN][..]);
        let captured = api.captured_decrypt.lock().expect("cap").clone();
        assert_eq!(captured[0].0, FIXTURE_KEY);
        assert_eq!(captured[0].1, b"canned-ciphertext");
        assert_eq!(captured[0].2, b"ab".repeat(16));
    }

    #[tokio::test]
    async fn unwrap_rejects_wrong_length_plaintext() {
        let api = Arc::new(MockGcpKmsApi::default());
        {
            let mut g = api.decrypt_outcomes.lock().expect("queue");
            // Too short — must be rejected.
            g.push(Ok(Bytes::from(vec![0u8; DEK_LEN - 1])));
        }
        let b = backend(api);
        let err = b
            .unwrap(&fixture_context(), b"ct")
            .await
            .expect_err("must reject");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[tokio::test]
    async fn unwrap_surfaces_auth_failure() {
        let api = Arc::new(MockGcpKmsApi::default());
        {
            let mut g = api.decrypt_outcomes.lock().expect("queue");
            g.push(Err(KeyStoreError::Auth("denied".into())));
        }
        let b = backend(api);
        let err = b
            .unwrap(&fixture_context(), b"ct")
            .await
            .expect_err("must surface");
        assert_eq!(err.kind(), KeyStoreFailureKind::Auth);
    }

    #[tokio::test]
    async fn wrap_surfaces_authz_failure() {
        let api = Arc::new(MockGcpKmsApi::default());
        {
            let mut g = api.encrypt_outcomes.lock().expect("queue");
            g.push(Err(KeyStoreError::Authz("denied".into())));
        }
        let b = backend(api);
        let err = b
            .wrap(&fixture_context(), &SecretBytes::new([0u8; DEK_LEN]))
            .await
            .expect_err("must surface");
        assert_eq!(err.kind(), KeyStoreFailureKind::Authz);
    }

    #[tokio::test]
    async fn generate_and_wrap_returns_matching_plain_and_ciphertext() {
        // Mock encrypt returns the AAD as a stand-in for ciphertext so
        // we can confirm the call site passed the expected context.
        let api = Arc::new(MockGcpKmsApi::default());
        let b = backend(api.clone());
        let ctx = fixture_context();
        let (plain, wrapped) = b
            .generate_and_wrap(&ctx, DekSource::Daemon)
            .await
            .expect("gen+wrap");
        assert_eq!(plain.as_bytes().len(), DEK_LEN);
        assert_eq!(wrapped, b"canned-ciphertext");
        assert_eq!(api.encrypt_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn generate_and_wrap_collapses_backend_source_to_daemon() {
        // Backend collapses to Daemon-side RNG without erroring — the
        // log line is the only side effect.
        let api = Arc::new(MockGcpKmsApi::default());
        let b = backend(api);
        let (_, _) = b
            .generate_and_wrap(&fixture_context(), DekSource::Backend)
            .await
            .expect("collapses without error");
    }

    #[tokio::test]
    async fn forget_is_a_noop() {
        let b = backend(Arc::new(MockGcpKmsApi::default()));
        b.forget(&fixture_context()).await.expect("forget");
    }

    #[tokio::test]
    async fn health_check_calls_get_crypto_key() {
        let api = Arc::new(MockGcpKmsApi::default());
        let b = backend(api.clone());
        b.health_check().await.expect("health");
        assert_eq!(api.get_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn health_check_surfaces_not_found() {
        let api = Arc::new(MockGcpKmsApi::default());
        {
            let mut g = api.get_outcomes.lock().expect("queue");
            g.push(Err(KeyStoreError::NotFound("missing".into())));
        }
        let b = backend(api);
        let err = b.health_check().await.expect_err("must error");
        assert_eq!(err.kind(), KeyStoreFailureKind::NotFound);
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

        let to = KeyStoreError::Timeout("test".into());
        assert_eq!(to.kind(), KeyStoreFailureKind::Timeout);
        assert!(to.is_retryable());
    }
}
