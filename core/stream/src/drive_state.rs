// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-drive persistent state — emulated drive NVRAM. Local-only,
//! never uploaded.
//!
//! On real LTO hardware, MODE SELECT with SP=1 saves to the drive's
//! own NVRAM, not to the tape. The values persist across cartridge
//! swaps: UNLOAD a tape, LOAD another, the drive still reports the
//! same MRIE / DRA values it was last set to. Thur VTL emulates that
//! with a single library-wide JSON file holding one [`DriveState`]
//! per drive id.
//!
//! Sits at `<data_dir>/library/drive_state.json`. Loaded by
//! `DriveManager` at daemon startup; written atomically (tmp +
//! rename) every time a host issues MODE SELECT with SP=1.
//! Deliberately *not* part of any cartridge's `manifest.json` — the
//! manifest rides through the cloud-backup pipeline and may live on
//! a retention-locked / object-locked backend, whereas drive-side
//! configuration must remain freely re-writable. Same local-only
//! treatment as `lru.idx`.
//!
//! **Why an envelope.** Future drive-side state additions (e.g. host-
//! set non-MAM attributes, persisted DRA values) extend [`DriveState`]
//! with a new field rather than introducing yet another sidecar file.
//! Every field uses `#[serde(default, skip_serializing_if = ...)]` so
//! adding new fields is a non-breaking change for old files.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::mode_state::DrivePageStore;

/// Per-drive runtime state. One per drive id, persisted as a value in
/// [`LibraryDriveState::drives`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveState {
    /// Opaque per-drive page blobs. The SCSI consumer (scsi-ssc) uses
    /// these to round-trip MODE SELECT SP=1 bodies. See
    /// [`DrivePageStore`] and `docs/SPEC.md` § "MODE SELECT round-trip".
    #[serde(default, skip_serializing_if = "DrivePageStore::is_empty")]
    pub mode_pages: DrivePageStore,
}

impl DriveState {
    pub fn new() -> Self {
        Self::default()
    }

    /// True iff every field is at its default — used to skip writes
    /// when the drive has no host-set state to persist.
    pub fn is_empty(&self) -> bool {
        self.mode_pages.is_empty()
    }
}

/// Library-wide envelope: one [`DriveState`] per drive id. Persisted
/// as `<data_dir>/library/drive_state.json` by `DriveManager`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LibraryDriveState {
    /// Map of `drive_id` → state. Drives without an entry use the
    /// default (empty) state — same as the "host has never issued
    /// MODE SELECT SP=1 on this drive" case.
    #[serde(default)]
    pub drives: BTreeMap<usize, DriveState>,
}

impl LibraryDriveState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.drives.values().all(|d| d.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_drive_state_is_empty() {
        assert!(DriveState::new().is_empty());
    }

    #[test]
    fn a_saved_mode_page_makes_the_drive_state_non_empty() {
        let mut ds = DriveState::new();
        ds.mode_pages.set(0x0F, 0x00, vec![1]);
        assert!(!ds.is_empty());
    }

    #[test]
    fn library_state_is_empty_until_a_drive_saves_a_page() {
        let mut lib = LibraryDriveState::new();
        assert!(lib.is_empty());
        // A drive entry whose state is still default keeps it "empty".
        lib.drives.insert(0, DriveState::new());
        assert!(lib.is_empty());
        // Saving a page on that drive flips it.
        lib.drives
            .get_mut(&0)
            .expect("drive 0")
            .mode_pages
            .set(0x10, 0x01, vec![0xAB]);
        assert!(!lib.is_empty());
    }

    #[test]
    fn library_state_survives_a_serde_round_trip() {
        let mut lib = LibraryDriveState::new();
        let mut ds = DriveState::new();
        ds.mode_pages.set(0x0A, 0xF0, vec![7, 7]);
        lib.drives.insert(2, ds);
        let json = serde_json::to_string(&lib).expect("serialize");
        let back: LibraryDriveState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, lib);
    }
}
