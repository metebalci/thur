// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-cartridge chunk index file.
//!
//! Replaces the inline `Vec<ChunkMeta>` that used to live in the
//! cartridge manifest. The chunk index is now an external file of
//! fixed-size 64-byte records, indexed by `chunk_id` (per-cartridge
//! monotonic, derived from file offset). `pwrite_at(id * 64)` for
//! both appends (new chunks) and overwrites (mark uploaded, mark
//! evicted). This makes manifest.json O(1) in size regardless of
//! chunk count and turns every per-chunk mutation into a single
//! `pwrite`.
//!
//! Last-accessed timestamps for disk-cache LRU eviction live in a
//! separate `lru.idx` sidecar (`lru_index.rs`) — they're a local
//! cache hint and don't belong in the storage-replicated index.
//! Splitting them out keeps `chunks.idx` pages clean during reads,
//! so the manifest-backup path stops shipping deltas on read-only
//! workloads.
//!
//! ## Layout
//!
//! ```text
//! <cartridge_root>/chunks.idx
//! ```
//!
//! Single file per cartridge — `chunk_id`s span partitions, so this is
//! not per-partition. Header is 32 bytes (mirrors `blocks-pN.idx`):
//!
//! ```text
//! header:  bytes 0..32
//! record:  offset = HEADER_SIZE + chunk_id * RECORD_SIZE
//! ```
//!
//! `next_id` is `(file_size - HEADER_SIZE) / RECORD_SIZE`. The header
//! catches "wrong file mistakenly opened as a chunk index" (magic) and
//! "future format change" (version) cases. ERASE / FORMAT MEDIUM
//! truncate the records-region via `ftruncate`.
//!
//! ## Why id is not stored
//!
//! `chunk_id` is per-cartridge monotonic 0, 1, 2, … with no gaps —
//! ALLOW OVERWRITE produces new chunks rather than reusing ids, and
//! FORMAT/ERASE truncates the whole file back to zero records. The id
//! is therefore purely positional, like LBA in `blocks-pN.idx`, and
//! storing it explicitly would be redundant.
//!
//! ## What is NOT stored here
//!
//! - **compressed_size.** Was a write-only field in the JSON manifest:
//!   set by `apply_chunk_upload_outcome` from the backend's PUT result
//!   and then never read by anyone. Compression metrics are emitted
//!   directly from the backend at upload time and don't consult the
//!   chunk index.
//! - **chunk_id.** Derivable from offset (see above).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::dirty_pages::{DirtyPageTracker, PAGE_SIZE};
use crate::errors::{Result, SmcError};
use shared_object_store::compression::CompressionAlgo;

/// On-disk record size. Asserted in tests to catch accidental drift.
pub const RECORD_SIZE: usize = 64;

/// On-disk header size. The records area begins at this offset.
///
/// Header layout:
/// - bytes 0..4    `MAGIC`
/// - bytes 4..8    `VERSION` (u32, little-endian)
/// - bytes 8..12   record size (u32, little-endian)
/// - bytes 12..32  reserved, zeroed
pub const HEADER_SIZE: usize = 32;

/// 4-byte file-format magic: "NVCI" (Thur VTL Chunk Index).
pub const MAGIC: [u8; 4] = *b"NVCI";

/// Format version of the records area. Bumped on schema breaks.
pub const VERSION: u32 = 1;

// Record layout (64 bytes):
//   bytes  0..4    size              (u32 LE; bounded by BlockRec.offset's
//                                     u32 width — no chunk can hold > 4 GiB)
//   bytes  4..36   hash              (32-byte raw BLAKE3; valid iff hash_present)
//   byte  36       flags
//   bytes 37..64   reserved          (27 B, zeroed)
//
// Flag layout (1 byte):
//   bit 0     hash_present (1 = sealed, 0 = unsealed staging chunk)
//   bit 1     uploaded
//   bits 2-3  location (0=LocalOnly, 1=StorageOnly, 2=Both)
//   bits 4-6  compression (0=None, 1=Lz4, 2=Zstd, 3=Sldc, 4..=7 reserved)
//   bit 7     reserved
const FLAG_HASH_PRESENT: u8 = 0b0000_0001;
const FLAG_UPLOADED: u8 = 0b0000_0010;
const FLAG_LOC_MASK: u8 = 0b0000_1100;
const FLAG_LOC_SHIFT: u8 = 2;
use crate::compression_codec::{pack_compression, unpack_compression};

/// Per-cartridge view of where a chunk's bytes are. Mirrors the
/// `ChunkLocation` enum in `cartridge.rs` but lives here so the codec
/// is self-contained — translated at the API boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LocationTag {
    LocalOnly = 0,
    StorageOnly = 1,
    Both = 2,
    // 3 reserved
}

impl LocationTag {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::LocalOnly),
            1 => Some(Self::StorageOnly),
            2 => Some(Self::Both),
            _ => None,
        }
    }
}

/// One chunk index record, in memory. 64 bytes on disk.
///
/// `hash` is the lowercase hex form of the chunk's BLAKE3 (matches the
/// rest of the codebase — `ChunkStore` paths, storage keys, and audit
/// records are all hex). The on-disk format stores the raw 32-byte
/// digest; hex translation happens at this struct's boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRec {
    pub size: u64,
    pub hash: Option<String>,
    pub location: LocationTag,
    pub uploaded: bool,
    pub compression: Option<CompressionAlgo>,
}

impl ChunkRec {
    /// Fresh staging chunk: no hash yet, zero size, LocalOnly,
    /// not uploaded, no compression metadata.
    pub fn staging() -> Self {
        Self {
            size: 0,
            hash: None,
            location: LocationTag::LocalOnly,
            uploaded: false,
            compression: None,
        }
    }

    fn pack_flags(&self) -> u8 {
        let mut f = 0u8;
        if self.hash.is_some() {
            f |= FLAG_HASH_PRESENT;
        }
        if self.uploaded {
            f |= FLAG_UPLOADED;
        }
        let loc = self.location as u8;
        f |= (loc << FLAG_LOC_SHIFT) & FLAG_LOC_MASK;
        f |= pack_compression(self.compression);
        f
    }

    fn unpack_flags(f: u8) -> Result<(bool, bool, LocationTag, Option<CompressionAlgo>)> {
        let hash_present = (f & FLAG_HASH_PRESENT) != 0;
        let uploaded = (f & FLAG_UPLOADED) != 0;
        let loc_bits = (f & FLAG_LOC_MASK) >> FLAG_LOC_SHIFT;
        let location = LocationTag::from_u8(loc_bits).ok_or(SmcError::InvalidOp(
            "chunk index record has unknown location tag",
        ))?;
        let comp = unpack_compression(f).ok_or(SmcError::InvalidOp(
            "chunk index record has unknown compression tag",
        ))?;
        Ok((hash_present, uploaded, location, comp))
    }

    fn encode(&self) -> Result<[u8; RECORD_SIZE]> {
        if self.size > u32::MAX as u64 {
            // BlockRec.offset is u32, so a chunk's bytes can never
            // exceed 4 GiB. Refusing to encode larger sizes catches
            // any future widening attempt at the on-disk boundary.
            return Err(SmcError::InvalidOp(
                "chunk index encode: size exceeds u32::MAX (chunk too large)",
            ));
        }
        let mut buf = [0u8; RECORD_SIZE];
        buf[0..4].copy_from_slice(&(self.size as u32).to_le_bytes());
        if let Some(ref hex_str) = self.hash {
            let bytes = hex::decode(hex_str)
                .map_err(|_| SmcError::InvalidOp("chunk index encode: hash is not valid hex"))?;
            if bytes.len() != 32 {
                return Err(SmcError::InvalidOp(
                    "chunk index encode: hash hex must decode to 32 bytes",
                ));
            }
            buf[4..36].copy_from_slice(&bytes);
        }
        buf[36] = self.pack_flags();
        // buf[37..64] reserved, zeroed
        Ok(buf)
    }

    fn decode(buf: &[u8; RECORD_SIZE]) -> Result<Self> {
        let size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        let (hash_present, uploaded, location, compression) = Self::unpack_flags(buf[36])?;
        let hash = if hash_present {
            Some(hex::encode(&buf[4..36]))
        } else {
            None
        };
        Ok(Self {
            size,
            hash,
            location,
            uploaded,
            compression,
        })
    }
}

/// Per-cartridge chunk index file. Wraps a single `File` plus a cached
/// `next_id` (= record count) so callers don't pay a `stat` per append.
///
/// Owns a `DirtyPageTracker` sidecar (`<path>.dirty`) so the
/// manifest-backup path can ship only the 1 MiB pages of the index
/// that changed since the last upload — see `dirty_pages.rs`. Every
/// mutation (`append`, `overwrite`, `truncate_to`, header init) marks
/// the affected page range dirty *before* the `pwrite_at` so a crash
/// never leaves a written-but-unmarked page.
///
/// **Single-writer invariant.** The mutating methods take `&self` and
/// `next_id` is an `AtomicU64`, but this type is *not* safe under
/// concurrent appends: `append` does a non-atomic load-then-store, so
/// two concurrent callers could mint the same id. The atomic buys
/// cheap interior mutability behind `&self` (positioned `pwrite_at`
/// needs no `&mut File`), nothing more. Correctness rests on the owner:
/// the `Cartridge` holding this file is always reached through
/// `&mut self`, which serializes all index mutation. Don't share a
/// `ChunkIndexFile` across threads for writing.
#[derive(Debug)]
pub struct ChunkIndexFile {
    path: PathBuf,
    file: File,
    next_id: AtomicU64,
    dirty: DirtyPageTracker,
}

impl ChunkIndexFile {
    pub fn path_for(cartridge_root: &Path) -> PathBuf {
        cartridge_root.join("chunks.idx")
    }

    /// Open or create the chunk index file for a cartridge. Empty/new
    /// files get a header written; existing files have their header
    /// validated. Existing record-region length determines the starting
    /// `next_id`.
    pub fn open_or_create(cartridge_root: &Path) -> Result<Self> {
        let path = Self::path_for(cartridge_root);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        if len == 0 {
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
                next_id: AtomicU64::new(0),
                dirty,
            });
        }
        if len < HEADER_SIZE as u64 {
            return Err(SmcError::InvalidOp(
                "chunk index file shorter than header — corrupt",
            ));
        }
        let mut hdr = [0u8; HEADER_SIZE];
        file.read_exact_at(&mut hdr, 0)?;
        if hdr[0..4] != MAGIC {
            return Err(SmcError::InvalidOp(
                "chunk index file magic mismatch — wrong file or corrupt",
            ));
        }
        let ver = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if ver != VERSION {
            return Err(SmcError::InvalidOp(
                "chunk index file version unsupported by this build",
            ));
        }
        let rec_sz = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        if rec_sz != RECORD_SIZE {
            return Err(SmcError::InvalidOp(
                "chunk index file record-size header field disagrees with build",
            ));
        }
        let records_bytes = len - HEADER_SIZE as u64;
        if !records_bytes.is_multiple_of(RECORD_SIZE as u64) {
            return Err(SmcError::InvalidOp(
                "chunk index records region is not a multiple of record size",
            ));
        }
        let next_id = records_bytes / RECORD_SIZE as u64;
        let dirty = DirtyPageTracker::open_or_create(&path)?;
        Ok(Self {
            path,
            file,
            next_id: AtomicU64::new(next_id),
            dirty,
        })
    }

    /// Byte offset of the record at `id`.
    fn record_offset(id: u64) -> u64 {
        HEADER_SIZE as u64 + id * RECORD_SIZE as u64
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.load(Ordering::Acquire)
    }

    /// Append a record at `next_id`. Returns the id written.
    pub fn append(&self, rec: &ChunkRec) -> Result<u64> {
        let id = self.next_id.load(Ordering::Acquire);
        let buf = rec.encode()?;
        let off = Self::record_offset(id);
        self.dirty.mark_range(off, RECORD_SIZE as u64);
        self.file.write_all_at(&buf, off)?;
        self.next_id.store(id + 1, Ordering::Release);
        Ok(id)
    }

    /// Overwrite an existing record. Used to flip `uploaded`,
    /// transition `location`, etc. Does not move `next_id`.
    pub fn overwrite(&self, id: u64, rec: &ChunkRec) -> Result<()> {
        if id >= self.next_id.load(Ordering::Acquire) {
            return Err(SmcError::InvalidOp("chunk index overwrite past next_id"));
        }
        let buf = rec.encode()?;
        let off = Self::record_offset(id);
        self.dirty.mark_range(off, RECORD_SIZE as u64);
        self.file.write_all_at(&buf, off)?;
        Ok(())
    }

    /// Read the record at `id`. Returns `InvalidOp` if `id` is past
    /// `next_id` — caller's bug.
    pub fn read(&self, id: u64) -> Result<ChunkRec> {
        if id >= self.next_id.load(Ordering::Acquire) {
            return Err(SmcError::InvalidOp("chunk index read past next_id"));
        }
        let mut buf = [0u8; RECORD_SIZE];
        self.file.read_exact_at(&mut buf, Self::record_offset(id))?;
        ChunkRec::decode(&buf)
    }

    /// Truncate to `new_next_id` records. Used by ERASE / FORMAT MEDIUM.
    /// Header is preserved.
    pub fn truncate_to(&self, new_next_id: u64) -> Result<()> {
        let new_len = HEADER_SIZE as u64 + new_next_id * RECORD_SIZE as u64;
        self.file.set_len(new_len)?;
        self.next_id.store(new_next_id, Ordering::Release);
        let new_pages = new_len.div_ceil(PAGE_SIZE as u64) as u32;
        self.dirty.truncate_to_pages(new_pages);
        Ok(())
    }

    /// Force file contents (and metadata) to disk. Call at chunk-roll,
    /// filemark, and Drop boundaries — same cadence as the manifest
    /// persist and the block-index fsync. Also persists the
    /// dirty-page sidecar so a crash never leaves a written-but-
    /// unmarked page.
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

    /// Iterate records sequentially, yielding `(id, ChunkRec)` for each.
    /// Reads in 64 KiB batches (1024 records per batch) to avoid both
    /// per-record syscall overhead and full-file materialization.
    pub fn iter(&self) -> ChunkIndexIter<'_> {
        ChunkIndexIter {
            file: &self.file,
            next_id: 0,
            end_id: self.next_id.load(Ordering::Acquire),
            buf: Vec::new(),
            buf_start_id: 0,
        }
    }
}

const ITER_BATCH_RECORDS: u64 = 1024;

pub struct ChunkIndexIter<'a> {
    file: &'a File,
    next_id: u64,
    end_id: u64,
    buf: Vec<u8>,
    buf_start_id: u64,
}

impl<'a> Iterator for ChunkIndexIter<'a> {
    type Item = Result<(u64, ChunkRec)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_id >= self.end_id {
            return None;
        }
        let buf_end_id = self.buf_start_id + (self.buf.len() / RECORD_SIZE) as u64;
        if self.next_id >= buf_end_id {
            // Refill.
            let remaining = self.end_id - self.next_id;
            let take = remaining.min(ITER_BATCH_RECORDS);
            let bytes = (take as usize) * RECORD_SIZE;
            self.buf.resize(bytes, 0);
            let off = ChunkIndexFile::record_offset(self.next_id);
            if let Err(e) = self.file.read_exact_at(&mut self.buf, off) {
                return Some(Err(SmcError::Io(e)));
            }
            self.buf_start_id = self.next_id;
        }
        let id = self.next_id;
        let rel = ((id - self.buf_start_id) as usize) * RECORD_SIZE;
        let mut rec_buf = [0u8; RECORD_SIZE];
        rec_buf.copy_from_slice(&self.buf[rel..rel + RECORD_SIZE]);
        self.next_id += 1;
        Some(ChunkRec::decode(&rec_buf).map(|r| (id, r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression_codec::FLAG_COMP_SHIFT;
    use tempfile::TempDir;

    fn sample_hash(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    #[test]
    fn record_size_locked() {
        assert_eq!(RECORD_SIZE, 64);
    }

    #[test]
    fn round_trip_staging() {
        let r = ChunkRec::staging();
        let enc = r.encode().unwrap();
        let dec = ChunkRec::decode(&enc).unwrap();
        assert_eq!(r, dec);
        assert!(dec.hash.is_none());
        assert_eq!(dec.location, LocationTag::LocalOnly);
        assert!(!dec.uploaded);
    }

    #[test]
    fn round_trip_sealed_uploaded() {
        let r = ChunkRec {
            size: 8 * 1024 * 1024,
            hash: Some(sample_hash(0xAB)),
            location: LocationTag::Both,
            uploaded: true,
            compression: Some(CompressionAlgo::Zstd),
        };
        let enc = r.encode().unwrap();
        let dec = ChunkRec::decode(&enc).unwrap();
        assert_eq!(r, dec);
    }

    #[test]
    fn round_trip_evicted() {
        let r = ChunkRec {
            size: 1024,
            hash: Some(sample_hash(0x01)),
            location: LocationTag::StorageOnly,
            uploaded: true,
            compression: Some(CompressionAlgo::Lz4),
        };
        let dec = ChunkRec::decode(&r.encode().unwrap()).unwrap();
        assert_eq!(r, dec);
    }

    #[test]
    fn all_compression_algos_round_trip() {
        for algo in [
            None,
            Some(CompressionAlgo::Lz4),
            Some(CompressionAlgo::Zstd),
            Some(CompressionAlgo::Sldc),
        ] {
            let r = ChunkRec {
                size: 100,
                hash: Some(sample_hash(0x55)),
                location: LocationTag::Both,
                uploaded: true,
                compression: algo,
            };
            let dec = ChunkRec::decode(&r.encode().unwrap()).unwrap();
            assert_eq!(r, dec, "round trip failed for {:?}", algo);
        }
    }

    #[test]
    fn reserved_bytes_ignored_on_decode() {
        // Bytes 37..64 are reserved; decode ignores whatever is there.
        let mut buf = [0u8; RECORD_SIZE];
        buf[0..4].copy_from_slice(&(4096u32).to_le_bytes());
        buf[4..36].copy_from_slice(&[0xAB; 32]);
        buf[36] = FLAG_HASH_PRESENT | FLAG_UPLOADED;
        buf[37..64].copy_from_slice(&[0xFF; 27]); // garbage in reserved area
        let dec = ChunkRec::decode(&buf).unwrap();
        assert_eq!(dec.size, 4096);
        assert!(dec.uploaded);
    }

    #[test]
    fn size_over_u32_max_rejected_on_encode() {
        let r = ChunkRec {
            size: u32::MAX as u64 + 1,
            hash: None,
            location: LocationTag::LocalOnly,
            uploaded: false,
            compression: None,
        };
        assert!(r.encode().is_err());
    }

    #[test]
    fn unknown_compression_tag_rejected() {
        let mut buf = [0u8; RECORD_SIZE];
        buf[36] = (4u8 << FLAG_COMP_SHIFT) | FLAG_HASH_PRESENT;
        let err = ChunkRec::decode(&buf).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn unknown_location_tag_rejected() {
        let mut buf = [0u8; RECORD_SIZE];
        buf[36] = 3u8 << FLAG_LOC_SHIFT; // location code 3 is reserved
        let err = ChunkRec::decode(&buf).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn append_read_truncate() {
        let tmp = TempDir::new().unwrap();
        let cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(cif.next_id(), 0);

        for i in 0..16u64 {
            let r = ChunkRec {
                size: i * 1024,
                hash: Some(sample_hash(i as u8)),
                location: LocationTag::LocalOnly,
                uploaded: false,
                compression: None,
            };
            let id = cif.append(&r).unwrap();
            assert_eq!(id, i);
        }
        assert_eq!(cif.next_id(), 16);

        let r = cif.read(7).unwrap();
        assert_eq!(r.size, 7 * 1024);
        assert_eq!(r.hash, Some(sample_hash(7)));

        cif.truncate_to(8).unwrap();
        assert_eq!(cif.next_id(), 8);
        assert!(cif.read(8).is_err());
        let r = cif.read(7).unwrap();
        assert_eq!(r.size, 7 * 1024);
    }

    #[test]
    fn reopen_preserves_state() {
        let tmp = TempDir::new().unwrap();
        {
            let cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
            for i in 0..4u64 {
                cif.append(&ChunkRec {
                    size: i * 100,
                    hash: Some(sample_hash(i as u8)),
                    location: LocationTag::LocalOnly,
                    uploaded: false,
                    compression: None,
                })
                .unwrap();
            }
            cif.fsync().unwrap();
        }
        let cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(cif.next_id(), 4);
        let r = cif.read(2).unwrap();
        assert_eq!(r.size, 200);
        assert_eq!(r.hash, Some(sample_hash(2)));
    }

    #[test]
    fn overwrite_in_place() {
        let tmp = TempDir::new().unwrap();
        let cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
        for _ in 0..3 {
            cif.append(&ChunkRec::staging()).unwrap();
        }
        let new_rec = ChunkRec {
            size: 7,
            hash: Some(sample_hash(0xFE)),
            location: LocationTag::Both,
            uploaded: true,
            compression: Some(CompressionAlgo::Lz4),
        };
        cif.overwrite(1, &new_rec).unwrap();
        assert_eq!(cif.next_id(), 3);
        assert_eq!(cif.read(1).unwrap(), new_rec);
        assert!(cif.overwrite(3, &new_rec).is_err());
    }

    #[test]
    fn read_past_end_errors() {
        let tmp = TempDir::new().unwrap();
        let cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
        assert!(cif.read(0).is_err());
        cif.append(&ChunkRec::staging()).unwrap();
        assert!(cif.read(0).is_ok());
        assert!(cif.read(1).is_err());
    }

    #[test]
    fn header_written_and_validated() {
        let tmp = TempDir::new().unwrap();
        let path = ChunkIndexFile::path_for(tmp.path());
        let cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
        cif.append(&ChunkRec::staging()).unwrap();
        drop(cif);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC);
        let ver = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(ver, VERSION);
        let rec_sz = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        assert_eq!(rec_sz, RECORD_SIZE);
        assert_eq!(bytes.len(), HEADER_SIZE + RECORD_SIZE);
        let _cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
    }

    #[test]
    fn wrong_magic_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = ChunkIndexFile::path_for(tmp.path());
        let mut hdr = [0u8; HEADER_SIZE];
        hdr[0..4].copy_from_slice(b"XXXX");
        std::fs::write(&path, hdr).unwrap();
        let err = ChunkIndexFile::open_or_create(tmp.path()).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn wrong_version_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = ChunkIndexFile::path_for(tmp.path());
        let mut hdr = [0u8; HEADER_SIZE];
        hdr[0..4].copy_from_slice(&MAGIC);
        hdr[4..8].copy_from_slice(&(VERSION + 7).to_le_bytes());
        hdr[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        std::fs::write(&path, hdr).unwrap();
        let err = ChunkIndexFile::open_or_create(tmp.path()).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn corrupt_length_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = ChunkIndexFile::path_for(tmp.path());
        let mut bytes = vec![0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        bytes.extend_from_slice(b"not-a-multiple-of-64");
        std::fs::write(&path, &bytes).unwrap();
        let err = ChunkIndexFile::open_or_create(tmp.path()).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn iter_walks_all_records() {
        let tmp = TempDir::new().unwrap();
        let cif = ChunkIndexFile::open_or_create(tmp.path()).unwrap();
        // Append more than one batch (>1024) to exercise refill.
        let total: u64 = 1500;
        for i in 0..total {
            cif.append(&ChunkRec {
                size: i,
                hash: Some(sample_hash((i & 0xFF) as u8)),
                location: LocationTag::LocalOnly,
                uploaded: false,
                compression: None,
            })
            .unwrap();
        }
        let collected: Vec<(u64, ChunkRec)> = cif.iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(collected.len(), total as usize);
        for (id, rec) in collected.iter() {
            assert_eq!(rec.size, *id);
            assert_eq!(rec.hash, Some(sample_hash((*id & 0xFF) as u8)));
        }
    }

    #[test]
    fn invalid_hex_hash_rejected_on_encode() {
        let r = ChunkRec {
            size: 0,
            hash: Some("not-hex".into()),
            location: LocationTag::LocalOnly,
            uploaded: false,
            compression: None,
        };
        assert!(r.encode().is_err());
    }
}
