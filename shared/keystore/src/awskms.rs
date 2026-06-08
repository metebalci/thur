// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! AWS KMS keystore backend.
//!
//! Envelope encryption: the DEK lives only in the manifest as the
//! wrapped (KMS-ciphertext) blob. KMS decrypts on `volume open` to
//! produce the plaintext the daemon hands to
//! `VolumeWriter::open_with_key`.
//!
//! Wrap operations are tagged with an encryption context
//! `{"volume_uuid": "<hex>"}`. The map field name `volume_uuid` is the
//! wire-format string — it's baked into every wrapped blob the
//! backend produces. The Rust-level parameter is `wrap_context`
//! (16 bytes, either a volume UUID for per-volume DEKs or a
//! product-bound binding constant for daemon-identity seeds); the
//! string `"volume_uuid"` is kept in the encryption-context map for
//! continuity. A stolen wrapped blob plus KMS access cannot decrypt
//! to the right key without also presenting the matching context
//! bytes (KMS validates the context byte-for-byte — see the AWS KMS
//! Developer Guide § Encryption context).

use std::collections::HashMap;

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_kms::Client;
use aws_sdk_kms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::types::DataKeySpec;
use tracing::debug;

use crate::error::KeyStoreError;
use crate::keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
use crate::keystore_config::ResolvedAwsKmsAuth;

/// AWS KMS-backed keystore. The DEK never appears on the Thur host's
/// disk; the manifest holds the wrapped (KMS-ciphertext) blob.
#[derive(Clone, Debug)]
pub struct AwsKmsBackend {
    client: Client,
    key_id: String,
    region: String,
}

impl AwsKmsBackend {
    /// Construct an AWS KMS client targeting `key_id` in `region`.
    /// `endpoint_url` overrides the SDK default (LocalStack, VPC
    /// endpoint). Credential loading mirrors the S3 backend's
    /// `auth` semantics — `None` falls back to the SDK chain;
    /// `Some(Static)` / `Some(Profile)` is a strict override.
    pub async fn new(
        key_id: String,
        region: String,
        endpoint_url: Option<String>,
        auth: Option<ResolvedAwsKmsAuth>,
    ) -> Result<Self, KeyStoreError> {
        debug!(
            "Initializing AWS KMS backend: key_id={}, region={}",
            key_id, region
        );

        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_kms::config::Region::new(region.clone()));
        match &auth {
            None => {
                debug!(
                    "KMS: using AWS credential chain (env vars, IRSA, SSO, IMDS, shared credentials)"
                );
            }
            Some(ResolvedAwsKmsAuth::Static {
                access_key_id,
                secret_access_key,
                session_token,
            }) => {
                let creds = Credentials::new(
                    access_key_id.clone(),
                    secret_access_key.clone(),
                    session_token.clone(),
                    None,
                    "thurvsa-keystore",
                );
                loader = loader.credentials_provider(creds);
            }
            Some(ResolvedAwsKmsAuth::Profile { name }) => {
                let provider = aws_config::profile::ProfileFileCredentialsProvider::builder()
                    .profile_name(name)
                    .build();
                loader = loader.credentials_provider(provider);
            }
        }
        let config = loader.load().await;
        let mut kms_config_builder = aws_sdk_kms::config::Builder::from(&config);
        if let Some(endpoint) = endpoint_url {
            kms_config_builder = kms_config_builder.endpoint_url(endpoint);
        }
        let kms_config = kms_config_builder.build();
        let client = Client::from_conf(kms_config);
        Ok(Self {
            client,
            key_id,
            region,
        })
    }

    fn context(wrap_context: &[u8; 16]) -> HashMap<String, String> {
        let mut ctx = HashMap::new();
        // Wire-format map field name stays "volume_uuid" — every
        // existing wrapped blob was bound against it. The value is
        // the hex-encoded wrap context (a volume UUID or a
        // daemon-identity binding constant; KMS treats both as
        // opaque bytes).
        ctx.insert("volume_uuid".to_string(), hex::encode(wrap_context));
        ctx
    }

    async fn encrypt(
        &self,
        wrap_context: &[u8; 16],
        plaintext: &[u8; DEK_LEN],
    ) -> Result<Vec<u8>, KeyStoreError> {
        let ctx = Self::context(wrap_context);
        let out = self
            .client
            .encrypt()
            .key_id(&self.key_id)
            .set_encryption_context(Some(ctx))
            .plaintext(Blob::new(plaintext.to_vec()))
            .send()
            .await
            .map_err(|e| classify_kms_err("kms.encrypt", e))?;
        let blob = out.ciphertext_blob.ok_or_else(|| {
            KeyStoreError::Other("kms.encrypt returned no ciphertext_blob".into())
        })?;
        Ok(blob.into_inner())
    }

    async fn decrypt(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<[u8; DEK_LEN], KeyStoreError> {
        let ctx = Self::context(wrap_context);
        let out = self
            .client
            .decrypt()
            .key_id(&self.key_id)
            .set_encryption_context(Some(ctx))
            .ciphertext_blob(Blob::new(wrapped.to_vec()))
            .send()
            .await
            .map_err(|e| classify_kms_err("kms.decrypt", e))?;
        let plain = out
            .plaintext
            .ok_or_else(|| KeyStoreError::Other("kms.decrypt returned no plaintext".into()))?
            .into_inner();
        if plain.len() != DEK_LEN {
            return Err(KeyStoreError::Other(format!(
                "kms.decrypt returned {} bytes, expected {}",
                plain.len(),
                DEK_LEN
            )));
        }
        let mut out = [0u8; DEK_LEN];
        out.copy_from_slice(&plain);
        Ok(out)
    }

    async fn generate_data_key(
        &self,
        wrap_context: &[u8; 16],
    ) -> Result<([u8; DEK_LEN], Vec<u8>), KeyStoreError> {
        let ctx = Self::context(wrap_context);
        let out = self
            .client
            .generate_data_key()
            .key_id(&self.key_id)
            .set_encryption_context(Some(ctx))
            .key_spec(DataKeySpec::Aes256)
            .send()
            .await
            .map_err(|e| classify_kms_err("kms.generate_data_key", e))?;
        let plain = out
            .plaintext
            .ok_or_else(|| {
                KeyStoreError::Other("kms.generate_data_key returned no plaintext".into())
            })?
            .into_inner();
        let cipher = out
            .ciphertext_blob
            .ok_or_else(|| {
                KeyStoreError::Other("kms.generate_data_key returned no ciphertext_blob".into())
            })?
            .into_inner();
        if plain.len() != DEK_LEN {
            return Err(KeyStoreError::Other(format!(
                "kms.generate_data_key returned {} plaintext bytes, expected {}",
                plain.len(),
                DEK_LEN
            )));
        }
        let mut key = [0u8; DEK_LEN];
        key.copy_from_slice(&plain);
        Ok((key, cipher))
    }
}

#[async_trait]
impl KeyStoreBackend for AwsKmsBackend {
    async fn generate_and_wrap(
        &self,
        wrap_context: &[u8; 16],
        source: DekSource,
    ) -> Result<(SecretBytes, Vec<u8>), KeyStoreError> {
        match source {
            DekSource::Backend => {
                let (plain, wrapped) = self.generate_data_key(wrap_context).await?;
                Ok((SecretBytes::new(plain), wrapped))
            }
            DekSource::Daemon => {
                use shared_crypto::{OsRng, RngCore};
                let mut plain = [0u8; DEK_LEN];
                OsRng.fill_bytes(&mut plain);
                let wrapped = self.encrypt(wrap_context, &plain).await?;
                Ok((SecretBytes::new(plain), wrapped))
            }
        }
    }

    async fn wrap(
        &self,
        wrap_context: &[u8; 16],
        plaintext: &SecretBytes,
    ) -> Result<Vec<u8>, KeyStoreError> {
        self.encrypt(wrap_context, plaintext.as_bytes()).await
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        let bytes = self.decrypt(wrap_context, wrapped).await?;
        Ok(SecretBytes::new(bytes))
    }

    async fn forget(&self, _wrap_context: &[u8; 16]) -> Result<(), KeyStoreError> {
        // KMS holds no per-context state. The wrapped blob at the
        // caller's persistence layer is the only thing tied to this
        // call site; deleting it is the caller's job. A
        // `kms:ScheduleKeyDeletion` on the CMK is out of scope (the
        // CMK serves every wrap operation).
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "awskms"
    }

    fn wrap_target_fingerprint(&self) -> String {
        // Region is part of the address: an alias like `alias/foo`
        // resolves to different CMKs in different regions, and
        // even an ARN is region-scoped. ARN form already embeds the
        // region but we include it explicitly for the alias / bare-id
        // cases too — operators see why two entries with the same
        // `key_id` but different regions don't collide.
        format!("awskms:{}:{}", self.region, self.key_id)
    }

    async fn health_check(&self) -> Result<(), KeyStoreError> {
        self.client
            .describe_key()
            .key_id(&self.key_id)
            .send()
            .await
            .map_err(|e| classify_kms_err("kms.describe_key", e))?;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn KeyStoreBackend> {
        Box::new(self.clone())
    }
}

/// Classify an SDK error into the keystore error taxonomy. KMS
/// service errors carry typed `code()` strings we map straight onto
/// `Auth` / `Authz` / `NotFound`; dispatch failures route to
/// `Network` / `Timeout`. Everything else goes to `Other` which is
/// retryable per the design (mirrors `is_retryable` of storage).
fn classify_kms_err<E, R>(op: &str, err: SdkError<E, R>) -> KeyStoreError
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    match &err {
        SdkError::ServiceError(svc) => {
            let inner = svc.err();
            let code = inner.code().unwrap_or("");
            let msg = inner.message().unwrap_or("no message");
            match code {
                "AccessDeniedException" | "KMSAccessDeniedException" => {
                    KeyStoreError::Authz(format!("{op}: {code}: {msg}"))
                }
                "NotFoundException" | "KMSInvalidStateException" => {
                    KeyStoreError::NotFound(format!("{op}: {code}: {msg}"))
                }
                "IncompleteSignature"
                | "InvalidClientTokenId"
                | "MissingAuthenticationToken"
                | "UnrecognizedClientException"
                | "ExpiredTokenException" => KeyStoreError::Auth(format!("{op}: {code}: {msg}")),
                "ThrottlingException" | "LimitExceededException" => {
                    KeyStoreError::Other(format!("{op}: {code}: {msg}"))
                }
                _ => KeyStoreError::Other(format!("{op}: {code}: {msg}")),
            }
        }
        SdkError::DispatchFailure(d) => {
            if d.is_io() {
                KeyStoreError::Network(format!("{op}: dispatch io: {err:?}"))
            } else if d.is_timeout() {
                KeyStoreError::Timeout(format!("{op}: dispatch timeout"))
            } else if d.is_user() {
                KeyStoreError::Auth(format!("{op}: dispatch user (credentials?): {err:?}"))
            } else {
                KeyStoreError::Other(format!("{op}: dispatch other: {err:?}"))
            }
        }
        SdkError::TimeoutError(_) => KeyStoreError::Timeout(format!("{op}: timeout")),
        SdkError::ConstructionFailure(_) => {
            KeyStoreError::Auth(format!("{op}: construction failure: {err:?}"))
        }
        SdkError::ResponseError(_) => {
            KeyStoreError::Other(format!("{op}: response error: {err:?}"))
        }
        _ => KeyStoreError::Other(format!("{op}: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::KeyStoreFailureKind;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture_uuid() -> [u8; 16] {
        [0x33; 16]
    }

    fn fixture_key() -> [u8; DEK_LEN] {
        let mut k = [0u8; DEK_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7);
        }
        k
    }

    /// Build a KMS backend pointed at a wiremock server. KMS speaks
    /// AWS JSON 1.1 over plain HTTP POST to `/`; the SDK is happy to
    /// talk to any endpoint we hand it.
    async fn backend_for(server: &MockServer) -> AwsKmsBackend {
        AwsKmsBackend::new(
            "alias/thurvsa-kek".into(),
            "us-east-1".into(),
            Some(server.uri()),
            Some(ResolvedAwsKmsAuth::Static {
                access_key_id: "AKIDTEST".into(),
                secret_access_key: "SECRETTEST".into(),
                session_token: None,
            }),
        )
        .await
        .expect("construct KMS backend")
    }

    #[tokio::test]
    async fn wrap_round_trips_through_wiremock() {
        let server = MockServer::start().await;
        // KMS Encrypt: the ciphertext_blob comes back base64 in the
        // AWS JSON wire form.
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.Encrypt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "KeyId": "alias/thurvsa-kek",
                "CiphertextBlob": B64.encode(b"kms-wrapped-blob"),
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let wrapped = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap");
        assert_eq!(wrapped, b"kms-wrapped-blob");
    }

    #[tokio::test]
    async fn unwrap_round_trips_through_wiremock() {
        let server = MockServer::start().await;
        let key = fixture_key();
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.Decrypt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "KeyId": "alias/thurvsa-kek",
                "Plaintext": B64.encode(key),
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let plain = backend
            .unwrap(&fixture_uuid(), b"kms-wrapped-blob")
            .await
            .expect("unwrap");
        assert_eq!(plain.as_bytes(), &key);
    }

    #[tokio::test]
    async fn generate_and_wrap_daemon_uses_encrypt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.Encrypt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "KeyId": "alias/thurvsa-kek",
                "CiphertextBlob": B64.encode(b"daemon-wrapped"),
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let (plain, wrapped) = backend
            .generate_and_wrap(&fixture_uuid(), DekSource::Daemon)
            .await
            .expect("generate");
        assert_eq!(wrapped, b"daemon-wrapped");
        // The daemon-side path mints 32 bytes from OsRng.
        assert_eq!(plain.as_bytes().len(), DEK_LEN);
    }

    #[tokio::test]
    async fn generate_and_wrap_backend_uses_generate_data_key() {
        let server = MockServer::start().await;
        let key = fixture_key();
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.GenerateDataKey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "KeyId": "alias/thurvsa-kek",
                "Plaintext": B64.encode(key),
                "CiphertextBlob": B64.encode(b"hsm-wrapped"),
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let (plain, wrapped) = backend
            .generate_and_wrap(&fixture_uuid(), DekSource::Backend)
            .await
            .expect("generate via data key");
        assert_eq!(plain.as_bytes(), &key);
        assert_eq!(wrapped, b"hsm-wrapped");
    }

    #[tokio::test]
    async fn health_check_calls_describe_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.DescribeKey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "KeyMetadata": { "KeyId": "alias/thurvsa-kek" }
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        backend.health_check().await.expect("describe-key OK");
    }

    #[tokio::test]
    async fn forget_is_a_noop() {
        let server = MockServer::start().await;
        let backend = backend_for(&server).await;
        // KMS holds no per-context state — forget never touches the
        // network.
        backend.forget(&fixture_uuid()).await.expect("noop forget");
    }

    #[tokio::test]
    async fn access_denied_classifies_as_authz() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.Encrypt"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "AccessDeniedException",
                "message": "no kms:Encrypt for this principal",
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let err = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect_err("access denied");
        assert_eq!(err.kind(), KeyStoreFailureKind::Authz);
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn not_found_classifies_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.Decrypt"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "NotFoundException",
                "message": "key id not found",
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let err = backend
            .unwrap(&fixture_uuid(), b"blob")
            .await
            .expect_err("not found");
        assert_eq!(err.kind(), KeyStoreFailureKind::NotFound);
    }

    #[tokio::test]
    async fn throttling_classifies_as_retryable_other() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.Encrypt"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "ThrottlingException",
                "message": "rate exceeded",
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let err = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect_err("throttled");
        assert_eq!(err.kind(), KeyStoreFailureKind::Other);
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn unwrap_rejects_wrong_length_plaintext() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-amz-target", "TrentService.Decrypt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "KeyId": "alias/thurvsa-kek",
                "Plaintext": B64.encode(b"too-short"),
            })))
            .mount(&server)
            .await;

        let backend = backend_for(&server).await;
        let err = backend
            .unwrap(&fixture_uuid(), b"blob")
            .await
            .expect_err("short plaintext");
        match err {
            KeyStoreError::Other(msg) => assert!(msg.contains("expected 32")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn backend_type_is_awskms() {
        let server = MockServer::start().await;
        let backend = backend_for(&server).await;
        assert_eq!(backend.backend_type(), "awskms");
        assert_eq!(backend.clone_box().backend_type(), "awskms");
    }

    #[test]
    fn context_carries_volume_uuid_hex() {
        let uuid = [0xABu8; 16];
        let ctx = AwsKmsBackend::context(&uuid);
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx.get("volume_uuid").unwrap(), &"ab".repeat(16));
    }

    #[tokio::test]
    async fn wrap_target_fingerprint_carries_region_and_key() {
        // Construct two backends with the same `key_id` but different
        // regions and verify they fingerprint differently.
        let a = AwsKmsBackend::new(
            "alias/foo".into(),
            "us-east-1".into(),
            None,
            Some(ResolvedAwsKmsAuth::Static {
                access_key_id: "AKID".into(),
                secret_access_key: "SAK".into(),
                session_token: None,
            }),
        )
        .await
        .expect("construct a");
        let b = AwsKmsBackend::new(
            "alias/foo".into(),
            "eu-west-1".into(),
            None,
            Some(ResolvedAwsKmsAuth::Static {
                access_key_id: "AKID".into(),
                secret_access_key: "SAK".into(),
                session_token: None,
            }),
        )
        .await
        .expect("construct b");
        assert_eq!(a.wrap_target_fingerprint(), "awskms:us-east-1:alias/foo");
        assert_eq!(b.wrap_target_fingerprint(), "awskms:eu-west-1:alias/foo");
        assert_ne!(a.wrap_target_fingerprint(), b.wrap_target_fingerprint());
    }

    #[test]
    fn classify_failure_kinds_route_correctly() {
        // Spot-check the taxonomy: synthetic Auth/Authz/NotFound
        // errors produce the expected KeyStoreFailureKind. The
        // service-error path is exercised against real SDK calls in
        // the integration tests (LocalStack-gated row).
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

        let to = KeyStoreError::Timeout("test".into());
        assert_eq!(to.kind(), KeyStoreFailureKind::Timeout);
        assert!(to.is_retryable());
    }
}
