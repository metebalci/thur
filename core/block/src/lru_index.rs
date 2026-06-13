// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-volume LRU sidecar for `pages.idx`.
//!
//! The page index stores immutable per-page metadata (chunk hash +
//! allocated flag). Last-accessed timestamps used to be a cache-only
//! concept stored nowhere — eviction picked candidates uniformly.
//! Splitting LRU into a separate file mirrors the block-side
//! parallel of `core_stream::lru_index`:
//!
//! - One fixed 8-byte record per `page_id` (u64 LE epoch seconds).
//! - Positional, mirroring `pages.idx` — same `page_id` ⇒ same
//!   record index. Page ids are sparse (hosts allocate any LBA they
//!   want), so the file is grown via sparse-file holes the same
//!   way `pages.idx` is. Reading past the current file size
//!   returns 0, which we treat as "never accessed" for sort
//!   purposes — same convention as `pages.idx` unallocated.
//! - **Local-only.** Never uploaded to storage. A fresh host
//!   doing cold-bucket DR rebuilds it from scratch as zeros.
//! - Missing / corrupt header ⇒ rebuilt empty. First eviction
//!   cycle picks oldest uniformly; subsequent cycles converge as
//!   touches arrive.
//!
//! ## Layout
//!
//! ```text
//! <volume_dir>/lru.idx
//! ```
//!
//! Header (32 bytes):
//!
//! ```text
//! bytes 0..4    magic "CSLI"
//! bytes 4..8    version (u32 LE) = 1
//! bytes 8..12   record_size (u32 LE) = 8
//! bytes 12..32  reserved, zero
//! ```

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::page_index::PageId;

/// On-disk record size: 8 bytes = u64 LE epoch seconds.
pub const RECORD_SIZE: u64 = 8;

/// On-disk header size — mirrors the 32-byte VTL LRU header so the
/// two sidecars line up byte-for-byte for cross-product tooling.
pub const HEADER_SIZE: u64 = 32;

/// File-format magic — `CSLI` = core-block lru index.
pub const MAGIC: [u8; 4] = *b"CSLI";

/// Format version of the records area.
pub const VERSION: u32 = 1;

/// Filename within a volume directory.
pub const FILENAME: &str = "lru.idx";

#[derive(Error, Debug)]
pub enum LruIndexError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-volume LRU sidecar file. One `u64` epoch-seconds per
/// `page_id`, positional. Sparse — unallocated page ids consume no
/// disk on ext4/btrfs/xfs/zfs.
///
/// Purely a local cache hint — never uploaded to storage, never
/// registered with the manifest-backup path. Losing the file
/// recovers gracefully (first eviction cycle picks uniformly).
#[derive(Debug)]
pub struct LruIndexFile {
    file: File,
}

impl LruIndexFile {
    /// Resolve the LRU sidecar path within a volume directory.
    pub fn path_for(volume_dir: &Path) -> PathBuf {
        volume_dir.join(FILENAME)
    }

    /// Open or create the LRU sidecar. Empty / new files get a
    /// header written; existing files have their header validated.
    /// Invalid header (wrong magic / version / record size) ⇒ file
    /// is rebuilt empty (losing LRU state is acceptable for a
    /// cache hint).
    pub fn open_or_create(volume_dir: &Path) -> Result<Self, LruIndexError> {
        let path = Self::path_for(volume_dir);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            Self::write_header(&file)?;
            return Ok(Self { file });
        }
        if len < HEADER_SIZE {
            file.set_len(0)?;
            Self::write_header(&file)?;
            return Ok(Self { file });
        }
        let mut hdr = [0u8; HEADER_SIZE as usize];
        file.read_exact_at(&mut hdr, 0)?;
        let magic_ok = hdr[0..4] == MAGIC;
        let ver = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let rec_sz = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        if !magic_ok || ver != VERSION || u64::from(rec_sz) != RECORD_SIZE {
            tracing::warn!(
                "lru.idx header mismatch (magic_ok={}, ver={}, rec_sz={}); rebuilding empty",
                magic_ok,
                ver,
                rec_sz
            );
            file.set_len(0)?;
            Self::write_header(&file)?;
        }
        Ok(Self { file })
    }

    fn write_header(file: &File) -> Result<(), LruIndexError> {
        let mut hdr = [0u8; HEADER_SIZE as usize];
        hdr[0..4].copy_from_slice(&MAGIC);
        hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
        hdr[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        // hdr[12..32] reserved, zeroed.
        file.write_all_at(&hdr, 0)?;
        Ok(())
    }

    fn record_offset(page_id: PageId) -> u64 {
        HEADER_SIZE + u64::from(page_id) * RECORD_SIZE
    }

    /// Bump the timestamp for `page_id`. 8-byte `pwrite` — sparse
    /// pages naturally extend the file. The pwrite goes through to
    /// the OS page cache; call [`Self::sync`] for durability.
    pub fn touch(&self, page_id: PageId, ts: u64) -> Result<(), LruIndexError> {
        let off = Self::record_offset(page_id);
        self.file.write_all_at(&ts.to_le_bytes(), off)?;
        Ok(())
    }

    /// Read the timestamp for `page_id`. Returns `0` if the slot is
    /// past EOF (sparse hole, never touched) — eviction sort
    /// treats absent ⇒ oldest, which is the correct behaviour.
    pub fn read(&self, page_id: PageId) -> Result<u64, LruIndexError> {
        let mut buf = [0u8; RECORD_SIZE as usize];
        let n = self.file.read_at(&mut buf, Self::record_offset(page_id))?;
        if n < RECORD_SIZE as usize {
            return Ok(0);
        }
        Ok(u64::from_le_bytes(buf))
    }

    /// Read every timestamp in one sequential pass. Returns a vector
    /// indexed by `page_id` (`vec[page_id]` = last-touched ts; absent
    /// trailing pages read as `0` = oldest). The eviction worker's
    /// whole-volume walk uses this instead of one 8-byte random
    /// `pread` per allocated page — turning O(pages) syscalls into one
    /// sequential read per volume (issue #152). A torn trailing record
    /// (file length not a record multiple) is ignored.
    pub fn read_all(&self) -> Result<Vec<u64>, LruIndexError> {
        let len = self.file.metadata()?.len();
        if len <= HEADER_SIZE {
            return Ok(Vec::new());
        }
        let body_len = (len - HEADER_SIZE) as usize;
        let mut body = vec![0u8; body_len];
        self.file.read_exact_at(&mut body, HEADER_SIZE)?;
        let count = body_len / RECORD_SIZE as usize;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let start = i * RECORD_SIZE as usize;
            let mut buf = [0u8; RECORD_SIZE as usize];
            buf.copy_from_slice(&body[start..start + RECORD_SIZE as usize]);
            out.push(u64::from_le_bytes(buf));
        }
        Ok(out)
    }

    /// Force file contents to disk. Called at write-page boundaries
    /// alongside `pages.idx::sync`.
    pub fn sync(&self) -> Result<(), LruIndexError> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Truncate back to a bare header so every page reads `0` (oldest)
    /// again — the LRU half of an in-place snapshot restore (issue #85).
    /// LRU is a pure cache hint, so discarding it is always safe; the
    /// first post-restore eviction cycle picks uniformly and converges
    /// as fresh touches arrive. Same inode/fd; `fdatasync` makes it
    /// durable.
    pub fn reset_to_clean(&self) -> Result<(), LruIndexError> {
        self.file.set_len(0)?;
        Self::write_header(&self.file)?;
        self.file.sync_data()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn record_and_header_size_locked() {
        assert_eq!(RECORD_SIZE, 8);
        assert_eq!(HEADER_SIZE, 32);
    }

    #[test]
    fn open_create_writes_header() {
        let tmp = TempDir::new().unwrap();
        let _lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        let bytes = std::fs::read(LruIndexFile::path_for(tmp.path())).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(bytes.len(), HEADER_SIZE as usize);
    }

    #[test]
    fn touch_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        lru.touch(7, 1_700_000_000).unwrap();
        assert_eq!(lru.read(7).unwrap(), 1_700_000_000);
        lru.touch(7, 1_800_000_000).unwrap();
        assert_eq!(lru.read(7).unwrap(), 1_800_000_000);
    }

    #[test]
    fn read_past_eof_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        // Sparse: any page id is legal; never-touched ⇒ 0.
        assert_eq!(lru.read(0).unwrap(), 0);
        assert_eq!(lru.read(99_999).unwrap(), 0);
    }

    #[test]
    fn read_all_returns_every_record_indexed_by_page_id() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        lru.touch(0, 100).unwrap();
        lru.touch(2, 300).unwrap();
        lru.touch(5, 600).unwrap();
        let all = lru.read_all().unwrap();
        // File extends to the highest touched page; intermediate holes
        // read as 0 (oldest), matching per-page `read`.
        assert_eq!(all.len(), 6);
        assert_eq!(all[0], 100);
        assert_eq!(all[1], 0);
        assert_eq!(all[2], 300);
        assert_eq!(all[5], 600);
        // Bulk read agrees with the per-page random `pread` it replaces.
        for (pid, &ts) in all.iter().enumerate() {
            assert_eq!(lru.read(pid as PageId).unwrap(), ts);
        }
    }

    #[test]
    fn read_all_on_empty_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        assert!(lru.read_all().unwrap().is_empty());
    }

    #[test]
    fn touch_sparse_page_extends_file() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        // Touching a high page_id creates a sparse hole — the
        // intermediate page slots read as 0 (treated as oldest).
        lru.touch(1000, 1_750_000_000).unwrap();
        assert_eq!(lru.read(1000).unwrap(), 1_750_000_000);
        assert_eq!(lru.read(500).unwrap(), 0);
    }

    #[test]
    fn reopen_preserves_state() {
        let tmp = TempDir::new().unwrap();
        {
            let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
            lru.touch(3, 1_234_567).unwrap();
            lru.sync().unwrap();
        }
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(lru.read(3).unwrap(), 1_234_567);
    }

    #[test]
    fn corrupt_header_rebuilt_empty() {
        let tmp = TempDir::new().unwrap();
        let path = LruIndexFile::path_for(tmp.path());
        std::fs::write(&path, b"garbage-not-a-valid-header-padding-pad").unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        // Rebuilt: every slot reads as zero.
        assert_eq!(lru.read(0).unwrap(), 0);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC);
    }

    #[test]
    fn reset_to_clean_truncates_to_header() {
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        lru.touch(1, 1_700_000_000).unwrap();
        lru.touch(1000, 1_700_000_000).unwrap();

        lru.reset_to_clean().unwrap();

        // Every page reads as 0 (oldest) again.
        assert_eq!(lru.read(1).unwrap(), 0);
        assert_eq!(lru.read(1000).unwrap(), 0);

        // File is exactly the header, magic intact.
        let bytes = std::fs::read(LruIndexFile::path_for(tmp.path())).unwrap();
        assert_eq!(bytes.len(), HEADER_SIZE as usize);
        assert_eq!(&bytes[0..4], &MAGIC);

        // Still usable after reset.
        lru.touch(2, 1_800_000_000).unwrap();
        assert_eq!(lru.read(2).unwrap(), 1_800_000_000);
    }

    #[test]
    fn no_dirty_sidecar_created() {
        // Critical invariant: lru.idx is intentionally local-only and
        // must NOT have a .dirty sidecar (which would route it through
        // the manifest-backup path on the tape side; not used on VSA
        // but kept as a defensive invariant).
        let tmp = TempDir::new().unwrap();
        let lru = LruIndexFile::open_or_create(tmp.path()).unwrap();
        lru.touch(0, 123).unwrap();
        lru.sync().unwrap();
        let dirty_path = tmp.path().join("lru.idx.dirty");
        assert!(
            !dirty_path.exists(),
            "lru.idx must not produce a dirty-page sidecar"
        );
    }
}
