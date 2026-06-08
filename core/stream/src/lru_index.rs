// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-cartridge LRU sidecar for `chunks.idx`.
//!
//! The chunk index stores immutable per-chunk metadata (size, hash,
//! upload state, location, compression). Last-accessed timestamps used
//! to live in the same record, but every read of a chunk would
//! `pwrite` the chunks-index page and dirty a 1 MiB delta that the
//! manifest-backup path then shipped to storage — pure write
//! amplification driven by what is, fundamentally, a local cache hint.
//!
//! `lru.idx` splits that hot column out:
//!
//! - One fixed 8-byte record per chunk_id (u64 LE epoch seconds).
//! - Positional, mirroring `chunks.idx` — same chunk_id ⇒ same record
//!   index. `next_id` matches `chunks.idx` next_id; the file grows in
//!   lockstep on append and shrinks in lockstep on truncate.
//! - **Local-only.** No `DirtyPageTracker` sidecar. Never registered
//!   with `index_backup`. Never enumerated for storage restore. A fresh
//!   host doing cold-bucket DR rebuilds it from scratch as zeros.
//! - Reset / corrupt / missing ⇒ rebuild as zero-filled to match the
//!   chunks.idx record count. First eviction cycle picks oldest
//!   uniformly; subsequent cycles converge as touches arrive.
//!
//! ## Layout
//!
//! ```text
//! <cartridge_root>/lru.idx
//! ```
//!
//! ```text
//! header:  bytes 0..32          (magic TVLI + version + record-size)
//! record:  offset = HEADER_SIZE + chunk_id * RECORD_SIZE
//! ```

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::errors::{Result, SmcError};

/// On-disk record size: 8 bytes = u64 LE epoch seconds.
pub const RECORD_SIZE: usize = 8;

/// On-disk header size. Mirrors `chunks.idx` / `blocks-pN.idx` for
/// consistency, even though most of it is reserved.
pub const HEADER_SIZE: usize = 32;

/// 4-byte file-format magic: "TVLI" (Thur VTL Lru Index).
pub const MAGIC: [u8; 4] = *b"TVLI";

/// Format version of the records area.
pub const VERSION: u32 = 1;

/// Per-cartridge LRU sidecar file. One u64 per chunk_id, positional.
///
/// Owns no `DirtyPageTracker` — this file is purely a local cache
/// hint and is intentionally invisible to the storage-backup path.
#[derive(Debug)]
pub struct LruIndexFile {
    path: PathBuf,
    file: File,
    next_id: AtomicU64,
}

impl LruIndexFile {
    pub fn path_for(cartridge_root: &Path) -> PathBuf {
        cartridge_root.join("lru.idx")
    }

    /// Open or create the LRU sidecar. Empty/new files get a header
    /// written; existing files have their header validated. If the
    /// header is invalid (wrong magic / version / record size), the
    /// file is rebuilt empty — losing LRU state is acceptable for a
    /// cache hint.
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
            Self::write_header(&file)?;
            return Ok(Self {
                path,
                file,
                next_id: AtomicU64::new(0),
            });
        }
        if len < HEADER_SIZE as u64 {
            // Corrupt: rebuild empty.
            file.set_len(0)?;
            Self::write_header(&file)?;
            return Ok(Self {
                path,
                file,
                next_id: AtomicU64::new(0),
            });
        }
        let mut hdr = [0u8; HEADER_SIZE];
        file.read_exact_at(&mut hdr, 0)?;
        let magic_ok = hdr[0..4] == MAGIC;
        let ver = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let rec_sz = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        if !magic_ok || ver != VERSION || rec_sz != RECORD_SIZE {
            tracing::warn!(
                "lru.idx header mismatch (magic_ok={}, ver={}, rec_sz={}); rebuilding empty",
                magic_ok,
                ver,
                rec_sz
            );
            file.set_len(0)?;
            Self::write_header(&file)?;
            return Ok(Self {
                path,
                file,
                next_id: AtomicU64::new(0),
            });
        }
        let records_bytes = len - HEADER_SIZE as u64;
        if !records_bytes.is_multiple_of(RECORD_SIZE as u64) {
            // Corrupt body — round down. Cache hint, not authoritative.
            let truncated = records_bytes - (records_bytes % RECORD_SIZE as u64);
            file.set_len(HEADER_SIZE as u64 + truncated)?;
            return Ok(Self {
                path,
                file,
                next_id: AtomicU64::new(truncated / RECORD_SIZE as u64),
            });
        }
        let next_id = records_bytes / RECORD_SIZE as u64;
        Ok(Self {
            path,
            file,
            next_id: AtomicU64::new(next_id),
        })
    }

    fn write_header(file: &File) -> Result<()> {
        let mut hdr = [0u8; HEADER_SIZE];
        hdr[0..4].copy_from_slice(&MAGIC);
        hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
        hdr[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        // hdr[12..32] reserved, zeroed.
        file.write_all_at(&hdr, 0)?;
        Ok(())
    }

    fn record_offset(id: u64) -> u64 {
        HEADER_SIZE as u64 + id * RECORD_SIZE as u64
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.load(Ordering::Acquire)
    }

    /// Append zero-valued slots until `next_id == target`. Used at
    /// cartridge open to bring the LRU file in line with `chunks.idx`
    /// when the latter was restored from storage or grew while the LRU
    /// sidecar was missing/stale.
    pub fn grow_to(&self, target: u64) -> Result<()> {
        let cur = self.next_id.load(Ordering::Acquire);
        if target <= cur {
            return Ok(());
        }
        let new_len = HEADER_SIZE as u64 + target * RECORD_SIZE as u64;
        self.file.set_len(new_len)?;
        self.next_id.store(target, Ordering::Release);
        Ok(())
    }

    /// Append a fresh slot with the given timestamp. Mirrors
    /// `ChunkIndexFile::append`: caller must ensure the LRU file's
    /// `next_id` matches the chunk index's `next_id` before the
    /// corresponding `chunks.idx` append.
    pub fn append(&self, ts: u64) -> Result<u64> {
        let id = self.next_id.load(Ordering::Acquire);
        let off = Self::record_offset(id);
        self.file.write_all_at(&ts.to_le_bytes(), off)?;
        self.next_id.store(id + 1, Ordering::Release);
        Ok(id)
    }

    /// Bump the timestamp for `id`. Read path's only mutation. 8-byte
    /// `pwrite` against `lru.idx` — no manifest-backup delta upload.
    pub fn touch(&self, id: u64, ts: u64) -> Result<()> {
        if id >= self.next_id.load(Ordering::Acquire) {
            return Err(SmcError::InvalidOp("lru index touch past next_id"));
        }
        let off = Self::record_offset(id);
        self.file.write_all_at(&ts.to_le_bytes(), off)?;
        Ok(())
    }

    /// Read the timestamp for `id`. Returns 0 if `id` is past
    /// `next_id` — treated as "never accessed" for eviction sort.
    pub fn read(&self, id: u64) -> Result<u64> {
        if id >= self.next_id.load(Ordering::Acquire) {
            return Ok(0);
        }
        let mut buf = [0u8; RECORD_SIZE];
        self.file.read_exact_at(&mut buf, Self::record_offset(id))?;
        Ok(u64::from_le_bytes(buf))
    }

    /// Truncate to `new_next_id` records. Header is preserved. Used by
    /// ERASE / FORMAT MEDIUM and by the trailing-empty-staging cleanup
    /// in `flush_and_seal`.
    pub fn truncate_to(&self, new_next_id: u64) -> Result<()> {
        let new_len = HEADER_SIZE as u64 + new_next_id * RECORD_SIZE as u64;
        self.file.set_len(new_len)?;
        self.next_id.store(new_next_id, Ordering::Release);
        Ok(())
    }

    /// Force file contents to disk. Called at chunk-roll, filemark,
    /// and Drop boundaries — same cadence as `chunks.idx::fsync`.
    pub fn fsync(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn record_size_locked() {
        assert_eq!(RECORD_SIZE, 8);
    }

    #[test]
    fn open_create_writes_header() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(lru.next_id(), 0);
        let bytes = std::fs::read(LruIndexFile::path_for(tmp.path())).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(bytes.len(), HEADER_SIZE);
    }

    #[test]
    fn append_touch_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        for i in 0..16u64 {
            let id = lru.append(i * 100).unwrap();
            assert_eq!(id, i);
        }
        assert_eq!(lru.next_id(), 16);
        assert_eq!(lru.read(7).unwrap(), 700);
        lru.touch(7, 999_999).unwrap();
        assert_eq!(lru.read(7).unwrap(), 999_999);
    }

    #[test]
    fn touch_past_end_errors() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        lru.append(0).unwrap();
        assert!(lru.touch(1, 1).is_err());
    }

    #[test]
    fn read_past_end_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        // Tolerated: eviction sort treats absent ⇒ oldest, which is fine.
        assert_eq!(lru.read(0).unwrap(), 0);
        assert_eq!(lru.read(99).unwrap(), 0);
    }

    #[test]
    fn truncate_then_grow() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        for i in 0..8 {
            lru.append(i).unwrap();
        }
        lru.truncate_to(3).unwrap();
        assert_eq!(lru.next_id(), 3);
        assert!(lru.touch(3, 1).is_err());
        lru.grow_to(10).unwrap();
        assert_eq!(lru.next_id(), 10);
        // Newly grown slots are zero-filled.
        assert_eq!(lru.read(9).unwrap(), 0);
    }

    #[test]
    fn reopen_preserves_state() {
        let tmp = TempDir::new().unwrap();
        {
            let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
            for i in 0..4u64 {
                lru.append(1_000 + i).unwrap();
            }
            lru.fsync().unwrap();
        }
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(lru.next_id(), 4);
        assert_eq!(lru.read(2).unwrap(), 1_002);
    }

    #[test]
    fn corrupt_header_rebuilt() {
        let tmp = TempDir::new().unwrap();
        let path = LruIndexFile::path_for(tmp.path());
        std::fs::write(&path, b"garbage-not-a-valid-header-padding-pad").unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(lru.next_id(), 0);
    }

    #[test]
    fn grow_idempotent_when_already_at_target() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        lru.append(42).unwrap();
        lru.grow_to(1).unwrap();
        assert_eq!(lru.next_id(), 1);
        assert_eq!(lru.read(0).unwrap(), 42);
    }

    #[test]
    fn no_dirty_sidecar_created() {
        // Critical invariant: lru.idx is intentionally local-only and
        // must NOT have a .dirty sidecar (which would route it through
        // the manifest-backup path).
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        lru.append(123).unwrap();
        lru.touch(0, 456).unwrap();
        lru.fsync().unwrap();
        let dirty_path = tmp.path().join("lru.idx.dirty");
        assert!(
            !dirty_path.exists(),
            "lru.idx must not produce a dirty-page sidecar"
        );
    }
}
