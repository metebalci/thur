// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SMC-3 medium-changer SCSI dispatch layer.
//!
//! Lifts changer-LUN opcode handlers out of `thurvtld` so the
//! workspace family is symmetric with `scsi-ssc` (drive-LUN) and the
//! upcoming `scsi-sbc` (block). Direct dep on `core-mediachanger`: SMC handlers
//! are inherently library-coupled — a trait wrapper buys nothing.
//!
//! Public surface:
//! - [`dispatch::dispatch_changer_lun`] — entry router for the six
//!   SMC opcodes (INITIALIZE / READ ELEMENT STATUS, MOVE / EXCHANGE
//!   MEDIUM, SEND VOLUME TAG, INITIALIZE WITH RANGE).
//! - [`dispatch::SmcScsiCtx`] — per-command context wrapping
//!   `scsi_ssc::dispatch::ScsiCtx` with `library` and
//!   `element_config` borrows the changer handlers need.
//! - [`changer`] — element-address topology (`ElementType`,
//!   `ElementAddressConfig`) plus low-level byte-construction
//!   helpers for the READ ELEMENT STATUS / MOVE MEDIUM bodies.
//!
//! Stays in `thurvtld`: changer-LUN INQUIRY / LOG SENSE /
//! MODE SENSE / MODE SELECT variants. They also consume
//! `SmcScsiCtx` (imported from this crate) but live in the daemon
//! for now — not minimum viable for the Pass 2 lift.

pub mod changer;
pub mod dispatch;

pub use dispatch::SmcScsiCtx;
