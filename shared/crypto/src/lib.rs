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
/// IV reuse with the same key is a confidentiality break in AES-GCM.
/// [`derive_iv`] makes the IV *probabilistically* unique, not injective
/// (see its doc); per NIST SP 800-38D §8.3 keep invocations under one key
/// below 2^32, i.e. rotate the DEK well before ~256 TiB of distinct
/// seals.
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
/// per-volume `uuid`; callers pick `counter_a` / `counter_b` so that no
/// two writes under the same key share the pair.
///
/// **Uniqueness is probabilistic, not guaranteed (issue #199).** The
/// truncating hash is not injective: distinct tuples collide in the
/// 96-bit output with birthday probability ~N²/2⁹⁷ after N seals, so the
/// derivation cannot by itself enforce the "no IV reuse" invariant. In
/// particular the block (SBC) caller draws `iv_salt` at random, which
/// puts it under NIST SP 800-38D §8.3's 2³² random-IV-invocations-per-key
/// bound — ~256 TiB written to one encrypted volume under a never-rotated
/// DEK. Nothing here tracks or enforces that count; operators must rotate
/// the DEK before approaching it. Making the nonce injective (a
/// deterministic counter IV, or per-key derivation with a packed counter)
/// is a format-affecting change tracked separately — do not rely on this
/// function for a hard uniqueness guarantee.
///
/// Typical callers:
/// - **Tape (SSC):** `derive_iv(uuid, chunk_id, offset)`. `chunk_id` is
///   per-cartridge monotonic and never reused (ALLOW OVERWRITE mints a
///   new chunk, doesn't recycle), so `(chunk_id, offset)` is unique for
///   the cartridge lifetime.
/// - **Block (SBC):** `derive_iv(crypto_uuid, page_id, iv_salt)`, where
///   `crypto_uuid` is the volume's *crypto identity* (`dek_uuid()` —
///   its own `uuid`, or the inherited source identity for a clone of an
///   encrypted volume, issue #86) and `iv_salt` is a fresh random
///   per-seal value persisted in the page's `pages.idx` record
///   (issue #87). Every distinct seal of a page — an in-place rewrite,
///   or a divergent write on a clone that still shares the source's
///   `crypto_uuid` — draws a new `iv_salt`, so the resulting nonce is
///   unique even though `page_id` and `crypto_uuid` repeat. Un-diverged
///   chunks shared copy-on-write keep their original salt (it travels
///   with the wholesale `pages.idx` copy), so the clone derives the
///   matching IV for the shared ciphertext. This removes the AES-GCM
///   nonce reuse that the earlier deterministic `counter_b = 0` caused
///   on both single-volume rewrites and encrypted-clone divergence. A
///   pre-salt (v1) `pages.idx` record reads `iv_salt = 0`, reproducing
///   the original IV, so existing encrypted volumes keep decrypting.
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

    // The remaining tests pin the issue #87 per-page IV-salt contract:
    // every other crypto test above hardcodes `TEST_IV`, so none of them
    // actually exercise a *derived* IV through the AEAD, nor prove that
    // the salt (counter_b) changes the keystream. A regression that
    // dropped the salt from the hash would silently reintroduce GCM
    // nonce reuse yet still pass every test above.

    #[test]
    fn derive_iv_roundtrips_through_encrypt_decrypt() {
        // The real path: re-derive the IV at decrypt time from the same
        // (uuid, page_id, salt) tuple rather than storing it.
        let key = [0x7Eu8; KEY_LEN];
        let uuid = [0x5Au8; 16];
        let (page_id, salt) = (42u64, 0xDEAD_BEEFu64);
        let plaintext = b"page bytes that round-trip via a derived nonce";

        let iv = derive_iv(&uuid, page_id, salt);
        let ct = encrypt_block(&key, &iv, plaintext).expect("encrypt");

        // Decrypt with a freshly re-derived IV (not the same binding).
        let iv2 = derive_iv(&uuid, page_id, salt);
        let pt = decrypt_block(&key, &iv2, &ct).expect("decrypt");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn iv_salt_changes_iv_and_ciphertext() {
        // Fix (uuid, page_id) — the part that *repeats* on a page
        // rewrite or an encrypted-clone divergence — and prove the salt
        // is what gives nonce uniqueness.
        let key = [0x33u8; KEY_LEN];
        let uuid = [0xC1u8; 16];
        let page_id = 7u64;
        let plaintext = b"same plaintext, same key, same page";

        let salts = [0u64, 1, 0xFF, 0xDEAD_BEEF, u64::MAX];
        let ivs: Vec<_> = salts
            .iter()
            .map(|&s| derive_iv(&uuid, page_id, s))
            .collect();
        for i in 0..ivs.len() {
            for j in (i + 1)..ivs.len() {
                assert_ne!(
                    ivs[i], ivs[j],
                    "salt {} vs {} must yield distinct IVs",
                    salts[i], salts[j]
                );
            }
        }

        // Distinct IVs under the same key must produce distinct
        // ciphertext for identical plaintext — the property that makes
        // the per-page salt a real nonce-reuse fix.
        let ct_a = encrypt_block(&key, &ivs[0], plaintext).expect("encrypt a");
        let ct_b = encrypt_block(&key, &ivs[3], plaintext).expect("encrypt b");
        assert_ne!(ct_a, ct_b, "distinct salts must change the keystream");
    }

    #[test]
    fn zero_salt_is_deterministic_and_legacy() {
        // A pre-salt (v1) `pages.idx` record reads `iv_salt = 0`, which
        // must reproduce the original deterministic IV so existing
        // encrypted volumes keep decrypting; a non-zero salt must
        // diverge from it.
        let uuid = [0x90u8; 16];
        let page_id = 3u64;
        let legacy = derive_iv(&uuid, page_id, 0);
        assert_eq!(
            legacy,
            derive_iv(&uuid, page_id, 0),
            "zero-salt is deterministic"
        );
        assert_eq!(legacy, derive_iv(&uuid, page_id, 0), "stable across calls");
        assert_ne!(
            legacy,
            derive_iv(&uuid, page_id, 1),
            "any non-zero salt diverges from legacy"
        );
    }
}
