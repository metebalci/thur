// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVM Command Set dispatch layer for thurvsa.
//!
//! Siblings: `scsi-sbc` (the SCSI counterpart over iSCSI), `nvme-base`
//! (SQE / CQE / status / Identify primitives this crate consumes),
//! `nvme-tcp` (the NVMe-over-Fabrics TCP transport that frames our
//! command set on the wire).
//!
//! Public surface:
//! - [`NvmeNvmDispatcher`] — implements [`NvmeCommandHandler`].
//!   Constructed by `thurvsad` with `Arc<dyn NamespaceLookup>`.
//! - [`NamespaceLookup`] — trait the daemon's `VolumeRegistry`
//!   implements so the dispatcher can resolve NSID → `PageCache`
//!   without depending on the daemon crate (analogous to
//!   `scsi_sbc::VolumeLookup`).
//! - [`NvmeCommandHandler`] — what `nvme-tcp`'s server loop calls
//!   into. Two methods: `handle_admin` (Identify / Get Features /
//!   ...) and `handle_io` (Read / Write / Flush / ...). The transport
//!   parses the SQE + collects data payloads then hands a typed
//!   request in; we hand back a `Cqe` plus any data-in payload.
//!
//! What stays in `thurvsad`: boot wiring, `VolumeRegistry`
//! lifecycle, transport setup. The dispatcher knows nothing about
//! how volumes are discovered or managed; it only resolves NSIDs
//! through the trait.
//!
//! **NSID convention.** Per NVMe Base §6, NSID 0 is reserved and
//! `0xFFFFFFFF` is the broadcast NSID. VSA maps `nsid = lun + 1`
//! one-to-one with the SCSI LUN space (LUN 0 → NSID 1). The mapping
//! is implementation detail of the daemon's `VolumeRegistry`'s
//! `NamespaceLookup` impl — this crate just sees opaque NSIDs.

#![forbid(unsafe_code)]

use std::sync::Arc;

use core_block::PageCache;

pub mod dispatcher;
pub mod handler;
pub mod opcode;
pub mod reservations;

pub use dispatcher::NvmeNvmDispatcher;
pub use handler::{AdminCommand, IoCommand, NvmeCommandHandler, NvmeResponse};
pub use opcode::NvmOpcode;

/// Daemon-side NSID → `PageCache` resolver. `thurvsad`'s
/// `VolumeRegistry` implements this trait; the dispatcher takes
/// `Arc<dyn NamespaceLookup>` and never sees the concrete registry.
///
/// Mirrors `scsi_sbc::VolumeLookup` byte-for-byte at the shape
/// level — only the identifier renaming (`lun` → `nsid`) differs.
///
/// Four methods:
/// - [`Self::get`] for per-command NSID resolution.
/// - [`Self::active_namespaces`] for the Identify CNS=0x02 list.
/// - [`Self::name_for_nsid`] for admission filtering — given an
///   NSID, the volume name the dispatcher compares against the
///   connection's admission set.
/// - [`Self::active_namespaces_filtered`] returns the NSID list
///   after applying an optional admission set. `None` = no fence
///   (same as `active_namespaces()`).
pub trait NamespaceLookup: Send + Sync {
    fn get(&self, nsid: u32) -> Option<Arc<PageCache>>;
    fn active_namespaces(&self) -> Vec<u32>;
    fn name_for_nsid(&self, nsid: u32) -> Option<String>;
    fn active_namespaces_filtered(&self, allow: Option<&[String]>) -> Vec<u32>;
}
