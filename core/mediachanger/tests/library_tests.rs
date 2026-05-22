// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for Library functionality
//!
//! These tests verify the tape library operations including:
//! - Library creation and configuration
//! - Slot management
//! - Load/unload operations
//! - Import/export (mail slots)
//! - Multi-drive operations

mod common;

use common::*;

#[test]
fn test_library_creation() {
    let dir = create_test_dir();
    let library = create_test_library(&dir, 10, 2, 2);

    assert_eq!(library.storage_slots().len(), 10);
    assert_eq!(library.mail_slots().len(), 2);
    assert_eq!(library.drives().len(), 2);
}

#[test]
fn test_add_cartridge_to_slot() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 0, 1);

    // Add cartridge (automatically assigns to next free slot)
    let slot_id = library.add_or_create_tape("BAR001", "primary").unwrap();

    // Verify slot is occupied
    let slot = library.get_storage_slot(slot_id).unwrap();
    assert!(slot.occupied);
    assert_eq!(slot.barcode.as_ref().unwrap(), "BAR001");
}

#[test]
fn test_load_to_drive() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 0, 2);

    // Add cartridge (gets assigned to slot 0)
    let slot_id = library.add_or_create_tape("BAR001", "primary").unwrap();

    // Load to drive 0
    library.load_to_drive(slot_id, 0).unwrap();

    // Verify slot is now empty
    let slot = library.get_storage_slot(slot_id).unwrap();
    assert!(!slot.occupied);

    // Verify drive has cartridge
    let drive = library.get_drive(0).unwrap();
    assert!(drive.occupied);
    assert_eq!(drive.barcode.as_ref().unwrap(), "BAR001");
    assert_eq!(drive.home_slot, Some(slot_id as u16));
}

#[test]
fn test_unload_from_drive() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 0, 1);

    // Add cartridge and load to drive
    let slot_id = library.add_or_create_tape("BAR002", "primary").unwrap();
    library.load_to_drive(slot_id, 0).unwrap();

    // Unload back to a different slot
    library.unload_from_drive(0, 2).unwrap();

    // Verify drive is empty
    let drive = library.get_drive(0).unwrap();
    assert!(!drive.occupied);

    // Verify cartridge is in destination slot
    let slot = library.get_storage_slot(2).unwrap();
    assert!(slot.occupied);
    assert_eq!(slot.barcode.as_ref().unwrap(), "BAR002");
}

#[test]
fn test_load_to_multiple_drives() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 10, 0, 3);

    // Add 3 cartridges
    let slot_id0 = library.add_or_create_tape("BAR001", "primary").unwrap();
    let slot_id1 = library.add_or_create_tape("BAR002", "primary").unwrap();
    let slot_id2 = library.add_or_create_tape("BAR003", "primary").unwrap();

    // Load to 3 different drives
    library.load_to_drive(slot_id0, 0).unwrap();
    library.load_to_drive(slot_id1, 1).unwrap();
    library.load_to_drive(slot_id2, 2).unwrap();

    // Verify all drives have correct cartridges
    let drive0 = library.get_drive(0).unwrap();
    assert_eq!(drive0.barcode.as_ref().unwrap(), "BAR001");

    let drive1 = library.get_drive(1).unwrap();
    assert_eq!(drive1.barcode.as_ref().unwrap(), "BAR002");

    let drive2 = library.get_drive(2).unwrap();
    assert_eq!(drive2.barcode.as_ref().unwrap(), "BAR003");
}

#[test]
fn test_cannot_load_empty_slot() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 0, 1);

    // Try to load from empty slot
    let result = library.load_to_drive(0, 0);
    assert!(result.is_err());
}

#[test]
fn test_cannot_load_to_occupied_drive() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 0, 1);

    // Load cartridge to drive
    let slot_id0 = library.add_or_create_tape("BAR001", "primary").unwrap();
    library.load_to_drive(slot_id0, 0).unwrap();

    // Try to load another cartridge to same drive
    let slot_id1 = library.add_or_create_tape("BAR002", "primary").unwrap();
    let result = library.load_to_drive(slot_id1, 0);
    assert!(result.is_err());
}

#[test]
fn test_export_to_mail_slot() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 3, 1);

    // Add cartridge to storage slot
    let slot_id = library.add_or_create_tape("BAR001", "primary").unwrap();

    // Export to mail slot 0
    library.export_to_mail(slot_id, 0).unwrap();

    // Verify storage slot is empty
    let slot = library.get_storage_slot(slot_id).unwrap();
    assert!(!slot.occupied);

    // Verify mail slot has cartridge
    let mail_slot = library.get_mail_slot(0).unwrap();
    assert!(mail_slot.occupied);
    assert_eq!(mail_slot.barcode.as_ref().unwrap(), "BAR001");
}

#[test]
fn test_import_from_mail_slot() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 2, 1);

    // Add cartridge to storage, then export to mail
    let slot_id = library.add_or_create_tape("BAR001", "primary").unwrap();
    library.export_to_mail(slot_id, 0).unwrap();

    // Import from mail slot to different storage slot
    library.import_from_mail(0, 2).unwrap();

    // Verify mail slot is empty
    let mail_slot = library.get_mail_slot(0).unwrap();
    assert!(!mail_slot.occupied);

    // Verify storage slot has cartridge
    let slot = library.get_storage_slot(2).unwrap();
    assert!(slot.occupied);
    assert_eq!(slot.barcode.as_ref().unwrap(), "BAR001");
}

#[test]
fn test_library_persistence() {
    let dir = create_test_dir();

    let slot_id0: u32;
    let slot_id1: u32;

    // Create library and add cartridges
    {
        let mut library = create_test_library(&dir, 5, 2, 1);
        slot_id0 = library.add_or_create_tape("BAR001", "primary").unwrap();
        slot_id1 = library.add_or_create_tape("BAR002", "primary").unwrap();
        library.load_to_drive(slot_id0, 0).unwrap();
    } // Library dropped, should persist

    // Reload library
    let root = dir.path().join("library");
    let tapes_dir = dir.path().join("tapes");
    let library = core_mediachanger::Library::open(&root, &tapes_dir).unwrap();

    // Verify configuration
    assert_eq!(library.storage_slots().len(), 5);

    // Verify slot 1 still has cartridge
    let slot = library.get_storage_slot(slot_id1).unwrap();
    assert!(slot.occupied);
    assert_eq!(slot.barcode.as_ref().unwrap(), "BAR002");

    // Verify drive 0 still has cartridge
    let drive = library.get_drive(0).unwrap();
    assert!(drive.occupied);
    assert_eq!(drive.barcode.as_ref().unwrap(), "BAR001");
}

#[test]
fn test_remove_cartridge() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 5, 0, 1);

    // Add cartridge
    let slot_id = library.add_or_create_tape("BAR001", "primary").unwrap();

    // Remove it
    library.remove_from_slot(slot_id).unwrap();

    // Verify slot is empty
    let slot = library.get_storage_slot(slot_id).unwrap();
    assert!(!slot.occupied);
    assert!(slot.barcode.is_none());
}

#[test]
fn test_get_all_slots() {
    let dir = create_test_dir();
    let mut library = create_test_library(&dir, 3, 0, 1);

    // Add cartridges to some slots
    let slot_id0 = library.add_or_create_tape("BAR001", "primary").unwrap();
    let slot_id1 = library.add_or_create_tape("BAR002", "primary").unwrap();

    // Get all slots - should have 3 total (0, 1, 2)
    let slot0 = library.get_storage_slot(slot_id0).unwrap();
    assert!(slot0.occupied);

    let slot1 = library.get_storage_slot(slot_id1).unwrap();
    assert!(slot1.occupied);

    // Slot 2 should be empty (only added 2 cartridges)
    let slot2 = library.get_storage_slot(2).unwrap();
    assert!(!slot2.occupied);
}
