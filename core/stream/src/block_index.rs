// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-partition block index file for a cartridge.
//!
//! Replaces the inline `Vec<BlockIndex>` that used to live in the
//! cartridge manifest. The index is now an external file of fixed-size
//! 16-byte records, written eagerly on every block write.
//! `pwrite_at(lba * 16)` for both writes and reads. This bounds the
//! manifest at chunk-level state and makes block lookup a single
//! `pread`.
//!
//! ## Layout
//!
//! ```text
//! <cartridge_root>/blocks-p0.idx   (always; index/data partition)
//! <cartridge_root>/blocks-p1.idx   (only on LTFS-partitioned cartridges)
//! ```
//!
//! Each file starts with a 32-byte header (magic + version) followed
//! by a flat array of 16-byte records, indexed by LBA:
//!
//! ```text
//! header:  bytes 0..32
//! record:  offset = HEADER_SIZE + lba * RECORD_SIZE
//! ```
//!
//! `next_lba` is `(file_size - HEADER_SIZE) / RECORD_SIZE`. The header
//! catches "wrong file mistakenly opened as a block index" (magic) and
//! "future format change" (version) cases. ALLOW OVERWRITE / FORMAT
//! MEDIUM / ERASE truncate the records-region via `ftruncate`.
//!
//! ## What is NOT stored here
//!
//! - **IV.** Real LTO drives derive their per-block IV from the
//!   block's recorded position; the IV is never on the medium. We
//!   follow the same model:
//!   `IV = BLAKE3(cartridge_uuid || chunk_id_le || offset_le)[..12]`.
//!   Reproducible at decrypt time without storage.
//! - **Auth tag.** AES-GCM appends the 16 B tag to the ciphertext;
//!   it lives in the chunk file's bytes. `len` already includes those
//!   bytes when the encrypted flag is set.
//! - **Per-block checksum.** Real LTO doesn't expose one to the host
//!   — drive-internal ECC + recorded-block CRC handle integrity. Our
//!   chunk-level BLAKE3 (`ChunkMeta.hash`) is the equivalent.
//! - **Uncompressed size.** lz4-frame and zstd self-frame their
//!   content; the decompressor returns the right number of bytes.
//!   Encrypted blocks are authenticated by the GCM tag; corrupt
//!   plaintext-but-compressed blocks are caught by the codec's frame
//!   checksum.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::dirty_pages::{DirtyPageTracker, PAGE_SIZE};
use crate::errors::{Result, SmcError};
use crate::tape::BlockKind;
use shared_object_store::compression::CompressionAlgo;

/// On-disk record size. Asserted in tests to catch accidental drift.
pub const RECORD_SIZE: usize = 16;

/// On-disk header size. The records area begins at this offset.
///
/// Header layout:
/// - bytes 0..4    `MAGIC`
/// - bytes 4..8    `VERSION` (u32, little-endian)
/// - bytes 8..12   record size (u32, little-endian)
/// - bytes 12..32  reserved, zeroed
pub const HEADER_SIZE: usize = 32;

/// 4-byte file-format magic: "NVBI" (Thur Block Index).
pub const MAGIC: [u8; 4] = *b"NVBI";

/// Format version of the records area. u32 because there's only one
/// header per file — extra bytes are essentially free and let us avoid
/// the "we need a version number wider than u8" cliff later. Bumped on
/// schema breaks.
pub const VERSION: u32 = 1;

// Flag layout (1 byte):
//   bit 0     filemark (1) / data (0)
//   bits 1-3  encryption algo (0 = none, 1 = AES-256-GCM, 2..=7 reserved)
//   bits 4-6  compression algo (0 = none, 1 = lz4, 2 = zstd, 3 = sldc, 4..=7 reserved)
//   bit 7     reserved
const FLAG_FILEMARK: u8 = 0b0000_0001;
const FLAG_ENC_MASK: u8 = 0b0000_1110;
const FLAG_ENC_SHIFT: u8 = 1;
use crate::compression_codec::{pack_compression, unpack_compression};

/// Encryption algorithm tag stored in the 3-bit encryption field of
/// `flags`. Real LTO drives all use AES-256-GCM today (SCSI algorithm
/// code 0x0001_0014); reserving 3 bits leaves room for future LTO
/// generations to add another AEAD without a schema break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EncryptionTag {
    None = 0,
    Aes256Gcm = 1,
    // 2..=7 reserved
}

impl EncryptionTag {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::Aes256Gcm),
            _ => None,
        }
    }
}

/// One block index record, in memory. Total 16 bytes on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRec {
    pub chunk_id: u32,
    pub offset: u32,
    /// On-disk byte count for this block's payload in the chunk file.
    /// For encrypted blocks this includes the trailing 16 B AES-GCM tag.
    /// 0 for filemarks.
    pub len: u32,
    pub kind: BlockKind,
    pub encryption: EncryptionTag,
    /// `None` = stored bytes are not compressed.
    pub compression: Option<CompressionAlgo>,
}

impl BlockRec {
    pub fn data() -> Self {
        Self {
            chunk_id: 0,
            offset: 0,
            len: 0,
            kind: BlockKind::Data,
            encryption: EncryptionTag::None,
            compression: None,
        }
    }
    pub fn filemark(chunk_id: u32, offset: u32) -> Self {
        Self {
            chunk_id,
            offset,
            len: 0,
            kind: BlockKind::Filemark,
            encryption: EncryptionTag::None,
            compression: None,
        }
    }

    /// Convenience: true iff `encryption != None`.
    pub fn encrypted(&self) -> bool {
        !matches!(self.encryption, EncryptionTag::None)
    }

    fn pack_flags(&self) -> u8 {
        let mut f = 0u8;
        if matches!(self.kind, BlockKind::Filemark) {
            f |= FLAG_FILEMARK;
        }
        let enc = self.encryption as u8;
        f |= (enc << FLAG_ENC_SHIFT) & FLAG_ENC_MASK;
        f |= pack_compression(self.compression);
        f
    }

    fn unpack_flags(f: u8) -> Result<(BlockKind, EncryptionTag, Option<CompressionAlgo>)> {
        let kind = if (f & FLAG_FILEMARK) != 0 {
            BlockKind::Filemark
        } else {
            BlockKind::Data
        };
        let enc_bits = (f & FLAG_ENC_MASK) >> FLAG_ENC_SHIFT;
        let enc = EncryptionTag::from_u8(enc_bits).ok_or(SmcError::InvalidOp(
            "block index record has unknown encryption tag",
        ))?;
        let comp = unpack_compression(f).ok_or(SmcError::InvalidOp(
            "block index record has unknown compression tag",
        ))?;
        Ok((kind, enc, comp))
    }

    fn encode(&self) -> [u8; RECORD_SIZE] {
        let mut buf = [0u8; RECORD_SIZE];
        buf[0..4].copy_from_slice(&self.chunk_id.to_le_bytes());
        buf[4..8].copy_from_slice(&self.offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.len.to_le_bytes());
        buf[12] = self.pack_flags();
        // buf[13..16] reserved, zeroed
        buf
    }

    fn decode(buf: &[u8; RECORD_SIZE]) -> Result<Self> {
        let chunk_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let offset = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let (kind, encryption, compression) = Self::unpack_flags(buf[12])?;
        Ok(Self {
            chunk_id,
            offset,
            len,
            kind,
            encryption,
            compression,
        })
    }
}

/// Per-partition block index file. Wraps a single `File` plus a
/// cached `next_lba` (= record count) so callers don't pay a `stat`
/// per append.
///
/// Owns a `DirtyPageTracker` sidecar (`<path>.dirty`) so the
/// manifest-backup path can ship only the 1 MiB pages of the index
/// that changed since the last upload — see `dirty_pages.rs`.
/// Every mutation (`append`, `overwrite`, `truncate_to`, header
/// init) marks the affected page range dirty *before* the
/// `pwrite_at` so a crash never leaves a written-but-unmarked page.
///
/// **Single-writer invariant.** The mutating methods take `&self` and
/// `next_lba` is an `AtomicU64`, but this type is *not* safe under
/// concurrent appends: `append` does a non-atomic load-then-store, so
/// two concurrent callers could mint the same lba. The atomic buys
/// cheap interior mutability behind `&self` (positioned `pwrite_at`
/// needs no `&mut File`), nothing more. Correctness rests on the owner:
/// the `Cartridge` holding this file is always reached through
/// `&mut self`, which serializes all index mutation. Don't share a
/// `BlockIndexFile` across threads for writing.
#[derive(Debug)]
pub struct BlockIndexFile {
    path: PathBuf,
    file: File,
    next_lba: AtomicU64,
    dirty: DirtyPageTracker,
}

impl BlockIndexFile {
    pub fn path_for(cartridge_root: &Path, partition: u8) -> PathBuf {
        cartridge_root.join(format!("blocks-p{partition}.idx"))
    }

    /// Open or create the block index file for a cartridge partition.
    /// Empty/new files get a header written; existing files have their
    /// header validated. Existing record-region length determines the
    /// starting `next_lba`.
    pub fn open_or_create(cartridge_root: &Path, partition: u8) -> Result<Self> {
        let path = Self::path_for(cartridge_root, partition);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            // Fresh file — write the header.
            let mut hdr = [0u8; HEADER_SIZE];
            hdr[0..4].copy_from_slice(&MAGIC);
            hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
            hdr[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
            // hdr[12..32] reserved, zeroed.
            let dirty = DirtyPageTracker::open_or_create(&path)?;
            dirty.mark_range(0, HEADER_SIZE as u64);
            file.write_all_at(&hdr, 0)?;
            return Ok(Self {
                path,
                file,
                next_lba: AtomicU64::new(0),
                dirty,
            });
        }
        if len < HEADER_SIZE as u64 {
            return Err(SmcError::InvalidOp(
                "block index file shorter than header — corrupt",
            ));
        }
        let mut hdr = [0u8; HEADER_SIZE];
        file.read_exact_at(&mut hdr, 0)?;
        if hdr[0..4] != MAGIC {
            return Err(SmcError::InvalidOp(
                "block index file magic mismatch — wrong file or corrupt",
            ));
        }
        let ver = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if ver != VERSION {
            return Err(SmcError::InvalidOp(
                "block index file version unsupported by this build",
            ));
        }
        let rec_sz = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        if rec_sz != RECORD_SIZE {
            return Err(SmcError::InvalidOp(
                "block index file record-size header field disagrees with build",
            ));
        }
        let records_bytes = len - HEADER_SIZE as u64;
        if !records_bytes.is_multiple_of(RECORD_SIZE as u64) {
            return Err(SmcError::InvalidOp(
                "block index records region is not a multiple of record size",
            ));
        }
        let next_lba = records_bytes / RECORD_SIZE as u64;
        let dirty = DirtyPageTracker::open_or_create(&path)?;
        Ok(Self {
            path,
            file,
            next_lba: AtomicU64::new(next_lba),
            dirty,
        })
    }

    /// Byte offset of the record at `lba`.
    fn record_offset(lba: u64) -> u64 {
        HEADER_SIZE as u64 + lba * RECORD_SIZE as u64
    }

    pub fn next_lba(&self) -> u64 {
        self.next_lba.load(Ordering::Acquire)
    }

    /// Append a record at `next_lba`. Returns the LBA written.
    pub fn append(&self, rec: &BlockRec) -> Result<u64> {
        let lba = self.next_lba.load(Ordering::Acquire);
        let buf = rec.encode();
        let off = Self::record_offset(lba);
        self.dirty.mark_range(off, RECORD_SIZE as u64);
        self.file.write_all_at(&buf, off)?;
        self.next_lba.store(lba + 1, Ordering::Release);
        Ok(lba)
    }

    /// Overwrite an existing record. Used by ALLOW OVERWRITE writes
    /// that land inside the existing partition span. Does not move
    /// `next_lba`.
    pub fn overwrite(&self, lba: u64, rec: &BlockRec) -> Result<()> {
        if lba >= self.next_lba.load(Ordering::Acquire) {
            return Err(SmcError::InvalidOp("block index overwrite past next_lba"));
        }
        let buf = rec.encode();
        let off = Self::record_offset(lba);
        self.dirty.mark_range(off, RECORD_SIZE as u64);
        self.file.write_all_at(&buf, off)?;
        Ok(())
    }

    /// Read the record at `lba`. Returns `InvalidOp` if `lba` is past
    /// `next_lba` — caller's bug; tape end-of-data semantics live one
    /// level up.
    pub fn read(&self, lba: u64) -> Result<BlockRec> {
        if lba >= self.next_lba.load(Ordering::Acquire) {
            return Err(SmcError::InvalidOp("block index read past next_lba"));
        }
        let mut buf = [0u8; RECORD_SIZE];
        self.file
            .read_exact_at(&mut buf, Self::record_offset(lba))?;
        BlockRec::decode(&buf)
    }

    /// Truncate to `new_next_lba` records. Used by ERASE / FORMAT
    /// MEDIUM, and by writes that erase everything past the head.
    /// Header is preserved.
    pub fn truncate_to(&self, new_next_lba: u64) -> Result<()> {
        let new_len = HEADER_SIZE as u64 + new_next_lba * RECORD_SIZE as u64;
        self.file.set_len(new_len)?;
        self.next_lba.store(new_next_lba, Ordering::Release);
        let new_pages = new_len.div_ceil(PAGE_SIZE as u64) as u32;
        self.dirty.truncate_to_pages(new_pages);
        Ok(())
    }

    /// Force file contents (and metadata) to disk. Call at chunk-roll,
    /// filemark, and Drop boundaries — same cadence as the manifest
    /// persist. Also persists the dirty-page sidecar so a crash never
    /// leaves a written-but-unmarked page.
    pub fn fsync(&self) -> Result<()> {
        self.file.sync_data()?;
        self.dirty.persist()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the dirty-page tracker. Used by the storage-backup path
    /// to snapshot dirty pages before uploading and to clear them
    /// after each successful PUT.
    pub fn dirty_tracker(&self) -> &DirtyPageTracker {
        &self.dirty
    }
}

/// Derive the per-block AES-GCM IV from the block's recorded position.
/// Real LTO drives derive IVs from position too — the IV is never
/// stored on the medium. `chunk_id` is per-cartridge monotonic and
/// never reused (ALLOW OVERWRITE produces new chunks, doesn't recycle
/// ids), so within a cartridge the (chunk_id, offset) pair is unique
/// forever — which gives GCM the (key, IV) uniqueness it needs.
///
/// Thin wrapper over `shared_crypto::derive_iv` so this crate keeps
/// its tape-flavored (chunk_id, offset) signature while `core-block`
/// can call the same generic primitive with (page_id, 0).
pub fn derive_iv(uuid: &[u8; 16], chunk_id: u64, offset: u64) -> [u8; 12] {
    shared_crypto::derive_iv(uuid, chunk_id, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression_codec::FLAG_COMP_SHIFT;
    use tempfile::TempDir;

    #[test]
    fn record_size_locked() {
        assert_eq!(RECORD_SIZE, 16);
    }

    #[test]
    fn round_trip_data_block() {
        let r = BlockRec {
            chunk_id: 42,
            offset: 1024,
            len: 65_536,
            kind: BlockKind::Data,
            encryption: EncryptionTag::None,
            compression: None,
        };
        let enc = r.encode();
        let dec = BlockRec::decode(&enc).unwrap();
        assert_eq!(r, dec);
    }

    #[test]
    fn round_trip_encrypted_compressed_block() {
        let r = BlockRec {
            chunk_id: u32::MAX,
            offset: 0xDEAD_BEEF,
            len: 1,
            kind: BlockKind::Data,
            encryption: EncryptionTag::Aes256Gcm,
            compression: Some(CompressionAlgo::Zstd),
        };
        let enc = r.encode();
        let dec = BlockRec::decode(&enc).unwrap();
        assert_eq!(r, dec);
        assert!(dec.encrypted());
    }

    #[test]
    fn round_trip_filemark() {
        let r = BlockRec::filemark(7, 12_345);
        let enc = r.encode();
        let dec = BlockRec::decode(&enc).unwrap();
        assert_eq!(r, dec);
        assert!(matches!(dec.kind, BlockKind::Filemark));
        assert_eq!(dec.len, 0);
    }

    #[test]
    fn all_compression_algos_round_trip() {
        for algo in [
            None,
            Some(CompressionAlgo::Lz4),
            Some(CompressionAlgo::Zstd),
            Some(CompressionAlgo::Sldc),
        ] {
            let r = BlockRec {
                chunk_id: 1,
                offset: 0,
                len: 100,
                kind: BlockKind::Data,
                encryption: EncryptionTag::None,
                compression: algo,
            };
            let dec = BlockRec::decode(&r.encode()).unwrap();
            assert_eq!(r, dec, "round trip failed for {:?}", algo);
        }
    }

    #[test]
    fn unknown_compression_tag_rejected() {
        // compression code 4 is currently reserved (only 0..=3 valid).
        let mut buf = [0u8; RECORD_SIZE];
        buf[12] = 4 << FLAG_COMP_SHIFT;
        let err = BlockRec::decode(&buf).unwrap_err();
        match err {
            SmcError::InvalidOp(_) => {}
            other => panic!("expected InvalidOp, got {:?}", other),
        }
    }

    #[test]
    fn unknown_encryption_tag_rejected() {
        // encryption code 2 is currently reserved (only 0..=1 valid).
        let mut buf = [0u8; RECORD_SIZE];
        buf[12] = 2 << FLAG_ENC_SHIFT;
        let err = BlockRec::decode(&buf).unwrap_err();
        match err {
            SmcError::InvalidOp(_) => {}
            other => panic!("expected InvalidOp, got {:?}", other),
        }
    }

    #[test]
    fn append_read_truncate() {
        let tmp = TempDir::new().unwrap();
        let bif = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap();
        assert_eq!(bif.next_lba(), 0);

        for i in 0..16u64 {
            let r = BlockRec {
                chunk_id: 1,
                offset: (i * 64) as u32,
                len: 64,
                kind: BlockKind::Data,
                encryption: EncryptionTag::None,
                compression: None,
            };
            let lba = bif.append(&r).unwrap();
            assert_eq!(lba, i);
        }
        assert_eq!(bif.next_lba(), 16);

        let r = bif.read(7).unwrap();
        assert_eq!(r.offset, 7 * 64);

        bif.truncate_to(8).unwrap();
        assert_eq!(bif.next_lba(), 8);
        assert!(bif.read(8).is_err());
        let r = bif.read(7).unwrap();
        assert_eq!(r.offset, 7 * 64);
    }

    #[test]
    fn reopen_preserves_state() {
        let tmp = TempDir::new().unwrap();
        {
            let bif = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap();
            for i in 0..4 {
                bif.append(&BlockRec {
                    chunk_id: 1,
                    offset: i * 100,
                    len: 100,
                    kind: BlockKind::Data,
                    encryption: EncryptionTag::None,
                    compression: None,
                })
                .unwrap();
            }
            bif.fsync().unwrap();
        }
        let bif = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap();
        assert_eq!(bif.next_lba(), 4);
        let r = bif.read(2).unwrap();
        assert_eq!(r.offset, 200);
    }

    #[test]
    fn overwrite_in_place() {
        let tmp = TempDir::new().unwrap();
        let bif = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap();
        for _ in 0..3 {
            bif.append(&BlockRec::data()).unwrap();
        }
        let new_rec = BlockRec {
            chunk_id: 99,
            offset: 0xAABB,
            len: 7,
            kind: BlockKind::Data,
            encryption: EncryptionTag::Aes256Gcm,
            compression: Some(CompressionAlgo::Lz4),
        };
        bif.overwrite(1, &new_rec).unwrap();
        assert_eq!(bif.next_lba(), 3);
        assert_eq!(bif.read(1).unwrap(), new_rec);
        // overwrite past end refused
        assert!(bif.overwrite(3, &new_rec).is_err());
    }

    #[test]
    fn read_past_end_errors() {
        let tmp = TempDir::new().unwrap();
        let bif = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap();
        assert!(bif.read(0).is_err());
        bif.append(&BlockRec::data()).unwrap();
        assert!(bif.read(0).is_ok());
        assert!(bif.read(1).is_err());
    }

    #[test]
    fn header_written_and_validated() {
        let tmp = TempDir::new().unwrap();
        let path = BlockIndexFile::path_for(tmp.path(), 0);
        let bif = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap();
        bif.append(&BlockRec::data()).unwrap();
        drop(bif);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC);
        let ver = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(ver, VERSION);
        let rec_sz = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        assert_eq!(rec_sz, RECORD_SIZE);
        assert_eq!(bytes.len(), HEADER_SIZE + RECORD_SIZE);
        // Re-open succeeds.
        let _bif = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap();
    }

    #[test]
    fn wrong_magic_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = BlockIndexFile::path_for(tmp.path(), 0);
        let mut hdr = [0u8; HEADER_SIZE];
        hdr[0..4].copy_from_slice(b"XXXX");
        std::fs::write(&path, hdr).unwrap();
        let err = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn wrong_version_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = BlockIndexFile::path_for(tmp.path(), 0);
        let mut hdr = [0u8; HEADER_SIZE];
        hdr[0..4].copy_from_slice(&MAGIC);
        hdr[4..8].copy_from_slice(&(VERSION + 7).to_le_bytes());
        hdr[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        std::fs::write(&path, hdr).unwrap();
        let err = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn corrupt_length_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = BlockIndexFile::path_for(tmp.path(), 0);
        // Valid header but trailing partial record.
        let mut bytes = vec![0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        bytes.extend_from_slice(b"not-a-multiple-of-16");
        std::fs::write(&path, &bytes).unwrap();
        let err = BlockIndexFile::open_or_create(tmp.path(), 0).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn iv_derivation_unique_per_position() {
        let uuid = [0xAB; 16];
        let a = derive_iv(&uuid, 1, 0);
        let b = derive_iv(&uuid, 1, 64);
        let c = derive_iv(&uuid, 2, 0);
        let d = derive_iv(&[0xCD; 16], 1, 0);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        // Same inputs -> same IV (deterministic).
        assert_eq!(a, derive_iv(&uuid, 1, 0));
    }

    #[test]
    fn iv_avalanche_offset() {
        // Single-bit change in offset must change most output bits.
        let uuid = [0; 16];
        let a = derive_iv(&uuid, 0, 0);
        let b = derive_iv(&uuid, 0, 1);
        let diff: u32 = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| (x ^ y).count_ones())
            .sum();
        assert!(diff > 32, "expected significant bit difference, got {diff}");
    }
}
