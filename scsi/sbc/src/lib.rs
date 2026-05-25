// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SBC-3 SCSI dispatch layer for thurvsa.
//!
//! Lifts the per-opcode handlers (READ / WRITE / COMPARE AND WRITE /
//! UNMAP / WRITE SAME / SYNCHRONIZE CACHE / INQUIRY + VPD / READ
//! CAPACITY / REPORT LUNS / MODE SENSE+SELECT / PERSISTENT
//! RESERVE IN-OUT / MAINTENANCE IN / probes) out of `thurvsad`
//! so the workspace family is symmetric with `scsi-ssc` (drive-LUN)
//! and `scsi-smc` (changer-LUN).
//!
//! Public surface:
//! - [`SbcScsiDispatcher`] — implements `shared_iscsi::ScsiHandler`.
//!   Constructed by `thurvsad` with `Arc<dyn VolumeLookup>`.
//! - [`VolumeLookup`] — trait the daemon's `VolumeRegistry`
//!   implements so the dispatcher can resolve LUN → `PageCache`
//!   without depending on the daemon crate (analogous to
//!   `scsi-ssc::TapeDeviceFacade`).
//!
//! What stays in `thurvsad`: boot wiring (config / discovery /
//! admin socket), `VolumeRegistry` lifecycle (admin create / destroy
//! still mutates the underlying map), iSCSI transport setup. The
//! dispatcher knows nothing about how volumes are discovered or
//! managed; it only resolves LUNs through the trait.

use std::sync::Arc;

use core_block::PageCache;

mod data_path;
mod dispatcher;
mod inquiry;
mod maintenance;
mod mode_sense;
mod odx;
mod probes;
mod reservations;
mod sizing;
mod types;

pub use dispatcher::{ISCSI_DISK_TARGET_IQN, SbcScsiDispatcher};

/// Daemon-side LUN → `PageCache` resolver. `thurvsad`'s
/// `VolumeRegistry` implements this trait; the dispatcher takes
/// `Arc<dyn VolumeLookup>` and never sees the concrete registry.
///
/// Two methods only:
/// - [`Self::get`] for per-opcode LUN resolution.
/// - [`Self::luns`] for REPORT LUNS (opcode 0xA0).
///
/// Mutation (admin-driven `register` / `unregister`) stays on the
/// concrete registry — the dispatcher is read-only over LUNs.
pub trait VolumeLookup: Send + Sync {
    fn get(&self, lun: u64) -> Option<Arc<PageCache>>;
    fn luns(&self) -> Vec<u64>;
}
