// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-drive opaque page storage.
//!
//! Drives can carry a small collection of `(code, subcode, body)` byte
//! blobs that survive a mount cycle. The storage layer treats these as
//! opaque: it persists them with `drive_state.json` and serves them back
//! verbatim on read. The consumer (scsi-ssc) gives them meaning — its
//! SCSI MODE SENSE / MODE SELECT round-trip writes here under PC=Saved
//! and reads back here on subsequent MODE SENSE calls.
//!
//! Default values come from the consumer (no entry for that code in
//! `DrivePageStore`); a saved entry overrides the default. SP=1
//! persistence is the consumer's responsibility — once the consumer
//! calls `set`, the storage layer persists the entry to the manifest
//! on the next drive_state flush.

use serde::{Deserialize, Serialize};

/// One opaque drive page the consumer has saved, replayed verbatim on
/// read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedDrivePage {
    pub code: u8,
    pub subcode: u8,
    pub body: Vec<u8>,
}

/// Per-drive collection of saved opaque page blobs.
///
/// Empty by default — the consumer's own builders emit defaults when
/// no entry exists. As entries are set, MODE SENSE PC=Current / PC=Saved
/// (the consumer's responsibility) consults the store first and falls
/// back to defaults only when no entry exists.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DrivePageStore {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pages: Vec<SavedDrivePage>,
}

impl DrivePageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a saved body. Returns `None` if no entry exists for this
    /// `(code, subcode)` pair (consumer should use its default).
    pub fn get(&self, code: u8, subcode: u8) -> Option<&[u8]> {
        self.pages
            .iter()
            .find(|p| p.code == code && p.subcode == subcode)
            .map(|p| p.body.as_slice())
    }

    /// Insert or replace a saved body.
    pub fn set(&mut self, code: u8, subcode: u8, body: Vec<u8>) {
        if let Some(existing) = self
            .pages
            .iter_mut()
            .find(|p| p.code == code && p.subcode == subcode)
        {
            existing.body = body;
        } else {
            self.pages.push(SavedDrivePage {
                code,
                subcode,
                body,
            });
        }
    }

    /// True iff at least one entry is saved. The SCSI consumer uses
    /// this to set the PS (Parameters Saveable) bit on MODE SENSE
    /// response page headers.
    pub fn has_any_saved(&self) -> bool {
        !self.pages.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}
