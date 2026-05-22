// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Common test utilities for core-mediachanger integration tests
//!
//! This module provides helper functions for creating test fixtures,
//! temporary directories, and common test scenarios. Each integration
//! test binary recompiles this module from scratch and only links the
//! helpers it actually uses, so clippy's per-binary dead-code analysis
//! flags the rest as unused even though they're called from a sibling
//! test file. Suppress at the module level rather than dotting
//! `#[allow]` over every helper.
#![allow(dead_code)]

use bytes::Bytes;
use core_mediachanger::{Cartridge, CartridgeOpenMode, DedupScope, Library};
use tempfile::TempDir;

/// Creates a temporary directory for test data
/// The directory is automatically cleaned up when the TempDir is dropped
pub fn create_test_dir() -> TempDir {
    tempfile::tempdir().expect("Failed to create temp dir")
}

/// Creates a test cartridge with a given name in a temporary directory
///
/// # Arguments
/// * `dir` - The parent directory where the cartridge should be created
/// * `name` - The name of the cartridge (e.g., "TEST001")
///
/// # Returns
/// A newly created Cartridge instance
pub fn create_test_cartridge(dir: &TempDir, name: &str) -> Cartridge {
    // Test cartridges default to DedupScope::Global so the cache /
    // refcount-aware eviction tests find chunks at the un-namespaced
    // shared-pool path. Tests that specifically exercise the
    // per-cartridge layout build cartridges directly with
    // `dedup: DedupScope::Local`.
    let tapes_path = dir.path().join("tapes");
    Cartridge::open(
        &tapes_path,
        name,
        CartridgeOpenMode::Create {
            backend: "primary".to_string(),
            worm: false,
            dedup: DedupScope::Global,
        },
    )
    .expect("Failed to create test cartridge")
}

/// Creates a test library with specified configuration
///
/// # Arguments
/// * `dir` - The directory where the library should be created
/// * `num_slots` - Number of cartridge storage slots
/// * `num_mail_slots` - Number of mail slots (import/export)
/// * `num_drives` - Number of tape drives
///
/// # Returns
/// A newly created Library instance
pub fn create_test_library(
    dir: &TempDir,
    num_slots: u32,
    num_mail_slots: u32,
    num_drives: u32,
) -> Library {
    let lib_root = dir.path().join("library");
    let tapes_dir = dir.path().join("tapes");
    Library::initialize(
        &lib_root,
        &tapes_dir,
        num_slots,
        num_mail_slots,
        num_drives,
        8,
        None,
        0,    // transport_base
        1001, // storage_base
        101,  // import_export_base
        1,    // data_transfer_base
    )
    .expect("Failed to create test library")
}

/// Creates a test library with default configuration (8 slots, 2 mail slots, 2 drives)
pub fn create_default_test_library(dir: &TempDir) -> Library {
    create_test_library(dir, 8, 2, 2)
}

/// Writes test data to a cartridge and returns the data for verification
///
/// # Arguments
/// * `cartridge` - The cartridge to write to
/// * `num_blocks` - Number of blocks to write
/// * `block_size` - Size of each block in bytes
///
/// # Returns
/// A vector of the written data blocks
pub fn write_test_data(
    cartridge: &mut Cartridge,
    num_blocks: usize,
    block_size: usize,
) -> Vec<Vec<u8>> {
    let mut written_data = Vec::new();

    for i in 0..num_blocks {
        // Create test data with predictable pattern
        let data: Vec<u8> = (0..block_size)
            .map(|j| ((i * block_size + j) % 256) as u8)
            .collect();

        let bytes = Bytes::from(data.clone());
        cartridge
            .write_data(bytes)
            .expect("Failed to write test data");
        written_data.push(data);
    }

    written_data
}

/// Writes test data with filemarks between blocks
///
/// # Arguments
/// * `cartridge` - The cartridge to write to
/// * `blocks_per_file` - Number of blocks per file (separated by filemarks)
/// * `num_files` - Number of files to write
/// * `block_size` - Size of each block in bytes
///
/// # Returns
/// A vector of vectors, where each inner vector contains the blocks for one file
pub fn write_test_files(
    cartridge: &mut Cartridge,
    blocks_per_file: usize,
    num_files: usize,
    block_size: usize,
) -> Vec<Vec<Vec<u8>>> {
    let mut files = Vec::new();

    for file_idx in 0..num_files {
        let mut file_blocks = Vec::new();

        for block_idx in 0..blocks_per_file {
            let data: Vec<u8> = (0..block_size)
                .map(|j| {
                    ((file_idx * blocks_per_file * block_size + block_idx * block_size + j) % 256)
                        as u8
                })
                .collect();

            let bytes = Bytes::from(data.clone());
            cartridge
                .write_data(bytes)
                .expect("Failed to write test data");
            file_blocks.push(data);
        }

        // Write filemark after each file
        cartridge
            .write_filemark()
            .expect("Failed to write filemark");
        files.push(file_blocks);
    }

    files
}

/// Loads a cartridge into the library (finds first free slot)
///
/// # Arguments
/// * `library` - The library instance
/// * `barcode` - The barcode of the cartridge
///
/// # Returns
/// The slot ID where the cartridge was loaded
pub fn load_cartridge_to_library(library: &mut Library, barcode: &str) -> u32 {
    library
        .add_or_create_tape(barcode, "primary")
        .expect("Failed to load cartridge to library")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_test_dir() {
        let dir = create_test_dir();
        assert!(dir.path().exists());
    }

    #[test]
    fn test_create_test_cartridge() {
        let dir = create_test_dir();
        let cartridge = create_test_cartridge(&dir, "TEST001");

        // Verify cartridge is usable
        assert_eq!(cartridge.next_lba(), 0);
    }

    #[test]
    fn test_create_test_library() {
        let dir = create_test_dir();
        let library = create_test_library(&dir, 5, 2, 1);

        // Verify library configuration
        assert_eq!(library.storage_slots().len(), 5);
        assert_eq!(library.mail_slots().len(), 2);
        assert_eq!(library.drives().len(), 1);
    }

    #[test]
    fn test_write_test_data() {
        let dir = create_test_dir();
        let mut cartridge = create_test_cartridge(&dir, "TEST002");

        let written = write_test_data(&mut cartridge, 3, 1024);

        assert_eq!(written.len(), 3);
        assert_eq!(written[0].len(), 1024);

        // Verify LBA advanced
        assert_eq!(cartridge.next_lba(), 3);
    }
}
