// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Re-export shell for the shared SPC-4 SCSI request / response /
//! sense surface. The local definitions moved to `scsi-spc` in
//! Step 5 Milestone 5.A.2 — see `shared/spc/src/sense.rs` and
//! `shared/spc/src/scsi.rs`.
//!
//! Existing call sites `crate::scsi::types::ScsiRequest`,
//! `super::types::SenseData::INVALID_OPCODE`, etc. resolve via the
//! `pub use` re-exports here so the dispatcher arms compile
//! unchanged. Future thurvsa-only sense codes (or wider behaviour
//! divergences from the shared surface) can grow here without
//! reaching into scsi-spc.

pub use scsi_spc::scsi::{ScsiRequest, ScsiResponse};
pub use scsi_spc::sense::SenseData;
