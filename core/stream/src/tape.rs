// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Data,
    Filemark,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub kind: BlockKind,
    /// Decoded block payload (empty for filemarks). A plain `Vec<u8>`
    /// rather than `bytes::Bytes` so the READ(6) handler can move it
    /// straight into the SCSI response instead of `Bytes::to_vec()`
    /// re-allocating + memcpying the whole (up to 16 MiB) payload on
    /// every read (issue #184).
    pub data: Vec<u8>,
    pub lba: u64, // logical block address (within active partition)
}

#[derive(Debug, Clone, Copy)]
pub struct Filemark;
