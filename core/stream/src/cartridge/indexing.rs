// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Block-index + chunk-index helpers on [`Cartridge`].
//!
//! Lifted out of `cartridge/mod.rs` so the on-disk write/read state
//! machine in mod.rs isn't tangled up with bookkeeping over
//! `blocks-p<N>.idx` and `chunks.idx`. Behaviour-identical move:
//! method bodies, signatures, ordering preserved. Methods stay
//! `pub(super) fn` so the write/read paths in mod.rs and the storage
//! pipeline in `cartridge/storage.rs` can call them; nothing leaks to
//! the wider crate.
//!
//! Covers:
//! - block-index lookups (`active_block_index`, `next_lba_of`,
//!   `active_next_lba`, `block_at`, `try_block_at`, `block_run_at`,
//!   `block_at_active`)
//! - on-disk encoding (`encode_block_rec`)
//! - chunk-index access (`read_chunk_rec`, `update_chunk_rec`)
//!
//! Stays in mod.rs (deferred to a future `reading.rs` extraction):
//! `maybe_decompress`, `maybe_decrypt`, `open_chunk_for_read`. They
//! straddle the read state machine and aren't pure indexing.

use shared_object_store::compression::CompressionAlgo;

use super::{
    BlockIndex, BlockIndexFile, BlockKind, BlockKindSerde, BlockRec, Cartridge, ChunkRec,
    EncryptionTag, Result, SmcError,
};

impl Cartridge {
    // -- Block-index helper methods --

    /// Reference to the block index file for the active partition.
    pub(super) fn active_block_index(&self) -> &BlockIndexFile {
        &self.block_indexes[self.runtime.active_partition as usize]
    }

    /// next_lba (= record count) for the given partition.
    pub(super) fn next_lba_of(&self, partition: u8) -> u64 {
        self.block_indexes[partition as usize].next_lba()
    }

    /// next_lba for the active partition.
    pub(super) fn active_next_lba(&self) -> u64 {
        self.active_block_index().next_lba()
    }

    /// Read the block-index record for an LBA in the given partition,
    /// returning the in-memory `BlockIndex` shape used by the rest of
    /// the cartridge code.
    pub(super) fn block_at(&self, partition: u8, lba: u64) -> Result<BlockIndex> {
        let rec = self.block_indexes[partition as usize].read(lba)?;
        Ok(BlockIndex::from_rec(lba, &rec))
    }

    /// Like `block_at` but returns `None` if the LBA is past `next_lba`.
    pub(super) fn try_block_at(&self, partition: u8, lba: u64) -> Option<BlockIndex> {
        if lba >= self.next_lba_of(partition) {
            return None;
        }
        self.block_at(partition, lba).ok()
    }

    /// Read a run of `n` block-index records starting at `lba` in one
    /// pread — the SPACE walks' batched lookup (issue #104). The run
    /// must not extend past the partition's `next_lba`; per-record
    /// decode failures stay positional (the inner `Result`) so a walk
    /// only observes corruption it actually reaches.
    pub(super) fn block_run_at(
        &self,
        partition: u8,
        lba: u64,
        n: usize,
    ) -> Result<Vec<Result<BlockRec>>> {
        self.block_indexes[partition as usize].read_run(lba, n)
    }

    /// Read a block-index record from the active partition.
    pub(super) fn block_at_active(&self, lba: u64) -> Result<BlockIndex> {
        self.block_at(self.runtime.active_partition, lba)
    }

    /// Encode a `BlockIndex`-equivalent into a `BlockRec` for append.
    /// Each of `chunk_id` / `offset` / `len` lands in a u32 field on
    /// disk; silently truncating a u64 input would corrupt the index
    /// (a wrap of `offset` produces phantom-aliased reads) so refuse
    /// to encode out-of-range inputs at the boundary. The 4 GiB cap
    /// on `offset` and `len` matches `chunk_index::ChunkRec::size`'s
    /// own u32 width — if either ever needs widening, both files
    /// must move together.
    pub(super) fn encode_block_rec(
        chunk_id: u64,
        offset: u64,
        len: u64,
        kind: BlockKindSerde,
        encrypted: bool,
        compression: Option<CompressionAlgo>,
    ) -> Result<BlockRec> {
        if chunk_id > u32::MAX as u64 {
            return Err(SmcError::InvalidOp(
                "block-index encode: chunk_id exceeds u32::MAX",
            ));
        }
        if offset > u32::MAX as u64 {
            return Err(SmcError::InvalidOp(
                "block-index encode: offset exceeds u32::MAX (chunk > 4 GiB)",
            ));
        }
        if len > u32::MAX as u64 {
            return Err(SmcError::InvalidOp(
                "block-index encode: len exceeds u32::MAX",
            ));
        }
        Ok(BlockRec {
            chunk_id: chunk_id as u32,
            offset: offset as u32,
            len: len as u32,
            kind: match kind {
                BlockKindSerde::Data => BlockKind::Data,
                BlockKindSerde::Filemark => BlockKind::Filemark,
            },
            encryption: if encrypted {
                EncryptionTag::Aes256Gcm
            } else {
                EncryptionTag::None
            },
            compression,
        })
    }

    /// Helper: load the chunk-index record for `chunk_id`, returning
    /// the cached `cur_chunk` for the active staging chunk so callers
    /// see in-flight (unpersisted) size updates.
    pub(super) fn read_chunk_rec(&self, chunk_id: u64) -> Result<ChunkRec> {
        if chunk_id == self.cur_chunk_id {
            return Ok(self.cur_chunk.clone());
        }
        self.chunk_index.read(chunk_id)
    }

    /// `read_chunk_rec` for a chunk id taken from a decoded block
    /// record. The raw read guard reports past-`next_id` as
    /// `InvalidOp` (a caller's bug for code-driven ids); here the id
    /// is data-driven by the on-disk block index, so a miss is
    /// corruption — a bit-flipped `chunk_id` or a truncated
    /// `chunks.idx` — and is reported as `IndexCorrupt` so the host
    /// sees MEDIUM ERROR instead of ILLEGAL REQUEST (issue #105).
    pub(super) fn read_chunk_rec_for_block(&self, chunk_id: u64) -> Result<ChunkRec> {
        self.read_chunk_rec(chunk_id).map_err(|e| match e {
            SmcError::InvalidOp(_) => {
                SmcError::IndexCorrupt("block record references a chunk id past the chunk index")
            }
            other => other,
        })
    }

    /// Helper: write `chunk_id`'s record back to chunk_index, also
    /// keeping the cached `cur_chunk` in sync if `chunk_id` is the
    /// active one.
    pub(super) fn update_chunk_rec(&mut self, chunk_id: u64, rec: &ChunkRec) -> Result<()> {
        if chunk_id == self.cur_chunk_id {
            self.cur_chunk = rec.clone();
        }
        self.chunk_index.overwrite(chunk_id, rec)
    }
}
