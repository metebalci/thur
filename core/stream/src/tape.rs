// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Data,
    Filemark,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub kind: BlockKind,
    pub data: Bytes, // empty for filemarks
    pub lba: u64,    // logical block address (within active partition)
}

#[derive(Debug, Clone, Copy)]
pub struct Filemark;
