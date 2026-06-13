// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Delta-page upload / restore for `chunks.idx` + `blocks-pN.idx`.
//!
//! Both index files are flat fixed-record arrays at known offsets and
//! grow O(N) with chunk / block count — at LTO-8 worst case
//! `blocks-p0.idx` is ~3.2 GB. Whole-file PUT after each manifest
//! backup would be wasteful, so this module ships only the 1 MiB pages
//! that the file's `DirtyPageTracker` reports as dirty, and stitches
//! them back into a contiguous file on restore.
//!
//! ## Object key shape
//!
//! Per-page objects live under the cartridge's manifest prefix,
//! sibling-but-separate from the JSON manifest backups so list
//! filtering stays simple:
//!
//! ```text
//! manifests/<barcode>/chunks/page-<NNNNNN>.dat
//! manifests/<barcode>/blocks-p<N>/page-<NNNNNN>.dat
//! ```
//!
//! `<NNNNNN>` is six-digit zero-padded so the natural lexical sort
//! matches the numeric one. Six digits cover up to 999_999 pages —
//! ~1 TB worst case at the 1 MiB page granularity, several orders of
//! magnitude past the 3.2 GB worst-case index file.
//!
//! ## Sentinel additions
//!
//! Restore needs to know which pages exist for which file at which
//! epoch. The cartridge manifest's `manifest-latest.json` sentinel
//! grows an `index_epoch` map (file label → `IndexEpoch { pages,
//! page_size, epoch, file_size }`) recording, per index file, the
//! count of pages that exist in storage and the file's logical size in
//! bytes. The sentinel is re-PUT after every successful upload pass,
//! and is the *last* object written so a torn upload leaves the
//! sentinel pointing at the previous (consistent) epoch — same
//! ordering rule that legal-hold uses.
//!
//! ## Crash semantics
//!
//! - `mark_range` runs *before* each `pwrite_at` on the index file,
//!   so a crash between mark and write only leaves a clean page
//!   marked dirty (re-uploaded harmlessly next pass).
//! - `clear_pages` runs *after* each successful PUT, so a crash
//!   between PUT and clear leaves the page dirty for re-upload.
//! - The sentinel is updated last; a crash before the sentinel write
//!   leaves restore reading the previous epoch and re-fetching the
//!   pages at that epoch — also consistent.

use std::path::Path;

use crate::block_index::{BlockIndexFile, HEADER_SIZE as BLOCK_HEADER_SIZE};
use crate::chunk_index::{ChunkIndexFile, HEADER_SIZE as CHUNK_HEADER_SIZE};
use crate::dirty_pages::{DirtyPageTracker, PAGE_SIZE};
use crate::errors::{Result, SmcError};
use serde::{Deserialize, Serialize};
use shared_object_store::ObjectStoreBackend;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;

/// Per-file restore manifest entry stamped into `manifest-latest.json`
/// by the upload pass. `file_size` is the logical byte length of the
/// index file at the moment the snapshot was taken — restore uses it
/// to pre-allocate the file before stitching pages back in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexEpoch {
    /// Number of pages currently in storage for this file.
    pub pages: u32,
    /// Page size in bytes. Carried alongside in case we ever change
    /// the constant; restore matches against the stamped value.
    pub page_size: u32,
    /// Monotonic epoch — bumped each successful upload pass. Restore
    /// can compare against the sentinel's epoch to detect a torn
    /// upload (sentinel epoch newer than any page's epoch ⇒ partial).
    pub epoch: u64,
    /// Logical file size at snapshot. May be a page boundary or shorter.
    pub file_size: u64,
}

/// One file's worth of upload work: name (used as key prefix label and
/// also as the sentinel-map key) plus the on-disk path. The label is
/// `chunks` for `chunks.idx`, `blocks-p<N>` for `blocks-p<N>.idx`.
pub struct IndexFileRef<'a> {
    pub label: String,
    pub path: &'a Path,
    pub file_size: u64,
    pub tracker: &'a DirtyPageTracker,
}

/// Upload every dirty page of a single index file to its
/// `manifests/<barcode>/<label>/page-<NNNNNN>.dat` keyspace and clear
/// the tracker bits as each PUT succeeds. Returns the resulting
/// `IndexEpoch` (to be stamped into the sentinel) plus the list of
/// page storage keys that were freshly PUT this pass — the daemon's
/// auto-hold-on-upload worker uses that list to extend an active
/// legal hold to the new index-page objects.
///
/// Errors abort early — partial uploads leave their pages still
/// dirty, so the next pass retries them.
pub async fn upload_one_index(
    barcode: &str,
    file_ref: &IndexFileRef<'_>,
    backend: &dyn ObjectStoreBackend,
) -> Result<(IndexEpoch, Vec<String>)> {
    let snapshot = file_ref.tracker.snapshot();
    let file = std::fs::File::open(file_ref.path)?;
    let total_pages = file_ref.file_size.div_ceil(PAGE_SIZE as u64) as u32;
    let mut uploaded_keys: Vec<String> = Vec::new();
    // Pages this pass handled (uploaded or dropped-past-EOF) and may
    // clear. We clear against a freshly-reloaded on-disk tracker at the
    // end rather than the in-memory `file_ref.tracker`: the upload worker
    // runs on a *view* handle whose tracker is a stale open-time snapshot,
    // so persisting it would clobber dirty bits the owning primary handle
    // marked after the view opened, dropping them from the next backup
    // pass (issue #117). We re-read the current on-disk dirty set and
    // clear only the pages we handled, preserving the primary's marks.
    let mut handled_pages: Vec<u32> = Vec::new();
    for page in &snapshot.pages {
        if (*page as u64) * PAGE_SIZE as u64 >= file_ref.file_size {
            // Page sits past EOF — happens after a truncate where the
            // boundary page is the only mark. Drop it.
            file_ref.tracker.clear_pages(&[*page]);
            handled_pages.push(*page);
            continue;
        }
        let off = (*page as u64) * PAGE_SIZE as u64;
        let remaining = file_ref.file_size - off;
        let len = remaining.min(PAGE_SIZE as u64) as usize;
        let mut buf = vec![0u8; len];
        file.read_exact_at(&mut buf, off)?;
        let key = page_key(barcode, &file_ref.label, *page);
        // Versioned write: same key gets overwritten with new content
        // every time the page mutates. `upload_versioned` bypasses the
        // meta-cache wrapper's once-and-done memoization.
        backend.upload_versioned(&key, &buf).await?;
        uploaded_keys.push(key);
        // Clear from the in-memory tracker immediately so a mid-pass crash
        // doesn't re-upload pages that already landed.
        file_ref.tracker.clear_pages(&[*page]);
        handled_pages.push(*page);
    }
    // Persist against the current on-disk dirty set so the owning
    // primary's post-open marks survive (issue #117): re-read the sidecar,
    // clear only the pages we handled, bump the epoch, and persist that.
    let on_disk = DirtyPageTracker::open_or_create(file_ref.path)?;
    on_disk.clear_pages(&handled_pages);
    let epoch = on_disk.bump_epoch();
    on_disk.persist()?;
    Ok((
        IndexEpoch {
            pages: total_pages,
            page_size: PAGE_SIZE,
            epoch,
            file_size: file_ref.file_size,
        },
        uploaded_keys,
    ))
}

/// Build the storage key for a single index page.
pub fn page_key(barcode: &str, label: &str, page: u32) -> String {
    format!("manifests/{}/{}/page-{:06}.dat", barcode, label, page)
}

/// Key prefix to list every page belonging to one index file.
pub fn page_key_prefix(barcode: &str, label: &str) -> String {
    format!("manifests/{}/{}/", barcode, label)
}

/// Restore all pages for one index file by downloading every entry in
/// the epoch in ascending order and writing them at their natural
/// offsets. Used by the cold-bucket DR path. The destination file is
/// truncated to `epoch.file_size` and a fresh `DirtyPageTracker`
/// sidecar is initialized empty (no pages dirty post-restore).
pub async fn restore_one_index(
    barcode: &str,
    label: &str,
    dest: &Path,
    epoch: &IndexEpoch,
    backend: &dyn ObjectStoreBackend,
) -> Result<()> {
    if epoch.page_size != PAGE_SIZE {
        return Err(SmcError::InvalidOp(
            "index page restore: page size mismatch with build",
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)?;
    file.set_len(epoch.file_size)?;
    for page in 0..epoch.pages {
        let off = (page as u64) * PAGE_SIZE as u64;
        if off >= epoch.file_size {
            break;
        }
        let key = page_key(barcode, label, page);
        let bytes = backend.download_chunk(&key).await?;
        let remaining = epoch.file_size - off;
        let want = remaining.min(PAGE_SIZE as u64) as usize;
        if bytes.len() != want {
            return Err(SmcError::InvalidOp(
                "index page restore: page payload size disagrees with sentinel",
            ));
        }
        file.write_all_at(&bytes, off)?;
    }
    file.sync_data()?;
    // Initialize a clean sidecar so the next mutation cycle starts
    // tracking from epoch 0 of the local copy. The storage epoch is
    // separately recorded in the manifest sentinel.
    let tracker = DirtyPageTracker::open_or_create(dest)?;
    tracker.persist()?;
    Ok(())
}

/// Convenience: build an `IndexFileRef` for the chunk index of a
/// cartridge. Picks up the file's logical size at call time.
pub fn chunk_index_file_ref<'a>(chunk_index: &'a ChunkIndexFile) -> Result<IndexFileRef<'a>> {
    let file_size = std::fs::metadata(chunk_index.path())?.len();
    Ok(IndexFileRef {
        label: "chunks".to_string(),
        path: chunk_index.path(),
        file_size,
        tracker: chunk_index.dirty_tracker(),
    })
}

/// Convenience: build an `IndexFileRef` for one partition's block
/// index. Label is `blocks-p<N>`, mirroring the on-disk filename.
pub fn block_index_file_ref<'a>(
    block_index: &'a BlockIndexFile,
    partition: u8,
) -> Result<IndexFileRef<'a>> {
    let file_size = std::fs::metadata(block_index.path())?.len();
    Ok(IndexFileRef {
        label: format!("blocks-p{}", partition),
        path: block_index.path(),
        file_size,
        tracker: block_index.dirty_tracker(),
    })
}

/// Sanity check: header sizes are consistent with the page granularity.
/// Pure assertion so a future change can't silently break the
/// restore-stitch math.
const _: () = {
    assert!(BLOCK_HEADER_SIZE < PAGE_SIZE as usize);
    assert!(CHUNK_HEADER_SIZE < PAGE_SIZE as usize);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_index::{BlockIndexFile, BlockRec, EncryptionTag};
    use crate::chunk_index::{ChunkIndexFile, ChunkRec, LocationTag};
    use crate::tape::BlockKind;
    use shared_object_store::compression::CompressionAlgo;
    use shared_object_store::local::LocalBackend;
    use tempfile::TempDir;

    #[tokio::test]
    async fn round_trip_chunk_index_via_local_backend() {
        let cart_dir = TempDir::new().unwrap();
        let backend_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();

        // Write a few records, fsync, then upload pages.
        {
            let cif = ChunkIndexFile::open_or_create(cart_dir.path()).unwrap();
            for i in 0..32u64 {
                cif.append(&ChunkRec {
                    size: i * 1024,
                    hash: Some(hex::encode([(i & 0xFF) as u8; 32])),
                    location: LocationTag::LocalOnly,
                    uploaded: false,
                    compression: None,
                })
                .unwrap();
            }
            cif.fsync().unwrap();
            assert!(cif.dirty_tracker().any_dirty());
            let file_ref = chunk_index_file_ref(&cif).unwrap();
            let (epoch, page_keys) = upload_one_index("TAPE001", &file_ref, &backend)
                .await
                .unwrap();
            assert_eq!(epoch.pages, 1); // 32*64+32 bytes fits in one 1 MiB page
            assert_eq!(page_keys.len(), 1);
            assert_eq!(page_keys[0], page_key("TAPE001", "chunks", 0));
            assert!(!cif.dirty_tracker().any_dirty());
            cif.dirty_tracker().persist().unwrap();
        }

        // Restore into a new cartridge dir and verify records match.
        let restored_dir = TempDir::new().unwrap();
        let dest = ChunkIndexFile::path_for(restored_dir.path());
        let epoch = IndexEpoch {
            pages: 1,
            page_size: PAGE_SIZE,
            epoch: 1,
            file_size: std::fs::metadata(ChunkIndexFile::path_for(cart_dir.path()))
                .unwrap()
                .len(),
        };
        restore_one_index("TAPE001", "chunks", &dest, &epoch, &backend)
            .await
            .unwrap();
        let cif = ChunkIndexFile::open_or_create(restored_dir.path()).unwrap();
        assert_eq!(cif.next_id(), 32);
        for i in 0..32u64 {
            let r = cif.read(i).unwrap();
            assert_eq!(r.size, i * 1024);
            assert_eq!(r.hash, Some(hex::encode([(i & 0xFF) as u8; 32])));
        }
        // Restored sidecar starts clean.
        assert!(!cif.dirty_tracker().any_dirty());
    }

    #[tokio::test]
    async fn round_trip_block_index_multi_page() {
        let cart_dir = TempDir::new().unwrap();
        let backend_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();

        // Append enough records to fill several pages: 1 MiB / 16 B
        // = 65536 records per page. 200_000 records ≈ 4 pages.
        let total = 200_000u64;
        {
            let bif = BlockIndexFile::open_or_create(cart_dir.path(), 0).unwrap();
            for i in 0..total {
                bif.append(&BlockRec {
                    chunk_id: (i / 1000) as u32,
                    offset: ((i % 1000) * 64) as u32,
                    len: 64,
                    kind: BlockKind::Data,
                    encryption: EncryptionTag::None,
                    compression: Some(CompressionAlgo::Zstd),
                })
                .unwrap();
            }
            bif.fsync().unwrap();
            let file_ref = block_index_file_ref(&bif, 0).unwrap();
            let (epoch, page_keys) = upload_one_index("TAPE777", &file_ref, &backend)
                .await
                .unwrap();
            assert!(epoch.pages >= 4, "expected >=4 pages, got {}", epoch.pages);
            assert_eq!(page_keys.len(), epoch.pages as usize);
            assert!(!bif.dirty_tracker().any_dirty());
            bif.dirty_tracker().persist().unwrap();
        }

        let restored_dir = TempDir::new().unwrap();
        let dest = BlockIndexFile::path_for(restored_dir.path(), 0);
        let src_size = std::fs::metadata(BlockIndexFile::path_for(cart_dir.path(), 0))
            .unwrap()
            .len();
        let pages = src_size.div_ceil(PAGE_SIZE as u64) as u32;
        let epoch = IndexEpoch {
            pages,
            page_size: PAGE_SIZE,
            epoch: 1,
            file_size: src_size,
        };
        restore_one_index("TAPE777", "blocks-p0", &dest, &epoch, &backend)
            .await
            .unwrap();
        let bif = BlockIndexFile::open_or_create(restored_dir.path(), 0).unwrap();
        assert_eq!(bif.next_lba(), total);
        // Spot-check first, middle, last.
        for &i in &[0u64, 1, total / 2, total - 1] {
            let r = bif.read(i).unwrap();
            assert_eq!(r.chunk_id, (i / 1000) as u32);
            assert_eq!(r.offset, ((i % 1000) * 64) as u32);
            assert_eq!(r.compression, Some(CompressionAlgo::Zstd));
        }
    }

    #[tokio::test]
    async fn delta_only_dirty_pages_uploaded() {
        let cart_dir = TempDir::new().unwrap();
        let backend_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();

        let bif = BlockIndexFile::open_or_create(cart_dir.path(), 0).unwrap();
        // First pass: append enough records to fill 3 pages.
        for _ in 0..200_000u64 {
            bif.append(&BlockRec::data()).unwrap();
        }
        bif.fsync().unwrap();
        {
            let file_ref = block_index_file_ref(&bif, 0).unwrap();
            upload_one_index("TAPE_DELTA", &file_ref, &backend)
                .await
                .unwrap();
        }
        bif.dirty_tracker().persist().unwrap();

        // After first pass, no pages are dirty.
        assert!(!bif.dirty_tracker().any_dirty());

        // Mutate one record near LBA 100 — only that one page should
        // dirty.
        bif.overwrite(
            100,
            &BlockRec {
                chunk_id: 7,
                offset: 9999,
                len: 1,
                kind: BlockKind::Data,
                encryption: EncryptionTag::None,
                compression: None,
            },
        )
        .unwrap();
        let snapshot = bif.dirty_tracker().snapshot();
        assert_eq!(snapshot.pages.len(), 1, "exactly one page should be dirty");
    }
}
