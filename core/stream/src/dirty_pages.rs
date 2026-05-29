// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-file dirty-page tracker for delta cloud backup.
//!
//! Used by `BlockIndexFile` and `ChunkIndexFile` to track which 1 MiB
//! pages of the index have changed since the last cloud upload, so the
//! manifest-backup path can PUT only the dirty pages instead of the
//! whole file. Both index files are flat fixed-record arrays at known
//! offsets, so a coarse page-level dirty bitmap captures all the
//! information the upload path needs.
//!
//! ## On-disk layout
//!
//! Each tracker has a sidecar file at `<index>.dirty` shaped like:
//!
//! ```text
//! bytes  0..4    MAGIC ("NVDP")
//! bytes  4..8    VERSION (u32 LE)
//! bytes  8..12   page size in bytes (u32 LE)
//! bytes 12..20   epoch (u64 LE — bumped each successful upload pass)
//! bytes 20..32   reserved, zeroed
//! bytes 32..     packed bitmap (1 bit per page, LSB first within each byte)
//! ```
//!
//! The bitmap covers `ceil(file_size / page_size)` pages. The sidecar
//! is rewritten in full on `persist()` — it is tiny (at LTO-8 worst
//! case, a 3.2 GB block index needs ~3200 pages → 400 bytes of
//! bitmap), so per-byte delta tracking would be wasted complexity.
//!
//! ## In-memory state
//!
//! The bitmap lives behind a `Mutex<Vec<u64>>`. The hot path
//! (`mark_range`) is called once per `pwrite_at` on the block / chunk
//! index — both of which are already serialized at the cartridge
//! level, so contention is nil and a `Mutex` is preferred over the
//! ergonomic clutter of atomics. `snapshot()` and `clear_pages()` are
//! called only on the manifest-backup path (rare).
//!
//! ## Crash semantics
//!
//! Pages are marked dirty *before* the underlying `pwrite_at` returns
//! — the bitmap is conservative. On daemon crash mid-write, the
//! sidecar may reflect a dirty page that the index file's `pwrite_at`
//! didn't actually finish; that's fine, the next backup will simply
//! re-upload an unchanged page. Missing-dirty (a `pwrite_at` landed
//! but the bitmap update was lost) is the dangerous case — we avoid
//! it by marking dirty *first*, then writing.
//!
//! `persist()` fsyncs the sidecar; callers should call it at the same
//! durability boundaries that fsync the main index file (chunk-roll,
//! filemark, drop). Sidecar fsync precedes the upload-clear path so a
//! crash between "PUT page" and "clear bit" leaves the page marked
//! dirty for the next cycle (also harmless re-upload).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::errors::{Result, SmcError};

/// Page granularity for the dirty bitmap. 1 MiB chosen so that a full
/// LTO-8 block index (~3.2 GB worst case) fits in ~3200 pages /
/// ~400 bytes of bitmap, while still keeping per-page upload payloads
/// small enough to fit a single S3 PUT comfortably.
pub const PAGE_SIZE: u32 = 1024 * 1024;

const HEADER_SIZE: usize = 32;
const MAGIC: [u8; 4] = *b"NVDP";
const VERSION: u32 = 1;

/// Snapshot of the bitmap at a point in time. Returned by
/// `snapshot()`; the upload path consumes this directly.
#[derive(Debug, Clone)]
pub struct DirtySnapshot {
    /// Page indices that are currently dirty, sorted ascending.
    pub pages: Vec<u32>,
    /// Epoch at the moment of the snapshot. Stamped into the manifest
    /// sentinel so a restore can match pages to a known-good upload
    /// pass.
    pub epoch: u64,
}

/// Persistent dirty-page bitmap for one index file. See module docs.
#[derive(Debug)]
pub struct DirtyPageTracker {
    sidecar: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Packed bitmap; bit i indicates page i is dirty.
    bits: Vec<u64>,
    /// Last persisted epoch; bumped on every successful upload pass.
    epoch: u64,
}

impl DirtyPageTracker {
    /// Sidecar path for an index file at `index_path`.
    pub fn sidecar_path(index_path: &Path) -> PathBuf {
        let mut s = index_path.as_os_str().to_owned();
        s.push(".dirty");
        PathBuf::from(s)
    }

    /// Open or create a tracker for an index file. If the sidecar
    /// already exists its bitmap and epoch are loaded; otherwise an
    /// empty bitmap (no pages dirty) is returned.
    ///
    /// `index_path` is the *index* file path — the sidecar lives
    /// alongside at `<index_path>.dirty`.
    pub fn open_or_create(index_path: &Path) -> Result<Self> {
        let sidecar = Self::sidecar_path(index_path);
        let inner = match File::open(&sidecar) {
            Ok(file) => Self::load(&file)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Inner {
                bits: Vec::new(),
                epoch: 0,
            },
            Err(e) => return Err(SmcError::Io(e)),
        };
        Ok(Self {
            sidecar,
            inner: Mutex::new(inner),
        })
    }

    fn load(file: &File) -> Result<Inner> {
        let len = file.metadata()?.len() as usize;
        if len < HEADER_SIZE {
            return Err(SmcError::InvalidOp(
                "dirty-page sidecar shorter than header — corrupt",
            ));
        }
        let mut hdr = [0u8; HEADER_SIZE];
        file.read_exact_at(&mut hdr, 0)?;
        if hdr[0..4] != MAGIC {
            return Err(SmcError::InvalidOp(
                "dirty-page sidecar magic mismatch — wrong file or corrupt",
            ));
        }
        let ver = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if ver != VERSION {
            return Err(SmcError::InvalidOp(
                "dirty-page sidecar version unsupported by this build",
            ));
        }
        let page_sz = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        if page_sz != PAGE_SIZE {
            return Err(SmcError::InvalidOp(
                "dirty-page sidecar page size disagrees with build",
            ));
        }
        let epoch = u64::from_le_bytes([
            hdr[12], hdr[13], hdr[14], hdr[15], hdr[16], hdr[17], hdr[18], hdr[19],
        ]);
        let bitmap_bytes = len - HEADER_SIZE;
        // Round up to multiple of 8 because we read into u64 words.
        let words = bitmap_bytes.div_ceil(8);
        let mut bits = vec![0u64; words];
        if bitmap_bytes > 0 {
            let mut buf = vec![0u8; bitmap_bytes];
            file.read_exact_at(&mut buf, HEADER_SIZE as u64)?;
            for (i, chunk) in buf.chunks(8).enumerate() {
                let mut word_buf = [0u8; 8];
                word_buf[..chunk.len()].copy_from_slice(chunk);
                bits[i] = u64::from_le_bytes(word_buf);
            }
        }
        Ok(Inner { bits, epoch })
    }

    /// Mark every page that overlaps `[offset, offset + len)` as dirty.
    /// Cheap: bit-set on a `Vec<u64>` under a mutex. Does not fsync;
    /// callers `persist()` at the same boundary they fsync the index
    /// file.
    pub fn mark_range(&self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        // Page indices are u32. The division is done in u64 first so
        // the intermediate can't truncate; the debug_assert documents
        // the bound (u32::MAX pages * 64 KiB ~= 256 TiB) that today's
        // device sizes stay well under.
        let first_u64 = offset / PAGE_SIZE as u64;
        let last_u64 = (offset + len - 1) / PAGE_SIZE as u64;
        debug_assert!(
            last_u64 <= u32::MAX as u64,
            "page index {last_u64} exceeds u32 (offset {offset}, len {len})"
        );
        let first = first_u64 as u32;
        let last = last_u64 as u32;
        let mut inner = self.inner.lock().expect("dirty-page mutex poisoned");
        Self::ensure_capacity(&mut inner.bits, last);
        for page in first..=last {
            let word = (page as usize) / 64;
            let bit = (page as usize) % 64;
            inner.bits[word] |= 1u64 << bit;
        }
    }

    fn ensure_capacity(bits: &mut Vec<u64>, max_page: u32) {
        let needed_words = (max_page as usize / 64) + 1;
        if bits.len() < needed_words {
            bits.resize(needed_words, 0);
        }
    }

    /// Truncate the bitmap so pages with index `>= new_page_count` are
    /// dropped. Used on `truncate_to` (ERASE / FORMAT MEDIUM) where
    /// the underlying index file shrinks. Pages strictly inside the
    /// new bound keep their dirty state; the partially-overlapping
    /// boundary page is marked dirty (truncate is itself a mutation
    /// the upload path needs to publish).
    pub fn truncate_to_pages(&self, new_page_count: u32) {
        let mut inner = self.inner.lock().expect("dirty-page mutex poisoned");
        let needed_words = (new_page_count as usize).div_ceil(64);
        if inner.bits.len() > needed_words {
            inner.bits.truncate(needed_words);
        }
        // Clear bits past new_page_count within the trailing word.
        let bits_used = (new_page_count as usize) % 64;
        if bits_used != 0 && !inner.bits.is_empty() {
            let last = inner
                .bits
                .last_mut()
                .expect("non-empty bitmap has a last word");
            let mask = (1u64 << bits_used) - 1;
            *last &= mask;
        }
        // Boundary page is dirtied if it now contains the new tail.
        if new_page_count > 0 {
            let boundary = new_page_count - 1;
            let word = (boundary as usize) / 64;
            let bit = (boundary as usize) % 64;
            if word < inner.bits.len() {
                inner.bits[word] |= 1u64 << bit;
            }
        }
    }

    /// Snapshot the currently-dirty pages plus the current epoch.
    /// Cheap (one mutex acquire, one Vec scan); does not modify state.
    pub fn snapshot(&self) -> DirtySnapshot {
        let inner = self.inner.lock().expect("dirty-page mutex poisoned");
        let mut pages = Vec::new();
        for (word_idx, &word) in inner.bits.iter().enumerate() {
            if word == 0 {
                continue;
            }
            let mut w = word;
            while w != 0 {
                let b = w.trailing_zeros() as usize;
                pages.push((word_idx * 64 + b) as u32);
                w &= w - 1;
            }
        }
        DirtySnapshot {
            pages,
            epoch: inner.epoch,
        }
    }

    /// Clear the given page indices from the bitmap. Called by the
    /// upload worker after each page PUT succeeds so a crash between
    /// PUTs leaves the still-dirty pages re-uploadable on the next
    /// cycle.
    pub fn clear_pages(&self, pages: &[u32]) {
        if pages.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().expect("dirty-page mutex poisoned");
        for &page in pages {
            let word = (page as usize) / 64;
            let bit = (page as usize) % 64;
            if word < inner.bits.len() {
                inner.bits[word] &= !(1u64 << bit);
            }
        }
    }

    /// Bump the epoch. Called once per successful upload pass after
    /// every dirty page has been PUT — restore matches the sentinel's
    /// epoch against this value to detect a torn upload.
    pub fn bump_epoch(&self) -> u64 {
        let mut inner = self.inner.lock().expect("dirty-page mutex poisoned");
        inner.epoch = inner.epoch.saturating_add(1);
        inner.epoch
    }

    pub fn current_epoch(&self) -> u64 {
        self.inner.lock().expect("dirty-page mutex poisoned").epoch
    }

    /// True iff at least one page is currently marked dirty.
    pub fn any_dirty(&self) -> bool {
        let inner = self.inner.lock().expect("dirty-page mutex poisoned");
        inner.bits.iter().any(|&w| w != 0)
    }

    /// Write the sidecar to disk and fsync. Called at the same
    /// durability boundaries that fsync the index file.
    pub fn persist(&self) -> Result<()> {
        let (bits, epoch) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| SmcError::InvalidOp("dirty-page mutex poisoned"))?;
            (inner.bits.clone(), inner.epoch)
        };
        let mut buf = Vec::with_capacity(HEADER_SIZE + bits.len() * 8);
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&PAGE_SIZE.to_le_bytes());
        buf.extend_from_slice(&epoch.to_le_bytes());
        buf.extend_from_slice(&[0u8; 12]); // reserved
        for word in &bits {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.sidecar)?;
        file.write_all_at(&buf, 0)?;
        file.sync_data()?;
        Ok(())
    }

    /// Sidecar path for diagnostics / tests.
    pub fn sidecar(&self) -> &Path {
        &self.sidecar
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn idx(tmp: &TempDir) -> PathBuf {
        tmp.path().join("test.idx")
    }

    #[test]
    fn empty_tracker_is_clean() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        assert!(!t.any_dirty());
        let s = t.snapshot();
        assert!(s.pages.is_empty());
        assert_eq!(s.epoch, 0);
    }

    #[test]
    fn mark_range_within_one_page() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        t.mark_range(0, 16);
        let s = t.snapshot();
        assert_eq!(s.pages, vec![0]);
    }

    #[test]
    fn mark_range_spans_pages() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        // Range straddling pages 1, 2, 3.
        let start = PAGE_SIZE as u64 + 100;
        let len = 2 * PAGE_SIZE as u64;
        t.mark_range(start, len);
        let s = t.snapshot();
        assert_eq!(s.pages, vec![1, 2, 3]);
    }

    #[test]
    fn mark_range_zero_len_no_op() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        t.mark_range(123, 0);
        assert!(!t.any_dirty());
    }

    #[test]
    fn clear_pages_drops_bits() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        t.mark_range(0, 5 * PAGE_SIZE as u64);
        let s = t.snapshot();
        assert_eq!(s.pages, vec![0, 1, 2, 3, 4]);
        t.clear_pages(&[1, 3]);
        assert_eq!(t.snapshot().pages, vec![0, 2, 4]);
    }

    #[test]
    fn persist_and_reload() {
        let tmp = TempDir::new().unwrap();
        let path = idx(&tmp);
        {
            let t = DirtyPageTracker::open_or_create(&path).unwrap();
            t.mark_range(0, 1);
            t.mark_range(1000 * PAGE_SIZE as u64, 1);
            t.bump_epoch();
            t.bump_epoch();
            t.persist().unwrap();
        }
        let t = DirtyPageTracker::open_or_create(&path).unwrap();
        let s = t.snapshot();
        assert_eq!(s.pages, vec![0, 1000]);
        assert_eq!(s.epoch, 2);
    }

    #[test]
    fn truncate_to_pages_drops_high_bits_and_dirties_boundary() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        for p in 0..10u32 {
            t.mark_range(p as u64 * PAGE_SIZE as u64, 1);
        }
        // Clear all so we can observe the truncate-induced dirty bit.
        t.clear_pages(&(0..10).collect::<Vec<_>>());
        assert!(!t.any_dirty());
        t.truncate_to_pages(3);
        let s = t.snapshot();
        assert_eq!(s.pages, vec![2]); // boundary page (3-1)
    }

    #[test]
    fn truncate_to_pages_zero_clears() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        for p in 0..4u32 {
            t.mark_range(p as u64 * PAGE_SIZE as u64, 1);
        }
        t.truncate_to_pages(0);
        // No boundary page when shrinking to zero.
        assert!(!t.any_dirty());
    }

    #[test]
    fn snapshot_sorted_ascending() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        // Mark in non-monotonic order; snapshot must still be sorted.
        for p in [50u32, 1, 200, 0, 64, 65] {
            t.mark_range(p as u64 * PAGE_SIZE as u64, 1);
        }
        let mut expected = vec![0, 1, 50, 64, 65, 200];
        expected.sort();
        assert_eq!(t.snapshot().pages, expected);
    }

    #[test]
    fn corrupt_sidecar_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = idx(&tmp);
        let sidecar = DirtyPageTracker::sidecar_path(&path);
        std::fs::write(&sidecar, b"XXXXversion-bytes-and-the-rest-is-bogus").unwrap();
        let err = DirtyPageTracker::open_or_create(&path).unwrap_err();
        assert!(matches!(err, SmcError::InvalidOp(_)));
    }

    #[test]
    fn large_page_index_grows_bitmap() {
        let tmp = TempDir::new().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        // 100k pages -> ~12.5 KiB bitmap. Should not panic, must
        // round-trip through persist().
        let big = 100_000u32;
        t.mark_range(big as u64 * PAGE_SIZE as u64, 1);
        t.persist().unwrap();
        let t = DirtyPageTracker::open_or_create(&idx(&tmp)).unwrap();
        assert_eq!(t.snapshot().pages, vec![big]);
    }
}
