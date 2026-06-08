// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-volume upload-state sidecar for `pages.idx`.
//!
//! The page index says "page N has hash H." This sidecar says "is
//! that hash actually in storage yet?" — the bit the async upload
//! worker needs to gate eviction and the bit
//! [`crate::cache::PageCache::synchronize_bytes`] needs to await
//! before SCSI SYNCHRONIZE CACHE returns.
//!
//! Two responsibilities, one byte each:
//!
//! - `0x00 = Uploaded` — pool chunk (if present) is also in storage;
//!   safe to evict. Also the legacy default for volumes created
//!   before async upload landed (no sidecar present ⇒ open creates
//!   one full of zeros ⇒ every pre-existing page reads as Uploaded,
//!   which is honest because the synchronous-seal era always had the
//!   storage copy before `write_page` returned).
//! - `0x01 = LocalOnly` — pool has the chunk, the upload worker
//!   hasn't acked the PUT yet. Eviction must skip this hash;
//!   SYNCHRONIZE CACHE for any range containing this page must
//!   wait. The worker flips it back to Uploaded on completion.
//!
//! No `StorageOnly` state: post-eviction the page-index still records
//! the hash, the pool entry is gone, and `read_page` transparently
//! refetches from storage via `ChunkPool::insert_verified_bytes`.
//! Tracking "currently not in pool" doesn't add safety the
//! page-index + cache layer doesn't already provide.
//!
//! ## File format
//!
//! ```text
//! <volume_dir>/upload.idx
//! ```
//!
//! Header (16 bytes):
//!
//! ```text
//! bytes 0..4    magic "CSUI"
//! bytes 4..8    version (u32 LE) = 1
//! bytes 8..12   record_size (u32 LE) = 1
//! bytes 12..16  reserved, zero
//! ```
//!
//! Record (1 byte at `HEADER_SIZE + page_id`):
//!
//! ```text
//! 0x00  Uploaded     (also: unallocated, legacy default)
//! 0x01  LocalOnly    (pool has it, storage upload pending)
//! ```
//!
//! Sparse — unallocated page ids consume no disk. Missing / corrupt
//! header ⇒ rebuilt empty (safe: every chunk reads as Uploaded, the
//! eviction filter behaves the same as today's pre-async invariant).
//! The sidecar is local-only — never uploaded to storage, never
//! registered with the manifest-backup path.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::page_index::PageId;

/// On-disk record size: 1 byte (the upload-state enum).
pub const RECORD_SIZE: u64 = 1;

/// On-disk header size — 16 bytes, half the LRU sidecar's 32 because
/// upload state has no growth path that would benefit from more
/// reserved bytes.
pub const HEADER_SIZE: u64 = 16;

/// File-format magic — `CSUI` = core-block upload index.
pub const MAGIC: [u8; 4] = *b"CSUI";

/// Format version of the records area.
pub const VERSION: u32 = 1;

/// Filename within a volume directory.
pub const FILENAME: &str = "upload.idx";

/// Per-page upload state. One byte on disk; the [`UploadIndexFile`]
/// API converts between this enum and the raw byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UploadState {
    /// Pool chunk (if present) is also in storage. Safe to evict.
    /// Also the legacy default for volumes created before async
    /// upload landed.
    Uploaded = 0x00,
    /// Pool has the chunk, storage upload still pending. Eviction
    /// must skip; SYNCHRONIZE CACHE awaits.
    LocalOnly = 0x01,
}

impl UploadState {
    /// Decode a raw byte from the sidecar into an [`UploadState`].
    /// Unknown bytes (a future schema bump that the running daemon
    /// doesn't yet understand) fall back to `Uploaded` — the safe
    /// default: eviction may run, SYNC won't wait spuriously, and
    /// the worker's next pass will overwrite with the canonical
    /// value. A panic here would turn an unknown byte into a
    /// host-visible outage.
    pub const fn from_byte(b: u8) -> Self {
        match b {
            0x01 => Self::LocalOnly,
            _ => Self::Uploaded,
        }
    }
}

#[derive(Error, Debug)]
pub enum UploadIndexError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-volume upload-state sidecar file. One [`UploadState`] byte
/// per `page_id`, positional. Sparse — unallocated page ids consume
/// no disk on ext4/btrfs/xfs/zfs.
///
/// Local-only — never uploaded to storage, never on the
/// manifest-backup path. Losing the file recovers gracefully (every
/// slot reads as `Uploaded`, which is the safe default).
#[derive(Debug)]
pub struct UploadIndexFile {
    file: File,
}

impl UploadIndexFile {
    /// Resolve the upload sidecar path within a volume directory.
    pub fn path_for(volume_dir: &Path) -> PathBuf {
        volume_dir.join(FILENAME)
    }

    /// Open or create the upload sidecar. Empty / new files get a
    /// header written; existing files have their header validated.
    /// Invalid header (wrong magic / version / record size) ⇒ file
    /// is rebuilt empty.
    pub fn open_or_create(volume_dir: &Path) -> Result<Self, UploadIndexError> {
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
                "upload.idx header mismatch (magic_ok={}, ver={}, rec_sz={}); rebuilding empty",
                magic_ok,
                ver,
                rec_sz
            );
            file.set_len(0)?;
            Self::write_header(&file)?;
        }
        Ok(Self { file })
    }

    fn write_header(file: &File) -> Result<(), UploadIndexError> {
        let mut hdr = [0u8; HEADER_SIZE as usize];
        hdr[0..4].copy_from_slice(&MAGIC);
        hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
        hdr[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        // hdr[12..16] reserved, zeroed.
        file.write_all_at(&hdr, 0)?;
        Ok(())
    }

    fn record_offset(page_id: PageId) -> u64 {
        HEADER_SIZE + u64::from(page_id) * RECORD_SIZE
    }

    /// Write `state` for `page_id` and `fdatasync`. Use this for the
    /// per-record fence (worker completion, recovery init); the
    /// hot write path uses [`Self::set_unsynced`] + a trailing
    /// [`Self::sync`].
    pub fn set(&self, page_id: PageId, state: UploadState) -> Result<(), UploadIndexError> {
        self.set_unsynced(page_id, state)?;
        self.sync()?;
        Ok(())
    }

    /// Write `state` for `page_id` without forcing to disk. Returns
    /// once the pwrite has reached the OS page cache. The cache /
    /// uploader's batched flush calls [`Self::sync`] once at the
    /// end of a cohort instead of paying `fdatasync` per record.
    pub fn set_unsynced(
        &self,
        page_id: PageId,
        state: UploadState,
    ) -> Result<(), UploadIndexError> {
        let off = Self::record_offset(page_id);
        self.file.write_all_at(&[state as u8], off)?;
        Ok(())
    }

    /// Read `state` for `page_id`. Returns `Uploaded` if the slot is
    /// past EOF (sparse hole, never written) — the safe legacy
    /// default. Unrecognised non-zero / non-one bytes also fall back
    /// to `Uploaded` (see [`UploadState::from_byte`]).
    pub fn read(&self, page_id: PageId) -> Result<UploadState, UploadIndexError> {
        let mut buf = [0u8; RECORD_SIZE as usize];
        let n = self.file.read_at(&mut buf, Self::record_offset(page_id))?;
        if n < RECORD_SIZE as usize {
            return Ok(UploadState::Uploaded);
        }
        Ok(UploadState::from_byte(buf[0]))
    }

    /// Force file contents to disk.
    pub fn sync(&self) -> Result<(), UploadIndexError> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Truncate back to a bare header so every page reads `Uploaded`
    /// again — the sidecar half of an in-place snapshot restore (issue
    /// #85). The frozen index a restore installs references only
    /// storage-durable chunks (the snapshot-create contract), so there is
    /// nothing `LocalOnly` to track; clearing the sidecar is honest and
    /// keeps the boot-recovery scan from re-enqueuing stale pages. Same
    /// inode/fd, so a live writer keeps using the handle. `fdatasync`
    /// makes the reset durable.
    pub fn reset_to_clean(&self) -> Result<(), UploadIndexError> {
        self.file.set_len(0)?;
        Self::write_header(&self.file)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Iterate every record in the file as `(page_id, state)`.
    /// Skips the header. Stops at EOF; sparse holes are not yielded
    /// (callers see "non-existent" as "Uploaded" via
    /// [`Self::read`], which is the safe default — iterating skips
    /// them so a recovery scan doesn't enqueue uploads for every
    /// page in a sparse multi-TB volume).
    ///
    /// Used by the crash-recovery scan to find surviving
    /// `LocalOnly` pages on daemon startup.
    pub fn iter(&self) -> Result<UploadIndexIter<'_>, UploadIndexError> {
        let len = self.file.metadata()?.len();
        Ok(UploadIndexIter {
            file: &self.file,
            next_offset: HEADER_SIZE,
            len,
        })
    }
}

/// Iterator over `(page_id, UploadState)` records in the file. See
/// [`UploadIndexFile::iter`].
pub struct UploadIndexIter<'a> {
    file: &'a File,
    next_offset: u64,
    len: u64,
}

impl Iterator for UploadIndexIter<'_> {
    type Item = Result<(PageId, UploadState), UploadIndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_offset >= self.len {
            return None;
        }
        let mut buf = [0u8; RECORD_SIZE as usize];
        let read_off = self.next_offset;
        match self.file.read_at(&mut buf, read_off) {
            Ok(0) => None,
            Ok(_n) => {
                let page_idx = (read_off - HEADER_SIZE) / RECORD_SIZE;
                let pid = match PageId::try_from(page_idx) {
                    Ok(p) => p,
                    Err(_) => {
                        // Page id past u32::MAX shouldn't happen with
                        // current limits (1 PB volume @ 256 KiB pages).
                        // Stop iteration if it does.
                        return None;
                    }
                };
                self.next_offset += RECORD_SIZE;
                Some(Ok((pid, UploadState::from_byte(buf[0]))))
            }
            Err(e) => Some(Err(UploadIndexError::Io(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn record_and_header_size_locked() {
        assert_eq!(RECORD_SIZE, 1);
        assert_eq!(HEADER_SIZE, 16);
    }

    #[test]
    fn open_create_writes_header() {
        let tmp = TempDir::new().unwrap();
        let _u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        let bytes = std::fs::read(UploadIndexFile::path_for(tmp.path())).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(bytes.len(), HEADER_SIZE as usize);
    }

    #[test]
    fn set_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        u.set(7, UploadState::LocalOnly).unwrap();
        assert_eq!(u.read(7).unwrap(), UploadState::LocalOnly);
        u.set(7, UploadState::Uploaded).unwrap();
        assert_eq!(u.read(7).unwrap(), UploadState::Uploaded);
    }

    #[test]
    fn read_past_eof_returns_uploaded() {
        // Legacy default: missing record ⇒ Uploaded (safe to evict,
        // mirrors the pre-async invariant).
        let tmp = TempDir::new().unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(u.read(0).unwrap(), UploadState::Uploaded);
        assert_eq!(u.read(99_999).unwrap(), UploadState::Uploaded);
    }

    #[test]
    fn set_sparse_page_extends_file() {
        let tmp = TempDir::new().unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        u.set(1000, UploadState::LocalOnly).unwrap();
        assert_eq!(u.read(1000).unwrap(), UploadState::LocalOnly);
        // Intermediate pages stay at default.
        assert_eq!(u.read(500).unwrap(), UploadState::Uploaded);
    }

    #[test]
    fn reopen_preserves_state() {
        let tmp = TempDir::new().unwrap();
        {
            let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
            u.set(3, UploadState::LocalOnly).unwrap();
        }
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(u.read(3).unwrap(), UploadState::LocalOnly);
    }

    #[test]
    fn corrupt_header_rebuilt_empty() {
        let tmp = TempDir::new().unwrap();
        let path = UploadIndexFile::path_for(tmp.path());
        std::fs::write(&path, b"garbage-not-a-header-").unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        // Rebuilt: every slot reads as Uploaded.
        assert_eq!(u.read(0).unwrap(), UploadState::Uploaded);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], &MAGIC);
    }

    #[test]
    fn unknown_byte_falls_back_to_uploaded() {
        // Defensive: a future schema variant the running daemon
        // doesn't yet understand reads as Uploaded (safe default).
        let tmp = TempDir::new().unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        // Manually pwrite an out-of-range byte at page 5.
        let path = UploadIndexFile::path_for(tmp.path());
        let mut bytes = std::fs::read(&path).unwrap();
        let off = (HEADER_SIZE + 5) as usize;
        while bytes.len() <= off {
            bytes.push(0);
        }
        bytes[off] = 0xAB;
        std::fs::write(&path, &bytes).unwrap();
        // Re-open and confirm the unknown byte falls back to
        // Uploaded.
        let u2 = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        assert_eq!(u2.read(5).unwrap(), UploadState::Uploaded);
        drop(u);
    }

    #[test]
    fn iter_yields_each_record_after_header() {
        let tmp = TempDir::new().unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        u.set(0, UploadState::Uploaded).unwrap();
        u.set(1, UploadState::LocalOnly).unwrap();
        u.set(2, UploadState::Uploaded).unwrap();
        let items: Vec<_> = u.iter().unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], (0, UploadState::Uploaded));
        assert_eq!(items[1], (1, UploadState::LocalOnly));
        assert_eq!(items[2], (2, UploadState::Uploaded));
    }

    #[test]
    fn iter_on_empty_file_yields_nothing() {
        let tmp = TempDir::new().unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        let items: Vec<_> = u.iter().unwrap().collect::<Result<_, _>>().unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn reset_to_clean_truncates_to_header() {
        let tmp = TempDir::new().unwrap();
        let u = UploadIndexFile::open_or_create(tmp.path()).unwrap();
        u.set(1, UploadState::LocalOnly).unwrap();
        u.set(1000, UploadState::LocalOnly).unwrap();

        u.reset_to_clean().unwrap();

        // Every page reads as Uploaded again, and the iter (recovery
        // scan) yields nothing.
        assert_eq!(u.read(1).unwrap(), UploadState::Uploaded);
        assert_eq!(u.read(1000).unwrap(), UploadState::Uploaded);
        assert!(u.iter().unwrap().next().is_none());

        // File is exactly the header, magic intact.
        let bytes = std::fs::read(UploadIndexFile::path_for(tmp.path())).unwrap();
        assert_eq!(bytes.len(), HEADER_SIZE as usize);
        assert_eq!(&bytes[0..4], &MAGIC);

        // Still usable after reset.
        u.set(2, UploadState::LocalOnly).unwrap();
        assert_eq!(u.read(2).unwrap(), UploadState::LocalOnly);
    }
}
