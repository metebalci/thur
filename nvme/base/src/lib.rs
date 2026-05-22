// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe Base Specification primitives.
//!
//! Sibling of `scsi-spc`. Provides the wire-shape types every NVMe
//! command set + every NVMe-oF transport needs:
//!
//! - [`Sqe`] — 64-byte Submission Queue Entry (NVMe Base §4.5).
//! - [`Cqe`] — 16-byte Completion Queue Entry (§4.6) + [`StatusField`].
//! - [`AdminOpcode`] — admin command set opcodes (§5).
//! - [`Fuse`] / [`Psdt`] — sub-fields of SQE CDW0.
//! - [`IdentifyController`] / [`IdentifyNamespace`] — Identify data
//!   structures (§5.17), 4 KiB each, builder + `to_bytes`.
//! - [`identify::CNS`] — Controller-or-Namespace-Structure selectors.
//!
//! What lives one layer up:
//! - NVM Command Set (Read / Write / Flush / Compare / DSM / Write
//!   Zeroes / Verify) — `nvme-nvm`.
//! - NVMe/TCP framing, capsules, ICReq/ICResp, Connect handshake —
//!   `nvme-tcp`.
//!
//! Endianness: NVMe is little-endian on the wire. All `to_bytes` /
//! `parse` helpers here use `to_le_bytes` / `from_le_bytes`.

#![forbid(unsafe_code)]

pub mod cqe;
pub mod error;
pub mod fabrics;
pub mod identify;
pub mod log_page;
pub mod opcode;
pub mod sqe;
pub mod status;

pub use cqe::Cqe;
pub use error::NvmeError;
pub use fabrics::{ConnectData, ControllerRegs, FabricsType};
pub use identify::{IdentifyController, IdentifyNamespace};
pub use opcode::{AdminOpcode, Fuse, Psdt};
pub use sqe::Sqe;
pub use status::{StatusCodeType, StatusField};

/// SQE wire size in bytes (NVMe Base §4.5).
pub const SQE_SIZE: usize = 64;

/// CQE wire size in bytes (NVMe Base §4.6).
pub const CQE_SIZE: usize = 16;

/// Identify Controller / Identify Namespace data structure size
/// (NVMe Base §5.17). Both are 4 KiB.
pub const IDENTIFY_DATA_SIZE: usize = 4096;
