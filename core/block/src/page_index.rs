// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-volume page index — sparse `page_id → BLAKE3 hash` map
//! persisted at `<data_dir>/volumes/<name>/pages.idx`.
//!
//! Layout philosophy mirrors thurvtl's `chunks.idx`: fixed-size
//! header + fixed-size records keyed by positional offset
//! (`offset = HEADER_SIZE + page_id * RECORD_SIZE`). Unlike
//! thurvtl, page ids are sparse (hosts allocate any LBA they want),
//! so the file is grown via sparse-file holes — unallocated pages
//! consume zero disk on ext4 / btrfs / xfs / zfs. Reading past the
//! current file size returns zero bytes, which we treat as
//! unallocated.
//!
//! ## File format
//!
//! Header (64 bytes):
//!
//! ```text
//! bytes 0..4    magic "CRPI"
//! bytes 4..8    schema_version (u32 LE) = 1
//! bytes 8..12   record_size (u32 LE) = 64
//! bytes 12..16  reserved
//! bytes 16..32  volume_uuid (binds the index to its volume)
//! bytes 32..40  page_size_bytes (u64 LE)
//! bytes 40..64  reserved
//! ```
//!
//! Record (64 bytes):
//!
//! ```text
//! bytes 0..32   BLAKE3 chunk hash (valid iff allocated)
//! byte 32       flags
//!                 bit 0  allocated
//!                 bits 1..8 reserved
//! bytes 33..64  reserved
//! ```
//!
//! ## Crash semantics
//!
//! Each `set` / `clear` is a single `pwrite_at(64-byte record)`
//! followed by `sync_data`. A torn write loses at most that one
//! record; the daemon will detect the inconsistency on the next
//! read and re-upload the affected page. There's no journal —
//! defer until we have evidence the cost is real.
//!
//! The cache's parallel-flush drain takes the cheaper path: each
//! per-page commit is a [`PageIndex::set_unsynced`] (pwrite only,
//! no `sync_data`), and one [`PageIndex::sync`] at the end of the
//! cohort makes the whole batch durable. SCSI SYNCHRONIZE CACHE
//! and the eviction-induced flush both go through the same
//! end-of-cohort `sync()` so the write-back-fence contract is
//! preserved. `set` / `clear` themselves keep their strict
//! "returns means durable" semantics for the few call sites (UNMAP,
//! external `write_page`) where the caller wants a per-record
//! fence.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use thiserror::Error;

/// Filename for the page index within a volume directory.
pub const FILENAME: &str = "pages.idx";

/// Header size in bytes. Records start here.
pub const HEADER_SIZE: u64 = 64;

/// Record size in bytes (every page slot, allocated or not, is
/// this many bytes).
pub const RECORD_SIZE: u64 = 64;

/// File-format magic — `CRPI` = core-block block-page index.
pub const MAGIC: [u8; 4] = *b"CRPI";

/// Page-index format version. Bumped on schema breaks.
pub const SCHEMA_VERSION: u32 = 1;

/// Logical page id (offset / page_size). 32-bit limit caps a 1 PB
/// volume at 256 KiB pages — bump to u64 in v2 if larger volumes
/// are ever needed.
pub type PageId = u32;

/// 32-byte BLAKE3 chunk hash. Same as thurvtl's chunk pool.
pub type ChunkHash = [u8; 32];

const FLAG_ALLOCATED: u8 = 0b0000_0001;

#[derive(Error, Debug)]
pub enum PageIndexError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a thurvsa page index (magic mismatch)")]
    BadMagic,

    #[error("page-index schema version {found} not understood (expected {expected})")]
    SchemaMismatch { found: u32, expected: u32 },

    #[error("record size in header ({found} B) differs from compiled-in size ({expected} B)")]
    RecordSizeMismatch { found: u32, expected: u32 },

    #[error("page-index uuid mismatch: index says {index_uuid}, volume says {volume_uuid}")]
    UuidMismatch {
        index_uuid: String,
        volume_uuid: String,
    },

    #[error("page-index page size mismatch: index says {index} B, volume says {volume} B")]
    PageSizeMismatch { index: u64, volume: u64 },
}

/// Sparse `page_id → ChunkHash` map. Always-on-disk — no
/// in-memory cache; every `get` / `set` / `clear` is a single
/// `pread_at` / `pwrite_at`. The OS page cache handles the hot
/// pages naturally.
#[derive(Debug)]
pub struct PageIndex {
    file: File,
    volume_uuid: [u8; 16],
    page_size_bytes: u64,
}

impl PageIndex {
    /// Resolve the page-index path within a volume directory.
    pub fn path_for(volume_dir: &Path) -> std::path::PathBuf {
        volume_dir.join(FILENAME)
    }

    /// Create a fresh page index. Refuses to overwrite an existing
    /// file (use `OpenOptions::create_new` semantics).
    pub fn create(
        path: &Path,
        volume_uuid: [u8; 16],
        page_size_bytes: u64,
    ) -> Result<Self, PageIndexError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let header = build_header(volume_uuid, page_size_bytes);
        file.write_at(&header, 0)?;
        file.sync_all()?;
        Ok(Self {
            file,
            volume_uuid,
            page_size_bytes,
        })
    }

    /// Open an existing page index and verify its header binds it
    /// to the given volume identity. Mismatches are loud errors —
    /// pointing the daemon at the wrong index file is the kind of
    /// bug we want to catch immediately, not silently corrupt with.
    pub fn open(
        path: &Path,
        expected_uuid: [u8; 16],
        expected_page_size: u64,
    ) -> Result<Self, PageIndexError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut header = [0u8; HEADER_SIZE as usize];
        file.read_exact_at(&mut header, 0)?;

        if header[0..4] != MAGIC {
            return Err(PageIndexError::BadMagic);
        }
        let version = read_u32_le(&header[4..8]);
        if version != SCHEMA_VERSION {
            return Err(PageIndexError::SchemaMismatch {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }
        let record_size = read_u32_le(&header[8..12]);
        if u64::from(record_size) != RECORD_SIZE {
            return Err(PageIndexError::RecordSizeMismatch {
                found: record_size,
                expected: RECORD_SIZE as u32,
            });
        }
        let mut volume_uuid = [0u8; 16];
        volume_uuid.copy_from_slice(&header[16..32]);
        let page_size_bytes = read_u64_le(&header[32..40]);

        if volume_uuid != expected_uuid {
            return Err(PageIndexError::UuidMismatch {
                index_uuid: hex::encode(volume_uuid),
                volume_uuid: hex::encode(expected_uuid),
            });
        }
        if page_size_bytes != expected_page_size {
            return Err(PageIndexError::PageSizeMismatch {
                index: page_size_bytes,
                volume: expected_page_size,
            });
        }

        Ok(Self {
            file,
            volume_uuid,
            page_size_bytes,
        })
    }

    /// Look up a page. Returns `None` for unallocated pages
    /// (including any page id past the current file size — sparse
    /// holes count as unallocated).
    pub fn get(&self, page_id: PageId) -> Result<Option<ChunkHash>, PageIndexError> {
        let offset = HEADER_SIZE + u64::from(page_id) * RECORD_SIZE;
        let mut record = [0u8; RECORD_SIZE as usize];
        let n = self.file.read_at(&mut record, offset)?;
        if n < RECORD_SIZE as usize {
            return Ok(None);
        }
        if record[32] & FLAG_ALLOCATED == 0 {
            return Ok(None);
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&record[0..32]);
        Ok(Some(hash))
    }

    /// Bind `page_id` to `hash`. Overwrites any prior mapping
    /// (the chunk-pool side handles refcount / orphan reclaim).
    /// `sync_data` is called after the write so a crash can lose
    /// at most this one update.
    pub fn set(&self, page_id: PageId, hash: &ChunkHash) -> Result<(), PageIndexError> {
        self.set_unsynced(page_id, hash)?;
        self.sync()
    }

    /// Bind `page_id` to `hash` *without* the trailing `sync_data`.
    /// The pwrite goes through to the OS page cache; durability
    /// requires a later [`Self::sync`] (or an OS-level flush).
    /// Used by the cache's parallel-flush drain so an N-page cohort
    /// pays one `fdatasync` instead of N redundant ones — see the
    /// crate-level "Crash semantics" comment.
    pub fn set_unsynced(&self, page_id: PageId, hash: &ChunkHash) -> Result<(), PageIndexError> {
        let offset = HEADER_SIZE + u64::from(page_id) * RECORD_SIZE;
        let mut record = [0u8; RECORD_SIZE as usize];
        record[0..32].copy_from_slice(hash);
        record[32] = FLAG_ALLOCATED;
        self.file.write_at(&record, offset)?;
        Ok(())
    }

    /// Flush every pwrite issued via `set_unsynced` / `clear_unsynced`
    /// to disk. Maps to `fdatasync(2)`; concurrent callers from
    /// different tasks are safe (the syscall is idempotent under
    /// concurrent invocation).
    pub fn sync(&self) -> Result<(), PageIndexError> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Drop the mapping for `page_id`. Reads of the slot
    /// afterward return `None`. The pool-side chunk that used to
    /// back the page is left for GC to sweep.
    pub fn clear(&self, page_id: PageId) -> Result<(), PageIndexError> {
        let offset = HEADER_SIZE + u64::from(page_id) * RECORD_SIZE;
        let zero = [0u8; RECORD_SIZE as usize];
        self.file.write_at(&zero, offset)?;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn volume_uuid(&self) -> &[u8; 16] {
        &self.volume_uuid
    }

    pub fn page_size_bytes(&self) -> u64 {
        self.page_size_bytes
    }

    /// Iterate every allocated `(page_id, hash)` pair in
    /// page-id order. Skips unallocated holes. Streams 64 KiB at
    /// a time so the cost is linear in *file size*, not allocated
    /// page count.
    pub fn iter(&self) -> PageIndexIter<'_> {
        PageIndexIter {
            file: &self.file,
            buf: vec![0u8; 64 * 1024],
            buf_filled: 0,
            buf_consumed: 0,
            file_offset: HEADER_SIZE,
            next_page_id: 0,
            done: false,
        }
    }

    /// The highest allocated page id, or `None` if the volume has no
    /// allocated pages. [`Self::iter`] yields in ascending page-id
    /// order, so the last entry is the maximum. O(file size). Used by
    /// `volume resize --shrink-to-fit` to find the smallest size that
    /// keeps every allocated page, and by the shrink guard rail to
    /// detect allocated data past a proposed new end (issue #77).
    pub fn highest_allocated_page(&self) -> Result<Option<PageId>, PageIndexError> {
        let mut highest = None;
        for entry in self.iter() {
            let (pid, _) = entry?;
            highest = Some(pid);
        }
        Ok(highest)
    }

    /// Drop every record at or beyond `from_page_id`. The index is a
    /// positional file (`offset = HEADER + page_id * RECORD_SIZE`), so
    /// truncating it to the boundary offset makes every higher slot read
    /// as an EOF hole — i.e. unallocated — in one `ftruncate` instead of
    /// a `clear` per dropped page. Only ever shrinks the file: a no-op
    /// when the boundary is already at or past the current length.
    /// `sync_data` makes the trim durable before returning.
    ///
    /// The pool-side chunks the dropped pages referenced are left for
    /// `system gc` to sweep, matching `volume destroy` (issue #77).
    pub fn truncate_from(&self, from_page_id: PageId) -> Result<(), PageIndexError> {
        let boundary = HEADER_SIZE + u64::from(from_page_id) * RECORD_SIZE;
        let current = self.file.metadata()?.len();
        if boundary >= current {
            return Ok(());
        }
        self.file.set_len(boundary)?;
        self.file.sync_data()?;
        Ok(())
    }
}

fn build_header(volume_uuid: [u8; 16], page_size_bytes: u64) -> [u8; HEADER_SIZE as usize] {
    let mut header = [0u8; HEADER_SIZE as usize];
    header[0..4].copy_from_slice(&MAGIC);
    header[4..8].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
    // 12..16 reserved zero
    header[16..32].copy_from_slice(&volume_uuid);
    header[32..40].copy_from_slice(&page_size_bytes.to_le_bytes());
    // 40..64 reserved zero
    header
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(buf)
}

fn read_u64_le(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

/// Streaming iterator over allocated entries. Reads in 64 KiB
/// chunks (= 1024 records per read) to amortize syscall cost; the
/// per-step API still hands the caller one record at a time.
pub struct PageIndexIter<'a> {
    file: &'a File,
    buf: Vec<u8>,
    buf_filled: usize,
    buf_consumed: usize,
    file_offset: u64,
    next_page_id: u64,
    done: bool,
}

impl Iterator for PageIndexIter<'_> {
    type Item = Result<(PageId, ChunkHash), PageIndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if self.buf_consumed + RECORD_SIZE as usize > self.buf_filled {
                // Refill from disk.
                let n = match self.file.read_at(&mut self.buf, self.file_offset) {
                    Ok(n) => n,
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e.into()));
                    }
                };
                if n == 0 {
                    self.done = true;
                    return None;
                }
                self.buf_filled = n;
                self.buf_consumed = 0;
                self.file_offset += n as u64;
                if n < RECORD_SIZE as usize {
                    // Partial trailing record — treat as EOF.
                    self.done = true;
                    return None;
                }
            }
            let start = self.buf_consumed;
            let end = start + RECORD_SIZE as usize;
            let record = &self.buf[start..end];
            self.buf_consumed = end;
            let pid = self.next_page_id;
            self.next_page_id += 1;
            if pid > u64::from(PageId::MAX) {
                self.done = true;
                return None;
            }
            if record[32] & FLAG_ALLOCATED != 0 {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&record[0..32]);
                #[allow(clippy::cast_possible_truncation)]
                return Some(Ok((pid as PageId, hash)));
            }
            // Else loop and try next record.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_uuid() -> [u8; 16] {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = i as u8;
        }
        u
    }

    fn fixture_hash(seed: u8) -> ChunkHash {
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        h
    }

    #[test]
    fn create_then_open_round_trips_identity() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let uuid = fixture_uuid();

        {
            let _idx = PageIndex::create(&path, uuid, 65_536).unwrap();
        }

        let opened = PageIndex::open(&path, uuid, 65_536).unwrap();
        assert_eq!(opened.volume_uuid(), &uuid);
        assert_eq!(opened.page_size_bytes(), 65_536);
    }

    #[test]
    fn open_rejects_uuid_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let uuid_a = fixture_uuid();
        let mut uuid_b = uuid_a;
        uuid_b[0] ^= 0xFF;

        let _idx = PageIndex::create(&path, uuid_a, 65_536).unwrap();
        let err = PageIndex::open(&path, uuid_b, 65_536).unwrap_err();
        assert!(matches!(err, PageIndexError::UuidMismatch { .. }));
    }

    #[test]
    fn open_rejects_page_size_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let uuid = fixture_uuid();

        let _idx = PageIndex::create(&path, uuid, 65_536).unwrap();
        let err = PageIndex::open(&path, uuid, 262_144).unwrap_err();
        assert!(matches!(err, PageIndexError::PageSizeMismatch { .. }));
    }

    #[test]
    fn set_get_clear_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();

        let h = fixture_hash(0x42);
        idx.set(7, &h).unwrap();
        assert_eq!(idx.get(7).unwrap(), Some(h));
        // Other pages are still vacant.
        assert_eq!(idx.get(0).unwrap(), None);
        assert_eq!(idx.get(8).unwrap(), None);
        assert_eq!(idx.get(1_000_000).unwrap(), None);

        idx.clear(7).unwrap();
        assert_eq!(idx.get(7).unwrap(), None);
    }

    #[test]
    fn overwriting_a_page_replaces_the_hash() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();

        let h_a = fixture_hash(0x01);
        let h_b = fixture_hash(0x02);
        idx.set(42, &h_a).unwrap();
        assert_eq!(idx.get(42).unwrap(), Some(h_a));
        idx.set(42, &h_b).unwrap();
        assert_eq!(idx.get(42).unwrap(), Some(h_b));
    }

    #[test]
    fn iter_visits_only_allocated_pages_in_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();

        let pages = [
            (3u32, fixture_hash(3)),
            (17u32, fixture_hash(17)),
            (5u32, fixture_hash(5)),
        ];
        for (pid, h) in &pages {
            idx.set(*pid, h).unwrap();
        }

        let collected: Vec<(PageId, ChunkHash)> =
            idx.iter().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].0, 3);
        assert_eq!(collected[1].0, 5);
        assert_eq!(collected[2].0, 17);
        assert_eq!(collected[0].1, fixture_hash(3));
        assert_eq!(collected[1].1, fixture_hash(5));
        assert_eq!(collected[2].1, fixture_hash(17));
    }

    #[test]
    fn iter_on_empty_index_yields_nothing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();
        let collected: Vec<_> = idx.iter().collect::<Result<Vec<_>, _>>().unwrap();
        assert!(collected.is_empty());
    }

    #[test]
    fn highest_allocated_page_reports_max_or_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();

        // Empty volume: no allocated pages.
        assert_eq!(idx.highest_allocated_page().unwrap(), None);

        idx.set(3, &fixture_hash(3)).unwrap();
        idx.set(17, &fixture_hash(17)).unwrap();
        idx.set(5, &fixture_hash(5)).unwrap();
        assert_eq!(idx.highest_allocated_page().unwrap(), Some(17));

        // Clearing the top page drops the high-water mark to the next.
        idx.clear(17).unwrap();
        assert_eq!(idx.highest_allocated_page().unwrap(), Some(5));
    }

    #[test]
    fn truncate_from_drops_records_at_and_beyond_boundary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();

        for pid in [2u32, 4, 8, 9, 20] {
            idx.set(pid, &fixture_hash(pid as u8)).unwrap();
        }

        // Drop everything from page 9 up: 9 and 20 go, 2/4/8 stay.
        idx.truncate_from(9).unwrap();
        assert_eq!(idx.get(8).unwrap(), Some(fixture_hash(8)));
        assert_eq!(idx.get(9).unwrap(), None);
        assert_eq!(idx.get(20).unwrap(), None);
        assert_eq!(idx.highest_allocated_page().unwrap(), Some(8));

        // The trim survives a reopen, and the freed slots read back as
        // unallocated holes.
        let reopened = PageIndex::open(&path, fixture_uuid(), 65_536).unwrap();
        assert_eq!(reopened.get(8).unwrap(), Some(fixture_hash(8)));
        assert_eq!(reopened.get(9).unwrap(), None);
        assert_eq!(reopened.get(20).unwrap(), None);

        // A boundary at or past the current length is a no-op.
        reopened.truncate_from(1000).unwrap();
        assert_eq!(reopened.get(8).unwrap(), Some(fixture_hash(8)));
    }

    #[test]
    fn set_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let uuid = fixture_uuid();
        let h = fixture_hash(0xab);

        {
            let idx = PageIndex::create(&path, uuid, 65_536).unwrap();
            idx.set(123, &h).unwrap();
            idx.set(456, &fixture_hash(0xcd)).unwrap();
        }

        let reopened = PageIndex::open(&path, uuid, 65_536).unwrap();
        assert_eq!(reopened.get(123).unwrap(), Some(h));
        assert_eq!(reopened.get(456).unwrap(), Some(fixture_hash(0xcd)));
        assert_eq!(reopened.get(789).unwrap(), None);
    }

    #[test]
    fn create_refuses_overwrite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let _idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();
        let err = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap_err();
        assert!(
            matches!(err, PageIndexError::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists)
        );
    }

    #[test]
    fn open_rejects_bad_magic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        // Write an arbitrary 64-byte file that isn't a CRPI.
        std::fs::write(&path, [0u8; HEADER_SIZE as usize]).unwrap();
        let err = PageIndex::open(&path, fixture_uuid(), 65_536).unwrap_err();
        assert!(matches!(err, PageIndexError::BadMagic));
    }

    #[test]
    fn set_unsynced_round_trip_with_explicit_sync() {
        // Direct exercise of the split API used by the cache's
        // parallel-flush drain: many `set_unsynced` calls then one
        // trailing `sync`. The reads themselves don't require the
        // sync (pwrite goes to the OS page cache, readable
        // immediately); the sync is purely a durability boundary.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();

        for pid in 0..16u32 {
            idx.set_unsynced(pid, &fixture_hash(pid as u8)).unwrap();
        }
        idx.sync().unwrap();

        for pid in 0..16u32 {
            assert_eq!(idx.get(pid).unwrap(), Some(fixture_hash(pid as u8)));
        }
    }

    #[test]
    fn sparse_holes_consume_no_extra_disk() {
        // Set page 100_000 only; verify the file is sparse — we
        // don't strictly check on-disk blocks (filesystem-dependent),
        // but the *logical* file size should be 1 GB-ish even though
        // we only wrote one record.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(FILENAME);
        let idx = PageIndex::create(&path, fixture_uuid(), 65_536).unwrap();
        idx.set(100_000, &fixture_hash(0xff)).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        // Logical file size = HEADER + (100_000 + 1) * RECORD_SIZE.
        let expected_logical = HEADER_SIZE + (100_000 + 1) * RECORD_SIZE;
        assert_eq!(meta.len(), expected_logical);
    }
}
