// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Shared SSC-4 / LTO tape iSCSI dispatch layer.
//!
//! This crate hosts the drive-side primitives `thurvtld`
//! reaches for:
//!
//! - [`drive_manager`] — per-drive state (loaded cartridge,
//!   reservations, PREVENT/ALLOW, compression knobs, session locks).
//!   Lifted from `thurvtld` in 5.B.6 follow-up; the
//!   manager is intentionally library-agnostic — it tracks drives by
//!   numeric id and never reaches into [`core_mediachanger::Library`].
//! - [`scsi`] — SSC / SPC helper modules (sense, mode pages, log
//!   pages, MAM attributes, tape data-encryption pages). Same code
//!   that used to live under `vtl/daemon/src/iscsi/scsi/`
//!   minus `changer.rs` (medium-changer SMC ops stay library-only).
//! - [`dispatch`] — drive-LUN SCSI types ([`Pdu`](dispatch::Pdu) /
//!   [`ScsiCtx`](dispatch::ScsiCtx) / [`ScsiResp`](dispatch::ScsiResp))
//!   plus shared audit helpers and every drive-LUN per-opcode handler
//!   that doesn't touch the SMC `Library` lock or
//!   `DiagnosticStore` — TUR / REQUEST SENSE / READ BLOCK LIMITS /
//!   REPORT DENSITY / REWIND / READ POSITION / LOAD UNLOAD / SPACE /
//!   FILEMARKS / LOCATE / ERASE / SET CAPACITY / READ-WRITE 6 /
//!   VERIFY / PREVENT-ALLOW / ALLOW OVERWRITE / FORMAT MEDIUM /
//!   READ-WRITE ATTRIBUTE / READ-WRITE BUFFER / RESERVE-RELEASE,
//!   facade-backed INQUIRY (general drive-LUN VPDs) / REPORT LUNS /
//!   LOG SENSE, plus SECURITY PROTOCOL IN/OUT, MAINTENANCE IN/OUT,
//!   and PR IN/OUT. Drive-LUN handlers that still need the
//!   `Library` lock (MODE SENSE / MODE SELECT drive pages, INQUIRY
//!   VPD `0xB4`) or the per-LUN `DiagnosticStore`
//!   (SEND/RECEIVE DIAGNOSTIC, LOG SELECT, changer-LUN LOG SENSE)
//!   stay in `thurvtld` along with the SMC changer ops
//!   (INITIALIZE/READ ELEMENT STATUS, MOVE/EXCHANGE MEDIUM, SEND
//!   VOLUME TAG).

pub mod diagnostics;
pub mod dispatch;
pub mod drive_manager;
pub mod scsi;
