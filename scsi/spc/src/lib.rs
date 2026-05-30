// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SPC-4 baseline primitives — the SCSI surface every product
//! type (SSC-4 sequential, SMC-3 medium changer, SBC-3
//! direct-access) shares.
//!
//! This crate carries the request/response shapes (collapsed out
//! of `shared/iscsi/src/handler.rs`), the unified sense-data type
//! and ASC/ASCQ table (lifted from `shared/iscsi/src/sense.rs`
//! and thurvsad's `scsi/types.rs`), the SAM-5 LUN encoder,
//! INQUIRY standard-data layout helpers, VPD page header +
//! descriptor framing, REPORT LUNS framing, MODE PARAMETER
//! HEADER encoders, and the persistent-reservation primitive
//! types (key, scope, type, service action enums).
//!
//! Per-product behavior — the SSC-4 / SMC-3 / SBC-3 dispatch
//! trees, the per-LUN data path, the device-type-specific VPD
//! pages (block limits, logical block provisioning, etc.) —
//! lives in the consuming crate (`core-mediachanger`/`core-stream`/`core-block`
//! after Step 5.B; today's `core-mediachanger` and `core-block`).
//!
//! The plain `shared-iscsi` crate re-exports the request /
//! response / sense surfaces from here so existing call sites
//! `shared_iscsi::sense::*` and `shared_iscsi::ScsiResponse` keep
//! resolving.

#![forbid(unsafe_code)]

pub mod inquiry;
pub mod lun;
pub mod mode;
pub mod naa;
pub mod pr;
pub mod report_luns;
pub mod reservations;
pub mod scsi;
pub mod sense;
pub mod vpd;

// Top-level re-exports of the most-used types so call sites can
// reach for `scsi_spc::SenseData` / `scsi_spc::ScsiResponse`
// without each module path.
pub use scsi::{ScsiRequest, ScsiResponse, ScsiStatus};
pub use sense::{SenseData, SenseDataBuilder, SenseFormat, SenseKey};
