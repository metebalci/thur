// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! AES-256-GCM primitives shared across products.
//!
//! Both Thur VTL (drive-level tape encryption, host-keyed via SSC-4
//! SPOUT) and Thur VSA (at-rest volume encryption, daemon-keyed via the
//! keystore) need the same byte-slice-in / byte-slice-out AES-GCM
//! surface plus the same IV-derivation discipline. Lifting the
//! primitives here lets `core-block` consume them without taking a
//! cross-product dependency on `core-stream`.
//!
//! What lives here vs. stays in `core-stream::encryption`:
//! - **Here:** `encrypt_block`, `decrypt_block`, length constants
//!   (`KEY_LEN` / `IV_LEN` / `TAG_LEN`), `derive_iv` IV-from-counters
//!   helper, and `CryptoError`. Pure crypto, no SCSI coupling.
//! - **There:** `EncryptionMode` (with the SSC-4 `EXTERNAL` value),
//!   `DecryptionMode`, `KeyScope`, `DriveEncryptionState`, and the
//!   `ALGORITHM_INDEX_AES_256_GCM` / `ALGORITHM_CODE_AES_256_GCM`
//!   SCSI-registry constants. Those serve the SCSI surface and stay
//!   tape-side.
//!
//! IV derivation: every (key, IV) pair MUST be unique for AES-GCM's
//! confidentiality guarantee to hold. We don't store IVs — we
//! re-derive them at decrypt time from a (uuid, counter_a, counter_b)
//! tuple. The uuid namespaces per cartridge/volume; the two counters
//! give the caller two degrees of freedom (e.g., tape uses
//! `(chunk_id, offset)`, block uses `(page_id, 0)`).

// aes-gcm 0.10 transitively pins generic-array 0.14, whose from_slice is
// flagged deprecated in newer toolchains. Suppressing here so the crate
// builds clean — the API itself is the documented entry point.
#![allow(deprecated)]
#![forbid(unsafe_code)]

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use thiserror::Error;

/// Re-export the OS CSPRNG from `aes_gcm::aead` so consumers that need
/// to mint a fresh AES-256 key (or any other random bytes) don't have
/// to pull `rand` as a separate workspace dependency. Both core-stream
/// (cartridge UUID generation) and the new VSA at-rest path (auto-
/// generated volume keys) use this.
pub use aes_gcm::aead::{OsRng, rand_core::RngCore};

/// AES-256-GCM key length in bytes.
pub const KEY_LEN: usize = 32;

/// AES-256-GCM nonce length in bytes (96-bit IV).
pub const IV_LEN: usize = 12;

/// AES-256-GCM authentication tag length in bytes.
pub const TAG_LEN: usize = 16;

/// Errors from the crypto layer. Callers map into their own product's
/// error type (`SmcError`, `UploaderError`, …) at the boundary.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// Caller-supplied key or IV had the wrong length, or some other
    /// pre-AEAD input shape violation.
    #[error("crypto input: {0}")]
    Input(String),
    /// AEAD encrypt failed inside the cipher. In practice AES-GCM
    /// encrypt never errors for well-formed inputs — this is a
    /// guard against an upstream surprise rather than an expected
    /// outcome.
    #[error("AES-256-GCM encrypt failed")]
    Encrypt,
    /// AEAD decrypt rejected the (key, IV, ciphertext, tag) tuple —
    /// wrong key, wrong IV, or tampered ciphertext.
    #[error("AES-256-GCM authentication failed: {0}")]
    Decrypt(&'static str),
}

/// Encrypt a plaintext block with AES-256-GCM. The IV is supplied by
/// the caller — typically `derive_iv(uuid, counter_a, counter_b)`.
/// Returns ciphertext concatenated with the 16-byte authentication
/// tag (the standard AES-GCM "ciphertext || tag" form).
///
/// IV reuse with the same key is a confidentiality break in
/// AES-GCM — callers must guarantee uniqueness via the derivation.
pub fn encrypt_block(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if key.len() != KEY_LEN {
        return Err(CryptoError::Input(format!(
            "AES-256 key must be {KEY_LEN} bytes, got {}",
            key.len()
        )));
    }
    if iv.len() != IV_LEN {
        return Err(CryptoError::Input(format!(
            "AES-256-GCM IV must be {IV_LEN} bytes, got {}",
            iv.len()
        )));
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CryptoError::Encrypt)
}

/// Decrypt a ciphertext+tag block with AES-256-GCM. Returns plaintext.
/// Authentication failure (wrong key, tampering, IV mismatch) maps to
/// [`CryptoError::Decrypt`] with a static descriptor for the failure
/// site.
pub fn decrypt_block(
    key: &[u8],
    iv: &[u8],
    ciphertext_with_tag: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if key.len() != KEY_LEN {
        return Err(CryptoError::Decrypt("no key for encrypted block"));
    }
    if iv.len() != IV_LEN {
        return Err(CryptoError::Decrypt("invalid IV length"));
    }
    if ciphertext_with_tag.len() < TAG_LEN {
        return Err(CryptoError::Decrypt("ciphertext too short for tag"));
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    cipher
        .decrypt(nonce, ciphertext_with_tag)
        .map_err(|_| CryptoError::Decrypt("AES-256-GCM authentication failed"))
}

/// Derive a 96-bit AES-GCM IV from a per-data-unit identity tuple.
///
/// Folds `uuid || counter_a || counter_b` through BLAKE3 and truncates
/// to 12 bytes. Domain separation comes from the per-cartridge /
/// per-volume `uuid`; uniqueness within that namespace is the caller's
/// responsibility — they pick `counter_a` and `counter_b` so that no
/// two writes under the same key ever share the pair.
///
/// Typical callers:
/// - **Tape (SSC):** `derive_iv(uuid, chunk_id, offset)`. `chunk_id` is
///   per-cartridge monotonic and never reused (ALLOW OVERWRITE mints a
///   new chunk, doesn't recycle), so `(chunk_id, offset)` is unique for
///   the cartridge lifetime.
/// - **Block (SBC):** `derive_iv(crypto_uuid, page_id, 0)`, where
///   `crypto_uuid` is the volume's *crypto identity* (`dek_uuid()` —
///   its own `uuid`, or the inherited source identity for a clone of an
///   encrypted volume, issue #86).
///   Within a single volume the same `page_id` reused after a rewrite
///   reuses the IV; that is accepted because the new ciphertext + tag
///   overwrite the old pool entry (chunk pool atomic-rename). A clone
///   of an encrypted volume shares the source's `crypto_uuid` so it
///   derives the matching IV for the shared (un-diverged) ciphertext
///   chunks — which means a source page and a *diverged* clone page at
///   the same `page_id` can hold two live ciphertexts under one
///   `(key, IV)`. This is the same class of nonce reuse as the rewrite
///   case, inherent to copy-on-write sharing of encrypted chunks, and
///   is a documented limitation (a per-page IV salt would remove it but
///   needs a `pages.idx` format change — tracked in issue #87).
pub fn derive_iv(uuid: &[u8; 16], counter_a: u64, counter_b: u64) -> [u8; IV_LEN] {
    let mut h = blake3::Hasher::new();
    h.update(uuid);
    h.update(&counter_a.to_le_bytes());
    h.update(&counter_b.to_le_bytes());
    let out = h.finalize();
    let mut iv = [0u8; IV_LEN];
    iv.copy_from_slice(&out.as_bytes()[..IV_LEN]);
    iv
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_IV: [u8; IV_LEN] = [0x11; IV_LEN];

    #[test]
    fn roundtrip() {
        let key = [0x42u8; KEY_LEN];
        let plaintext = b"hello, encrypted block";
        let ct = encrypt_block(&key, &TEST_IV, plaintext).expect("encrypt");
        assert_eq!(ct.len(), plaintext.len() + TAG_LEN);
        let pt = decrypt_block(&key, &TEST_IV, &ct).expect("decrypt");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key = [0x42u8; KEY_LEN];
        let bad = [0x00u8; KEY_LEN];
        let ct = encrypt_block(&key, &TEST_IV, b"data").expect("encrypt");
        let err = decrypt_block(&bad, &TEST_IV, &ct).expect_err("must fail");
        assert!(matches!(err, CryptoError::Decrypt(_)));
    }

    #[test]
    fn no_key_fails() {
        let ct = encrypt_block(&[0x42u8; KEY_LEN], &TEST_IV, b"data").expect("encrypt");
        let err = decrypt_block(&[], &TEST_IV, &ct).expect_err("must fail");
        assert!(matches!(err, CryptoError::Decrypt(_)));
    }

    #[test]
    fn tag_tamper_fails() {
        let key = [0x42u8; KEY_LEN];
        let mut ct = encrypt_block(&key, &TEST_IV, b"data").expect("encrypt");
        let last = ct.len() - 1;
        ct[last] ^= 1;
        let err = decrypt_block(&key, &TEST_IV, &ct).expect_err("must fail");
        assert!(matches!(err, CryptoError::Decrypt(_)));
    }

    #[test]
    fn wrong_iv_fails() {
        let key = [0x42u8; KEY_LEN];
        let ct = encrypt_block(&key, &TEST_IV, b"data").expect("encrypt");
        let bad_iv = [0x22; IV_LEN];
        let err = decrypt_block(&key, &bad_iv, &ct).expect_err("must fail");
        assert!(matches!(err, CryptoError::Decrypt(_)));
    }

    #[test]
    fn iv_length_validated_on_encrypt() {
        let key = [0x42u8; KEY_LEN];
        let err = encrypt_block(&key, &[0; 8], b"data").expect_err("must fail");
        assert!(matches!(err, CryptoError::Input(_)));
    }

    #[test]
    fn iv_length_validated_on_decrypt() {
        let err =
            decrypt_block(&[0u8; KEY_LEN], &[0u8; 8], &[0u8; TAG_LEN + 1]).expect_err("must fail");
        assert!(matches!(err, CryptoError::Decrypt(_)));
    }

    #[test]
    fn derive_iv_is_deterministic_and_unique() {
        let uuid = [0xAB; 16];
        let a = derive_iv(&uuid, 1, 0);
        let b = derive_iv(&uuid, 1, 64);
        let c = derive_iv(&uuid, 2, 0);
        let d = derive_iv(&[0xCD; 16], 1, 0);

        assert_ne!(a, b, "different counter_b -> different IV");
        assert_ne!(a, c, "different counter_a -> different IV");
        assert_ne!(a, d, "different uuid -> different IV");
        assert_eq!(a, derive_iv(&uuid, 1, 0), "deterministic");
    }

    #[test]
    fn derive_iv_is_12_bytes() {
        let iv = derive_iv(&[0; 16], 0, 0);
        assert_eq!(iv.len(), IV_LEN);
    }
}
