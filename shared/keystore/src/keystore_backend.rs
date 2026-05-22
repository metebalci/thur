// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Pluggable keystore-backend trait.
//!
//! Three real backends today: [`super::local::LocalBackend`] (on-disk
//! plaintext keyfile, same shape the daemon used pre-trait),
//! [`super::awskms::AwsKmsBackend`] (KMS envelope encryption), and
//! [`super::vault::VaultBackend`] (HashiCorp Vault Transit). All three
//! return / accept 32-byte AES-256 DEKs through the same
//! `generate_and_wrap` / `wrap` / `unwrap` surface; the daemon doesn't
//! special-case any backend in the data path.

use std::fmt::Debug;

use async_trait::async_trait;

use crate::error::KeyStoreError;

/// AES-256 DEK length, re-exported so callers don't have to depend on
/// `shared-crypto` for the constant.
pub const DEK_LEN: usize = shared_crypto::KEY_LEN;

/// Where the DEK is minted on `volume create`.
///
/// - [`DekSource::Daemon`] — daemon mints 32 bytes from `OsRng`, then
///   asks the backend to [`KeyStoreBackend::wrap`] them. Default.
///   `local` always uses this (no remote RNG available).
/// - [`DekSource::Backend`] — daemon calls
///   [`KeyStoreBackend::generate_and_wrap`] with this variant; backends
///   that have a real RNG primitive (KMS `GenerateDataKey`, Vault
///   `transit/datakey/plaintext`) use it directly, saving one
///   round-trip and yielding HSM-grade randomness. `local` silently
///   treats this as `Daemon`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DekSource {
    Daemon,
    Backend,
}

impl DekSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            DekSource::Daemon => "daemon",
            DekSource::Backend => "backend",
        }
    }

    /// Parse the CLI / RPC string form.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "daemon" => Some(DekSource::Daemon),
            "backend" => Some(DekSource::Backend),
            _ => None,
        }
    }
}

/// 32-byte AES-256 DEK with a best-effort zeroize on Drop. We match
/// the volatile-write pattern from `core/ssc/src/encryption.rs` and
/// `core/sbc/src/uploader.rs` instead of pulling the `zeroize` crate
/// into the workspace.
pub struct SecretBytes(pub [u8; DEK_LEN]);

impl SecretBytes {
    /// Construct from raw bytes without further validation. Callers
    /// already know they have a 32-byte slice (KMS / Vault decrypt
    /// responses are checked at the dispatch boundary).
    pub fn new(bytes: [u8; DEK_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw 32 bytes. Prefer this over moving the array out
    /// when the consumer doesn't need ownership — the zeroize on Drop
    /// only fires if the `SecretBytes` value itself drops.
    pub fn as_bytes(&self) -> &[u8; DEK_LEN] {
        &self.0
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        // Volatile loop — defeats the optimizer's dead-store
        // elimination (a memset to zero is otherwise legal to remove
        // when the buffer isn't read again). Matches the discipline
        // used in core/ssc/src/encryption.rs:136-142.
        for b in &mut self.0 {
            // Safety: ptr is to a u8 inside our own owned buffer; the
            // write is in-bounds, aligned (u8 has no alignment), and
            // single-threaded since we're inside Drop. We use
            // write_volatile only to keep the writes observable.
            //
            // The crate forbids unsafe_code so we go through
            // core::hint::black_box, which the optimizer treats as an
            // opaque operation; the compiler must materialize the
            // store. (Equivalent to the pattern in
            // core/sbc/src/uploader.rs:134-145.)
            *b = std::hint::black_box(0);
        }
    }
}

impl Debug for SecretBytes {
    /// Never print the bytes. Logging a `SecretBytes` should make it
    /// obvious from the output that material was redacted, not
    /// truncate-displayed as garbage hex.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBytes(<redacted>)")
    }
}

/// Pluggable keystore backend.
///
/// Implementors are reachable through `Arc<dyn KeyStoreBackend>` from
/// the daemon admin handlers. `clone_box` + the `Clone for Box<dyn ...>`
/// blanket impl below let callers stash them in the same shape as
/// `Arc<dyn shared_cloud::CloudBackend>`.
#[async_trait]
pub trait KeyStoreBackend: Debug + Send + Sync {
    /// Mint a 32-byte secret bound to `wrap_context` and return
    /// `(plaintext, wrapped_ciphertext)`. The caller in tree is
    /// per-volume DEKs (context = volume UUID; daemon hands plaintext
    /// to `VolumeWriter::open_with_key` and persists the ciphertext in
    /// the manifest's `encryption.wrapped_dek`).
    ///
    /// `source` selects RNG location. Backends without their own RNG
    /// (just `local`) treat both variants as [`DekSource::Daemon`].
    async fn generate_and_wrap(
        &self,
        wrap_context: &[u8; 16],
        source: DekSource,
    ) -> Result<(SecretBytes, Vec<u8>), KeyStoreError>;

    /// Encrypt an externally-supplied plaintext secret (e.g. operator
    /// passed `--key-file`). Wrap context binds the output: a blob
    /// wrapped against one context cannot be unwrapped against another.
    async fn wrap(
        &self,
        wrap_context: &[u8; 16],
        plaintext: &SecretBytes,
    ) -> Result<Vec<u8>, KeyStoreError>;

    /// Decrypt a wrapped blob back into the 32-byte secret. `wrapped`
    /// should be the bytes the caller's persistence layer holds; for
    /// `local` this is empty (the local backend reads its sidecar
    /// file) — see [`KeyStoreBackend::manages_local_blob`].
    async fn unwrap(
        &self,
        wrap_context: &[u8; 16],
        wrapped: &[u8],
    ) -> Result<SecretBytes, KeyStoreError>;

    /// Drop any backend-managed side state for `wrap_context`. KMS and
    /// Vault are stateless from the operator's view (the wrapped
    /// blob is the storage), so this is a no-op for them. Local
    /// deletes `<data_dir>/keys/<context_hex>.key`. Idempotent —
    /// missing is not an error so destroy paths can rerun without
    /// checking.
    async fn forget(&self, wrap_context: &[u8; 16]) -> Result<(), KeyStoreError>;

    /// Tag string for logs / audit payloads (`"local"`, `"awskms"`,
    /// `"vault"`).
    fn backend_type(&self) -> &'static str;

    /// True if the backend keeps its own on-disk sidecar (the local
    /// backend's `<data_dir>/keys/<uuid>.key`). The daemon uses this
    /// to decide whether to also write `wrapped_dek` into the
    /// manifest — for `local` it stays `None`, so a manifest from a
    /// local-keystore volume looks the same after the migration to
    /// the trait as it did before.
    fn manages_local_blob(&self) -> bool {
        false
    }

    /// Lightweight reachability probe used at startup. KMS calls
    /// `DescribeKey`; Vault hits `sys/health`; local stats the
    /// keystore directory mode. Errors classify the same way as
    /// data-path failures so operators get a consistent message
    /// shape across keystore problems.
    async fn health_check(&self) -> Result<(), KeyStoreError>;

    /// Canonical identifier of the external wrap target this backend
    /// instance addresses. Two backends that produce the same string
    /// store DEKs at the same external location regardless of the
    /// operator-facing name under `keystore.backends:`.
    ///
    /// Used by `thurvsa volume key migrate` to refuse a no-op
    /// migration when the source and destination resolve to the same
    /// target (two `local` entries with the same `data_dir`, two
    /// `awskms` entries pointing at the same key ARN, etc.). The
    /// format is operator-facing — surfaced verbatim in the
    /// "no-op: …" error — so each backend produces a string that
    /// names the target the way an operator would recognize it.
    fn wrap_target_fingerprint(&self) -> String;

    /// Clone the trait object for `Arc<dyn KeyStoreBackend>` storage.
    /// Implementors typically wrap their state in `Arc` internally
    /// and return `Box::new(self.clone())`.
    fn clone_box(&self) -> Box<dyn KeyStoreBackend>;
}

impl Clone for Box<dyn KeyStoreBackend> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
