// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Inventory-mutation surface on [`Library`].
//!
//! Lifted out of `library/mod.rs` (the parent was a ~2300-line file
//! with a 1000-line impl Library). Holds the ten public verbs the
//! SMC dispatch handlers and admin socket call to move cartridges
//! around: slot ↔ drive, slot ↔ slot, slot ↔ mail. Each verb mutates
//! the in-memory inventory under the per-Library lock and calls
//! `persist()` before returning. No behaviour change from the move.

use crate::cartridge::{Cartridge, CartridgeOpenMode};
use crate::errors::{Result, SmcError};

use super::{Library, LoadedCartridge};

impl Library {
    /// Add an existing or new cartridge to the first free cartridge slot.
    /// If the tape directory doesn't exist yet, it will be created as an
    /// empty cartridge bound to the named storage backend. Pass
    /// `backend_name` matching an entry in `storage.backends`.
    pub fn add_or_create_tape(&mut self, barcode: &str, backend_name: &str) -> Result<u32> {
        // ensure tape dir exists (create empty cartridge if needed)
        let tape_root = self.tapes_dir.join(barcode);
        if !tape_root.exists() {
            let _ = Cartridge::open(
                &self.tapes_dir,
                barcode,
                CartridgeOpenMode::Create {
                    backend: backend_name.to_string(),
                    // Library-level auto-create is for tests / import
                    // path scaffolding — never WORM. WORM cartridges
                    // are created explicitly via the CLI's
                    // `cartridge create --worm` flow.
                    worm: false,
                    // Library-level auto-create is test scaffolding;
                    // pick `Local` so chunks land under the cartridge's
                    // own namespace and don't pollute a shared pool the
                    // operator may be using elsewhere.
                    dedup: crate::cartridge::DedupScope::Local,
                },
            )?;
        }

        // find empty slot and fill it, but end the mutable borrow before persist()
        let slot_id = {
            let slot = self
                .inventory
                .storage_slots
                .iter_mut()
                .find(|s| !s.occupied)
                .ok_or(SmcError::InvalidOp("no empty cartridge slots"))?;
            slot.barcode = Some(barcode.to_string());
            slot.occupied = true;
            slot.id
        };

        self.persist()?;
        Ok(slot_id)
    }

    /// Remove tape from a cartridge slot (logical removal), does not delete data.
    pub fn remove_from_slot(&mut self, slot_id: u32) -> Result<()> {
        {
            let s = self.storage_slot_mut(slot_id)?;
            s.barcode = None;
            s.occupied = false;
        }
        self.persist()
    }

    /// Load a cartridge from a storage slot into a drive.
    /// The cartridge is moved from the storage slot to the drive slot.
    pub fn load_to_drive(&mut self, storage_slot_id: u32, drive_id: u32) -> Result<()> {
        // Read the source barcode WITHOUT mutating, then validate the
        // destination (incl. the drive-id lookup) before touching either
        // element. A destination-full / bad-drive-id error must leave the
        // cartridge in its source slot — clearing the source first dropped
        // the barcode on the floor and made the cartridge vanish from
        // inventory (issue #120).
        let barcode = {
            let s = self.storage_slot_mut(storage_slot_id)?;
            if !s.occupied {
                return Err(SmcError::InvalidOp("storage slot empty"));
            }
            s.barcode
                .clone()
                .ok_or(SmcError::InvalidOp("slot occupied but barcode missing"))?
        };
        {
            let d = self.drive_slot_mut(drive_id)?;
            if d.occupied {
                return Err(SmcError::InvalidOp("drive already has cartridge"));
            }
        }

        // Both endpoints validated — now mutate.
        {
            let s = self.storage_slot_mut(storage_slot_id)?;
            s.occupied = false;
            s.barcode = None;
        }
        {
            let d = self.drive_slot_mut(drive_id)?;
            d.occupied = true;
            d.barcode = Some(barcode);
            d.home_slot = Some(storage_slot_id as u16);
        }

        self.persist()
    }

    /// Unload a cartridge from a drive back to a storage slot.
    pub fn unload_from_drive(&mut self, drive_id: u32, storage_slot_id: u32) -> Result<()> {
        // Validate both endpoints before mutating either (issue #120).
        let barcode = {
            let d = self.drive_slot_mut(drive_id)?;
            if !d.occupied {
                return Err(SmcError::InvalidOp("drive has no cartridge"));
            }
            d.barcode
                .clone()
                .ok_or(SmcError::InvalidOp("drive occupied but barcode missing"))?
        };
        {
            let s = self.storage_slot_mut(storage_slot_id)?;
            if s.occupied {
                return Err(SmcError::InvalidOp("storage slot occupied"));
            }
        }

        {
            let d = self.drive_slot_mut(drive_id)?;
            d.occupied = false;
            d.barcode = None;
            d.home_slot = None;
        }
        {
            let s = self.storage_slot_mut(storage_slot_id)?;
            s.occupied = true;
            s.barcode = Some(barcode);
        }

        self.persist()
    }

    /// Move a cartridge from one storage slot to another (changes home slot).
    pub fn move_cartridge(&mut self, from_slot_id: u32, to_slot_id: u32) -> Result<()> {
        // Validate inputs
        if from_slot_id == to_slot_id {
            return Err(SmcError::InvalidOp(
                "source and destination slots must be different",
            ));
        }

        // Validate both endpoints before mutating either (issue #120).
        let barcode = {
            let s = self.storage_slot_mut(from_slot_id)?;
            if !s.occupied {
                return Err(SmcError::InvalidOp("source slot empty"));
            }
            s.barcode
                .clone()
                .ok_or(SmcError::InvalidOp("slot occupied but barcode missing"))?
        };
        {
            let s = self.storage_slot_mut(to_slot_id)?;
            if s.occupied {
                return Err(SmcError::InvalidOp("destination slot occupied"));
            }
        }

        {
            let s = self.storage_slot_mut(from_slot_id)?;
            s.occupied = false;
            s.barcode = None;
        }
        {
            let s = self.storage_slot_mut(to_slot_id)?;
            s.occupied = true;
            s.barcode = Some(barcode);
        }

        self.persist()
    }

    /// Export a cartridge from a storage slot to a mail slot.
    pub fn export_to_mail(&mut self, storage_slot_id: u32, mail_slot_id: u32) -> Result<()> {
        // Validate both endpoints before mutating either (issue #120).
        let barcode = {
            let s = self.storage_slot_mut(storage_slot_id)?;
            if !s.occupied {
                return Err(SmcError::InvalidOp("storage slot empty"));
            }
            s.barcode
                .clone()
                .ok_or(SmcError::InvalidOp("slot occupied but barcode missing"))?
        };
        {
            let m = self.mail_slot_mut(mail_slot_id)?;
            if m.occupied {
                return Err(SmcError::InvalidOp("mail slot occupied"));
            }
        }

        {
            let s = self.storage_slot_mut(storage_slot_id)?;
            s.occupied = false;
            s.barcode = None;
        }
        {
            let m = self.mail_slot_mut(mail_slot_id)?;
            m.occupied = true;
            m.barcode = Some(barcode);
        }

        self.persist()
    }

    /// Import a cartridge from a mail slot to a storage slot.
    pub fn import_from_mail(&mut self, mail_slot_id: u32, storage_slot_id: u32) -> Result<()> {
        // Validate both endpoints before mutating either (issue #120).
        let barcode = {
            let m = self.mail_slot_mut(mail_slot_id)?;
            if !m.occupied {
                return Err(SmcError::InvalidOp("mail slot empty"));
            }
            m.barcode.clone().ok_or(SmcError::InvalidOp(
                "mail slot occupied but barcode missing",
            ))?
        };
        {
            let s = self.storage_slot_mut(storage_slot_id)?;
            if s.occupied {
                return Err(SmcError::InvalidOp("storage slot occupied"));
            }
        }

        {
            let m = self.mail_slot_mut(mail_slot_id)?;
            m.occupied = false;
            m.barcode = None;
        }
        {
            let s = self.storage_slot_mut(storage_slot_id)?;
            s.occupied = true;
            s.barcode = Some(barcode);
        }

        self.persist()
    }

    /// Load a cartridge from a slot (backward compatibility - for old API).
    /// Returns a LoadedCartridge; the slot becomes empty until you `unload()` it back.
    pub fn load(&mut self, slot_id: u32) -> Result<LoadedCartridge> {
        let barcode = {
            let s = self.storage_slot_mut(slot_id)?;
            if !s.occupied {
                return Err(SmcError::InvalidOp("slot empty"));
            }

            s.barcode
                .clone()
                .ok_or(SmcError::InvalidOp("slot occupied but barcode missing"))?
        };

        let cart = Cartridge::open(&self.tapes_dir, &barcode, CartridgeOpenMode::Open)?;

        {
            let s = self.storage_slot_mut(slot_id)?;
            s.occupied = false;
            s.barcode = None;
        }
        self.persist()?;

        Ok(LoadedCartridge {
            slot_id,
            barcode,
            cartridge: cart,
        })
    }

    /// Unload the cartridge back into its original slot (backward compatibility).
    pub fn unload(&mut self, loaded: LoadedCartridge) -> Result<()> {
        {
            let s = self.storage_slot_mut(loaded.slot_id)?;
            if s.occupied {
                return Err(SmcError::InvalidOp("slot already occupied"));
            }
            s.occupied = true;
            s.barcode = Some(loaded.barcode);
        }
        self.persist()
    }
}
