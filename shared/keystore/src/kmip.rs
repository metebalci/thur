// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! KMIP 1.4 keystore backend.
//!
//! Wraps / unwraps a per-volume AES-256 DEK against a long-lived AES
//! KEK held by a KMIP server (Thales CipherTrust, Entrust nShield /
//! KeyControl, Fortanix DSM, Utimaco, HashiCorp Vault Enterprise's
//! KMIP endpoint, IBM SKLM, PyKMIP, …). The KEK never leaves the
//! KMIP server; the daemon-minted DEK only travels in plaintext over
//! a single mTLS request before the server returns the wrapped form.
//!
//! **Encryption-context binding.** KMIP 1.4 `Encrypt` / `Decrypt`
//! support a native `AuthenticatedEncryptionAdditionalData` field for
//! AEAD modes; we pass `hex(wrap_context)` as AAD on every call. The
//! server validates the AAD byte-for-byte on `Decrypt` and refuses
//! mismatches — same protection profile we get from AWS KMS
//! encryption context and GCP KMS `additional_authenticated_data`.
//!
//! Because the KMIP `Encrypt` response returns ciphertext + IV +
//! AEAD tag as separate fields (unlike AWS/GCP, which return an
//! opaque blob with KEK metadata embedded), the caller's persistence
//! layer stores them inside our own JSON envelope shape:
//!
//! ```text
//! { "v": 1, "uuid": "<hex>", "iv": "<b64>", "ct": "<b64>", "tag": "<b64>" }
//! ```
//!
//! The JSON field name `uuid` is wire format and stays — every
//! envelope produced by this backend uses it. Same defensive shape
//! Azure KV uses, with one extra envelope field per AES-GCM output.
//! On unwrap we parse the envelope, verify the embedded context
//! matches the call's `wrap_context`, then feed iv/ct/tag back
//! through KMIP `Decrypt` along with the rebound AAD.
//!
//! Threat model + auth flow: see `docs/admin/ENCRYPTION.md` § VSA keystore
//! backends.

use std::sync::Arc;
use std::sync::Once;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::debug;

use crate::error::KeyStoreError;
use crate::keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
use crate::keystore_config::{ResolvedKmipCaBundle, ResolvedKmipCredential, ResolvedKmipMtls};
use crate::kmip_wire::{self as ttlv, Field};

/// Envelope schema version. Bump in lockstep with any field-shape
/// change; current readers refuse anything they don't recognize so a
/// future writer must also bump.
const ENVELOPE_VERSION: u8 = 1;

/// KMIP protocol version we negotiate. 1.4 is the floor — earlier
/// revisions do not include `AuthenticatedEncryptionAdditionalData`
/// as a first-class field on `Encrypt`/`Decrypt`.
const PROTO_MAJOR: i32 = 1;
const PROTO_MINOR: i32 = 4;

/// Hard cap on the response body we'll read off the wire. A
/// well-formed `Encrypt` / `Decrypt` / `Query` response is well under
/// 1 KiB; 16 KiB gives plenty of headroom while preventing a malicious
/// peer from steering us into an OOM via a forged length header.
const MAX_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct WrappedEnvelope {
    v: u8,
    uuid: String,
    iv: String,
    ct: String,
    tag: String,
}

/// Decoded form of an envelope after the UUID-binding check passes.
/// Field order matches KMIP's `Decrypt` payload (which is why we
/// keep them as separate fields rather than a single tuple).
#[derive(Debug)]
pub(crate) struct UnpackedEnvelope {
    pub iv: Vec<u8>,
    pub ct: Vec<u8>,
    pub tag: Vec<u8>,
}

/// KMIP-backed keystore.
#[derive(Clone)]
pub struct KmipBackend {
    endpoint: String,
    server_name: ServerName<'static>,
    kek_uid: String,
    tls_config: Arc<ClientConfig>,
    credential: Option<ResolvedKmipCredential>,
}

impl std::fmt::Debug for KmipBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // ClientConfig + the (potentially-sensitive) ResolvedKmipCredential
        // don't get printed. ServerName carries no secrets so we surface
        // the SNI hostname for diagnostics.
        let cred = if self.credential.is_some() {
            "<set>"
        } else {
            "<none>"
        };
        f.debug_struct("KmipBackend")
            .field("endpoint", &self.endpoint)
            .field("server_name", &self.server_name)
            .field("kek_uid", &self.kek_uid)
            .field("credential", &cred)
            .finish()
    }
}

// rustls 0.23 requires installing a CryptoProvider before the first
// ClientConfig::builder() call. We install the ring provider lazily
// once per process; calling install_default a second time returns Err
// which we deliberately discard (another shared-keystore consumer in
// the same process may have already installed one).
static CRYPTO_PROVIDER_INSTALL: Once = Once::new();
fn install_crypto_provider() {
    CRYPTO_PROVIDER_INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl KmipBackend {
    /// Construct a KMIP backend handle. The TLS config is built once
    /// here (cert + key + trust roots) and reused for every RPC. PEM
    /// files are read from disk eagerly so a bad path surfaces at
    /// `daemon start` instead of at the first volume create.
    pub async fn new(
        endpoint: String,
        kek_uid: String,
        server_name_override: Option<String>,
        ca_bundle: Option<ResolvedKmipCaBundle>,
        mtls: ResolvedKmipMtls,
        credential: Option<ResolvedKmipCredential>,
    ) -> Result<Self, KeyStoreError> {
        install_crypto_provider();

        // Derive SNI hostname from `endpoint` ("host:port") if the
        // operator didn't override it. Strip the bracket form of an
        // IPv6 literal so `try_from` accepts it.
        let host_portion = endpoint
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(endpoint.as_str())
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let server_name_str = server_name_override.unwrap_or(host_portion);
        let server_name: ServerName<'static> = ServerName::try_from(server_name_str.clone())
            .map_err(|e| {
                KeyStoreError::Other(format!(
                    "kmip: invalid server_name '{server_name_str}': {e}"
                ))
            })?;

        let mut root_store = RootCertStore::empty();
        match ca_bundle {
            Some(ResolvedKmipCaBundle::Path { path }) => {
                let bytes = tokio::fs::read(&path).await.map_err(|e| {
                    KeyStoreError::Auth(format!("kmip: read CA bundle '{path}': {e}"))
                })?;
                load_pem_certs_into(&mut root_store, &bytes)?;
                if root_store.is_empty() {
                    return Err(KeyStoreError::Auth(format!(
                        "kmip: CA bundle '{path}' contained no certificates"
                    )));
                }
            }
            Some(ResolvedKmipCaBundle::SystemRoots) | None => {
                let res = rustls_native_certs::load_native_certs();
                for c in res.certs {
                    let _ = root_store.add(c);
                }
                if !res.errors.is_empty() {
                    debug!(
                        "kmip: rustls_native_certs reported {} non-fatal errors loading system \
                         trust store",
                        res.errors.len()
                    );
                }
                if root_store.is_empty() {
                    return Err(KeyStoreError::Auth(
                        "kmip: system trust store is empty; specify ca_bundle explicitly".into(),
                    ));
                }
            }
        }

        let ResolvedKmipMtls::ClientCert {
            cert_path,
            key_path,
        } = mtls;
        let cert_bytes = tokio::fs::read(&cert_path).await.map_err(|e| {
            KeyStoreError::Auth(format!("kmip: read client cert '{cert_path}': {e}"))
        })?;
        let key_bytes = tokio::fs::read(&key_path)
            .await
            .map_err(|e| KeyStoreError::Auth(format!("kmip: read client key '{key_path}': {e}")))?;
        let client_certs = parse_pem_certs(&cert_bytes)?;
        let client_key = parse_pem_private_key(&key_bytes)?;

        let tls_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(client_certs, client_key)
            .map_err(|e| KeyStoreError::Auth(format!("kmip: tls config build: {e}")))?;

        debug!(
            "Initialized KMIP backend: endpoint={} server_name={:?} kek_uid={} credential={}",
            endpoint,
            server_name,
            kek_uid,
            if credential.is_some() {
                "username/password"
            } else {
                "mTLS-only"
            }
        );

        Ok(Self {
            endpoint,
            server_name,
            kek_uid,
            tls_config: Arc::new(tls_config),
            credential,
        })
    }

    fn aad(wrap_context: &[u8; 16]) -> Vec<u8> {
        hex::encode(wrap_context).into_bytes()
    }

    /// Build the envelope that binds the wrapped ciphertext to
    /// `wrap_context`. Crate-internal so the unit tests can
    /// hand-craft envelopes for the context-mismatch check.
    pub(crate) fn build_envelope(
        wrap_context: &[u8; 16],
        iv: &[u8],
        ct: &[u8],
        tag: &[u8],
    ) -> Vec<u8> {
        let env = WrappedEnvelope {
            v: ENVELOPE_VERSION,
            uuid: hex::encode(wrap_context),
            iv: B64.encode(iv),
            ct: B64.encode(ct),
            tag: B64.encode(tag),
        };
        serde_json::to_vec(&env).unwrap_or_default()
    }

    /// Parse an envelope, verify the embedded context matches the
    /// call, return the unpacked AES-GCM trio. Authz error on
    /// mismatch — refused before any bytes hit the KMIP server.
    pub(crate) fn parse_envelope(
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<UnpackedEnvelope, KeyStoreError> {
        let env: WrappedEnvelope = serde_json::from_slice(wrapped).map_err(|e| {
            KeyStoreError::Other(format!(
                "kmip: wrapped_dek does not parse as a v1 JSON envelope: {e}"
            ))
        })?;
        if env.v != ENVELOPE_VERSION {
            return Err(KeyStoreError::Other(format!(
                "kmip: envelope version {} not understood (expected {})",
                env.v, ENVELOPE_VERSION
            )));
        }
        let expected = hex::encode(wrap_context);
        if env.uuid != expected {
            return Err(KeyStoreError::Authz(format!(
                "kmip: envelope wrap_context mismatch (envelope='{}', call='{}'); refusing to \
                 unwrap — wrapped blob does not belong to this call site",
                env.uuid, expected
            )));
        }
        let iv = B64
            .decode(env.iv.as_bytes())
            .map_err(|e| KeyStoreError::Other(format!("kmip: envelope iv base64 decode: {e}")))?;
        let ct = B64
            .decode(env.ct.as_bytes())
            .map_err(|e| KeyStoreError::Other(format!("kmip: envelope ct base64 decode: {e}")))?;
        let tag = B64
            .decode(env.tag.as_bytes())
            .map_err(|e| KeyStoreError::Other(format!("kmip: envelope tag base64 decode: {e}")))?;
        Ok(UnpackedEnvelope { iv, ct, tag })
    }

    /// Open one TLS connection, send `req_bytes`, read one TTLV
    /// message back, drop the connection. Connection-per-call by
    /// design — wrap/unwrap is rare (volume create + daemon boot),
    /// pooling would be premature.
    async fn rpc(&self, req_bytes: &[u8]) -> Result<Vec<u8>, KeyStoreError> {
        let tcp = TcpStream::connect(&self.endpoint).await.map_err(|e| {
            KeyStoreError::Network(format!("kmip: tcp connect '{}': {e}", self.endpoint))
        })?;
        let connector = TlsConnector::from(self.tls_config.clone());
        let mut tls = connector
            .connect(self.server_name.clone(), tcp)
            .await
            .map_err(|e| KeyStoreError::Network(format!("kmip: tls handshake: {e}")))?;
        tls.write_all(req_bytes)
            .await
            .map_err(|e| KeyStoreError::Network(format!("kmip: tls write: {e}")))?;
        tls.flush()
            .await
            .map_err(|e| KeyStoreError::Network(format!("kmip: tls flush: {e}")))?;

        let mut header = [0u8; 8];
        tls.read_exact(&mut header)
            .await
            .map_err(|e| KeyStoreError::Network(format!("kmip: tls read header: {e}")))?;
        let body_len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if 8 + body_len > MAX_RESPONSE_BYTES {
            return Err(KeyStoreError::Other(format!(
                "kmip: response length {body_len} exceeds MAX_RESPONSE_BYTES ({MAX_RESPONSE_BYTES})"
            )));
        }
        let mut buf = vec![0u8; 8 + body_len];
        buf[..8].copy_from_slice(&header);
        tls.read_exact(&mut buf[8..])
            .await
            .map_err(|e| KeyStoreError::Network(format!("kmip: tls read body: {e}")))?;
        Ok(buf)
    }

    fn request_header(&self) -> Field {
        let mut children = vec![Field::structure(
            ttlv::TAG_PROTOCOL_VERSION,
            vec![
                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MAJOR, PROTO_MAJOR),
                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MINOR, PROTO_MINOR),
            ],
        )];
        if let Some(cred) = &self.credential {
            children.push(Self::build_authentication(cred));
        }
        children.push(Field::integer(ttlv::TAG_BATCH_COUNT, 1));
        Field::structure(ttlv::TAG_REQUEST_HEADER, children)
    }

    fn build_authentication(cred: &ResolvedKmipCredential) -> Field {
        match cred {
            ResolvedKmipCredential::UsernamePassword { username, password } => Field::structure(
                ttlv::TAG_AUTHENTICATION,
                vec![Field::structure(
                    ttlv::TAG_CREDENTIAL,
                    vec![
                        Field::enumeration(
                            ttlv::TAG_CREDENTIAL_TYPE,
                            ttlv::CRED_TYPE_USERNAME_AND_PASSWORD,
                        ),
                        Field::structure(
                            ttlv::TAG_CREDENTIAL_VALUE,
                            vec![
                                Field::text_string(ttlv::TAG_USERNAME, username.clone()),
                                Field::text_string(ttlv::TAG_PASSWORD, password.clone()),
                            ],
                        ),
                    ],
                )],
            ),
        }
    }

    fn build_request(&self, op: u32, payload: Field) -> Vec<u8> {
        let batch_item = Field::structure(
            ttlv::TAG_BATCH_ITEM,
            vec![Field::enumeration(ttlv::TAG_OPERATION, op), payload],
        );
        let msg = Field::structure(
            ttlv::TAG_REQUEST_MESSAGE,
            vec![self.request_header(), batch_item],
        );
        ttlv::encode_message(&msg)
    }

    fn parse_response(buf: &[u8], expected_op: u32) -> Result<Field, KeyStoreError> {
        let msg = ttlv::decode_message(buf)
            .map_err(|e| KeyStoreError::Other(format!("kmip: response decode: {e}")))?;
        let batch = msg
            .child(ttlv::TAG_BATCH_ITEM)
            .ok_or_else(|| KeyStoreError::Other("kmip: response missing BatchItem".to_string()))?;
        let status = batch
            .child(ttlv::TAG_RESULT_STATUS)
            .and_then(|f| f.as_enumeration())
            .ok_or_else(|| {
                KeyStoreError::Other("kmip: response missing ResultStatus".to_string())
            })?;
        if status != ttlv::RS_SUCCESS {
            let reason = batch
                .child(ttlv::TAG_RESULT_REASON)
                .and_then(|f| f.as_enumeration())
                .unwrap_or(0);
            let message = batch
                .child(ttlv::TAG_RESULT_MESSAGE)
                .and_then(|f| f.as_text_string())
                .unwrap_or("(no ResultMessage)")
                .to_string();
            return Err(classify_kmip_failure(status, reason, &message));
        }
        if let Some(echoed_op) = batch
            .child(ttlv::TAG_OPERATION)
            .and_then(|f| f.as_enumeration())
            && echoed_op != expected_op
        {
            return Err(KeyStoreError::Other(format!(
                "kmip: batch echoed op 0x{echoed_op:x}, expected 0x{expected_op:x}"
            )));
        }
        let payload = batch.child(ttlv::TAG_RESPONSE_PAYLOAD).ok_or_else(|| {
            KeyStoreError::Other("kmip: response missing ResponsePayload".to_string())
        })?;
        Ok(payload.clone())
    }

    fn build_encrypt_payload(&self, plaintext: &[u8], aad: &[u8]) -> Field {
        Field::structure(
            ttlv::TAG_REQUEST_PAYLOAD,
            vec![
                Field::text_string(ttlv::TAG_UNIQUE_IDENTIFIER, self.kek_uid.clone()),
                Field::structure(
                    ttlv::TAG_CRYPTOGRAPHIC_PARAMETERS,
                    vec![
                        Field::enumeration(ttlv::TAG_BLOCK_CIPHER_MODE, ttlv::MODE_GCM),
                        Field::enumeration(ttlv::TAG_CRYPTOGRAPHIC_ALGORITHM, ttlv::ALG_AES),
                        Field::integer(ttlv::TAG_TAG_LENGTH, 16),
                    ],
                ),
                Field::byte_string(ttlv::TAG_DATA, plaintext.to_vec()),
                Field::byte_string(
                    ttlv::TAG_AUTHENTICATED_ENCRYPTION_ADDITIONAL_DATA,
                    aad.to_vec(),
                ),
            ],
        )
    }

    fn build_decrypt_payload(
        &self,
        ciphertext: &[u8],
        iv: &[u8],
        gcm_tag: &[u8],
        aad: &[u8],
    ) -> Field {
        // Field order matters — KMIP 1.4 spec § 4.30 specifies the
        // Decrypt payload fields in this exact sequence (UniqueId,
        // CryptoParams, Data, IV, AAD, Tag), and PyKMIP (and most
        // strict KMIP servers) refuse to reorder. Wrong order
        // surfaces as a server-side "Invalid Message" with
        // bytes-remaining noise; field order is the load-bearing
        // contract.
        Field::structure(
            ttlv::TAG_REQUEST_PAYLOAD,
            vec![
                Field::text_string(ttlv::TAG_UNIQUE_IDENTIFIER, self.kek_uid.clone()),
                Field::structure(
                    ttlv::TAG_CRYPTOGRAPHIC_PARAMETERS,
                    vec![
                        Field::enumeration(ttlv::TAG_BLOCK_CIPHER_MODE, ttlv::MODE_GCM),
                        Field::enumeration(ttlv::TAG_CRYPTOGRAPHIC_ALGORITHM, ttlv::ALG_AES),
                        Field::integer(ttlv::TAG_TAG_LENGTH, 16),
                    ],
                ),
                Field::byte_string(ttlv::TAG_DATA, ciphertext.to_vec()),
                Field::byte_string(ttlv::TAG_IV_COUNTER_NONCE, iv.to_vec()),
                Field::byte_string(
                    ttlv::TAG_AUTHENTICATED_ENCRYPTION_ADDITIONAL_DATA,
                    aad.to_vec(),
                ),
                Field::byte_string(ttlv::TAG_AUTHENTICATED_ENCRYPTION_TAG, gcm_tag.to_vec()),
            ],
        )
    }

    fn build_query_payload() -> Field {
        Field::structure(
            ttlv::TAG_REQUEST_PAYLOAD,
            vec![Field::enumeration(
                ttlv::TAG_QUERY_FUNCTION,
                ttlv::QF_QUERY_OPERATIONS,
            )],
        )
    }
}

#[async_trait]
impl KeyStoreBackend for KmipBackend {
    async fn generate_and_wrap(
        &self,
        wrap_context: &[u8; 16],
        source: DekSource,
    ) -> Result<(SecretBytes, Vec<u8>), KeyStoreError> {
        // KMIP `Create` would mint a per-context server-side key, but
        // that forces per-context managed objects on the server and
        // complicates teardown. Collapse Backend → Daemon so every
        // KMIP server (including PyKMIP / minimal HSMs) works without
        // operator setup beyond provisioning the KEK once.
        if matches!(source, DekSource::Backend) {
            debug!(
                "kmip: DekSource::Backend requested; collapsing to Daemon (no per-context \
                 server-side key in this backend by design)"
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
        let aad = Self::aad(wrap_context);
        let payload = self.build_encrypt_payload(plaintext.as_bytes(), &aad);
        let req_bytes = self.build_request(ttlv::OP_ENCRYPT, payload);
        let resp_bytes = self.rpc(&req_bytes).await?;
        let resp_payload = Self::parse_response(&resp_bytes, ttlv::OP_ENCRYPT)?;

        let ct = resp_payload
            .child(ttlv::TAG_DATA)
            .and_then(|f| f.as_byte_string())
            .ok_or_else(|| KeyStoreError::Other("kmip.encrypt: response missing Data".into()))?
            .to_vec();
        let iv = resp_payload
            .child(ttlv::TAG_IV_COUNTER_NONCE)
            .and_then(|f| f.as_byte_string())
            .ok_or_else(|| {
                KeyStoreError::Other("kmip.encrypt: response missing IVCounterNonce".into())
            })?
            .to_vec();
        let tag = resp_payload
            .child(ttlv::TAG_AUTHENTICATED_ENCRYPTION_TAG)
            .and_then(|f| f.as_byte_string())
            .ok_or_else(|| {
                KeyStoreError::Other(
                    "kmip.encrypt: response missing AuthenticatedEncryptionTag".into(),
                )
            })?
            .to_vec();

        Ok(Self::build_envelope(wrap_context, &iv, &ct, &tag))
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        let UnpackedEnvelope {
            iv,
            ct,
            tag: gcm_tag,
        } = Self::parse_envelope(wrap_context, wrapped)?;
        let aad = Self::aad(wrap_context);
        let payload = self.build_decrypt_payload(&ct, &iv, &gcm_tag, &aad);
        let req_bytes = self.build_request(ttlv::OP_DECRYPT, payload);
        let resp_bytes = self.rpc(&req_bytes).await?;
        let resp_payload = Self::parse_response(&resp_bytes, ttlv::OP_DECRYPT)?;

        let plain = resp_payload
            .child(ttlv::TAG_DATA)
            .and_then(|f| f.as_byte_string())
            .ok_or_else(|| KeyStoreError::Other("kmip.decrypt: response missing Data".into()))?;
        if plain.len() != DEK_LEN {
            return Err(KeyStoreError::Other(format!(
                "kmip.decrypt returned {} bytes, expected {}",
                plain.len(),
                DEK_LEN
            )));
        }
        let mut out = [0u8; DEK_LEN];
        out.copy_from_slice(plain);
        Ok(SecretBytes::new(out))
    }

    async fn forget(&self, _wrap_context: &[u8; 16]) -> Result<(), KeyStoreError> {
        // KMIP holds no per-context state (we don't `Create`
        // per-context objects). The envelope at the caller's
        // persistence layer is the only thing tied to this call
        // site; the KEK serves every call site bound to this
        // backend.
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "kmip"
    }

    fn wrap_target_fingerprint(&self) -> String {
        // Endpoint + KEK UID is the wrap target. The KEK Unique
        // Identifier is globally unique within a KMIP server; the
        // endpoint distinguishes servers.
        format!("kmip:{}/{}", self.endpoint, self.kek_uid)
    }

    async fn health_check(&self) -> Result<(), KeyStoreError> {
        let payload = Self::build_query_payload();
        let req_bytes = self.build_request(ttlv::OP_QUERY, payload);
        let resp_bytes = self.rpc(&req_bytes).await?;
        let resp_payload = Self::parse_response(&resp_bytes, ttlv::OP_QUERY)?;
        // Query Operations response carries one Operation child per
        // supported KMIP op. Refuse to start if the server doesn't
        // advertise Encrypt + Decrypt — surfaces "wrong KMIP version"
        // and "wrong KEK profile" together with a single message
        // shape, before any volume create lands at `wrap()`.
        let ops: Vec<u32> = resp_payload
            .children(ttlv::TAG_OPERATION)
            .into_iter()
            .filter_map(|f| f.as_enumeration())
            .collect();
        if !ops.contains(&ttlv::OP_ENCRYPT) || !ops.contains(&ttlv::OP_DECRYPT) {
            return Err(KeyStoreError::Other(format!(
                "kmip: server does not advertise both Encrypt and Decrypt operations \
                 (advertised: {ops:?})"
            )));
        }
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn KeyStoreBackend> {
        Box::new(self.clone())
    }
}

fn classify_kmip_failure(status: u32, reason: u32, message: &str) -> KeyStoreError {
    let label = format!("kmip: status=0x{status:02x} reason=0x{reason:02x} message={message:?}");
    match reason {
        ttlv::RR_AUTHENTICATION_NOT_SUCCESSFUL => KeyStoreError::Auth(label),
        ttlv::RR_PERMISSION_DENIED => KeyStoreError::Authz(label),
        ttlv::RR_ITEM_NOT_FOUND => KeyStoreError::NotFound(label),
        _ => KeyStoreError::Other(label),
    }
}

fn parse_pem_certs(bytes: &[u8]) -> Result<Vec<CertificateDer<'static>>, KeyStoreError> {
    let certs: Result<Vec<CertificateDer<'static>>, _> =
        CertificateDer::pem_slice_iter(bytes).collect();
    let certs =
        certs.map_err(|e| KeyStoreError::Auth(format!("kmip: client cert PEM parse: {e}")))?;
    if certs.is_empty() {
        return Err(KeyStoreError::Auth(
            "kmip: no certificates in client cert PEM".to_string(),
        ));
    }
    Ok(certs)
}

fn parse_pem_private_key(bytes: &[u8]) -> Result<PrivateKeyDer<'static>, KeyStoreError> {
    PrivateKeyDer::from_pem_slice(bytes)
        .map_err(|e| KeyStoreError::Auth(format!("kmip: client key PEM parse: {e}")))
}

fn load_pem_certs_into(store: &mut RootCertStore, bytes: &[u8]) -> Result<(), KeyStoreError> {
    for cert in CertificateDer::pem_slice_iter(bytes) {
        let cert =
            cert.map_err(|e| KeyStoreError::Auth(format!("kmip: CA bundle PEM parse: {e}")))?;
        let _ = store.add(cert);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::KeyStoreFailureKind;

    fn fixture_uuid() -> [u8; 16] {
        [0xABu8; 16]
    }

    /// Build a KmipBackend bypassing PEM loading + cert verification.
    /// The TLS config is never exercised by these tests (they only
    /// drive the pure-data wire helpers); we just need *some* config
    /// to satisfy the struct invariant. Server-cert verification is
    /// effectively a no-op (empty root store) — fine since no real
    /// network IO happens here.
    fn test_backend(credential: Option<ResolvedKmipCredential>) -> KmipBackend {
        install_crypto_provider();
        let tls_config = ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        KmipBackend {
            endpoint: "test.invalid:5696".into(),
            server_name: ServerName::try_from("test.invalid").expect("static ServerName parses"),
            kek_uid: "kek-1".into(),
            tls_config: Arc::new(tls_config),
            credential,
        }
    }

    #[test]
    fn wrap_target_fingerprint_carries_endpoint_and_kek() {
        let b = test_backend(None);
        assert_eq!(b.wrap_target_fingerprint(), "kmip:test.invalid:5696/kek-1");
    }

    #[test]
    fn aad_is_hex_volume_uuid() {
        let uuid = fixture_uuid();
        let aad = KmipBackend::aad(&uuid);
        assert_eq!(aad, b"ab".repeat(16));
        assert_eq!(aad.len(), 32);
    }

    #[test]
    fn envelope_round_trips_with_correct_uuid() {
        let uuid = fixture_uuid();
        let iv = vec![1u8; 12];
        let ct = vec![2u8; 32];
        let tag = vec![3u8; 16];
        let env = KmipBackend::build_envelope(&uuid, &iv, &ct, &tag);
        let unpacked = KmipBackend::parse_envelope(&uuid, &env).expect("round-trip");
        assert_eq!(unpacked.iv, iv);
        assert_eq!(unpacked.ct, ct);
        assert_eq!(unpacked.tag, tag);
    }

    #[test]
    fn envelope_carries_volume_uuid_hex() {
        let uuid = fixture_uuid();
        let env = KmipBackend::build_envelope(&uuid, b"\x01", b"\x02", b"\x03");
        let parsed: WrappedEnvelope = serde_json::from_slice(&env).expect("parse");
        assert_eq!(parsed.v, ENVELOPE_VERSION);
        assert_eq!(parsed.uuid, "ab".repeat(16));
    }

    #[test]
    fn envelope_refuses_uuid_mismatch() {
        // The cross-volume-replay guard. KMIP's Encrypt AAD binding
        // catches mismatches server-side too, but this check refuses
        // before any bytes hit the network.
        let uuid_a = [0x01u8; 16];
        let uuid_b = [0x02u8; 16];
        let env = KmipBackend::build_envelope(&uuid_a, b"\x01", b"\x02", b"\x03");
        let err = KmipBackend::parse_envelope(&uuid_b, &env).expect_err("must reject");
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
            iv: B64.encode(b"\x01"),
            ct: B64.encode(b"\x02"),
            tag: B64.encode(b"\x03"),
        })
        .expect("encode bad-version envelope");
        let err = KmipBackend::parse_envelope(&fixture_uuid(), &bad).expect_err("must reject");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[test]
    fn envelope_refuses_garbage() {
        let err = KmipBackend::parse_envelope(&fixture_uuid(), b"not-json")
            .expect_err("must reject garbage");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[test]
    fn classify_kmip_failure_routes_correctly() {
        let auth = classify_kmip_failure(
            ttlv::RS_OPERATION_FAILED,
            ttlv::RR_AUTHENTICATION_NOT_SUCCESSFUL,
            "bad creds",
        );
        assert_eq!(auth.kind(), KeyStoreFailureKind::Auth);
        assert!(!auth.is_retryable());

        let authz = classify_kmip_failure(
            ttlv::RS_OPERATION_FAILED,
            ttlv::RR_PERMISSION_DENIED,
            "denied",
        );
        assert_eq!(authz.kind(), KeyStoreFailureKind::Authz);
        assert!(!authz.is_retryable());

        let nf = classify_kmip_failure(
            ttlv::RS_OPERATION_FAILED,
            ttlv::RR_ITEM_NOT_FOUND,
            "no such key",
        );
        assert_eq!(nf.kind(), KeyStoreFailureKind::NotFound);
        assert!(!nf.is_retryable());

        let other = classify_kmip_failure(
            ttlv::RS_OPERATION_FAILED,
            ttlv::RR_CRYPTOGRAPHIC_FAILURE,
            "bad tag",
        );
        assert_eq!(other.kind(), KeyStoreFailureKind::Other);
        assert!(other.is_retryable());
    }

    #[test]
    fn parse_response_extracts_failed_status_as_keystore_error() {
        // Build a minimal failed-response message by hand.
        let resp = Field::structure(
            ttlv::TAG_RESPONSE_MESSAGE,
            vec![
                Field::structure(
                    ttlv::TAG_RESPONSE_HEADER,
                    vec![
                        Field::structure(
                            ttlv::TAG_PROTOCOL_VERSION,
                            vec![
                                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MAJOR, 1),
                                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MINOR, 4),
                            ],
                        ),
                        Field::integer(ttlv::TAG_BATCH_COUNT, 1),
                    ],
                ),
                Field::structure(
                    ttlv::TAG_BATCH_ITEM,
                    vec![
                        Field::enumeration(ttlv::TAG_OPERATION, ttlv::OP_ENCRYPT),
                        Field::enumeration(ttlv::TAG_RESULT_STATUS, ttlv::RS_OPERATION_FAILED),
                        Field::enumeration(ttlv::TAG_RESULT_REASON, ttlv::RR_ITEM_NOT_FOUND),
                        Field::text_string(ttlv::TAG_RESULT_MESSAGE, "no such key"),
                    ],
                ),
            ],
        );
        let buf = ttlv::encode_message(&resp);
        let err = KmipBackend::parse_response(&buf, ttlv::OP_ENCRYPT)
            .expect_err("failed status must surface");
        assert!(matches!(err, KeyStoreError::NotFound(_)));
    }

    #[test]
    fn parse_response_success_returns_payload() {
        let resp = Field::structure(
            ttlv::TAG_RESPONSE_MESSAGE,
            vec![
                Field::structure(
                    ttlv::TAG_RESPONSE_HEADER,
                    vec![
                        Field::structure(
                            ttlv::TAG_PROTOCOL_VERSION,
                            vec![
                                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MAJOR, 1),
                                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MINOR, 4),
                            ],
                        ),
                        Field::integer(ttlv::TAG_BATCH_COUNT, 1),
                    ],
                ),
                Field::structure(
                    ttlv::TAG_BATCH_ITEM,
                    vec![
                        Field::enumeration(ttlv::TAG_OPERATION, ttlv::OP_ENCRYPT),
                        Field::enumeration(ttlv::TAG_RESULT_STATUS, ttlv::RS_SUCCESS),
                        Field::structure(
                            ttlv::TAG_RESPONSE_PAYLOAD,
                            vec![
                                Field::text_string(ttlv::TAG_UNIQUE_IDENTIFIER, "kek-1"),
                                Field::byte_string(ttlv::TAG_DATA, vec![0xCDu8; 32]),
                                Field::byte_string(ttlv::TAG_IV_COUNTER_NONCE, vec![0xEFu8; 12]),
                                Field::byte_string(
                                    ttlv::TAG_AUTHENTICATED_ENCRYPTION_TAG,
                                    vec![0x10u8; 16],
                                ),
                            ],
                        ),
                    ],
                ),
            ],
        );
        let buf = ttlv::encode_message(&resp);
        let payload = KmipBackend::parse_response(&buf, ttlv::OP_ENCRYPT).expect("success");
        assert_eq!(
            payload
                .child(ttlv::TAG_UNIQUE_IDENTIFIER)
                .unwrap()
                .as_text_string(),
            Some("kek-1")
        );
        assert_eq!(
            payload.child(ttlv::TAG_DATA).unwrap().as_byte_string(),
            Some(vec![0xCDu8; 32].as_slice())
        );
    }

    #[test]
    fn parse_response_rejects_op_mismatch() {
        // Server echoes Operation = Decrypt, but caller expected Encrypt.
        let resp = Field::structure(
            ttlv::TAG_RESPONSE_MESSAGE,
            vec![
                Field::structure(
                    ttlv::TAG_RESPONSE_HEADER,
                    vec![
                        Field::structure(
                            ttlv::TAG_PROTOCOL_VERSION,
                            vec![
                                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MAJOR, 1),
                                Field::integer(ttlv::TAG_PROTOCOL_VERSION_MINOR, 4),
                            ],
                        ),
                        Field::integer(ttlv::TAG_BATCH_COUNT, 1),
                    ],
                ),
                Field::structure(
                    ttlv::TAG_BATCH_ITEM,
                    vec![
                        Field::enumeration(ttlv::TAG_OPERATION, ttlv::OP_DECRYPT),
                        Field::enumeration(ttlv::TAG_RESULT_STATUS, ttlv::RS_SUCCESS),
                        Field::structure(ttlv::TAG_RESPONSE_PAYLOAD, vec![]),
                    ],
                ),
            ],
        );
        let buf = ttlv::encode_message(&resp);
        let err = KmipBackend::parse_response(&buf, ttlv::OP_ENCRYPT).expect_err("op mismatch");
        assert!(matches!(err, KeyStoreError::Other(_)));
    }

    #[test]
    fn encrypt_request_wire_shape_round_trips_through_codec() {
        // Build a full Encrypt request via the production code paths,
        // then decode it back and assert every nested field. This is
        // the closest unit test can get to "what bytes would the
        // KMIP server actually see"; the surviving uncovered surface
        // is the TLS handshake + raw TCP read/write order, which
        // can't be exercised without a real server.
        let backend = test_backend(None);
        let aad = KmipBackend::aad(&fixture_uuid());
        let payload = backend.build_encrypt_payload(&[0xCDu8; 32], &aad);
        let req_bytes = backend.build_request(ttlv::OP_ENCRYPT, payload);

        let msg = ttlv::decode_message(&req_bytes).expect("server-side decode");
        assert_eq!(msg.tag, ttlv::TAG_REQUEST_MESSAGE);

        let header = msg
            .child(ttlv::TAG_REQUEST_HEADER)
            .expect("RequestHeader present");
        let pv = header
            .child(ttlv::TAG_PROTOCOL_VERSION)
            .expect("ProtocolVersion present");
        assert_eq!(
            pv.child(ttlv::TAG_PROTOCOL_VERSION_MAJOR)
                .and_then(|f| f.as_integer()),
            Some(1)
        );
        assert_eq!(
            pv.child(ttlv::TAG_PROTOCOL_VERSION_MINOR)
                .and_then(|f| f.as_integer()),
            Some(4)
        );
        assert_eq!(
            header
                .child(ttlv::TAG_BATCH_COUNT)
                .and_then(|f| f.as_integer()),
            Some(1)
        );
        // No credential configured → no Authentication header.
        assert!(header.child(ttlv::TAG_AUTHENTICATION).is_none());

        let bi = msg.child(ttlv::TAG_BATCH_ITEM).expect("BatchItem present");
        assert_eq!(
            bi.child(ttlv::TAG_OPERATION)
                .and_then(|f| f.as_enumeration()),
            Some(ttlv::OP_ENCRYPT)
        );
        let rp = bi
            .child(ttlv::TAG_REQUEST_PAYLOAD)
            .expect("RequestPayload present");
        assert_eq!(
            rp.child(ttlv::TAG_UNIQUE_IDENTIFIER)
                .and_then(|f| f.as_text_string()),
            Some("kek-1")
        );
        let cp = rp
            .child(ttlv::TAG_CRYPTOGRAPHIC_PARAMETERS)
            .expect("CryptographicParameters present");
        assert_eq!(
            cp.child(ttlv::TAG_BLOCK_CIPHER_MODE)
                .and_then(|f| f.as_enumeration()),
            Some(ttlv::MODE_GCM)
        );
        assert_eq!(
            cp.child(ttlv::TAG_CRYPTOGRAPHIC_ALGORITHM)
                .and_then(|f| f.as_enumeration()),
            Some(ttlv::ALG_AES)
        );
        assert_eq!(
            cp.child(ttlv::TAG_TAG_LENGTH).and_then(|f| f.as_integer()),
            Some(16)
        );
        assert_eq!(
            rp.child(ttlv::TAG_DATA).and_then(|f| f.as_byte_string()),
            Some(&[0xCDu8; 32][..])
        );
        assert_eq!(
            rp.child(ttlv::TAG_AUTHENTICATED_ENCRYPTION_ADDITIONAL_DATA)
                .and_then(|f| f.as_byte_string()),
            Some(aad.as_slice())
        );
    }

    #[test]
    fn request_includes_authentication_when_credential_configured() {
        let backend = test_backend(Some(ResolvedKmipCredential::UsernamePassword {
            username: "alice".into(),
            password: "s3cr3t".into(),
        }));
        let payload = KmipBackend::build_query_payload();
        let req_bytes = backend.build_request(ttlv::OP_QUERY, payload);
        let msg = ttlv::decode_message(&req_bytes).expect("decode");
        let header = msg.child(ttlv::TAG_REQUEST_HEADER).expect("header");
        let auth = header
            .child(ttlv::TAG_AUTHENTICATION)
            .expect("Authentication header injected when credential is Some");
        let cred = auth.child(ttlv::TAG_CREDENTIAL).expect("credential");
        let cv = cred.child(ttlv::TAG_CREDENTIAL_VALUE).expect("value");
        assert_eq!(
            cv.child(ttlv::TAG_USERNAME)
                .and_then(|f| f.as_text_string()),
            Some("alice")
        );
        assert_eq!(
            cv.child(ttlv::TAG_PASSWORD)
                .and_then(|f| f.as_text_string()),
            Some("s3cr3t")
        );
    }

    #[test]
    fn build_authentication_emits_username_password_credential() {
        let cred = ResolvedKmipCredential::UsernamePassword {
            username: "alice".into(),
            password: "s3cr3t".into(),
        };
        let auth = KmipBackend::build_authentication(&cred);
        assert_eq!(auth.tag, ttlv::TAG_AUTHENTICATION);
        let credential = auth.child(ttlv::TAG_CREDENTIAL).expect("credential child");
        assert_eq!(
            credential
                .child(ttlv::TAG_CREDENTIAL_TYPE)
                .unwrap()
                .as_enumeration(),
            Some(ttlv::CRED_TYPE_USERNAME_AND_PASSWORD)
        );
        let cv = credential.child(ttlv::TAG_CREDENTIAL_VALUE).unwrap();
        assert_eq!(
            cv.child(ttlv::TAG_USERNAME).unwrap().as_text_string(),
            Some("alice")
        );
        assert_eq!(
            cv.child(ttlv::TAG_PASSWORD).unwrap().as_text_string(),
            Some("s3cr3t")
        );
    }
}
