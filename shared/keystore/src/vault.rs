// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! HashiCorp Vault Transit keystore backend.
//!
//! Hand-rolled `reqwest` client — vaultrs adds a heavyweight builder
//! surface for the four endpoints we need (encrypt, decrypt, datakey,
//! sys/health) plus AppRole login. Mirrors `shared-object-store`'s posture
//! against the Azure / GCP SDK split: lean on the official SDK when
//! it's first-party, otherwise hand-roll the JSON.
//!
//! Endpoints:
//! - `POST /v1/{mount}/encrypt/{key}`             — wrap a plaintext DEK
//! - `POST /v1/{mount}/decrypt/{key}`             — unwrap
//! - `POST /v1/{mount}/datakey/plaintext/{key}`   — HSM-grade DEK + wrap
//! - `GET  /v1/sys/health`                        — readiness probe
//! - `POST /v1/auth/approle/login`                — AppRole token mint

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::error::KeyStoreError;
use crate::keystore_backend::{DEK_LEN, DekSource, KeyStoreBackend, SecretBytes};
use crate::keystore_config::ResolvedVaultAuth;

const VAULT_TOKEN_HEADER: &str = "x-vault-token";
const VAULT_NAMESPACE_HEADER: &str = "x-vault-namespace";

/// HashiCorp Vault Transit-backed keystore.
///
/// The session token lives behind a `RwLock`: AppRole logs in once at
/// `new()` and re-logs in lazily on 401/403 (covers token TTL
/// expiry). Static-token auth fails fast with `KeyStoreError::Auth`
/// on the same status codes — operators should rotate the token and
/// restart.
#[derive(Clone, Debug)]
pub struct VaultBackend {
    inner: Arc<VaultInner>,
}

#[derive(Debug)]
struct VaultInner {
    client: Client,
    address: String,
    transit_mount: String,
    transit_key: String,
    namespace: Option<String>,
    auth: ResolvedVaultAuth,
    token: RwLock<String>,
}

impl VaultBackend {
    pub async fn new(
        address: String,
        transit_mount: String,
        transit_key: String,
        namespace: Option<String>,
        tls_skip_verify: bool,
        auth: ResolvedVaultAuth,
    ) -> Result<Self, KeyStoreError> {
        let address = address.trim_end_matches('/').to_string();
        let client = build_http_client(tls_skip_verify)?;
        let initial_token = match &auth {
            ResolvedVaultAuth::Token(t) => t.clone(),
            ResolvedVaultAuth::AppRole { role_id, secret_id } => {
                approle_login(&client, &address, namespace.as_deref(), role_id, secret_id).await?
            }
        };
        let inner = Arc::new(VaultInner {
            client,
            address,
            transit_mount,
            transit_key,
            namespace,
            auth,
            token: RwLock::new(initial_token),
        });
        Ok(Self { inner })
    }

    async fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let token = self.inner.token.read().await.clone();
        if let Ok(v) = HeaderValue::from_str(&token) {
            headers.insert(HeaderName::from_static(VAULT_TOKEN_HEADER), v);
        }
        if let Some(ns) = self.inner.namespace.as_deref()
            && let Ok(v) = HeaderValue::from_str(ns)
        {
            headers.insert(HeaderName::from_static(VAULT_NAMESPACE_HEADER), v);
        }
        headers
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/{}", self.inner.address, path.trim_start_matches('/'))
    }

    /// Retry the closure once after refreshing the AppRole token on a
    /// 401/403. Static-token auth bypasses the refresh (returns the
    /// classified error immediately).
    async fn with_token_refresh<F, Fut, T>(
        &self,
        op: &'static str,
        f: F,
    ) -> Result<T, KeyStoreError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, KeyStoreError>>,
    {
        match f().await {
            Ok(v) => Ok(v),
            Err(err) => {
                let should_refresh =
                    matches!(&err, KeyStoreError::Auth(_) | KeyStoreError::Authz(_))
                        && matches!(&self.inner.auth, ResolvedVaultAuth::AppRole { .. });
                if !should_refresh {
                    return Err(err);
                }
                debug!(
                    "vault: {} returned auth failure; refreshing AppRole token and retrying",
                    op
                );
                if let ResolvedVaultAuth::AppRole { role_id, secret_id } = &self.inner.auth {
                    let new = approle_login(
                        &self.inner.client,
                        &self.inner.address,
                        self.inner.namespace.as_deref(),
                        role_id,
                        secret_id,
                    )
                    .await?;
                    *self.inner.token.write().await = new;
                }
                f().await
            }
        }
    }

    async fn encrypt_call(
        &self,
        wrap_context: &[u8; 16],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, KeyStoreError> {
        let body = EncryptRequest {
            plaintext: B64.encode(plaintext),
            // Vault Transit's `context` field is the wire-format AAD
            // binding; it accepts any opaque bytes. We feed the 16-byte
            // wrap context (a volume UUID or daemon-identity binding).
            context: B64.encode(wrap_context),
        };
        let path = format!(
            "{}/encrypt/{}",
            self.inner.transit_mount, self.inner.transit_key
        );
        self.with_token_refresh("transit.encrypt", || async {
            let headers = self.auth_headers().await;
            let resp = self
                .inner
                .client
                .post(self.url(&path))
                .headers(headers)
                .json(&body)
                .send()
                .await
                .map_err(|e| classify_reqwest_err("transit.encrypt", e))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(classify_vault_status(
                    "transit.encrypt",
                    status,
                    read_body(resp).await,
                ));
            }
            let parsed: VaultDataResponse<EncryptResponse> = resp
                .json()
                .await
                .map_err(|e| KeyStoreError::Other(format!("transit.encrypt parse: {e}")))?;
            Ok(parsed.data.ciphertext.into_bytes())
        })
        .await
    }

    async fn decrypt_call(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<[u8; DEK_LEN], KeyStoreError> {
        let ct = std::str::from_utf8(wrapped).map_err(|e| {
            KeyStoreError::Other(format!(
                "transit.decrypt: wrapped blob is not UTF-8 ({e}) — expected 'vault:v1:…' string"
            ))
        })?;
        let body = DecryptRequest {
            ciphertext: ct.to_string(),
            context: B64.encode(wrap_context),
        };
        let path = format!(
            "{}/decrypt/{}",
            self.inner.transit_mount, self.inner.transit_key
        );
        self.with_token_refresh("transit.decrypt", || async {
            let headers = self.auth_headers().await;
            let resp = self
                .inner
                .client
                .post(self.url(&path))
                .headers(headers)
                .json(&body)
                .send()
                .await
                .map_err(|e| classify_reqwest_err("transit.decrypt", e))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(classify_vault_status(
                    "transit.decrypt",
                    status,
                    read_body(resp).await,
                ));
            }
            let parsed: VaultDataResponse<DecryptResponse> = resp
                .json()
                .await
                .map_err(|e| KeyStoreError::Other(format!("transit.decrypt parse: {e}")))?;
            let raw = B64.decode(parsed.data.plaintext.as_bytes()).map_err(|e| {
                KeyStoreError::Other(format!("transit.decrypt: plaintext base64 decode: {e}"))
            })?;
            if raw.len() != DEK_LEN {
                return Err(KeyStoreError::Other(format!(
                    "transit.decrypt: plaintext was {} bytes, expected {}",
                    raw.len(),
                    DEK_LEN
                )));
            }
            let mut out = [0u8; DEK_LEN];
            out.copy_from_slice(&raw);
            Ok(out)
        })
        .await
    }

    async fn datakey_call(
        &self,
        wrap_context: &[u8; 16],
    ) -> Result<([u8; DEK_LEN], Vec<u8>), KeyStoreError> {
        let body = DataKeyRequest {
            context: B64.encode(wrap_context),
            bits: 256,
        };
        let path = format!(
            "{}/datakey/plaintext/{}",
            self.inner.transit_mount, self.inner.transit_key
        );
        self.with_token_refresh("transit.datakey", || async {
            let headers = self.auth_headers().await;
            let resp = self
                .inner
                .client
                .post(self.url(&path))
                .headers(headers)
                .json(&body)
                .send()
                .await
                .map_err(|e| classify_reqwest_err("transit.datakey", e))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(classify_vault_status(
                    "transit.datakey",
                    status,
                    read_body(resp).await,
                ));
            }
            let parsed: VaultDataResponse<DataKeyResponse> = resp
                .json()
                .await
                .map_err(|e| KeyStoreError::Other(format!("transit.datakey parse: {e}")))?;
            let plain = B64.decode(parsed.data.plaintext.as_bytes()).map_err(|e| {
                KeyStoreError::Other(format!("transit.datakey: plaintext base64 decode: {e}"))
            })?;
            if plain.len() != DEK_LEN {
                return Err(KeyStoreError::Other(format!(
                    "transit.datakey: plaintext was {} bytes, expected {}",
                    plain.len(),
                    DEK_LEN
                )));
            }
            let mut key = [0u8; DEK_LEN];
            key.copy_from_slice(&plain);
            Ok((key, parsed.data.ciphertext.into_bytes()))
        })
        .await
    }
}

#[async_trait]
impl KeyStoreBackend for VaultBackend {
    async fn generate_and_wrap(
        &self,
        wrap_context: &[u8; 16],
        source: DekSource,
    ) -> Result<(SecretBytes, Vec<u8>), KeyStoreError> {
        match source {
            DekSource::Backend => {
                let (plain, ct) = self.datakey_call(wrap_context).await?;
                Ok((SecretBytes::new(plain), build_vault_envelope(wrap_context, &ct)?))
            }
            DekSource::Daemon => {
                use shared_crypto::{OsRng, RngCore};
                let mut plain = [0u8; DEK_LEN];
                OsRng.fill_bytes(&mut plain);
                let ct = self.encrypt_call(wrap_context, &plain).await?;
                Ok((SecretBytes::new(plain), build_vault_envelope(wrap_context, &ct)?))
            }
        }
    }

    async fn wrap(
        &self,
        wrap_context: &[u8; 16],
        plaintext: &SecretBytes,
    ) -> Result<Vec<u8>, KeyStoreError> {
        let ct = self.encrypt_call(wrap_context, plaintext.as_bytes()).await?;
        build_vault_envelope(wrap_context, &ct)
    }

    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError> {
        // Verify the local wrap-context binding before asking Vault to
        // decrypt — a DEK bound to a different volume is refused here
        // even on a non-derived Transit key (issue #198).
        let ct = parse_vault_envelope(wrap_context, wrapped)?;
        let bytes = self.decrypt_call(wrap_context, ct.as_bytes()).await?;
        Ok(SecretBytes::new(bytes))
    }

    async fn forget(&self, _wrap_context: &[u8; 16]) -> Result<(), KeyStoreError> {
        // Vault Transit holds no per-context state. The wrapped blob
        // at the caller's persistence layer is the only thing tied
        // to this call site. (`transit/keys/<name>/rotate` is a
        // CMK-level rotation event, not per-context.)
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "vault"
    }

    fn wrap_target_fingerprint(&self) -> String {
        // Address + mount + key is the wrap target. Namespace
        // (Enterprise) lives in a separate routing dimension; include
        // it so two entries that share address/mount/key but differ in
        // namespace don't alias.
        let ns = self
            .inner
            .namespace
            .as_deref()
            .map(|n| format!(":ns={n}"))
            .unwrap_or_default();
        format!(
            "vault:{}/{}/{}{}",
            self.inner.address, self.inner.transit_mount, self.inner.transit_key, ns
        )
    }

    async fn health_check(&self) -> Result<(), KeyStoreError> {
        let headers = self.auth_headers().await;
        let resp = self
            .inner
            .client
            .get(self.url("sys/health"))
            .headers(headers)
            .send()
            .await
            .map_err(|e| classify_reqwest_err("sys.health", e))?;
        let status = resp.status();
        // Vault returns rich status codes from sys/health:
        //   200 — initialized, unsealed, active
        //   429 — standby (HA secondary). Still healthy for our use.
        //   472 — performance-standby (Enterprise). Healthy.
        //   473 — DR-secondary. Healthy.
        //   501 — uninitialized.
        //   503 — sealed.
        // Everything in 200/429/472/473 is OK for transit calls
        // routed through the active node (Vault transparently
        // forwards). 501/503 are fatal.
        match status.as_u16() {
            200 | 429 | 472 | 473 => {}
            501 => return Err(KeyStoreError::Other("vault: not initialized".into())),
            503 => return Err(KeyStoreError::Other("vault: sealed".into())),
            _ => {
                return Err(classify_vault_status(
                    "sys.health",
                    status,
                    read_body(resp).await,
                ));
            }
        }

        // Verify the Transit key is derived. A non-derived key silently
        // ignores the per-volume `context`, so wrap-context binding would
        // rest on the local envelope alone; refuse here so misprovisioning
        // surfaces (issue #198). If the daemon's policy can't read the key
        // metadata, warn instead of failing — the binding still holds via
        // the envelope and a correctly-provisioned derived key.
        let key_path = format!("{}/keys/{}", self.inner.transit_mount, self.inner.transit_key);
        let headers = self.auth_headers().await;
        let resp = self
            .inner
            .client
            .get(self.url(&key_path))
            .headers(headers)
            .send()
            .await
            .map_err(|e| classify_reqwest_err("transit.read_key", e))?;
        let status = resp.status();
        if status.is_success() {
            let parsed: VaultDataResponse<TransitKeyInfo> = resp
                .json()
                .await
                .map_err(|e| KeyStoreError::Other(format!("transit.read_key parse: {e}")))?;
            if !parsed.data.derived {
                return Err(KeyStoreError::Other(format!(
                    "vault: Transit key '{}' is not derived (derived=false); the per-volume \
                     wrap-context binding requires `vault write -f {}/keys/{} derived=true` — \
                     a non-derived key silently ignores the context field (issue #198)",
                    self.inner.transit_key, self.inner.transit_mount, self.inner.transit_key
                )));
            }
        } else if status.as_u16() == 403 {
            warn!(
                "vault: cannot read {}/keys/{} to verify derived=true (policy lacks read); \
                 ensure the key was created with derived=true",
                self.inner.transit_mount, self.inner.transit_key
            );
        } else {
            return Err(classify_vault_status(
                "transit.read_key",
                status,
                read_body(resp).await,
            ));
        }
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn KeyStoreBackend> {
        Box::new(self.clone())
    }
}

fn build_http_client(tls_skip_verify: bool) -> Result<Client, KeyStoreError> {
    let mut builder = Client::builder()
        .user_agent("thurvsa-keystore/0.1")
        // reqwest has no default request or connect timeout, so a
        // black-holed Vault (silent packet drop, half-open connection,
        // a server that accepts TCP but never responds) would make every
        // Transit call pend forever — hanging daemon-start volume
        // discovery and the `volume create` job with no error or log
        // (issue #196). Bound both.
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30));
    if tls_skip_verify {
        warn!("vault: tls_skip_verify=true (development only - TLS cert validation disabled)");
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder
        .build()
        .map_err(|e| KeyStoreError::Other(format!("vault: build reqwest client: {e}")))
}

async fn approle_login(
    client: &Client,
    address: &str,
    namespace: Option<&str>,
    role_id: &str,
    secret_id: &str,
) -> Result<String, KeyStoreError> {
    let url = format!("{}/v1/auth/approle/login", address.trim_end_matches('/'));
    let body = AppRoleLoginRequest { role_id, secret_id };
    let mut req = client.post(&url).json(&body);
    if let Some(ns) = namespace
        && let Ok(v) = HeaderValue::from_str(ns)
    {
        req = req.header(VAULT_NAMESPACE_HEADER, v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| classify_reqwest_err("auth.approle.login", e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(classify_vault_status(
            "auth.approle.login",
            status,
            read_body(resp).await,
        ));
    }
    let parsed: AppRoleLoginResponse = resp
        .json()
        .await
        .map_err(|e| KeyStoreError::Other(format!("auth.approle.login parse: {e}")))?;
    Ok(parsed.auth.client_token)
}

async fn read_body(resp: reqwest::Response) -> String {
    resp.text()
        .await
        .unwrap_or_else(|_| "<no body>".to_string())
}

/// Map an HTTP status from Vault to a [`KeyStoreError`]. 401 → Auth,
/// 403 → Authz, 404 → NotFound, 5xx → Other (transient).
pub(crate) fn classify_vault_status(op: &str, status: StatusCode, body: String) -> KeyStoreError {
    let label = format!("{op}: HTTP {status} — {body}");
    match status.as_u16() {
        401 => KeyStoreError::Auth(label),
        403 => KeyStoreError::Authz(label),
        404 => KeyStoreError::NotFound(label),
        408 => KeyStoreError::Timeout(label),
        500..=599 => KeyStoreError::Other(label),
        _ => KeyStoreError::Other(label),
    }
}

fn classify_reqwest_err(op: &'static str, err: reqwest::Error) -> KeyStoreError {
    if err.is_timeout() {
        return KeyStoreError::Timeout(format!("{op}: {err}"));
    }
    if err.is_connect() || err.is_request() {
        return KeyStoreError::Network(format!("{op}: {err}"));
    }
    KeyStoreError::Other(format!("{op}: {err}"))
}

#[derive(Debug, Serialize)]
struct EncryptRequest {
    plaintext: String,
    context: String,
}

#[derive(Debug, Deserialize)]
struct EncryptResponse {
    ciphertext: String,
}

#[derive(Debug, Serialize)]
struct DecryptRequest {
    ciphertext: String,
    context: String,
}

#[derive(Debug, Deserialize)]
struct DecryptResponse {
    plaintext: String,
}

#[derive(Debug, Serialize)]
struct DataKeyRequest {
    context: String,
    bits: u32,
}

#[derive(Debug, Deserialize)]
struct DataKeyResponse {
    plaintext: String,
    ciphertext: String,
}

#[derive(Debug, Deserialize)]
struct VaultDataResponse<T> {
    data: T,
}

/// Transit key metadata (subset) — read by [`VaultBackend::health_check`]
/// to verify the key is derived (issue #198).
#[derive(Debug, Deserialize)]
struct TransitKeyInfo {
    #[serde(default)]
    derived: bool,
}

/// Envelope version for the local wrap-context binding (issue #198).
const VAULT_ENVELOPE_VERSION: u8 = 1;

/// JSON envelope binding the Transit ciphertext to its `wrap_context`,
/// mirroring `azurekv` / `kmip`. Vault Transit's `context` field only
/// binds when the key is *derived* (`derived=true`); a non-derived key
/// silently ignores it, so without this local check a `wrapped_dek`
/// lifted from one volume's manifest would unwrap under another volume's
/// context. The embedded `uuid` (hex of `wrap_context`) is verified on
/// unwrap (issue #198). `ct` is the Transit `vault:v1:…` ciphertext
/// string verbatim.
#[derive(Debug, Serialize, Deserialize)]
struct VaultEnvelope {
    v: u8,
    uuid: String,
    ct: String,
}

/// Build the envelope around a Transit ciphertext string.
fn build_vault_envelope(wrap_context: &[u8; 16], vault_ct: &[u8]) -> Result<Vec<u8>, KeyStoreError> {
    let ct = std::str::from_utf8(vault_ct)
        .map_err(|e| {
            KeyStoreError::Other(format!(
                "vault: Transit ciphertext is not UTF-8 ({e}) — expected 'vault:v1:…'"
            ))
        })?
        .to_string();
    let env = VaultEnvelope {
        v: VAULT_ENVELOPE_VERSION,
        uuid: hex::encode(wrap_context),
        ct,
    };
    serde_json::to_vec(&env)
        .map_err(|e| KeyStoreError::Other(format!("vault: envelope encode: {e}")))
}

/// Parse an envelope and verify it was bound to `wrap_context`; returns
/// the inner Transit ciphertext string on success.
fn parse_vault_envelope(wrap_context: &[u8; 16], wrapped: &[u8]) -> Result<String, KeyStoreError> {
    let env: VaultEnvelope = serde_json::from_slice(wrapped).map_err(|e| {
        KeyStoreError::Other(format!(
            "vault: wrapped_dek does not parse as a v1 JSON envelope: {e}"
        ))
    })?;
    if env.v != VAULT_ENVELOPE_VERSION {
        return Err(KeyStoreError::Other(format!(
            "vault: envelope version {} not understood (expected {})",
            env.v, VAULT_ENVELOPE_VERSION
        )));
    }
    let expected = hex::encode(wrap_context);
    if env.uuid != expected {
        return Err(KeyStoreError::Authz(format!(
            "vault: envelope wrap_context mismatch (envelope='{}', call='{}'); refusing to unwrap a DEK bound to a different volume",
            env.uuid, expected
        )));
    }
    Ok(env.ct)
}

#[derive(Debug, Serialize)]
struct AppRoleLoginRequest<'a> {
    role_id: &'a str,
    secret_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct AppRoleLoginResponse {
    auth: AppRoleAuth,
}

#[derive(Debug, Deserialize)]
struct AppRoleAuth {
    client_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::KeyStoreFailureKind;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture_uuid() -> [u8; 16] {
        [0x11; 16]
    }

    fn fixture_key() -> [u8; DEK_LEN] {
        let mut k = [0u8; DEK_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn classify_status_table() {
        assert!(matches!(
            classify_vault_status("op", StatusCode::UNAUTHORIZED, "x".into()).kind(),
            KeyStoreFailureKind::Auth
        ));
        assert!(matches!(
            classify_vault_status("op", StatusCode::FORBIDDEN, "x".into()).kind(),
            KeyStoreFailureKind::Authz
        ));
        assert!(matches!(
            classify_vault_status("op", StatusCode::NOT_FOUND, "x".into()).kind(),
            KeyStoreFailureKind::NotFound
        ));
        assert!(matches!(
            classify_vault_status("op", StatusCode::INTERNAL_SERVER_ERROR, "x".into()).kind(),
            KeyStoreFailureKind::Other
        ));
        assert!(matches!(
            classify_vault_status("op", StatusCode::REQUEST_TIMEOUT, "x".into()).kind(),
            KeyStoreFailureKind::Timeout
        ));
    }

    #[test]
    fn encrypt_request_serializes() {
        let req = EncryptRequest {
            plaintext: B64.encode([1u8, 2, 3]),
            context: B64.encode([0xABu8; 16]),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("plaintext"));
        assert!(s.contains("context"));
    }

    #[tokio::test]
    async fn wrap_round_trips_through_wiremock() {
        let server = MockServer::start().await;

        // /encrypt returns a synthetic vault:v1:… string.
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .and(header("x-vault-token", "s.devroot"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "ciphertext": "vault:v1:DEADBEEF" }
            })))
            .mount(&server)
            .await;

        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();

        let wrapped = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .unwrap();
        // wrap now returns a wrap-context-binding envelope (issue #198);
        // the Transit ciphertext is recoverable by parsing it back.
        let ct = parse_vault_envelope(&fixture_uuid(), &wrapped).unwrap();
        assert_eq!(ct, "vault:v1:DEADBEEF");
    }

    #[tokio::test]
    async fn unwrap_round_trips_through_wiremock() {
        let server = MockServer::start().await;
        let plaintext_b64 = B64.encode(fixture_key());

        Mock::given(method("POST"))
            .and(path("/v1/transit/decrypt/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "plaintext": plaintext_b64 }
            })))
            .mount(&server)
            .await;

        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();

        let envelope = build_vault_envelope(&fixture_uuid(), b"vault:v1:DEADBEEF").unwrap();
        let plain = backend.unwrap(&fixture_uuid(), &envelope).await.unwrap();
        assert_eq!(plain.as_bytes(), &fixture_key());
    }

    #[tokio::test]
    async fn unwrap_refuses_wrong_wrap_context() {
        // Issue #198: a DEK envelope bound to volume A must not unwrap
        // under volume B's context — refused locally before any Vault
        // call, so the binding holds even on a non-derived Transit key.
        let server = MockServer::start().await;
        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();
        let envelope = build_vault_envelope(&[0xAAu8; 16], b"vault:v1:DEADBEEF").unwrap();
        let err = backend
            .unwrap(&[0xBBu8; 16], &envelope)
            .await
            .expect_err("context mismatch must be refused");
        assert_eq!(err.kind(), KeyStoreFailureKind::Authz);
    }

    #[tokio::test]
    async fn wrap_target_fingerprint_includes_namespace_when_set() {
        let server = MockServer::start().await;
        let no_ns = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();
        let with_ns = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            Some("team-a".into()),
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();
        assert_eq!(
            no_ns.wrap_target_fingerprint(),
            format!("vault:{}/transit/thurvsa-volumes", server.uri())
        );
        assert_eq!(
            with_ns.wrap_target_fingerprint(),
            format!("vault:{}/transit/thurvsa-volumes:ns=team-a", server.uri())
        );
        assert_ne!(
            no_ns.wrap_target_fingerprint(),
            with_ns.wrap_target_fingerprint()
        );
    }

    #[tokio::test]
    async fn health_check_treats_429_as_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sys/health"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        // health_check also verifies the Transit key is derived (#198).
        Mock::given(method("GET"))
            .and(path("/v1/transit/keys/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "derived": true }
            })))
            .mount(&server)
            .await;
        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();
        backend.health_check().await.expect("standby still healthy");
    }

    #[tokio::test]
    async fn health_check_refuses_non_derived_key() {
        // Issue #198: a non-derived Transit key silently ignores the
        // per-volume context, so health_check must refuse it.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sys/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/transit/keys/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "derived": false }
            })))
            .mount(&server)
            .await;
        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();
        let err = backend
            .health_check()
            .await
            .expect_err("non-derived key must be refused");
        assert!(format!("{err}").contains("derived"));
    }

    #[tokio::test]
    async fn health_check_503_is_sealed_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sys/health"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .unwrap();
        let err = backend.health_check().await.unwrap_err();
        assert!(format!("{err}").contains("sealed"));
    }

    async fn token_backend(server: &MockServer) -> VaultBackend {
        VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .expect("construct token-auth backend")
    }

    #[tokio::test]
    async fn health_check_501_is_uninitialized_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sys/health"))
            .respond_with(ResponseTemplate::new(501))
            .mount(&server)
            .await;
        let backend = token_backend(&server).await;
        let err = backend.health_check().await.expect_err("501 is fatal");
        assert!(format!("{err}").contains("not initialized"));
    }

    #[tokio::test]
    async fn health_check_other_status_classifies() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sys/health"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        let backend = token_backend(&server).await;
        let err = backend.health_check().await.expect_err("403 is fatal");
        assert_eq!(err.kind(), KeyStoreFailureKind::Authz);
    }

    #[tokio::test]
    async fn datakey_backend_source_round_trips() {
        let server = MockServer::start().await;
        let plaintext_b64 = B64.encode(fixture_key());
        Mock::given(method("POST"))
            .and(path("/v1/transit/datakey/plaintext/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "plaintext": plaintext_b64,
                    "ciphertext": "vault:v1:DATAKEY",
                }
            })))
            .mount(&server)
            .await;
        let backend = token_backend(&server).await;
        let (plain, wrapped) = backend
            .generate_and_wrap(&fixture_uuid(), DekSource::Backend)
            .await
            .expect("datakey");
        assert_eq!(plain.as_bytes(), &fixture_key());
        assert_eq!(
            parse_vault_envelope(&fixture_uuid(), &wrapped).unwrap(),
            "vault:v1:DATAKEY"
        );
    }

    #[tokio::test]
    async fn generate_daemon_source_uses_encrypt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "ciphertext": "vault:v1:DAEMONKEY" }
            })))
            .mount(&server)
            .await;
        let backend = token_backend(&server).await;
        let (plain, wrapped) = backend
            .generate_and_wrap(&fixture_uuid(), DekSource::Daemon)
            .await
            .expect("encrypt path");
        assert_eq!(plain.as_bytes().len(), DEK_LEN);
        assert_eq!(
            parse_vault_envelope(&fixture_uuid(), &wrapped).unwrap(),
            "vault:v1:DAEMONKEY"
        );
    }

    #[tokio::test]
    async fn unwrap_rejects_wrong_length_plaintext() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transit/decrypt/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "plaintext": B64.encode(b"short") }
            })))
            .mount(&server)
            .await;
        let backend = token_backend(&server).await;
        let wrapped = build_vault_envelope(&fixture_uuid(), b"vault:v1:X").unwrap();
        let err = backend
            .unwrap(&fixture_uuid(), &wrapped)
            .await
            .expect_err("short plaintext");
        match err {
            KeyStoreError::Other(msg) => assert!(msg.contains("expected 32")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unwrap_rejects_malformed_envelope() {
        let server = MockServer::start().await;
        let backend = token_backend(&server).await;
        // A non-envelope blob is rejected before any network call: unwrap
        // parses the wrap-context envelope first (issue #198).
        let err = backend
            .unwrap(&fixture_uuid(), &[0xFF, 0xFE])
            .await
            .expect_err("malformed envelope");
        match err {
            KeyStoreError::Other(msg) => assert!(msg.contains("envelope")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_error_classifies_as_retryable_other() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .mount(&server)
            .await;
        let backend = token_backend(&server).await;
        let err = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect_err("500");
        assert_eq!(err.kind(), KeyStoreFailureKind::Other);
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn token_auth_does_not_refresh_on_403() {
        // Static-token auth: a 403 fails fast, no AppRole re-login.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(403).set_body_string("perm denied"))
            .mount(&server)
            .await;
        let backend = token_backend(&server).await;
        let err = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect_err("403");
        assert_eq!(err.kind(), KeyStoreFailureKind::Authz);
    }

    #[tokio::test]
    async fn approle_login_mints_initial_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/approle/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "auth": { "client_token": "s.approle-token" }
            })))
            .mount(&server)
            .await;
        // The encrypt call must carry the AppRole-minted token.
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .and(header("x-vault-token", "s.approle-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "ciphertext": "vault:v1:APPROLE" }
            })))
            .mount(&server)
            .await;
        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::AppRole {
                role_id: "role-1".into(),
                secret_id: "secret-1".into(),
            },
        )
        .await
        .expect("approle login");
        let wrapped = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap with approle token");
        assert_eq!(
            parse_vault_envelope(&fixture_uuid(), &wrapped).unwrap(),
            "vault:v1:APPROLE"
        );
    }

    #[tokio::test]
    async fn approle_login_failure_classifies() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/approle/login"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid role"))
            .mount(&server)
            .await;
        let err = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::AppRole {
                role_id: "bad".into(),
                secret_id: "bad".into(),
            },
        )
        .await
        .expect_err("login must fail");
        assert_eq!(err.kind(), KeyStoreFailureKind::Other);
    }

    #[tokio::test]
    async fn approle_refreshes_token_on_403_then_retries() {
        // The first encrypt 403s; the backend re-logs in via AppRole
        // and retries. The 403 mock is capped at one hit and given a
        // lower priority so the header-matched success mock answers
        // the post-refresh retry.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/approle/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "auth": { "client_token": "s.fresh-token" }
            })))
            .mount(&server)
            .await;
        // The retry carries the fresh token and succeeds.
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .and(header("x-vault-token", "s.fresh-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "ciphertext": "vault:v1:RETRIED" }
            })))
            .mount(&server)
            .await;
        // The initial call 403s exactly once, forcing the refresh.
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .respond_with(ResponseTemplate::new(403).set_body_string("token expired"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::AppRole {
                role_id: "role-1".into(),
                secret_id: "secret-1".into(),
            },
        )
        .await
        .expect("initial approle login");
        let wrapped = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect("retry after refresh");
        assert_eq!(
            parse_vault_envelope(&fixture_uuid(), &wrapped).unwrap(),
            "vault:v1:RETRIED"
        );
    }

    #[tokio::test]
    async fn classify_reqwest_err_connect_is_network() {
        // No server listening on this port — reqwest fails to connect.
        let backend = VaultBackend::new(
            "http://127.0.0.1:1".into(),
            "transit".into(),
            "thurvsa-volumes".into(),
            None,
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .expect("construct (no eager connect for token auth)");
        let err = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect_err("connection refused");
        assert_eq!(err.kind(), KeyStoreFailureKind::Network);
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn forget_is_a_noop() {
        let server = MockServer::start().await;
        let backend = token_backend(&server).await;
        backend.forget(&fixture_uuid()).await.expect("noop");
    }

    #[tokio::test]
    async fn backend_type_and_clone_box() {
        let server = MockServer::start().await;
        let backend = token_backend(&server).await;
        assert_eq!(backend.backend_type(), "vault");
        assert_eq!(backend.clone_box().backend_type(), "vault");
    }

    #[tokio::test]
    async fn namespace_header_is_sent_when_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transit/encrypt/thurvsa-volumes"))
            .and(header("x-vault-namespace", "team-b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "ciphertext": "vault:v1:NS" }
            })))
            .mount(&server)
            .await;
        let backend = VaultBackend::new(
            server.uri(),
            "transit".into(),
            "thurvsa-volumes".into(),
            Some("team-b".into()),
            false,
            ResolvedVaultAuth::Token("s.devroot".into()),
        )
        .await
        .expect("construct");
        let wrapped = backend
            .wrap(&fixture_uuid(), &SecretBytes::new(fixture_key()))
            .await
            .expect("wrap with namespace");
        assert_eq!(
            parse_vault_envelope(&fixture_uuid(), &wrapped).unwrap(),
            "vault:v1:NS"
        );
    }
}
