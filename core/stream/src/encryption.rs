// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Drive-level encryption for Thur VTL.
//!
//! Models LTO Application-Managed Encryption (AME): the host pushes a
//! 256-bit AES key into the drive via SECURITY PROTOCOL OUT (0xB5,
//! protocol 0x20, page 0x0010 Set Data Encryption). The drive then
//! AES-256-GCM-encrypts every block written until the key is cleared
//! or the cartridge is unloaded. Reads decrypt with the current key
//! (or fail with DATA DECRYPTION ERROR if the key is missing/wrong).
//!
//! What lives in this module vs. [`shared_crypto`]: the pure AES-GCM
//! primitives (encrypt/decrypt + IV derivation + length constants)
//! live in `shared-crypto` so `core-block` can reuse them without
//! depending on `core-stream`. The SCSI-flavored types (`EncryptionMode`,
//! `DecryptionMode`, `KeyScope`, `DriveEncryptionState`, plus the
//! SCSI-registry `ALGORITHM_INDEX_AES_256_GCM` /
//! `ALGORITHM_CODE_AES_256_GCM` constants) stay here.
//!
//! `EncryptionMode` deliberately omits the SSC-4 `EXTERNAL` (0x01)
//! value: that mode is for inline FIPS-bump-in-the-wire appliances and
//! has no analogue in a virtual library. The SP-IN Data Encryption
//! Capabilities page advertises CAP_C=00 (no EXTERNAL), and SP-OUT
//! Set Data Encryption with `ENCRYPTION_MODE = 0x01` is rejected by
//! `EncryptionMode::from_u8`, surfacing as CHECK CONDITION at the
//! dispatcher.
//!
//! References: SPC-4 §7.6, SSC-4 §8.5 (Tape Data Encryption).

use crate::errors::{Result, SmcError};
use shared_crypto::{self, CryptoError};

// Re-export the pure crypto surface so existing call sites
// (`encryption::encrypt_block`, `encryption::KEY_LEN`, …) keep
// compiling unchanged.
pub use shared_crypto::{IV_LEN, KEY_LEN, TAG_LEN};

/// Algorithm index reported in SP IN Data Encryption Capabilities and
/// expected back from the host in SP OUT Set Data Encryption.
pub const ALGORITHM_INDEX_AES_256_GCM: u8 = 0x01;

/// SCSI 32-bit ALGORITHM CODE for AES-256-GCM (SCSI registry).
pub const ALGORITHM_CODE_AES_256_GCM: u32 = 0x0001_0014;

/// SP OUT Set Data Encryption — ENCRYPTION_MODE field. EXTERNAL (0x01)
/// is intentionally not a variant; see the module-level note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EncryptionMode {
    Disable = 0x00,
    Encrypt = 0x02,
}

impl EncryptionMode {
    pub fn from_u8(b: u8) -> Result<Self> {
        match b {
            0x00 => Ok(Self::Disable),
            0x02 => Ok(Self::Encrypt),
            _ => Err(SmcError::InvalidField),
        }
    }
}

/// SP OUT Set Data Encryption — DECRYPTION_MODE field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DecryptionMode {
    Disable = 0x00,
    Raw = 0x01,
    Decrypt = 0x02,
    Mixed = 0x03,
}

impl DecryptionMode {
    pub fn from_u8(b: u8) -> Result<Self> {
        match b {
            0x00 => Ok(Self::Disable),
            0x01 => Ok(Self::Raw),
            0x02 => Ok(Self::Decrypt),
            0x03 => Ok(Self::Mixed),
            _ => Err(SmcError::InvalidField),
        }
    }
}

/// SP OUT Set Data Encryption — SCOPE field (bits 7:5 of byte 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyScope {
    Public = 0x00,
    Local = 0x01,
    AllItNexus = 0x02,
}

impl KeyScope {
    pub fn from_u8(b: u8) -> Result<Self> {
        match b {
            0x00 => Ok(Self::Public),
            0x01 => Ok(Self::Local),
            0x02 => Ok(Self::AllItNexus),
            _ => Err(SmcError::InvalidField),
        }
    }
}

/// In-memory drive encryption state. Lives for as long as the cartridge
/// is loaded; cleared on UNLOAD or when the host disables encryption.
///
/// In a real LTO drive the key is held in volatile drive RAM and zeroed
/// on power loss. Here it lives in the `Cartridge` struct and is wiped
/// in `Drop`; UNLOAD drops the cartridge, which drops the state.
#[derive(Clone)]
pub struct DriveEncryptionState {
    pub mode: EncryptionMode,
    pub decryption_mode: DecryptionMode,
    pub scope: KeyScope,
    pub algorithm_index: u8,
    pub key: Vec<u8>,
    pub kad: Vec<u8>,
}

impl std::fmt::Debug for DriveEncryptionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveEncryptionState")
            .field("mode", &self.mode)
            .field("decryption_mode", &self.decryption_mode)
            .field("scope", &self.scope)
            .field("algorithm_index", &self.algorithm_index)
            .field("key_len", &self.key.len())
            .field("kad_len", &self.kad.len())
            .finish()
    }
}

impl Drop for DriveEncryptionState {
    fn drop(&mut self) {
        for b in self.key.iter_mut() {
            *b = 0;
        }
    }
}

impl DriveEncryptionState {
    /// True iff outgoing writes should be encrypted by the drive.
    pub fn encrypt_on_write(&self) -> bool {
        self.mode == EncryptionMode::Encrypt && self.key.len() == KEY_LEN
    }

    /// True iff incoming reads of encrypted blocks should be decrypted.
    /// In MIXED mode plaintext blocks are passed through; that's
    /// handled by the cartridge layer based on per-block metadata.
    pub fn decrypt_on_read(&self) -> bool {
        matches!(
            self.decryption_mode,
            DecryptionMode::Decrypt | DecryptionMode::Mixed
        ) && self.key.len() == KEY_LEN
    }
}

// Map `shared_crypto::CryptoError` into this crate's `SmcError`. Encrypt
// inputs are daemon-controlled so we surface the precise message in
// `EncryptionError`; decrypt failures keep the existing static-string
// `DataDecryptionError` shape because they propagate to the SCSI sense
// path and the variant carries a `&'static str` for the sense
// descriptor.
fn map_encrypt_error(e: CryptoError) -> SmcError {
    match e {
        CryptoError::Input(msg) => SmcError::EncryptionError(msg),
        CryptoError::Encrypt => SmcError::EncryptionError("AES-256-GCM encrypt failed".into()),
        CryptoError::Decrypt(msg) => SmcError::DataDecryptionError(msg),
    }
}

fn map_decrypt_error(e: CryptoError) -> SmcError {
    // Every variant maps to a `&'static str` sense descriptor — that
    // matches the existing `SmcError::DataDecryptionError(&'static str)`
    // shape used by the SCSI sense-builder. Input-shape and
    // encrypt-side variants shouldn't happen on the decrypt path, but
    // we route them through the same arm so a future bug surfaces as
    // a normal decryption error instead of a panic.
    match e {
        CryptoError::Decrypt(msg) => SmcError::DataDecryptionError(msg),
        CryptoError::Input(_) => SmcError::DataDecryptionError("invalid decrypt input"),
        CryptoError::Encrypt => SmcError::DataDecryptionError("encrypt error during decrypt"),
    }
}

/// Encrypt a plaintext block with AES-256-GCM. The IV is supplied by
/// the caller — typically `block_index::derive_iv(uuid, chunk_id,
/// offset)`. Returns ciphertext concatenated with the 16-byte
/// authentication tag (the standard AES-GCM "ciphertext || tag" form).
///
/// Real LTO drives derive their per-block IV from the block's recorded
/// position rather than storing it; the IV is reproducible from
/// (uuid, chunk_id, offset) at decrypt time.
pub fn encrypt_block(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    shared_crypto::encrypt_block(key, iv, plaintext).map_err(map_encrypt_error)
}

/// Decrypt a ciphertext+tag block with AES-256-GCM. Returns plaintext.
/// Authentication failure (wrong key, tampering) maps to DataDecryptionError.
pub fn decrypt_block(key: &[u8], iv: &[u8], ciphertext_with_tag: &[u8]) -> Result<Vec<u8>> {
    shared_crypto::decrypt_block(key, iv, ciphertext_with_tag).map_err(map_decrypt_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_IV: [u8; IV_LEN] = [0x11; IV_LEN];

    #[test]
    fn roundtrip() {
        let key = [0x42u8; KEY_LEN];
        let plaintext = b"hello, encrypted tape";
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
        assert!(matches!(err, SmcError::DataDecryptionError(_)));
    }

    #[test]
    fn no_key_fails() {
        let ct = encrypt_block(&[0x42u8; KEY_LEN], &TEST_IV, b"data").expect("encrypt");
        let err = decrypt_block(&[], &TEST_IV, &ct).expect_err("must fail");
        assert!(matches!(err, SmcError::DataDecryptionError(_)));
    }

    #[test]
    fn tag_tamper_fails() {
        let key = [0x42u8; KEY_LEN];
        let mut ct = encrypt_block(&key, &TEST_IV, b"data").expect("encrypt");
        let last = ct.len() - 1;
        ct[last] ^= 1;
        let err = decrypt_block(&key, &TEST_IV, &ct).expect_err("must fail");
        assert!(matches!(err, SmcError::DataDecryptionError(_)));
    }

    #[test]
    fn wrong_iv_fails() {
        let key = [0x42u8; KEY_LEN];
        let ct = encrypt_block(&key, &TEST_IV, b"data").expect("encrypt");
        let bad_iv = [0x22; IV_LEN];
        let err = decrypt_block(&key, &bad_iv, &ct).expect_err("must fail");
        assert!(matches!(err, SmcError::DataDecryptionError(_)));
    }

    #[test]
    fn iv_length_validated_on_encrypt() {
        let key = [0x42u8; KEY_LEN];
        let err = encrypt_block(&key, &[0; 8], b"data").expect_err("must fail");
        assert!(matches!(err, SmcError::EncryptionError(_)));
    }

    #[test]
    fn iv_length_validated_on_decrypt() {
        let err =
            decrypt_block(&[0u8; KEY_LEN], &[0u8; 8], &[0u8; TAG_LEN + 1]).expect_err("must fail");
        assert!(matches!(err, SmcError::DataDecryptionError(_)));
    }
}
