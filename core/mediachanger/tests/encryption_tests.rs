// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for drive-level (LTO Application-Managed) encryption.
//!
//! Covers the full path: install a key on the cartridge, write encrypted
//! blocks, read them back with the same key, and verify that wrong-key /
//! no-key reads return DataDecryptionError. Also covers the LTO mixed-
//! mode case where a tape contains both plaintext and encrypted blocks.

mod common;

use bytes::Bytes;
use common::*;
use core_mediachanger::encryption::{
    ALGORITHM_INDEX_AES_256_GCM, DecryptionMode, DriveEncryptionState, EncryptionMode, KEY_LEN,
    KeyScope,
};
use core_mediachanger::errors::SmcError;

fn make_state(key: [u8; KEY_LEN]) -> DriveEncryptionState {
    DriveEncryptionState {
        mode: EncryptionMode::Encrypt,
        decryption_mode: DecryptionMode::Decrypt,
        scope: KeyScope::Public,
        algorithm_index: ALGORITHM_INDEX_AES_256_GCM,
        key: key.to_vec(),
        kad: Vec::new(),
    }
}

#[test]
fn write_then_read_with_same_key_succeeds() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "ENC001");

    let key = [0xAAu8; KEY_LEN];
    cart.set_encryption_state(make_state(key));

    let plaintext = b"the quick brown fox jumps over the lazy dog".to_vec();
    cart.write_data(Bytes::from(plaintext.clone())).unwrap();

    cart.rewind();
    let block = cart.read_next().unwrap();
    assert_eq!(block.data, plaintext);
}

#[test]
fn read_without_key_returns_data_decryption_error() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "ENC002");

    let key = [0xBBu8; KEY_LEN];
    cart.set_encryption_state(make_state(key));
    cart.write_data(Bytes::from_static(b"top secret")).unwrap();

    // Drop the key — model an UNLOAD where the drive's volatile state goes away.
    cart.clear_encryption();
    cart.rewind();

    match cart.read_next() {
        Err(SmcError::DataDecryptionError(_)) => {}
        other => panic!("expected DataDecryptionError, got {other:?}"),
    }
}

#[test]
fn read_with_wrong_key_returns_data_decryption_error() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "ENC003");

    cart.set_encryption_state(make_state([0xCCu8; KEY_LEN]));
    cart.write_data(Bytes::from_static(b"sensitive data"))
        .unwrap();

    cart.set_encryption_state(make_state([0xDDu8; KEY_LEN]));
    cart.rewind();

    match cart.read_next() {
        Err(SmcError::DataDecryptionError(_)) => {}
        other => panic!("expected DataDecryptionError, got {other:?}"),
    }
}

#[test]
fn mixed_plaintext_and_encrypted_blocks_on_same_tape() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "ENC004");

    // First block plaintext (no key set).
    cart.write_data(Bytes::from_static(b"clear block")).unwrap();

    // Second block encrypted (key installed).
    cart.set_encryption_state(make_state([0xEEu8; KEY_LEN]));
    cart.write_data(Bytes::from_static(b"cipher block"))
        .unwrap();

    // Rewind. Plaintext block should be readable even though the drive
    // currently has a key — read path passes plaintext blocks through.
    cart.rewind();
    let b0 = cart.read_next().unwrap();
    assert_eq!(b0.data, b"clear block".to_vec());

    let b1 = cart.read_next().unwrap();
    assert_eq!(b1.data, b"cipher block".to_vec());
}

#[test]
fn next_block_status_reports_encryption_per_block() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "ENC005");

    cart.write_data(Bytes::from_static(b"clear")).unwrap();
    cart.set_encryption_state(make_state([0x11u8; KEY_LEN]));
    cart.write_data(Bytes::from_static(b"cipher")).unwrap();

    cart.rewind();
    assert!(!cart.next_block_is_encrypted());
    assert_eq!(cart.next_block_algorithm_index(), 0);

    let _ = cart.read_next().unwrap();
    assert!(cart.next_block_is_encrypted());
    assert_eq!(
        cart.next_block_algorithm_index(),
        ALGORITHM_INDEX_AES_256_GCM
    );
}

#[test]
fn manifest_persists_encryption_metadata() {
    use core_mediachanger::{Cartridge, CartridgeOpenMode};

    let dir = create_test_dir();
    let tapes_path = dir.path().join("tapes");
    let key = [0xF0u8; KEY_LEN];

    {
        let mut cart = Cartridge::open(
            &tapes_path,
            "ENC006",
            CartridgeOpenMode::Create {
                backend: "primary".to_string(),
                worm: false,
                dedup: core_mediachanger::DedupScope::Local,
            },
        )
        .unwrap();
        cart.set_encryption_state(make_state(key));
        cart.write_data(Bytes::from_static(b"persisted ciphertext"))
            .unwrap();
    } // cartridge dropped — encryption state goes with it (UNLOAD semantics)

    // Reopen and try to read without a key — must fail with DataDecryptionError
    // because the manifest still says the block is encrypted.
    let mut reopened = Cartridge::open(&tapes_path, "ENC006", CartridgeOpenMode::Open).unwrap();
    reopened.rewind();
    match reopened.read_next() {
        Err(SmcError::DataDecryptionError(_)) => {}
        other => panic!("expected DataDecryptionError after reopen, got {other:?}"),
    }

    // Install the original key and the read should succeed.
    reopened.set_encryption_state(make_state(key));
    reopened.rewind();
    let block = reopened.read_next().unwrap();
    assert_eq!(block.data, b"persisted ciphertext".to_vec());
}

#[test]
fn verify_succeeds_after_decrypt() {
    let dir = create_test_dir();
    let mut cart = create_test_cartridge(&dir, "ENC007");
    cart.set_encryption_state(make_state([0x22u8; KEY_LEN]));
    cart.write_data(Bytes::from_static(b"verify me")).unwrap();
    cart.rewind();
    cart.read_next_verify()
        .expect("verify should pass after decrypt");
}
