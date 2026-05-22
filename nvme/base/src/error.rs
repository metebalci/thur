// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Crate-local error types. Wire-side status (the SCT / SC pair
//! returned in CQE DW3) lives in [`crate::status`]; this module is
//! for parse / encode errors raised when the *host* fed us a
//! malformed SQE or asked us to emit an oversized field.

use thiserror::Error;

/// Errors raised when decoding an SQE from bytes, encoding a CQE,
/// or building an Identify data structure.
#[derive(Debug, Error)]
pub enum NvmeError {
    /// SQE buffer wasn't exactly 64 bytes. The transport always
    /// hands us a fixed slice; a wrong length is an internal bug.
    #[error("SQE buffer must be 64 bytes, got {0}")]
    SqeLength(usize),

    /// CQE write buffer wasn't exactly 16 bytes.
    #[error("CQE buffer must be 16 bytes, got {0}")]
    CqeLength(usize),

    /// String field that has to fit a fixed ASCII slot (Identify
    /// Controller SN / MN / FR) was oversized. The Identify builders
    /// validate at construction so callers see this once, not at
    /// every `to_bytes` call.
    #[error("ASCII field '{field}' is {got} bytes, max {max}")]
    FieldTooLong {
        field: &'static str,
        got: usize,
        max: usize,
    },

    /// NSID 0 is reserved for broadcast / "no namespace" semantics in
    /// some commands and rejected as a namespace identifier in others.
    /// Surfaced when callers attempt to attach a namespace at NSID 0.
    #[error("NSID 0 is reserved")]
    ReservedNsid,

    /// Connect Data must be exactly 1024 bytes (NVMe-oF §6.3.1.1).
    #[error("Connect Data buffer must be 1024 bytes, got {0}")]
    ConnectDataLength(usize),

    /// SUBNQN / HOSTNQN field contained non-ASCII bytes. NVMe-oF
    /// requires ASCII; we reject rather than substitute a placeholder.
    #[error("ASCII field '{0}' contained non-ASCII bytes")]
    NonAsciiField(&'static str),
}
