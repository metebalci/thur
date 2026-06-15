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

/// A storage or import/export element addressed by its in-type id, used
/// by [`Library::exchange_medium`] so the SMC EXCHANGE MEDIUM handler can
/// express a three-element swap without knowing the inventory internals.
/// Data-transfer (drive) elements are intentionally excluded — the SMC
/// dispatcher refuses drive-involving exchanges (issue #133).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeSlot {
    Storage(u32),
    Mail(u32),
}

impl Library {
    /// Add an existing or new cartridge to the first free cartridge slot.
    /// If the tape directory doesn't exist yet, it will be created as an
    /// empty cartridge bound to the named storage backend. Pass
    /// `backend_name` matching an entry in `storage.backends`.
    pub fn add_or_create_tape(&mut self, barcode: &str, backend_name: &str) -> Result<u32> {
        let slot_id = self.seat_one_tape(barcode, backend_name)?;
        self.persist()?;
        Ok(slot_id)
    }

    /// Seat (creating the cartridge dir if missing) every barcode into
    /// the first free storage slot, persisting the inventory ONCE after
    /// the whole batch instead of once per barcode. DR restore seats up
    /// to thousands of cartridges; the per-barcode persist re-serialized
    /// and rewrote the full (multi-MB at the 65535-slot scale)
    /// inventory.json on every call — O(N x slots) writes for what needs
    /// one (issue #286). Returns the seated slot ids in order.
    pub fn add_or_create_tapes(
        &mut self,
        barcodes: &[String],
        backend_name: &str,
    ) -> Result<Vec<u32>> {
        let mut slot_ids = Vec::with_capacity(barcodes.len());
        for barcode in barcodes {
            slot_ids.push(self.seat_one_tape(barcode, backend_name)?);
        }
        self.persist()?;
        Ok(slot_ids)
    }

    /// Create the cartridge dir if missing and fill the first free
    /// storage slot — the in-memory half of [`Self::add_or_create_tape`],
    /// without persisting, so callers can batch the durable write.
    fn seat_one_tape(&mut self, barcode: &str, backend_name: &str) -> Result<u32> {
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

        // find empty slot and fill it
        let slot = self
            .inventory
            .storage_slots
            .iter_mut()
            .find(|s| !s.occupied)
            .ok_or(SmcError::InvalidOp("no empty cartridge slots"))?;
        slot.barcode = Some(barcode.to_string());
        slot.occupied = true;
        Ok(slot.id)
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

    /// Read the barcode of an exchange element, requiring it to be
    /// occupied. Helper for [`Self::exchange_medium`].
    fn exchange_slot_barcode(&mut self, slot: ExchangeSlot) -> Result<String> {
        let (occupied, barcode) = match slot {
            ExchangeSlot::Storage(id) => {
                let s = self.storage_slot_mut(id)?;
                (s.occupied, s.barcode.clone())
            }
            ExchangeSlot::Mail(id) => {
                let m = self.mail_slot_mut(id)?;
                (m.occupied, m.barcode.clone())
            }
        };
        if !occupied {
            return Err(SmcError::InvalidOp("exchange element empty"));
        }
        barcode.ok_or(SmcError::InvalidOp(
            "exchange element occupied but barcode missing",
        ))
    }

    /// Whether an exchange element currently holds a cartridge. Helper
    /// for [`Self::exchange_medium`].
    fn exchange_slot_occupied(&mut self, slot: ExchangeSlot) -> Result<bool> {
        Ok(match slot {
            ExchangeSlot::Storage(id) => self.storage_slot_mut(id)?.occupied,
            ExchangeSlot::Mail(id) => self.mail_slot_mut(id)?.occupied,
        })
    }

    /// Set (or clear, with `None`) the cartridge in an exchange element.
    /// Helper for [`Self::exchange_medium`].
    fn exchange_slot_assign(&mut self, slot: ExchangeSlot, barcode: Option<String>) -> Result<()> {
        match slot {
            ExchangeSlot::Storage(id) => {
                let s = self.storage_slot_mut(id)?;
                s.occupied = barcode.is_some();
                s.barcode = barcode;
            }
            ExchangeSlot::Mail(id) => {
                let m = self.mail_slot_mut(id)?;
                m.occupied = barcode.is_some();
                m.barcode = barcode;
            }
        }
        Ok(())
    }

    /// SMC EXCHANGE MEDIUM as a single atomic inventory transaction: the
    /// medium at `src` moves to `dst1`, and the medium that was at `dst1`
    /// moves to `dst2`. Performed as a read-then-write over the in-memory
    /// inventory with a single `persist()`, so a refusal leaves the
    /// inventory untouched and there is no half-applied durable state
    /// (issue #191).
    ///
    /// `dst2 == src` is the canonical two-element swap (`mtx exchange A B`
    /// issues src=A, dst1=B, dst2=A): the source slot is vacated by this
    /// same exchange, so reusing it as the second destination is valid.
    pub fn exchange_medium(
        &mut self,
        src: ExchangeSlot,
        dst1: ExchangeSlot,
        dst2: ExchangeSlot,
    ) -> Result<()> {
        if src == dst1 {
            return Err(SmcError::InvalidOp(
                "exchange source and first destination must differ",
            ));
        }
        if dst1 == dst2 {
            return Err(SmcError::InvalidOp(
                "exchange first and second destination must differ",
            ));
        }

        // Snapshot the two media that move (both must be present).
        let src_barcode = self.exchange_slot_barcode(src)?;
        let dst1_barcode = self.exchange_slot_barcode(dst1)?;

        // The second destination must be free unless it is the source
        // slot itself, which this exchange vacates.
        if dst2 != src && self.exchange_slot_occupied(dst2)? {
            return Err(SmcError::InvalidOp("exchange second destination occupied"));
        }

        // Apply from the snapshot. The write targets are distinct
        // (src != dst1, dst1 != dst2, and the only permitted dst2
        // collision is dst2 == src), so write order is immaterial.
        if dst2 != src {
            self.exchange_slot_assign(src, None)?;
        }
        self.exchange_slot_assign(dst1, Some(src_barcode))?;
        self.exchange_slot_assign(dst2, Some(dst1_barcode))?;

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

#[cfg(test)]
mod exchange_tests {
    use super::*;
    use tempfile::TempDir;

    fn lib_with_slots(slots: u32) -> (TempDir, Library) {
        let temp_dir = TempDir::new().unwrap();
        let library = Library::initialize(
            &temp_dir.path().join("library"),
            &temp_dir.path().join("tapes"),
            slots,
            0,
            2,
            8,
            None,
            0,
            1001,
            101,
            1,
        )
        .unwrap();
        (temp_dir, library)
    }

    fn barcode_at(lib: &Library, id: u32) -> Option<String> {
        lib.storage_slots()[id as usize].barcode.clone()
    }

    #[test]
    fn exchange_canonical_swap_dst2_equals_src() {
        // mtx exchange A B issues src=A, dst1=B, dst2=A — the canonical
        // two-element swap that the prior two-move composition refused.
        let (_t, mut lib) = lib_with_slots(8);
        lib.add_or_create_tape("TAPE001", "primary").unwrap(); // slot 0
        lib.add_or_create_tape("TAPE002", "primary").unwrap(); // slot 1

        lib.exchange_medium(
            ExchangeSlot::Storage(0),
            ExchangeSlot::Storage(1),
            ExchangeSlot::Storage(0),
        )
        .unwrap();

        assert_eq!(barcode_at(&lib, 0).as_deref(), Some("TAPE002"));
        assert_eq!(barcode_at(&lib, 1).as_deref(), Some("TAPE001"));
    }

    /// Issue #286: the batch seat places each barcode in the first free
    /// slot (one persist for the whole batch) and survives a reload.
    #[test]
    fn add_or_create_tapes_batch_seats_all_in_order() {
        let temp_dir = TempDir::new().unwrap();
        let lib_root = temp_dir.path().join("library");
        let tapes = temp_dir.path().join("tapes");
        let mut lib =
            Library::initialize(&lib_root, &tapes, 8, 0, 2, 8, None, 0, 1001, 101, 1).unwrap();

        let barcodes = vec![
            "TAPE001".to_string(),
            "TAPE002".to_string(),
            "TAPE003".to_string(),
        ];
        let slots = lib.add_or_create_tapes(&barcodes, "primary").unwrap();
        assert_eq!(slots.len(), 3);
        assert_eq!(barcode_at(&lib, 0).as_deref(), Some("TAPE001"));
        assert_eq!(barcode_at(&lib, 1).as_deref(), Some("TAPE002"));
        assert_eq!(barcode_at(&lib, 2).as_deref(), Some("TAPE003"));

        // Persisted: a fresh open sees the same seating.
        let reopened = Library::open(&lib_root, &tapes).unwrap();
        assert_eq!(
            reopened.storage_slots()[2].barcode.as_deref(),
            Some("TAPE003")
        );
    }

    #[test]
    fn exchange_three_distinct_elements() {
        // src -> dst1, dst1's medium -> empty dst2, src left empty.
        let (_t, mut lib) = lib_with_slots(8);
        lib.add_or_create_tape("TAPE001", "primary").unwrap(); // slot 0
        lib.add_or_create_tape("TAPE002", "primary").unwrap(); // slot 1

        lib.exchange_medium(
            ExchangeSlot::Storage(0),
            ExchangeSlot::Storage(1),
            ExchangeSlot::Storage(2),
        )
        .unwrap();

        assert!(!lib.storage_slots()[0].occupied, "source must be empty");
        assert_eq!(barcode_at(&lib, 1).as_deref(), Some("TAPE001"));
        assert_eq!(barcode_at(&lib, 2).as_deref(), Some("TAPE002"));
    }

    #[test]
    fn exchange_refuses_occupied_second_destination_without_state_change() {
        let (_t, mut lib) = lib_with_slots(8);
        lib.add_or_create_tape("TAPE001", "primary").unwrap(); // slot 0
        lib.add_or_create_tape("TAPE002", "primary").unwrap(); // slot 1
        lib.add_or_create_tape("TAPE003", "primary").unwrap(); // slot 2

        let err = lib
            .exchange_medium(
                ExchangeSlot::Storage(0),
                ExchangeSlot::Storage(1),
                ExchangeSlot::Storage(2),
            )
            .unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
        // No half-applied state: all three slots keep their tapes.
        assert_eq!(barcode_at(&lib, 0).as_deref(), Some("TAPE001"));
        assert_eq!(barcode_at(&lib, 1).as_deref(), Some("TAPE002"));
        assert_eq!(barcode_at(&lib, 2).as_deref(), Some("TAPE003"));
    }

    #[test]
    fn exchange_refuses_empty_source() {
        let (_t, mut lib) = lib_with_slots(8);
        lib.add_or_create_tape("TAPE002", "primary").unwrap(); // slot 0
        // src = slot 1 (empty), dst1 = slot 0.
        let err = lib
            .exchange_medium(
                ExchangeSlot::Storage(1),
                ExchangeSlot::Storage(0),
                ExchangeSlot::Storage(1),
            )
            .unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
        assert_eq!(barcode_at(&lib, 0).as_deref(), Some("TAPE002"));
    }
}
