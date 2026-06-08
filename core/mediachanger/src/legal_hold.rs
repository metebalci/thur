// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Library-side legal-hold helpers.
//!
//! The cartridge sentinel logic + storage-key collection lives in
//! `core_stream::legal_hold` (Step 5 Milestone 5.B.1). What stays here
//! is the inventory-aware "is this cartridge currently mounted in a
//! drive?" lookup — it reads `LibraryInventory`, which is smc-side
//! state.

use crate::errors::{Result, SmcError};
use crate::library::LibraryInventory;
use std::path::Path;

/// Look up which drive (if any) currently has the cartridge loaded
/// according to `<data_dir>/library/inventory.json`. Returns
/// `Ok(Some(drive_id))` if the cartridge is in a drive, `Ok(None)` if
/// not. `Err(...)` only on file IO / parse failures.
///
/// Used by `legal-hold set` / `clear` to refuse changing hold state
/// while a cartridge is mounted: the held flag is read once at drive
/// load and pinned for the load's lifetime, so an in-flight load must
/// see a stable answer. Operator must `unload` first.
pub fn find_drive_for_loaded_cartridge(data_dir: &Path, barcode: &str) -> Result<Option<u32>> {
    let inv_path = data_dir.join("library").join("inventory.json");
    let raw = std::fs::read_to_string(&inv_path).map_err(SmcError::Io)?;
    let inv: LibraryInventory = serde_json::from_str(&raw).map_err(SmcError::SerdeJson)?;
    Ok(inv
        .drives
        .iter()
        .find(|d| d.occupied && d.barcode.as_deref() == Some(barcode))
        .map(|d| d.id))
}
