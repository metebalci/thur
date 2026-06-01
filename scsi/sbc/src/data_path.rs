// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SBC-3 data-path opcodes: WRITE 10/16, READ 10/16, SYNCHRONIZE
//! CACHE 10/16, COMPARE AND WRITE (0x89), UNMAP (0x42).
//!
//! thurvsa volumes are page-grained internally (default 64 KiB) but
//! advertise 4 KiB sectors over SBC-3. Sub-page host I/O is
//! supported end-to-end: the dispatcher routes every WRITE / READ /
//! CAW / UNMAP through a per-volume [`PageCache`] which loads the
//! affected page(s), splices the host bytes in at sector grain, and
//! marks the page dirty for asynchronous flush. SYNCHRONIZE CACHE
//! turns into a real fence — it awaits the cache's flush of every
//! dirty page in the requested LBA range through to cloud-ack.
//!
//! COMPARE AND WRITE serializes through a per-LUN async mutex so
//! the read+compare+write triple is atomic against other CAWs on
//! the same LUN. Concurrent regular WRITE/READ is not fenced —
//! mixing fence-by-CAW workloads with raw WRITEs on the same LBA
//! range is a host-side bug; our scope is "safe against other
//! CAWs", which is what VMware / Windows clusters actually need.
//!
//! UNMAP at sub-page grain zeros the affected sectors and marks
//! the page dirty (so the next flush commits the partial erase).
//! UNMAP that fully covers a page drops the cached entry and
//! synchronously clears the page-index slot; the cloud-side chunk
//! lingers in the per-backend pool until `system gc` reclaims it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use core_block::PageCache;
use core_block::upload_index::UploadState;
use core_block::uploader::UploaderError;
use tokio::sync::Mutex as AsyncMutex;

use super::VolumeLookup;
use super::inquiry::naa_locally_assigned;
use super::odx::{JobResult, JobStatus, ROD_TOKEN_LEN, RodToken, TokenManager, TokenState};
use super::reservations::{Nexus, ReservationManager};
use super::types::{ScsiRequest, ScsiResponse, SenseData};

/// Per-LUN async mutex registry. COMPARE AND WRITE holds the lock
/// for the LUN it targets across the read+compare+write window so
/// two concurrent CAWs against the same LUN are serialized. The
/// inner sync mutex is held for one BTreeMap lookup; the
/// `Arc<AsyncMutex>` is what callers actually `.lock().await` on.
#[derive(Default)]
pub struct CawLocks {
    inner: StdMutex<BTreeMap<u64, Arc<AsyncMutex<()>>>>,
}

impl CawLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up (and lazily create) the per-LUN lock. Cheap — one
    /// BTreeMap lookup per CAW; the lock entries linger for the
    /// daemon's lifetime, which is fine at thurvsa's LUN counts.
    pub fn lock_for(&self, lun: u64) -> Arc<AsyncMutex<()>> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(lun).or_default().clone()
    }
}

/// Decoded LBA + transfer length, normalized to u64. Both READ and
/// WRITE 10/16 share this shape — only the CDB byte layout differs.
struct LbaRange {
    lba: u64,
    blocks: u64,
}

/// Parse the LBA + TRANSFER LENGTH fields out of a 10-byte READ /
/// WRITE / SYNC CACHE CDB (opcodes 0x28 / 0x2A / 0x35). Bytes 2-5
/// hold the LBA, bytes 7-8 hold the 16-bit transfer length.
fn parse_10(cdb: &[u8]) -> Result<LbaRange, SenseData> {
    if cdb.len() < 10 {
        return Err(SenseData::INVALID_FIELD_IN_CDB);
    }
    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
    let blocks = u16::from_be_bytes([cdb[7], cdb[8]]);
    Ok(LbaRange {
        lba: u64::from(lba),
        blocks: u64::from(blocks),
    })
}

/// Parse the LBA + TRANSFER LENGTH fields out of a 16-byte READ /
/// WRITE / SYNC CACHE CDB (opcodes 0x88 / 0x8A / 0x91). Bytes 2-9
/// hold the 64-bit LBA, bytes 10-13 hold the 32-bit transfer length.
fn parse_16(cdb: &[u8]) -> Result<LbaRange, SenseData> {
    if cdb.len() < 16 {
        return Err(SenseData::INVALID_FIELD_IN_CDB);
    }
    let lba = u64::from_be_bytes([
        cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
    ]);
    let blocks = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]);
    Ok(LbaRange {
        lba,
        blocks: u64::from(blocks),
    })
}

/// Per-volume sizing the data path needs on every command: sector
/// size and the volume's last addressable LBA. Pulled once per
/// request to keep the math readable.
struct Sizing {
    sector: u64,
    total_blocks: u64,
}

impl Sizing {
    fn from(cache: &PageCache) -> Self {
        let m = cache.manifest();
        let sector = u64::from(m.sector_bytes);
        Self {
            sector,
            total_blocks: m.size_bytes / sector,
        }
    }
}

/// Validate that `range` lies within the volume. Returns `Ok(())`
/// on success. Sub-page LBA / block counts are now allowed — the
/// cache layer handles RMW. Range overflow / past-end-of-volume
/// still surfaces as LBA OUT OF RANGE.
fn validate_in_range(range: &LbaRange, sz: &Sizing) -> Result<(), SenseData> {
    let end = range
        .lba
        .checked_add(range.blocks)
        .ok_or(SenseData::LBA_OUT_OF_RANGE)?;
    if end > sz.total_blocks {
        return Err(SenseData::LBA_OUT_OF_RANGE);
    }
    Ok(())
}

/// Resolve an LBA range to `(byte_offset, byte_len)` against the
/// volume, mapping any past-end-of-volume / overflow condition to LBA
/// OUT OF RANGE. Wraps the shared [`PageCache::resolve_range`] — the
/// same overflow-safe range check the NVMe data path uses — so the
/// LBA -> byte invariant lives in one place. `byte_len` is returned as
/// `usize` since every caller feeds it straight to a buffer length.
fn resolve_range_sense(cache: &PageCache, range: &LbaRange) -> Result<(u64, usize), SenseData> {
    cache
        .resolve_range(range.lba, range.blocks)
        .map(|(off, len)| (off, len as usize))
        .map_err(|_| SenseData::LBA_OUT_OF_RANGE)
}

/// WRITE (10) / WRITE (16). Routes through the per-volume
/// [`PageCache`] which loads the affected pages, splices in the
/// host bytes at sector grain, and marks them dirty for async
/// flush. Reservation-gated: an active SBC-3 persistent reservation
/// that excludes the requesting I_T nexus surfaces as RESERVATION
/// CONFLICT (status 0x18) before any I/O.
pub(super) async fn write(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if !reservations.allow_write(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    if cache.manifest().worm {
        return ScsiResponse::check(SenseData::WRITE_PROTECTED);
    }
    let parsed = match req.cdb.first().copied() {
        Some(0x2A) => parse_10(req.cdb),
        Some(0x8A) => parse_16(req.cdb),
        _ => return ScsiResponse::check(SenseData::INVALID_OPCODE),
    };
    let range = match parsed {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    if range.blocks == 0 {
        // SBC-3: transfer length 0 is "no blocks transferred", not
        // an error.
        return ScsiResponse::good(Vec::new());
    }
    let (byte_offset, want_bytes) = match resolve_range_sense(cache, &range) {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    if req.data_out.len() != want_bytes {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if let Err(e) = cache.write_bytes(byte_offset, req.data_out).await {
        return ScsiResponse::check(map_write_error(&e));
    }
    ScsiResponse::good(Vec::new())
}

/// READ (10) / READ (16). Walks the requested LBA range through
/// the cache; missing pages fall through to the underlying
/// `VolumeWriter::read_page` (cloud or local pool) and unallocated
/// pages return zeros per SBC-3 §5.7. Sub-sector reads work end-
/// to-end via the cache. Reservation-gated for ExclusiveAccess /
/// *_ExclusiveAccessRegistrantsOnly / ExclusiveAccessAllRegistrants
/// types.
pub(super) async fn read(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if !reservations.allow_read(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    let parsed = match req.cdb.first().copied() {
        Some(0x28) => parse_10(req.cdb),
        Some(0x88) => parse_16(req.cdb),
        _ => return ScsiResponse::check(SenseData::INVALID_OPCODE),
    };
    let range = match parsed {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    if range.blocks == 0 {
        return ScsiResponse::good(Vec::new());
    }
    let (byte_offset, want_bytes) = match resolve_range_sense(cache, &range) {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };

    let mut out = match cache.read_bytes(byte_offset, want_bytes).await {
        Ok(v) => v,
        Err(e) => return ScsiResponse::check(map_read_error(&e)),
    };

    if out.len() > req.data_in_max {
        out.truncate(req.data_in_max);
    }
    ScsiResponse::good(out)
}

/// SYNCHRONIZE CACHE (10) / SYNCHRONIZE CACHE (16). Awaits flush
/// of every dirty page whose page id falls in the requested LBA
/// range. SBC-3 §5.21: NUMBER OF BLOCKS = 0 means "from LBA to the
/// end of the medium." Reservation-gated as a write-side opcode:
/// committing cached writes counts as a write.
pub(super) async fn synchronize_cache(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if !reservations.allow_write(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    let parsed = match req.cdb.first().copied() {
        Some(0x35) => parse_10(req.cdb),
        Some(0x91) => parse_16(req.cdb),
        _ => return ScsiResponse::check(SenseData::INVALID_OPCODE),
    };
    let range = match parsed {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    let sz = Sizing::from(cache);
    // SBC-3 §5.21: NUMBER OF BLOCKS = 0 means "from LBA to the end
    // of the medium". That's still in-range by definition.
    let blocks_for_range = if range.blocks > 0 {
        let end = match range.lba.checked_add(range.blocks) {
            Some(v) => v,
            None => return ScsiResponse::check(SenseData::LBA_OUT_OF_RANGE),
        };
        if end > sz.total_blocks {
            return ScsiResponse::check(SenseData::LBA_OUT_OF_RANGE);
        }
        range.blocks
    } else {
        if range.lba > sz.total_blocks {
            return ScsiResponse::check(SenseData::LBA_OUT_OF_RANGE);
        }
        // "From LBA to end of medium."
        sz.total_blocks - range.lba
    };

    let byte_offset = range.lba * sz.sector;
    let len_bytes = blocks_for_range * sz.sector;
    if let Err(e) = cache.synchronize_bytes(byte_offset, len_bytes).await {
        return ScsiResponse::check(map_write_error(&e));
    }
    ScsiResponse::good(Vec::new())
}

/// COMPARE AND WRITE (16) — opcode 0x89. SBC-3 §5.2 atomic
/// test-and-set: read the current bytes for the requested LBA
/// range, compare against the host's first half of `data_out`, and
/// only commit the second half if every byte matches. A diff
/// surfaces as MISCOMPARE (sense key 0x0E + ASC/ASCQ 0x1D/0x00).
///
/// CDB layout (16 bytes):
///   byte 0     opcode = 0x89
///   byte 1     WRPROTECT (bits 7-5) | DPO (bit 4) | FUA (bit 3) | reserved
///   bytes 2-9  LBA (64-bit BE)
///   byte 10    reserved
///   bytes 11-12 reserved
///   byte 13    NUMBER OF LOGICAL BLOCKS (8-bit)
///   byte 14    GROUP NUMBER (5 bits)
///   byte 15    CONTROL
///
/// Data-Out PDU is `2 * blocks * sector_bytes` long: first the
/// compare buffer, then the write buffer. Sub-page CAW (e.g., the
/// 1-sector VMFS heartbeat) is honored via the cache: load the
/// affected page(s), compare at the sector range, splice on match.
/// Cluster-wide atomicity is preserved by the per-LUN async mutex
/// in [`CawLocks`].
pub(super) async fn compare_and_write(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
    nexus: Nexus,
    reservations: &ReservationManager,
    caw_locks: &CawLocks,
) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if !reservations.allow_write(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    if cache.manifest().worm {
        return ScsiResponse::check(SenseData::WRITE_PROTECTED);
    }
    if req.cdb.len() < 16 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let lba = u64::from_be_bytes([
        req.cdb[2], req.cdb[3], req.cdb[4], req.cdb[5], req.cdb[6], req.cdb[7], req.cdb[8],
        req.cdb[9],
    ]);
    let blocks = u64::from(req.cdb[13]);
    if blocks == 0 {
        return ScsiResponse::good(Vec::new()); // SBC-3: no transfer
    }
    let range = LbaRange { lba, blocks };
    let (byte_offset, want_each) = match resolve_range_sense(cache, &range) {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    let want_total = match want_each.checked_mul(2) {
        Some(v) => v,
        None => return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
    };
    if req.data_out.len() != want_total {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let (compare_buf, write_buf) = req.data_out.split_at(want_each);

    // Serialize against other CAWs on the same LUN. Concurrent
    // regular WRITEs are not fenced — mixing CAW-fenced and raw
    // WRITE on the same LBA range is host-side undefined behavior,
    // matching real SAN targets that lock at the page (not byte)
    // level.
    let lock = caw_locks.lock_for(req.lun);
    let _guard = lock.lock().await;

    match cache
        .compare_and_write_bytes(byte_offset, compare_buf, write_buf)
        .await
    {
        Ok(true) => ScsiResponse::good(Vec::new()),
        // SPC-4 says the COMMAND-SPECIFIC INFORMATION field carries
        // the LBA of the first miscompare. Our descriptor-format
        // sense doesn't surface command-specific information yet —
        // the sense key + ASC alone is enough for every initiator we
        // care about (Windows / VMware / Linux MPIO) to recognize
        // the miscompare.
        Ok(false) => ScsiResponse::check(SenseData::MISCOMPARE),
        Err(e) => ScsiResponse::check(map_read_error(&e)),
    }
}

/// UNMAP — opcode 0x42. Thin-provisioning hint: the host is
/// telling us a contiguous LBA range is no longer in use, and we
/// are free to release any backing storage. thurvsa implements this
/// at sector grain via the cache: descriptors that fully cover a
/// page drop the cached entry and clear the page-index slot
/// synchronously; sub-page descriptors zero the affected sectors
/// in the cached page and mark it dirty so the partial erase
/// commits on the next flush.
///
/// CDB layout (10 bytes):
///   byte 0     opcode = 0x42
///   byte 1     bit 0 = ANCHOR; bits 1-7 reserved
///   bytes 2-5  reserved
///   byte 6     GROUP NUMBER (5 bits)
///   bytes 7-8  PARAMETER LIST LENGTH (16-bit BE)
///   byte 9     CONTROL
///
/// Parameter list (in `data_out`):
///   bytes 0-1  UNMAP DATA LENGTH (= total - 2)
///   bytes 2-3  UNMAP BLOCK DESCRIPTOR DATA LENGTH (= n*16)
///   bytes 4-7  reserved
///   bytes 8..  N × 16-byte UNMAP BLOCK DESCRIPTOR:
///                bytes 0-7  UNMAP LOGICAL BLOCK ADDRESS (u64 BE)
///                bytes 8-11 NUMBER OF LOGICAL BLOCKS (u32 BE)
///                bytes 12-15 reserved
///
/// Anchored unmap (CDB byte 1 bit 0 = 1) isn't implemented — we
/// don't preserve a "was unmapped" state separately from "never
/// allocated", so anchored UNMAP returns INVALID FIELD IN CDB.
/// Out-of-range descriptors return LBA OUT OF RANGE. Validation
/// runs over every descriptor before any state change so a
/// malformed UNMAP leaves the volume untouched.
pub(super) async fn unmap(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if !reservations.allow_write(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    if cache.manifest().worm {
        return ScsiResponse::check(SenseData::WRITE_PROTECTED);
    }
    if req.cdb.len() < 10 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if req.cdb[1] & 0x01 != 0 {
        // ANCHOR=1 — we don't model anchored unmap (no separate
        // "deallocated" state), reject so the host can fall back.
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let parameter_list_length = u16::from_be_bytes([req.cdb[7], req.cdb[8]]) as usize;
    if parameter_list_length == 0 {
        // SBC-3 §5.27: zero-length parameter list is not an error.
        return ScsiResponse::good(Vec::new());
    }
    if parameter_list_length < 8 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    if req.data_out.len() < parameter_list_length {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let p = &req.data_out[..parameter_list_length];
    let descriptor_total = u16::from_be_bytes([p[2], p[3]]) as usize;
    if !descriptor_total.is_multiple_of(16) {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    if 8 + descriptor_total > parameter_list_length {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }

    let n = descriptor_total / 16;

    // Two-phase: validate + resolve every descriptor first so a
    // malformed entry doesn't leave half the volume cleared. SBC-3
    // §5.27 doesn't require all-or-nothing semantics, but it's the
    // user-friendly behavior — an initiator that retries gets a
    // clean slate to retry against. Tuple is
    // `(byte_offset, len_bytes, lba, blocks)`; the trailing LBA/blocks
    // are kept only for the failure log.
    let mut to_clear: Vec<(u64, u64, u64, u64)> = Vec::with_capacity(n);
    for i in 0..n {
        let off = 8 + i * 16;
        let lba = u64::from_be_bytes(p[off..off + 8].try_into().expect("8 bytes"));
        let blocks = u32::from_be_bytes(p[off + 8..off + 12].try_into().expect("4 bytes"));
        if blocks == 0 {
            continue; // zero-length descriptor → no-op
        }
        let range = LbaRange {
            lba,
            blocks: u64::from(blocks),
        };
        let (byte_offset, len_bytes) = match resolve_range_sense(cache, &range) {
            Ok(v) => v,
            Err(s) => return ScsiResponse::check(s),
        };
        to_clear.push((byte_offset, len_bytes as u64, lba, range.blocks));
    }

    for (byte_offset, len_bytes, lba, blocks) in to_clear {
        if let Err(e) = cache.unmap_bytes(byte_offset, len_bytes).await {
            tracing::warn!(error = %e, lba = lba, blocks = blocks, "UNMAP: cache clear failed");
            return ScsiResponse::check(SenseData::WRITE_ERROR);
        }
    }
    ScsiResponse::good(Vec::new())
}

/// VERIFY (10) — opcode 0x2F. VERIFY (16) — opcode 0x8F. SBC-3
/// §5.46 / §5.47. Two operating modes selected by the BYTCHK field
/// in CDB byte 1 bits 2-1:
///
///   00b  No compare. The device server reads each requested block
///        from medium and reports any unrecovered read errors. For
///        a cloud-backed virtual volume that means the cache /
///        VolumeWriter pipeline successfully resolved every page.
///   01b  Compare with data-out. Initiator supplies one block of
///        data per logical block; mismatch surfaces as MISCOMPARE.
///   10b  Reserved.
///   11b  Compare with stored protection info — we don't model
///        logical block protection, reject as INVALID FIELD IN CDB.
///
/// VRPROTECT (byte 1 bits 7-5) must be 0 — same reason. DPO bit is
/// ignored (we have no separate "don't cache" hint to honor).
/// Reservation-gated as a read-side opcode.
pub(super) async fn verify(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if !reservations.allow_read(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    let opcode = match req.cdb.first().copied() {
        Some(v) => v,
        None => return ScsiResponse::check(SenseData::INVALID_OPCODE),
    };
    let parsed = match opcode {
        0x2F => parse_10(req.cdb),
        0x8F => parse_16(req.cdb),
        _ => return ScsiResponse::check(SenseData::INVALID_OPCODE),
    };
    let range = match parsed {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    if (req.cdb[1] >> 5) != 0 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB); // VRPROTECT
    }
    let bytchk = (req.cdb[1] >> 1) & 0x03;
    if bytchk == 0b10 || bytchk == 0b11 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if range.blocks == 0 {
        return ScsiResponse::good(Vec::new());
    }
    let (byte_offset, want_bytes) = match resolve_range_sense(cache, &range) {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };

    if bytchk == 0b00 {
        // Medium-readability check: the cache resolves every page
        // (sparse-hole pages return zero bytes without an error).
        return match cache.read_bytes(byte_offset, want_bytes).await {
            Ok(_) => ScsiResponse::good(Vec::new()),
            Err(e) => ScsiResponse::check(map_read_error(&e)),
        };
    }

    // BYTCHK = 01b: compare data-out against on-medium bytes.
    if req.data_out.len() != want_bytes {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let on_medium = match cache.read_bytes(byte_offset, want_bytes).await {
        Ok(v) => v,
        Err(e) => return ScsiResponse::check(map_read_error(&e)),
    };
    if on_medium != req.data_out {
        return ScsiResponse::check(SenseData::MISCOMPARE);
    }
    ScsiResponse::good(Vec::new())
}

/// WRITE SAME (10) — opcode 0x41. WRITE SAME (16) — opcode 0x93.
/// SBC-3 §5.49 / §5.50. Replicates a single logical block of data
/// across a contiguous LBA range; the dominant uses are VAAI Block
/// Zero (UNMAP=0 + zero pattern), `blkdiscard --zeroout` (UNMAP=1
/// + zero pattern), and bulk filesystem zero-fill.
///
/// Field handling:
///   WRPROTECT (byte 1 bits 7-5) — must be 0; we don't model
///       logical block protection.
///   ANCHOR (byte 1 bit 4) — rejected, mirrors UNMAP behavior.
///   UNMAP (byte 1 bit 3) — when set with a zero pattern, route
///       through `cache.unmap_bytes` (sparse hole = zeros).
///   PBDATA / LBDATA (byte 1 bits 2-1) — rejected; we don't honor
///       physical-block / logical-block replacement formats.
///   NDOB (16-byte form, byte 1 bit 0) — when set, no Data-Out
///       buffer is sent; the implicit pattern is a zero block.
///
/// NUMBER OF BLOCKS = 0 means "no transfer" for the 10-byte form
/// (SBC-3 §5.49.2) and "from LBA to end of medium" for the
/// 16-byte form (§5.50.2). Reservation-gated as a write-side
/// opcode. WORM volumes refuse with WRITE PROTECTED.
pub(super) async fn write_same(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if !reservations.allow_write(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    if cache.manifest().worm {
        return ScsiResponse::check(SenseData::WRITE_PROTECTED);
    }
    let opcode = match req.cdb.first().copied() {
        Some(v) => v,
        None => return ScsiResponse::check(SenseData::INVALID_OPCODE),
    };
    let cdb_len_ok = match opcode {
        0x41 => req.cdb.len() >= 10,
        0x93 => req.cdb.len() >= 16,
        _ => return ScsiResponse::check(SenseData::INVALID_OPCODE),
    };
    if !cdb_len_ok {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }

    if (req.cdb[1] >> 5) != 0 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB); // WRPROTECT
    }
    let anchor = (req.cdb[1] & 0x10) != 0;
    let unmap = (req.cdb[1] & 0x08) != 0;
    let pbdata = (req.cdb[1] & 0x04) != 0;
    let lbdata = (req.cdb[1] & 0x02) != 0;
    let ndob = opcode == 0x93 && (req.cdb[1] & 0x01) != 0;
    if anchor || pbdata || lbdata {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }

    let (lba, blocks_field): (u64, u64) = if opcode == 0x41 {
        let lba = u32::from_be_bytes([req.cdb[2], req.cdb[3], req.cdb[4], req.cdb[5]]);
        let blocks = u16::from_be_bytes([req.cdb[7], req.cdb[8]]);
        (u64::from(lba), u64::from(blocks))
    } else {
        let lba = u64::from_be_bytes([
            req.cdb[2], req.cdb[3], req.cdb[4], req.cdb[5], req.cdb[6], req.cdb[7], req.cdb[8],
            req.cdb[9],
        ]);
        let blocks = u32::from_be_bytes([req.cdb[10], req.cdb[11], req.cdb[12], req.cdb[13]]);
        (lba, u64::from(blocks))
    };

    let sz = Sizing::from(cache);
    if lba > sz.total_blocks {
        return ScsiResponse::check(SenseData::LBA_OUT_OF_RANGE);
    }
    let blocks = if blocks_field == 0 {
        if opcode == 0x41 {
            // 10-byte form: zero blocks = no transfer.
            return ScsiResponse::good(Vec::new());
        }
        sz.total_blocks - lba // 16-byte form: to end of medium.
    } else {
        blocks_field
    };
    let range = LbaRange { lba, blocks };
    let (byte_offset, len_bytes) = match resolve_range_sense(cache, &range) {
        Ok((off, len)) => (off, len as u64),
        Err(s) => return ScsiResponse::check(s),
    };

    // Resolve the per-sector pattern. NDOB skips data-out and
    // implies zero; otherwise data_out must carry exactly one
    // logical block.
    let sector = sz.sector as usize;
    let pattern: &[u8] = if ndob {
        // Use a zero-filled scratch buffer; allocate once to avoid
        // a giant heap copy when we expand below.
        &[]
    } else {
        if req.data_out.len() != sector {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        }
        req.data_out
    };
    let pattern_is_zero = pattern.iter().all(|&b| b == 0);

    if unmap && pattern_is_zero {
        // Cheapest path: drop the cached entries and clear the
        // page-index slots. Sparse-hole reads return zero, matching
        // the host's expressed intent.
        return match cache.unmap_bytes(byte_offset, len_bytes).await {
            Ok(()) => ScsiResponse::good(Vec::new()),
            Err(e) => ScsiResponse::check(map_write_error(&e)),
        };
    }

    // Expand the pattern across the full transfer length and route
    // through the cache. A zero pattern with UNMAP=0 still has to
    // commit zeros (host explicitly didn't ask to unmap). Pattern
    // expansion: cap the in-memory buffer at ~16 MiB so a multi-GB
    // WRITE SAME doesn't allocate a massive block; iterate in
    // sector-aligned chunks and call `write_bytes` per chunk.
    const TARGET_CHUNK_BYTES: usize = 16 * 1024 * 1024;
    let chunk_sectors = (TARGET_CHUNK_BYTES / sector).max(1);
    let chunk_bytes = chunk_sectors * sector;
    let mut remaining = len_bytes as usize;
    let mut cursor = byte_offset;
    while remaining > 0 {
        let this = remaining.min(chunk_bytes);
        let buf = if pattern_is_zero {
            vec![0u8; this]
        } else {
            let mut b = Vec::with_capacity(this);
            while b.len() < this {
                b.extend_from_slice(pattern);
            }
            b
        };
        if let Err(e) = cache.write_bytes(cursor, &buf).await {
            return ScsiResponse::check(map_write_error(&e));
        }
        cursor += this as u64;
        remaining -= this;
    }
    ScsiResponse::good(Vec::new())
}

/// EXTENDED COPY (LID1) — opcode 0x83 service action 0x00. The
/// VMware VAAI "Hardware Accelerated Copy" primitive plus a
/// passable subset of the SPC-3 §6.3 surface every other initiator
/// expects when the page is advertised in VPD 0x8F.
///
/// Wire surface implemented (the same subset LIO and SCST expose):
///   - Service action 0x00 (LID1). LID4 (0x01) is handled by
///     `extended_copy_lid4` (same descriptor subset, different
///     header); ODX actions (POPULATE TOKEN 0x10 / WRITE USING TOKEN
///     0x11 / CANCEL ROD TOKEN 0x12) dispatch separately. ESXi and
///     Windows VAAI XCOPY both issue LID1.
///   - Identification target descriptors (type 0xE4) carrying a
///     T10 vendor-ID designator — the same format thurvsa publishes
///     in INQUIRY VPD 0x83. NAA designators (designator type 0x03)
///     aren't accepted yet because VPD 0x83 doesn't publish one.
///   - Block-device-to-block-device segment descriptors (type 0x02)
///     with a 16-bit block count and 64-bit source / destination
///     LBAs (the spec's "small" form — sufficient for VAAI's per-
///     command 4 MiB cap).
///   - Inline-data descriptors aren't supported; a non-zero
///     INLINE DATA LENGTH in the header is rejected.
///
/// Per-segment routing:
///   - Fast path — when src and dst resolve to the same volume,
///     the LBAs and block count are all multiples of the volume's
///     page size, and src/dst ranges don't overlap: drop into
///     `PageCache::clone_page_range`. A clean source page's chunk
///     hash is rebound to the destination's page-index entry; no
///     bytes cross the pool boundary.
///   - Slow path — every other combination: drive 1 MiB chunks
///     through `cache.read_bytes(src)` + `cache.write_bytes(dst)`.
///     Handles unaligned ranges, cross-volume copies, dirty-source
///     pages, and same-volume overlaps.
///
/// Synchronous: the whole copy completes before the handler returns
/// GOOD. VAAI caps per-command transfer at a few MiB so latency is
/// bounded. RECEIVE COPY RESULTS SA 0x00 reports "operation
/// completed without errors" against any list ID.
///
/// Reservation-gated: every destination LUN is checked against the
/// PERSISTENT RESERVATIONS manager before any cloning. A reservation
/// conflict on any destination short-circuits the whole copy.
/// WORM destination volumes refuse with WRITE PROTECTED.
pub(super) async fn extended_copy(
    req: &ScsiRequest<'_>,
    registry: &Arc<dyn VolumeLookup>,
    nexus: Nexus,
    reservations: &ReservationManager,
    tokens: &Arc<TokenManager>,
) -> ScsiResponse {
    if req.cdb.len() < 16 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let sa = req.cdb[1] & 0x1F;
    match sa {
        0x00 => extended_copy_lid1(req, registry, nexus, reservations).await,
        0x01 => extended_copy_lid4(req, registry, nexus, reservations).await,
        0x10 => populate_token(req, registry, nexus, tokens).await,
        0x11 => write_using_token(req, registry, nexus, reservations, tokens).await,
        0x12 => cancel_rod_token(req, tokens),
        _ => ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
    }
}

/// CANCEL ROD TOKEN (EXTENDED COPY service action 0x12, SPC-4 §6.5).
/// Invalidates the ROD token minted by the POPULATE TOKEN whose LIST
/// IDENTIFIER matches the one in this CDB (bytes 6-9, the 0x83
/// token-operation layout), releasing the token's chunk pins so
/// eviction + GC can reclaim them ahead of the inactivity TTL.
///
/// We identify the token to cancel by LIST IDENTIFIER. SPC-4 also
/// allows a parameter list carrying the ROD token itself; we accept
/// (and ignore) any parameter list and key off the list ID, which is
/// unambiguous in the CDB. Cancelling a token the copy manager no
/// longer holds — unknown / already-expired / never-minted list ID —
/// is a GOOD no-op per SPC-4, not an error.
fn cancel_rod_token(req: &ScsiRequest<'_>, tokens: &Arc<TokenManager>) -> ScsiResponse {
    let list_id = u32::from_be_bytes([req.cdb[6], req.cdb[7], req.cdb[8], req.cdb[9]]);
    let _ = tokens.cancel(list_id);
    ScsiResponse::good(Vec::new())
}

async fn extended_copy_lid1(
    req: &ScsiRequest<'_>,
    registry: &Arc<dyn VolumeLookup>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let (targets, planned) = match parse_extended_copy(req, registry, &nexus, reservations) {
        Ok(v) => v,
        Err(ExtendedCopyParseError::Noop) => {
            // Zero-length parameter list or all-zero-block segments
            // are valid no-ops; SPC-3 doesn't call that "success or
            // reject" so we don't bump either counter.
            return ScsiResponse::good(Vec::new());
        }
        Err(ExtendedCopyParseError::ReservationConflict) => {
            shared_telemetry::record::scsi_xcopy("reject");
            return ScsiResponse::reservation_conflict();
        }
        Err(ExtendedCopyParseError::Sense(s)) => {
            shared_telemetry::record::scsi_xcopy("reject");
            return ScsiResponse::check(s);
        }
    };
    extended_copy_execute(targets, planned).await
}

/// EXTENDED COPY (LID4) — opcode 0x83 service action 0x01 (SPC-4
/// §6.4). LID4 carries a richer 48-byte parameter-list header (a
/// 4-byte LIST IDENTIFIER, header-CSCD support, immediate-mode flag)
/// but reuses the exact same CSCD target descriptor (0xE4) and
/// block-to-block segment descriptor (0x02) bodies as LID1, so once
/// the header is parsed the resolution + execution path is shared.
///
/// We implement the same block-to-block subset as the LID1 path. The
/// IMMED bit is accepted and ignored — the copy still runs
/// synchronously and RECEIVE COPY RESULTS reports it complete.
/// Header CSCD descriptors and inline data are rejected (no host in
/// our path uses them); no production initiator issues LID4 at all,
/// so this exists purely for SPC-4 completeness.
async fn extended_copy_lid4(
    req: &ScsiRequest<'_>,
    registry: &Arc<dyn VolumeLookup>,
    nexus: Nexus,
    reservations: &ReservationManager,
) -> ScsiResponse {
    let (targets, planned) = match parse_extended_copy_lid4(req, registry, &nexus, reservations) {
        Ok(v) => v,
        Err(ExtendedCopyParseError::Noop) => return ScsiResponse::good(Vec::new()),
        Err(ExtendedCopyParseError::ReservationConflict) => {
            shared_telemetry::record::scsi_xcopy("reject");
            return ScsiResponse::reservation_conflict();
        }
        Err(ExtendedCopyParseError::Sense(s)) => {
            shared_telemetry::record::scsi_xcopy("reject");
            return ScsiResponse::check(s);
        }
    };
    extended_copy_execute(targets, planned).await
}

/// Reasons `parse_extended_copy` returns early.
enum ExtendedCopyParseError {
    /// Zero-length parameter list — valid no-op.
    Noop,
    /// Active persistent reservation on the destination LUN.
    ReservationConflict,
    /// Any other rejection that surfaces as CHECK CONDITION.
    Sense(SenseData),
}

/// (LUN, PageCache) pair resolved from one identification target
/// descriptor. Segment descriptors' src / dst CSCD indices select
/// into a `Vec<TargetHandle>` built by `parse_extended_copy`.
type TargetHandle = (u64, Arc<PageCache>);

/// Parse the EXTENDED COPY parameter list and run every pre-flight
/// validation: CDB / header / descriptor shape, NAA resolution,
/// destination reservation + WORM gates, range bounds. Returns the
/// resolved target list and the executable plan, or the first
/// rejection reason.
fn parse_extended_copy(
    req: &ScsiRequest<'_>,
    registry: &Arc<dyn VolumeLookup>,
    nexus: &Nexus,
    reservations: &ReservationManager,
) -> Result<(Vec<TargetHandle>, Vec<PlannedSegment>), ExtendedCopyParseError> {
    // Caller (`extended_copy`) has already checked CDB length and
    // dispatched on service action — this function is only called
    // for the LID1 path (SA 0x00).
    //
    // PARAMETER LIST LENGTH in CDB bytes 10..14 (32-bit BE).
    let plist_len =
        u32::from_be_bytes([req.cdb[10], req.cdb[11], req.cdb[12], req.cdb[13]]) as usize;
    if plist_len == 0 {
        // SPC-3 §6.3.2: a zero parameter list length is not an
        // error — it specifies "no copy operation".
        return Err(ExtendedCopyParseError::Noop);
    }
    if req.data_out.len() < plist_len {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let plist = &req.data_out[..plist_len];
    // SPC-3 §6.3.3 parameter list header (16 bytes):
    //   byte 0       LIST IDENTIFIER (ignored; we don't track lists)
    //   byte 1       PRIORITY + LIST_ID_USAGE + reserved (ignored)
    //   bytes 2-3    TARGET DESCRIPTOR LIST LENGTH (16-bit BE)
    //   bytes 4-7    reserved
    //   bytes 8-11   SEGMENT DESCRIPTOR LIST LENGTH (32-bit BE)
    //   bytes 12-15  INLINE DATA LENGTH (32-bit BE)
    if plist.len() < 16 {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let tdesc_len = u16::from_be_bytes([plist[2], plist[3]]) as usize;
    let sdesc_len = u32::from_be_bytes([plist[8], plist[9], plist[10], plist[11]]) as usize;
    let inline_len = u32::from_be_bytes([plist[12], plist[13], plist[14], plist[15]]) as usize;
    if inline_len != 0 {
        // Inline data isn't supported; VAAI doesn't use it.
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    if tdesc_len == 0 || sdesc_len == 0 {
        // Need at least one target and one segment to copy anything.
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let need = 16usize.saturating_add(tdesc_len).saturating_add(sdesc_len);
    if need > plist.len() {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    // Target descriptors begin right after the 16-byte LID1 header;
    // segment descriptors follow them. The descriptor formats (0xE4
    // identification CSCD, 0x02 block-to-block segment) are identical
    // to LID4, so the resolution + planning is shared.
    resolve_targets_and_plan(
        plist,
        16,
        tdesc_len,
        16 + tdesc_len,
        sdesc_len,
        registry,
        nexus,
        reservations,
    )
}

/// Resolve the CSCD (target) descriptors and build the segment plan,
/// then range-check + reservation/WORM-gate every segment. Shared by
/// the LID1 (`parse_extended_copy`) and LID4
/// (`parse_extended_copy_lid4`) header parsers, which differ only in
/// header shape — the target descriptor (0xE4, 32 bytes) and
/// block-to-block segment descriptor (0x02, 28 bytes) bodies are the
/// same in both. `tdesc_off` / `sdesc_off` are byte offsets into
/// `plist` where the target and segment descriptor lists begin; the
/// caller has already checked both regions fit within `plist`.
#[allow(clippy::too_many_arguments)]
fn resolve_targets_and_plan(
    plist: &[u8],
    tdesc_off: usize,
    tdesc_len: usize,
    sdesc_off: usize,
    sdesc_len: usize,
    registry: &Arc<dyn VolumeLookup>,
    nexus: &Nexus,
    reservations: &ReservationManager,
) -> Result<(Vec<TargetHandle>, Vec<PlannedSegment>), ExtendedCopyParseError> {
    // Identification target descriptors are 32 bytes each (SPC-3
    // §6.3.6.3). VAAI sends one per CSCD reference; the source
    // and destination indices in each segment descriptor index
    // into this list.
    if !tdesc_len.is_multiple_of(32) {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let tdesc_count = tdesc_len / 32;
    if tdesc_count == 0 || tdesc_count > u16::MAX as usize {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }

    // Resolve every target descriptor up front. Each entry pairs the
    // resolved LUN with its PageCache handle so per-segment
    // reservation checks have everything they need without going
    // back to the registry.
    let mut targets: Vec<TargetHandle> = Vec::with_capacity(tdesc_count);
    for i in 0..tdesc_count {
        let off = tdesc_off + i * 32;
        let desc = &plist[off..off + 32];
        let pair =
            resolve_target_descriptor(registry, desc).map_err(ExtendedCopyParseError::Sense)?;
        targets.push(pair);
    }

    // Walk segment descriptors. Each block-to-block descriptor is
    // 4 (header) + 0x18 (body) = 28 bytes; reject any other
    // segment type up front so a malformed parameter list never
    // half-commits.
    let mut seg_cursor = 0usize;
    let mut planned: Vec<PlannedSegment> = Vec::new();
    while seg_cursor < sdesc_len {
        if seg_cursor + 4 > sdesc_len {
            return Err(ExtendedCopyParseError::Sense(
                SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
            ));
        }
        let sd = &plist[sdesc_off + seg_cursor..sdesc_off + sdesc_len];
        let sdtype = sd[0];
        let dlen = u16::from_be_bytes([sd[2], sd[3]]) as usize;
        let total = 4 + dlen;
        if seg_cursor + total > sdesc_len {
            return Err(ExtendedCopyParseError::Sense(
                SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
            ));
        }
        if sdtype != 0x02 || dlen != 0x18 {
            return Err(ExtendedCopyParseError::Sense(
                SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
            ));
        }
        // Block-to-block descriptor (SPC-3 §6.3.5.4):
        //   bytes 4-5   source CSCD descriptor index
        //   bytes 6-7   destination CSCD descriptor index
        //   bytes 10-11 BLOCK DEVICE NUMBER OF BLOCKS (16-bit BE)
        //   bytes 12-19 source LBA (64-bit BE)
        //   bytes 20-27 destination LBA (64-bit BE)
        let src_idx = u16::from_be_bytes([sd[4], sd[5]]) as usize;
        let dst_idx = u16::from_be_bytes([sd[6], sd[7]]) as usize;
        let blocks = u16::from_be_bytes([sd[10], sd[11]]) as u64;
        let src_lba = u64::from_be_bytes([
            sd[12], sd[13], sd[14], sd[15], sd[16], sd[17], sd[18], sd[19],
        ]);
        let dst_lba = u64::from_be_bytes([
            sd[20], sd[21], sd[22], sd[23], sd[24], sd[25], sd[26], sd[27],
        ]);
        if src_idx >= targets.len() || dst_idx >= targets.len() {
            return Err(ExtendedCopyParseError::Sense(
                SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
            ));
        }
        planned.push(PlannedSegment {
            src_idx,
            dst_idx,
            blocks,
            src_lba,
            dst_lba,
        });
        seg_cursor += total;
    }

    // Range-check every segment + gate every destination LUN
    // before any data motion. SPC-3 doesn't require all-or-nothing
    // semantics across segments, but doing the validation up front
    // means a malformed segment N never half-commits a successful
    // segment N-1.
    for seg in &planned {
        let (_src_lun, src_cache) = &targets[seg.src_idx];
        let (dst_lun, dst_cache) = &targets[seg.dst_idx];
        if dst_cache.manifest().worm {
            return Err(ExtendedCopyParseError::Sense(SenseData::WRITE_PROTECTED));
        }
        if !reservations.allow_write(*dst_lun, nexus) {
            return Err(ExtendedCopyParseError::ReservationConflict);
        }
        let src_sz = Sizing::from(src_cache.as_ref());
        let dst_sz = Sizing::from(dst_cache.as_ref());
        let src_range = LbaRange {
            lba: seg.src_lba,
            blocks: seg.blocks,
        };
        let dst_range = LbaRange {
            lba: seg.dst_lba,
            blocks: seg.blocks,
        };
        if seg.blocks == 0 {
            continue;
        }
        if validate_in_range(&src_range, &src_sz).is_err()
            || validate_in_range(&dst_range, &dst_sz).is_err()
        {
            return Err(ExtendedCopyParseError::Sense(SenseData::LBA_OUT_OF_RANGE));
        }
    }
    Ok((targets, planned))
}

/// Parse the EXTENDED COPY (LID4) parameter list (SPC-4 §6.4) and
/// hand off to the shared resolver. The 48-byte header:
///   byte 0       LIST FORMAT — must be 0x01 (the basic LID4 list
///                format; other codes select header layouts we don't
///                model)
///   byte 1       flags (ignored)
///   bytes 2-3    HEADER CSCD LIST LENGTH — must be 0 (no header CSCDs)
///   bytes 4-14   reserved
///   byte 15      flags2 (bit 0 IMMED, bit 1 G_SENSE) — accepted, ignored
///   byte 16      HEADER CSCD TYPE CODE (unused when no header CSCDs)
///   bytes 17-19  reserved
///   bytes 20-23  LIST IDENTIFIER (ignored — XCOPY isn't list-tracked)
///   bytes 24-41  reserved
///   bytes 42-43  CSCD (target) DESCRIPTOR LIST LENGTH (16-bit BE)
///   bytes 44-45  SEGMENT DESCRIPTOR LIST LENGTH (16-bit BE)
///   bytes 46-47  INLINE DATA LENGTH (16-bit BE) — must be 0
/// then the CSCD descriptors (0xE4), segment descriptors (0x02), and
/// inline data, in that order.
fn parse_extended_copy_lid4(
    req: &ScsiRequest<'_>,
    registry: &Arc<dyn VolumeLookup>,
    nexus: &Nexus,
    reservations: &ReservationManager,
) -> Result<(Vec<TargetHandle>, Vec<PlannedSegment>), ExtendedCopyParseError> {
    // PARAMETER LIST LENGTH in CDB bytes 10..14 (32-bit BE).
    let plist_len =
        u32::from_be_bytes([req.cdb[10], req.cdb[11], req.cdb[12], req.cdb[13]]) as usize;
    if plist_len == 0 {
        // Zero parameter list length specifies "no copy operation".
        return Err(ExtendedCopyParseError::Noop);
    }
    if req.data_out.len() < plist_len {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let plist = &req.data_out[..plist_len];
    const LID4_HEADER_LEN: usize = 48;
    /// SPC-4 LID4 LIST FORMAT code for the basic parameter list.
    const LID4_LIST_FORMAT: u8 = 0x01;
    if plist.len() < LID4_HEADER_LEN {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    if plist[0] != LID4_LIST_FORMAT {
        // Unknown LIST FORMAT — we can't trust the header layout.
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let header_cscd_len = u16::from_be_bytes([plist[2], plist[3]]) as usize;
    if header_cscd_len != 0 {
        // Header CSCD descriptors aren't modeled; the resolver
        // expects the CSCD list to begin right after the header.
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let tdesc_len = u16::from_be_bytes([plist[42], plist[43]]) as usize;
    let sdesc_len = u16::from_be_bytes([plist[44], plist[45]]) as usize;
    let inline_len = u16::from_be_bytes([plist[46], plist[47]]) as usize;
    if inline_len != 0 {
        // Inline data isn't supported (mirrors the LID1 path).
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    if tdesc_len == 0 || sdesc_len == 0 {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    let need = LID4_HEADER_LEN
        .saturating_add(tdesc_len)
        .saturating_add(sdesc_len);
    if need > plist.len() {
        return Err(ExtendedCopyParseError::Sense(
            SenseData::INVALID_FIELD_IN_PARAMETER_LIST,
        ));
    }
    resolve_targets_and_plan(
        plist,
        LID4_HEADER_LEN,
        tdesc_len,
        LID4_HEADER_LEN + tdesc_len,
        sdesc_len,
        registry,
        nexus,
        reservations,
    )
}

/// Execute a pre-validated XCOPY plan. Returns GOOD on full success,
/// CHECK CONDITION on the first per-segment failure. Records the
/// success / error outcome and the per-segment bytes / path
/// telemetry.
async fn extended_copy_execute(
    targets: Vec<TargetHandle>,
    planned: Vec<PlannedSegment>,
) -> ScsiResponse {
    // SPC-3 §6.3.7: failures stop further segments. Earlier segments
    // stay committed (no rollback) — the COMMAND-SPECIFIC INFORMATION
    // field would carry the failing segment index in a richer sense
    // format than we emit today.
    for seg in &planned {
        if seg.blocks == 0 {
            continue;
        }
        let (_src_lun, src_cache) = &targets[seg.src_idx];
        let (_dst_lun, dst_cache) = &targets[seg.dst_idx];
        match execute_segment(
            src_cache.as_ref(),
            seg.src_lba,
            dst_cache.as_ref(),
            seg.dst_lba,
            seg.blocks,
        )
        .await
        {
            Ok(path) => {
                let bytes = seg.blocks * Sizing::from(src_cache.as_ref()).sector;
                shared_telemetry::record::scsi_xcopy_bytes(path, bytes);
                shared_telemetry::record::scsi_xcopy_segment(path);
            }
            Err(s) => {
                shared_telemetry::record::scsi_xcopy("error");
                return ScsiResponse::check(s);
            }
        }
    }
    shared_telemetry::record::scsi_xcopy("success");
    ScsiResponse::good(Vec::new())
}

/// RECEIVE COPY RESULTS — opcode 0x84. Companion to EXTENDED COPY.
///
/// Service actions implemented (SPC-4 §6.20; the full LID1 set plus
/// the ODX token-info action):
///   - 0x00 COPY STATUS — synchronous XCOPY always completes before
///     EXTENDED COPY returns, so any list ID the host queries gets
///     "operation completed without errors" with zero per-segment
///     accounting. ESXi rarely polls.
///   - 0x01 RECEIVE DATA — retrieves held data produced by
///     inline-data / host-bound segment descriptors. We accept
///     neither, so there is never held data: the response is the
///     bare AVAILABLE DATA = 0 header.
///   - 0x03 OPERATING PARAMETERS — what the host actually relies on
///     to gate VAAI: the per-XCOPY limits and the descriptor types
///     we accept. Numbers match VPD 0x8F's descriptor 0x0004 and
///     0x8001 advertisements.
///   - 0x04 FAILED SEGMENT DETAILS — per-LIST IDENTIFIER failure
///     record. Synchronous XCOPY surfaces a failing segment inline
///     as CHECK CONDITION on the EXTENDED COPY itself and retains no
///     per-list record, so this always reports "no failed segment".
///   - 0x07 RECEIVE ROD TOKEN INFORMATION — ODX token-info channel
///     (handled next to the token manager below).
///
/// Service action 0x05 is reserved in SPC-4 (there is no "operations
/// count" action); like every other unimplemented SA it rejects with
/// INVALID FIELD IN CDB.
pub(super) fn receive_copy_results(
    req: &ScsiRequest<'_>,
    tokens: &Arc<TokenManager>,
) -> ScsiResponse {
    if req.cdb.len() < 16 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let sa = req.cdb[1] & 0x1F;
    let alloc_len =
        u32::from_be_bytes([req.cdb[10], req.cdb[11], req.cdb[12], req.cdb[13]]) as usize;
    let body = match sa {
        0x00 => build_copy_status_response(),
        0x01 => build_receive_data_response(),
        0x03 => build_operating_parameters_response(),
        0x04 => build_failed_segment_details_response(),
        0x07 => {
            let list_id = u32::from_be_bytes([req.cdb[2], req.cdb[3], req.cdb[4], req.cdb[5]]);
            build_rrti_response(tokens.job_result(list_id))
        }
        _ => return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
    };
    let truncated: Vec<u8> = body.into_iter().take(alloc_len).collect();
    ScsiResponse::good(truncated)
}

/// Build the RECEIVE ROD TOKEN INFORMATION (SA 0x07) response per
/// SPC-4 §6.21.2.3. Three cases:
///
/// - `Some(JobResult { token: Some(t), .. })` — completed POPULATE
///   TOKEN: response carries the 512-byte ROD token wrapped in a
///   single token descriptor.
/// - `Some(JobResult { token: None, .. })` — completed WRITE USING
///   TOKEN: response carries TRANSFER COUNT but no token descriptor.
/// - `None` — no operation in progress for this LIST IDENTIFIER:
///   response carries COPY OPERATION STATUS = 0x00 ("no copy
///   operation in progress") and zero token / transfer counts.
fn build_rrti_response(job: Option<JobResult>) -> Vec<u8> {
    // Common base layout: 32-byte fixed header + 4-byte ROD TOKEN
    // DESCRIPTORS LENGTH + optional token descriptor.
    let mut out: Vec<u8> = Vec::with_capacity(32 + 4 + 4 + ROD_TOKEN_LEN);
    // Reserve space for AVAILABLE DATA (filled at the end).
    out.extend_from_slice(&[0u8; 4]);
    let (response_to_sa, copy_op_status, transfer_blocks, token) = match job.as_ref() {
        Some(j) => {
            let resp_sa = if j.token.is_some() { 0x10 } else { 0x11 };
            let status = match &j.status {
                JobStatus::Done => 0x02u8,          // operation completed without errors
                JobStatus::Failed { .. } => 0x03u8, // completed with errors
            };
            (resp_sa, status, j.transfer_blocks, j.token)
        }
        None => (0x10u8, 0x00u8, 0u64, None), // no operation in progress
    };
    out.push(response_to_sa);
    out.push(copy_op_status);
    out.extend_from_slice(&0u16.to_be_bytes()); // OPERATION COUNTER
    out.extend_from_slice(&0u32.to_be_bytes()); // ESTIMATED STATUS UPDATE DELAY
    // EXTENDED COPY COMPLETION STATUS — sense key surface; 0 on success.
    let completion_status = match job.as_ref().map(|j| &j.status) {
        Some(JobStatus::Failed { completion_status }) => *completion_status,
        _ => 0,
    };
    out.push(completion_status);
    out.push(0); // SENSE DATA FIELD LENGTH (we don't attach sense data)
    out.push(0); // SENSE DATA LENGTH
    out.push(0x01); // TRANSFER COUNT UNITS = blocks
    out.extend_from_slice(&transfer_blocks.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // SEGMENTS PROCESSED
    out.extend_from_slice(&[0u8; 6]); // reserved
    // ROD TOKEN DESCRIPTORS LENGTH + descriptor body.
    if let Some(t) = token {
        // One descriptor: 2-byte type-prefix + 2-byte length + 512-byte token.
        let desc_len: u32 = (2 + 2 + ROD_TOKEN_LEN) as u32;
        out.extend_from_slice(&desc_len.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // descriptor type = 0x0000 (ROD token)
        out.extend_from_slice(&(ROD_TOKEN_LEN as u16).to_be_bytes());
        out.extend_from_slice(&t);
    } else {
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    let available = (out.len() - 4) as u32;
    out[0..4].copy_from_slice(&available.to_be_bytes());
    out
}

/// One block-to-block segment with everything the executor needs.
/// CSCD indices are resolved into the segment's `targets` slice;
/// LBAs / block count are in the source / destination volume's
/// native sector units (4 KiB).
struct PlannedSegment {
    src_idx: usize,
    dst_idx: usize,
    blocks: u64,
    src_lba: u64,
    dst_lba: u64,
}

/// Run one segment, choosing the fast or slow path. Caller has
/// already validated that the LBA ranges fit and that the
/// destination accepts writes (reservation + WORM). Returns the
/// path that ran (`"fast"` or `"slow"`) so the caller can tag
/// per-path telemetry.
async fn execute_segment(
    src: &PageCache,
    src_lba: u64,
    dst: &PageCache,
    dst_lba: u64,
    blocks: u64,
) -> Result<&'static str, SenseData> {
    let src_sz = Sizing::from(src);
    let dst_sz = Sizing::from(dst);
    if src_sz.sector != dst_sz.sector {
        // Cross-LUN copy between volumes with different sector
        // sizes is theoretically meaningful but we don't model it.
        return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let src_off = src_lba * src_sz.sector;
    let dst_off = dst_lba * dst_sz.sector;
    let len = blocks * src_sz.sector;

    let same_lun = std::ptr::eq(src as *const PageCache, dst as *const PageCache);
    let same_page_size = src.page_size() == dst.page_size();
    let page = src.page_size();
    let aligned = same_page_size
        && src_off.is_multiple_of(page)
        && dst_off.is_multiple_of(page)
        && len.is_multiple_of(page);
    let overlap = same_lun && ranges_overlap(src_off, src_off + len, dst_off, dst_off + len);
    if aligned && !overlap {
        // Fast path — per-page hash rebind via the cross-volume
        // clone helper. Same-LUN delegates through the receiver-as-
        // dst shim; cross-LUN uses the explicit src/dst signature.
        // The helper internally falls back to a bytes copy per page
        // when source and destination live in distinct chunk pools
        // (mismatched backend or `DedupScope::Local` namespace).
        src.clone_page_range_into(src_off, dst, dst_off, len)
            .await
            .map_err(|e| map_write_error(&e))?;
        return Ok("fast");
    }
    // Slow path — 1 MiB-bounded streaming bytes copy. Handles
    // cross-LUN copies, unaligned ranges, and same-LUN overlap.
    const CHUNK: u64 = 1024 * 1024;
    let mut remaining = len;
    let mut s_cur = src_off;
    let mut d_cur = dst_off;
    while remaining > 0 {
        let this = std::cmp::min(remaining, CHUNK) as usize;
        let buf = src
            .read_bytes(s_cur, this)
            .await
            .map_err(|e| map_read_error(&e))?;
        dst.write_bytes(d_cur, &buf)
            .await
            .map_err(|e| map_write_error(&e))?;
        s_cur += this as u64;
        d_cur += this as u64;
        remaining -= this as u64;
    }
    Ok("slow")
}

fn ranges_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

/// Resolve one 32-byte identification target descriptor to the
/// matching LUN + cache handle. Accepts NAA designators
/// (designator type 0x03) only — the SPC-3 EXTENDED COPY target
/// descriptor has just 20 bytes for the designator, which the T10
/// form (44 bytes today) doesn't fit. VPD 0x83 publishes NAA
/// alongside T10 specifically for this path.
fn resolve_target_descriptor(
    registry: &Arc<dyn VolumeLookup>,
    desc: &[u8],
) -> Result<(u64, Arc<PageCache>), SenseData> {
    if desc.len() < 32 || desc[0] != 0xE4 {
        return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    // SPC-3 §6.3.6.3 identification CSCD descriptor:
    //   byte 4       CODE SET (bits 3-0)
    //   byte 5       bit 4 PIV | bits 3-0 DESIGNATOR TYPE
    //   byte 6       reserved / association
    //   byte 7       DESIGNATOR LENGTH (N)
    //   bytes 8..8+N DESIGNATOR
    let designator_type = desc[5] & 0x0F;
    let designator_len = desc[7] as usize;
    if 8 + designator_len > 28 {
        return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let designator = &desc[8..8 + designator_len];
    if designator_type != 0x03 || designator_len != 8 {
        return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    for lun in registry.luns() {
        let Some(cache) = registry.get(lun) else {
            continue;
        };
        let expected = naa_locally_assigned(&cache.manifest().uuid);
        if designator == expected.as_slice() {
            return Ok((lun, cache));
        }
    }
    Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST)
}

/// Build the SPC-3 §6.18.4 OPERATING PARAMETERS response (RECEIVE
/// COPY RESULTS service action 0x03). 44 bytes header + per-
/// implemented-descriptor entries (one byte each).
fn build_operating_parameters_response() -> Vec<u8> {
    // We accept two descriptor type codes today: target descriptor
    // 0xE4 (Identification) and segment descriptor 0x02 (Block to
    // Block).
    let supported: [u8; 2] = [0xE4, 0x02];
    let header_len: usize = 44;
    let body_len: usize = header_len + supported.len();
    let mut data = vec![0u8; body_len];
    // AVAILABLE DATA = length following these 4 bytes.
    let avail = (body_len - 4) as u32;
    data[0..4].copy_from_slice(&avail.to_be_bytes());
    // byte 4 bit 0: SNLID = 1 — we support the SUPPORTED NO LIST
    // ID form so initiators that prefer it can use it.
    data[4] = 0x01;
    // bytes 8-9: MAXIMUM TARGET DESCRIPTOR COUNT = 2 (source + dst
    // for a single same-LUN or cross-LUN copy).
    data[8..10].copy_from_slice(&2u16.to_be_bytes());
    // bytes 10-11: MAXIMUM SEGMENT DESCRIPTOR COUNT = 1. Per-
    // command segment count starts small; we lift it once we have
    // a real ESXi workload to measure against.
    data[10..12].copy_from_slice(&1u16.to_be_bytes());
    // bytes 12-15: MAXIMUM DESCRIPTOR LIST LENGTH (bytes).
    // 16-byte header + 2 × 32-byte target descriptors + 1 ×
    // 28-byte block-to-block segment = 108 bytes; round to 128.
    data[12..16].copy_from_slice(&128u32.to_be_bytes());
    // bytes 16-19: MAXIMUM SEGMENT LENGTH (bytes). 16 MiB.
    data[16..20].copy_from_slice(&(16u32 << 20).to_be_bytes());
    // bytes 20-23: MAXIMUM INLINE DATA LENGTH = 0 (inline data
    // unsupported).
    // bytes 24-27: HELD DATA LIMIT = 0.
    // bytes 28-31: MAXIMUM STREAM DEVICE TRANSFER SIZE = 0 (we
    // only do block-to-block).
    // bytes 32-33: reserved.
    // bytes 34-35: TOTAL CONCURRENT COPIES = 0 (we don't track).
    // byte 36: MAXIMUM CONCURRENT COPIES = 0 (synchronous; one at
    // a time per connection).
    // byte 37: DATA SEGMENT GRANULARITY = log2(page_size). 64 KiB
    // page = 16; we publish the per-VSA default for the
    // operating-parameters page since the wire field is target-
    // wide, not per-LUN.
    data[37] = 16; // log2(64 KiB)
    // byte 38: INLINE DATA GRANULARITY = 0 (inline data unsupported).
    // byte 39: HELD DATA GRANULARITY = 0.
    // bytes 40-42: reserved.
    // byte 43: IMPLEMENTED DESCRIPTOR LIST LENGTH.
    data[43] = supported.len() as u8;
    // bytes 44..: per-descriptor type codes.
    data[44..44 + supported.len()].copy_from_slice(&supported);
    data
}

/// Build the SPC-3 §6.18.2 COPY STATUS response (RECEIVE COPY
/// RESULTS service action 0x00). 16-byte fixed shape. Synchronous
/// XCOPY completes before EXTENDED COPY returns GOOD, so every list
/// ID query reports "completed without errors" with zero per-segment
/// accounting — matching the LIO behavior.
fn build_copy_status_response() -> Vec<u8> {
    let mut data = vec![0u8; 16];
    // bytes 0-3: AVAILABLE DATA = 12 (bytes following).
    data[0..4].copy_from_slice(&12u32.to_be_bytes());
    // byte 4: COPY MANAGER STATUS = 0x02 ("operation completed
    // without errors"). The previous EXTENDED COPY is implicitly
    // the one this status answers.
    data[4] = 0x02;
    // bytes 5-6: SEGMENTS PROCESSED = 0 (not tracked).
    // byte 7: TRANSFER COUNT UNITS = 0.
    // bytes 8-11: TRANSFER COUNT = 0 (not tracked).
    // bytes 12-15: reserved.
    data
}

/// Build the SPC-4 §6.20.2 RECEIVE DATA response (RECEIVE COPY
/// RESULTS service action 0x01). RECEIVE DATA returns "held data"
/// produced by inline-data or host-bound segment descriptors. We
/// reject inline data (MAXIMUM INLINE DATA LENGTH = 0 in VPD 0x8F /
/// OPERATING PARAMETERS) and implement no held-data-producing
/// segment type, so there is never held data to return: the
/// response is the 4-byte AVAILABLE DATA header set to zero.
fn build_receive_data_response() -> Vec<u8> {
    // bytes 0-3: AVAILABLE DATA = 0 (no held data follows).
    vec![0u8; 4]
}

/// Build the SPC-4 §6.20.4 FAILED SEGMENT DETAILS response (RECEIVE
/// COPY RESULTS service action 0x04). The fixed copy-results header
/// is 60 bytes: the per-segment failure fields land at byte 56
/// (EXTENDED COPY COMMAND STATUS) and bytes 58-59 (SENSE DATA
/// LENGTH), with sense data — when present — at byte 60+. Our XCOPY
/// is synchronous and surfaces a failing segment inline as CHECK
/// CONDITION on the EXTENDED COPY command itself; it keeps no
/// per-LIST IDENTIFIER failure record, so this always reports "no
/// failed segment": command status 0, SENSE DATA LENGTH 0, no sense.
fn build_failed_segment_details_response() -> Vec<u8> {
    let mut data = vec![0u8; 60];
    // bytes 0-3: AVAILABLE DATA = 56 (bytes following these four).
    data[0..4].copy_from_slice(&56u32.to_be_bytes());
    // byte 56: EXTENDED COPY COMMAND STATUS = 0 (no error).
    // bytes 58-59: SENSE DATA LENGTH = 0 (no sense data follows).
    data
}

/// Map an `UploaderError` from the WRITE pipeline into a SCSI
/// sense. Internal validation errors that should have been caught
/// upstream collapse to INVALID FIELD IN CDB (defensive); upload
/// backpressure surfaces as NOT READY OPERATION IN PROGRESS so
/// backup software retries; cloud / io / hash failures collapse to
/// MEDIUM ERROR + WRITE ERROR.
fn map_write_error(e: &UploaderError) -> SenseData {
    match e {
        UploaderError::PageSizeMismatch { .. } | UploaderError::PageOutOfRange { .. } => {
            SenseData::INVALID_FIELD_IN_CDB
        }
        UploaderError::Backpressured(_) => SenseData::LU_NOT_READY_OPERATION_IN_PROGRESS,
        _ => SenseData::WRITE_ERROR,
    }
}

/// Map an `UploaderError` from the READ pipeline into a SCSI
/// sense. Same shape as [`map_write_error`] but the medium-error
/// ASC differs (UNRECOVERED READ ERROR vs WRITE ERROR). The read
/// path doesn't go through the upload backpressure gate, but the
/// match arm is kept symmetric so a future refetch-path gate would
/// drop in cleanly.
fn map_read_error(e: &UploaderError) -> SenseData {
    match e {
        UploaderError::PageSizeMismatch { .. } | UploaderError::PageOutOfRange { .. } => {
            SenseData::INVALID_FIELD_IN_CDB
        }
        UploaderError::Backpressured(_) => SenseData::LU_NOT_READY_OPERATION_IN_PROGRESS,
        _ => SenseData::READ_ERROR,
    }
}

// ---------------------------------------------------------------------
// Hyper-V ODX — POPULATE TOKEN (0x83 sa 0x10) + WRITE USING TOKEN
// (0x83 sa 0x11). RECEIVE ROD TOKEN INFORMATION (0x84 sa 0x07) lives
// next to `receive_copy_results` above.
//
// Parameter list layouts follow SPC-4 §6.18 (POPULATE TOKEN) and
// §6.19 (WRITE USING TOKEN). Both build their data plane on top of
// the Block Device Range Descriptor (16 bytes: 64-bit LBA + 32-bit
// block count + 32 reserved). We cap N at 8 to match the value
// published in VPD 0x8F descriptor 0x0000.
// ---------------------------------------------------------------------

const ODX_MAX_RANGE_DESCRIPTORS: usize = 8;
const ODX_BDRD_BYTES: usize = 16;

/// One 16-byte ODX Block Device Range Descriptor.
struct Bdrd {
    lba: u64,
    blocks: u32,
}

/// Parse N consecutive BDRDs from `buf`. `buf.len()` must be exactly
/// `N * 16`. Returns the descriptor list or `INVALID FIELD IN
/// PARAMETER LIST` on shape errors.
fn parse_bdrd_list(buf: &[u8]) -> Result<Vec<Bdrd>, SenseData> {
    if !buf.len().is_multiple_of(ODX_BDRD_BYTES) {
        return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let count = buf.len() / ODX_BDRD_BYTES;
    if count > ODX_MAX_RANGE_DESCRIPTORS {
        return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * ODX_BDRD_BYTES;
        let lba = u64::from_be_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
            buf[off + 4],
            buf[off + 5],
            buf[off + 6],
            buf[off + 7],
        ]);
        let blocks = u32::from_be_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
        out.push(Bdrd { lba, blocks });
    }
    Ok(out)
}

/// POPULATE TOKEN (EXTENDED COPY service action 0x10). Snapshots the
/// source's per-page chunk hashes across the requested LBA range,
/// pins them in the chunk pool against eviction, mints a 512-byte
/// ROD token, and records a `Done` job result under the CDB's LIST
/// IDENTIFIER. The host fetches the token via
/// RECEIVE ROD TOKEN INFORMATION (`0x84` sa `0x07`).
///
/// Sync-inline: the token is ready before this returns. The host's
/// first RRTI poll sees `Done`. SPC-4 lets implementations report
/// "in progress" intermediates, but our pipeline doesn't have
/// useful intermediate state to surface.
async fn populate_token(
    req: &ScsiRequest<'_>,
    registry: &Arc<dyn VolumeLookup>,
    nexus: Nexus,
    tokens: &Arc<TokenManager>,
) -> ScsiResponse {
    // POPULATE TOKEN's source is the LUN the CDB was sent to. No
    // CSCD target descriptors.
    let _ = nexus;
    let src_cache = match registry.get(req.lun) {
        Some(c) => c,
        None => return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED),
    };
    // LIST IDENTIFIER sits at CDB bytes 6-9 for the 0x83 token
    // operations (POPULATE / WRITE USING / CANCEL ROD TOKEN); bytes
    // 2-5 are reserved. RECEIVE ROD TOKEN INFORMATION (0x84 sa 0x07)
    // is the one that carries it at bytes 2-5.
    let list_id = u32::from_be_bytes([req.cdb[6], req.cdb[7], req.cdb[8], req.cdb[9]]);
    let plist_len =
        u32::from_be_bytes([req.cdb[10], req.cdb[11], req.cdb[12], req.cdb[13]]) as usize;
    if plist_len == 0 {
        // Zero-length parameter list is a no-op per SPC-4 §6.18.
        return ScsiResponse::good(Vec::new());
    }
    if req.data_out.len() < plist_len || plist_len < 16 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let plist = &req.data_out[..plist_len];
    // Parameter list header (SPC-4 §6.18.2):
    //   bytes 0-1   ROD TOKEN DATA LENGTH (BE16, plist_len - 2 here)
    //   byte  2     IMMED (bit 0; we ignore — always sync)
    //   byte  3     reserved
    //   bytes 4-7   INACTIVITY TIMEOUT (BE32, seconds; 0 → default)
    //   bytes 8-13  reserved
    //   bytes 14-15 BLOCK DEVICE RANGE DESCRIPTORS LIST LENGTH (BE16)
    let inactivity = u32::from_be_bytes([plist[4], plist[5], plist[6], plist[7]]);
    let bdrd_total = u16::from_be_bytes([plist[14], plist[15]]) as usize;
    if 16 + bdrd_total > plist.len() {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let bdrds = match parse_bdrd_list(&plist[16..16 + bdrd_total]) {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    if bdrds.is_empty() {
        return ScsiResponse::good(Vec::new());
    }
    // Range, alignment, sector-size validation against the source
    // volume — every range must align to whole pages so the snapshot
    // can record one chunk hash per page.
    let sector = src_cache.sector_size();
    let page = src_cache.page_size();
    if !page.is_multiple_of(sector) || page == 0 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let sectors_per_page = page / sector;
    let sz = Sizing::from(src_cache.as_ref());
    let mut total_blocks: u64 = 0;
    for bd in &bdrds {
        if bd.blocks == 0 {
            continue;
        }
        let range = LbaRange {
            lba: bd.lba,
            blocks: bd.blocks as u64,
        };
        if validate_in_range(&range, &sz).is_err() {
            return ScsiResponse::check(SenseData::LBA_OUT_OF_RANGE);
        }
        if !bd.lba.is_multiple_of(sectors_per_page)
            || !(bd.blocks as u64).is_multiple_of(sectors_per_page)
        {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        total_blocks = total_blocks.saturating_add(bd.blocks as u64);
    }
    // Force a flush of any dirty source pages in the requested range
    // so the page-index snapshot reflects host writes the dispatcher
    // has acked. Hosts typically issue SYNCHRONIZE CACHE before ODX
    // anyway; this is defense-in-depth so a missing host sync can't
    // produce a stale snapshot.
    for bd in &bdrds {
        if bd.blocks == 0 {
            continue;
        }
        let first_page = (bd.lba * sector) / page;
        let last_page = ((bd.lba + bd.blocks as u64) * sector - 1) / page;
        let first_u32 = match u32::try_from(first_page) {
            Ok(v) => v,
            Err(_) => return ScsiResponse::check(SenseData::LBA_OUT_OF_RANGE),
        };
        let last_u32 = match u32::try_from(last_page) {
            Ok(v) => v,
            Err(_) => return ScsiResponse::check(SenseData::LBA_OUT_OF_RANGE),
        };
        if let Err(e) = src_cache.flush_pages_in_range(first_u32, last_u32).await {
            return ScsiResponse::check(map_write_error(&e));
        }
    }
    // Snapshot per-page hashes + pin every referenced chunk.
    let pool = src_cache.writer().pool();
    let page_index = src_cache.writer().page_index();
    let mut source_pages: Vec<u32> = Vec::new();
    let mut hashes: Vec<Option<[u8; 32]>> = Vec::new();
    let mut pins: Vec<shared_pool::PoolPinGuard> = Vec::new();
    let mut pinned: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
    for bd in &bdrds {
        if bd.blocks == 0 {
            continue;
        }
        let first_page = ((bd.lba * sector) / page) as u32;
        let pages_in_range = ((bd.blocks as u64) * sector / page) as u32;
        for p in 0..pages_in_range {
            let page_id = first_page + p;
            let hash = match page_index.get(page_id) {
                Ok(opt) => opt,
                Err(e) => {
                    return ScsiResponse::check(map_read_error(&UploaderError::PageIndex(e)));
                }
            };
            source_pages.push(page_id);
            if let Some(h) = hash.as_ref()
                && pinned.insert(*h)
            {
                pins.push(pool.pin(&hex::encode(h)));
            }
            hashes.push(hash);
        }
    }
    let ttl = tokens.resolve_ttl(inactivity);
    let manifest = src_cache.manifest();
    let state = TokenState {
        source_volume_uuid: manifest.uuid,
        source_lun: req.lun,
        source_backend: manifest.backend.clone(),
        source_namespace: manifest.pool_namespace(),
        source_page_size: manifest.page_size_bytes,
        sector_size: manifest.sector_bytes,
        source_pages,
        hashes,
        total_blocks,
        deadline: std::time::Instant::now() + ttl,
        pins,
    };
    let _ = tokens.mint_token(list_id, state);
    ScsiResponse::good(Vec::new())
}

/// WRITE USING TOKEN (EXTENDED COPY service action 0x11). Look up
/// the snapshot under the supplied ROD token; apply its per-page
/// chunk hashes to the destination volume's `pages.idx` across the
/// destination range. Cross-volume by design — Hyper-V's primary
/// use of ODX is moving a VHDX between LUNs.
///
/// Sync-inline; records a job outcome under the CDB's LIST IDENTIFIER
/// so RRTI can answer.
async fn write_using_token(
    req: &ScsiRequest<'_>,
    registry: &Arc<dyn VolumeLookup>,
    nexus: Nexus,
    reservations: &ReservationManager,
    tokens: &Arc<TokenManager>,
) -> ScsiResponse {
    let dst_cache = match registry.get(req.lun) {
        Some(c) => c,
        None => return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED),
    };
    if dst_cache.manifest().worm {
        return ScsiResponse::check(SenseData::WRITE_PROTECTED);
    }
    if !reservations.allow_write(req.lun, &nexus) {
        return ScsiResponse::reservation_conflict();
    }
    // LIST IDENTIFIER at CDB bytes 6-9 (0x83 token-operation
    // layout); see the note in `populate_token`.
    let list_id = u32::from_be_bytes([req.cdb[6], req.cdb[7], req.cdb[8], req.cdb[9]]);
    let plist_len =
        u32::from_be_bytes([req.cdb[10], req.cdb[11], req.cdb[12], req.cdb[13]]) as usize;
    if plist_len == 0 {
        return ScsiResponse::good(Vec::new());
    }
    // Header (SPC-4 §6.19.2):
    //   bytes 0-1     PARAMETER DATA LENGTH (BE16, plist_len - 2)
    //   byte  2       IMMED (bit 0; ignored — sync)
    //   byte  3       reserved
    //   bytes 4-15    reserved
    //   bytes 16-527  ROD TOKEN (512 bytes)
    //   bytes 528-529 BLOCK DEVICE RANGE DESCRIPTORS LIST LENGTH (BE16)
    //   bytes 530-535 reserved
    //   bytes 536+    BDRD list
    if req.data_out.len() < plist_len || plist_len < 16 + ROD_TOKEN_LEN + 8 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let plist = &req.data_out[..plist_len];
    let mut token: RodToken = [0u8; ROD_TOKEN_LEN];
    token.copy_from_slice(&plist[16..16 + ROD_TOKEN_LEN]);
    let bdrd_off = 16 + ROD_TOKEN_LEN + 8;
    let bdrd_total =
        u16::from_be_bytes([plist[16 + ROD_TOKEN_LEN], plist[16 + ROD_TOKEN_LEN + 1]]) as usize;
    if bdrd_off + bdrd_total > plist.len() {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    let bdrds = match parse_bdrd_list(&plist[bdrd_off..bdrd_off + bdrd_total]) {
        Ok(v) => v,
        Err(s) => return ScsiResponse::check(s),
    };
    if bdrds.is_empty() {
        return ScsiResponse::good(Vec::new());
    }
    // Token lookup + TTL check.
    let snapshot = match tokens.snapshot_for(&token) {
        Some(s) => s,
        None => {
            // Token state lookup miss: distinguish "expired" (token
            // existed but past inactivity deadline) from "never
            // existed" (invalid token). SPC-4 §6.18 maps the former
            // to ASC 0x23/0x05 INVALID TOKEN OPERATION, TOKEN NOT
            // MAINTAINED and the latter to ASC 0x23/0x07 INVALID
            // TOKEN OPERATION, TOKEN INVALID — both under IllegalRequest.
            let sense = if tokens.is_expired(&token) {
                SenseData::new(scsi_spc::sense::SenseKey::IllegalRequest, 0x23, 0x05)
            } else {
                SenseData::new(scsi_spc::sense::SenseKey::IllegalRequest, 0x23, 0x07)
            };
            tokens.record_write_outcome(
                list_id,
                JobStatus::Failed {
                    completion_status: sense.key as u8,
                },
                0,
                tokens.resolve_ttl(0),
            );
            return ScsiResponse::check(sense);
        }
    };
    // Page-size compatibility between source-at-snapshot-time and
    // destination-now. Mismatched page sizes can't share hashes.
    let dst_manifest = dst_cache.manifest();
    if dst_manifest.page_size_bytes != snapshot.source_page_size {
        let sense = SenseData::INVALID_FIELD_IN_PARAMETER_LIST;
        tokens.record_write_outcome(
            list_id,
            JobStatus::Failed {
                completion_status: sense.key as u8,
            },
            0,
            tokens.resolve_ttl(0),
        );
        return ScsiResponse::check(sense);
    }
    let sector = dst_cache.sector_size();
    let page = dst_cache.page_size();
    let sectors_per_page = page / sector;
    let sz = Sizing::from(dst_cache.as_ref());
    // Validate every destination range + count pages so we can
    // index into the snapshot one source page per destination page.
    let mut total_blocks: u64 = 0;
    let mut total_dst_pages: u64 = 0;
    for bd in &bdrds {
        if bd.blocks == 0 {
            continue;
        }
        let range = LbaRange {
            lba: bd.lba,
            blocks: bd.blocks as u64,
        };
        if validate_in_range(&range, &sz).is_err() {
            let sense = SenseData::LBA_OUT_OF_RANGE;
            tokens.record_write_outcome(
                list_id,
                JobStatus::Failed {
                    completion_status: sense.key as u8,
                },
                0,
                tokens.resolve_ttl(0),
            );
            return ScsiResponse::check(sense);
        }
        if !bd.lba.is_multiple_of(sectors_per_page)
            || !(bd.blocks as u64).is_multiple_of(sectors_per_page)
        {
            let sense = SenseData::INVALID_FIELD_IN_PARAMETER_LIST;
            tokens.record_write_outcome(
                list_id,
                JobStatus::Failed {
                    completion_status: sense.key as u8,
                },
                0,
                tokens.resolve_ttl(0),
            );
            return ScsiResponse::check(sense);
        }
        total_blocks = total_blocks.saturating_add(bd.blocks as u64);
        total_dst_pages = total_dst_pages.saturating_add((bd.blocks as u64) / sectors_per_page);
    }
    if total_dst_pages != snapshot.source_pages.len() as u64 {
        // Destination must consume exactly as many pages as the
        // token holds; partial consumption is allowed by SPC-4 but
        // not modeled here.
        let sense = SenseData::INVALID_FIELD_IN_PARAMETER_LIST;
        tokens.record_write_outcome(
            list_id,
            JobStatus::Failed {
                completion_status: sense.key as u8,
            },
            0,
            tokens.resolve_ttl(0),
        );
        return ScsiResponse::check(sense);
    }
    // Apply: per destination page, take the matching snapshot hash
    // and rebind dst.pages.idx to it. Cross-pool (source + dest in
    // different pools) requires copying the chunk bytes between
    // pools first, which we don't model in v1; if the pools don't
    // match, refuse with INVALID FIELD IN PARAMETER LIST.
    let same_pool = snapshot.source_backend == dst_manifest.backend
        && snapshot.source_namespace == dst_manifest.pool_namespace();
    if !same_pool {
        let sense = SenseData::INVALID_FIELD_IN_PARAMETER_LIST;
        tokens.record_write_outcome(
            list_id,
            JobStatus::Failed {
                completion_status: sense.key as u8,
            },
            0,
            tokens.resolve_ttl(0),
        );
        return ScsiResponse::check(sense);
    }
    let dst_pi = dst_cache.writer().page_index();
    let dst_ui = dst_cache.writer().upload_index();
    let mut snap_idx = 0usize;
    for bd in &bdrds {
        if bd.blocks == 0 {
            continue;
        }
        let first_page = ((bd.lba * sector) / page) as u32;
        let pages_in_range = ((bd.blocks as u64) * sector / page) as u32;
        for p in 0..pages_in_range {
            let dst_page = first_page + p;
            let hash = &snapshot.hashes[snap_idx];
            snap_idx += 1;
            // Drop any cached destination entry so subsequent reads
            // repopulate from the new page-index binding.
            dst_cache.invalidate_cached_page(dst_page).await;
            let res: Result<(), UploaderError> = (|| {
                match hash {
                    Some(h) => {
                        dst_pi.set(dst_page, h)?;
                        dst_ui.set(dst_page, UploadState::Uploaded)?;
                    }
                    None => {
                        dst_pi.clear(dst_page)?;
                        dst_ui.set(dst_page, UploadState::Uploaded)?;
                    }
                };
                Ok(())
            })();
            if let Err(e) = res {
                let sense = map_write_error(&e);
                tokens.record_write_outcome(
                    list_id,
                    JobStatus::Failed {
                        completion_status: sense.key as u8,
                    },
                    0,
                    tokens.resolve_ttl(0),
                );
                return ScsiResponse::check(sense);
            }
        }
    }
    tokens.record_write_outcome(
        list_id,
        JobStatus::Done,
        total_blocks,
        tokens.resolve_ttl(0),
    );
    ScsiResponse::good(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use std::sync::Arc;
    use tempfile::TempDir;

    const SECTOR: usize = DEFAULT_SECTOR_BYTES as usize;
    const PAGE: usize = DEFAULT_PAGE_SIZE_BYTES as usize;
    const SECTORS_PER_PAGE: usize = PAGE / SECTOR; // 16

    async fn fixture_cache(
        data_dir: &std::path::Path,
        size_bytes: u64,
        worm: bool,
    ) -> Arc<PageCache> {
        let cloud_root = data_dir.join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        VolumeManifest::new(
            "vol1".into(),
            size_bytes,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            worm,
            0,
        )
        .unwrap()
        .create(data_dir)
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(data_dir, "vol1", backend).unwrap());
        PageCache::new(writer)
    }

    fn write10_cdb(lba: u32, blocks: u16) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x2A;
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn read10_cdb(lba: u32, blocks: u16) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x28;
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn write16_cdb(lba: u64, blocks: u32) -> Vec<u8> {
        let mut cdb = vec![0u8; 16];
        cdb[0] = 0x8A;
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn read16_cdb(lba: u64, blocks: u32) -> Vec<u8> {
        let mut cdb = vec![0u8; 16];
        cdb[0] = 0x88;
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn sync10_cdb(lba: u32, blocks: u16) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x35;
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn sync16_cdb(lba: u64, blocks: u32) -> Vec<u8> {
        let mut cdb = vec![0u8; 16];
        cdb[0] = 0x91;
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn req<'a>(cdb: &'a [u8], data_out: &'a [u8], data_in_max: usize) -> ScsiRequest<'a> {
        req_lun(0, cdb, data_out, data_in_max)
    }

    fn req_lun<'a>(
        lun: u64,
        cdb: &'a [u8],
        data_out: &'a [u8],
        data_in_max: usize,
    ) -> ScsiRequest<'a> {
        ScsiRequest {
            lun,
            cdb,
            data_out,
            data_in_max,
            tsih: 0,
            initiator_iqn: None,
            initiator_isid: [0u8; 6],
            cid: 0,
            peer: "",
            session_partition: None,
            session_volumes: None,
        }
    }

    /// An I_T nexus that doesn't hold any registration — every test
    /// in this module exercises the data path against an empty
    /// `ReservationManager`, so the nexus identity doesn't matter.
    fn test_nexus() -> Nexus {
        Nexus::iscsi(None, [0u8; 6])
    }

    fn test_mgr() -> ReservationManager {
        ReservationManager::new()
    }

    fn test_tokens() -> Arc<TokenManager> {
        Arc::new(TokenManager::new())
    }

    fn page_pattern(seed: u8) -> Vec<u8> {
        (0..PAGE)
            .map(|i| seed.wrapping_add((i & 0xFF) as u8))
            .collect()
    }

    #[tokio::test]
    async fn write10_then_read10_round_trip_one_page() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let payload = page_pattern(0x42);

        let cdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = write(
            &req(&cdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        let cdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&cdb, &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        assert_eq!(r.data_in, payload);
    }

    #[tokio::test]
    async fn write16_then_read16_round_trip_two_pages() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let mut payload = page_pattern(0x10);
        payload.extend(page_pattern(0x20));
        assert_eq!(payload.len(), 2 * PAGE);

        let cdb = write16_cdb(0, (2 * SECTORS_PER_PAGE) as u32);
        let r = write(
            &req(&cdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        let cdb = read16_cdb(0, (2 * SECTORS_PER_PAGE) as u32);
        let r = read(
            &req(&cdb, &[], 2 * PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert_eq!(r.data_in, payload);
    }

    #[tokio::test]
    async fn read_unallocated_page_returns_zeros() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&cdb, &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), PAGE);
        assert!(r.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn write_with_zero_blocks_is_noop() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = write10_cdb(0, 0);
        let r = write(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn write_one_sector_round_trip_is_subpage() {
        // The previous dispatcher rejected this with INVALID FIELD
        // IN CDB; with the cache layer it round-trips cleanly. This
        // is the test that proves the alignment constraint is gone.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let one_sector = vec![0xAA; SECTOR];
        let cdb = write10_cdb(1, 1);
        let r = write(
            &req(&cdb, &one_sector, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        let rcdb = read10_cdb(1, 1);
        let r = read(
            &req(&rcdb, &[], SECTOR),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        assert_eq!(r.data_in, one_sector);

        // Sector 0 of the same page is still zero — RMW didn't
        // smear the host write across the whole page.
        let rcdb = read10_cdb(0, 1);
        let r = read(
            &req(&rcdb, &[], SECTOR),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert!(r.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn write_rejects_lba_past_end_of_volume() {
        let tmp = TempDir::new().unwrap();
        // 4 MiB volume / 4 KiB sector = 1024 blocks total.
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let payload = page_pattern(0);
        let cdb = write10_cdb(1024, SECTORS_PER_PAGE as u16);
        let r = write(
            &req(&cdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn read_rejects_lba_past_end_of_volume() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = read10_cdb(1024, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&cdb, &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn write_rejects_data_out_length_mismatch() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let too_short = vec![0u8; PAGE - 1];
        let r = write(
            &req(&cdb, &too_short, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn write_refused_on_worm_volume() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), true).await;
        let payload = page_pattern(0);
        let cdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = write(
            &req(&cdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::WRITE_PROTECTED));
    }

    #[tokio::test]
    async fn write_against_unmapped_lun_check_condition() {
        let cdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let payload = page_pattern(0);
        let r = write(&req(&cdb, &payload, 0), None, test_nexus(), &test_mgr()).await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn read_against_unmapped_lun_check_condition() {
        let cdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(&req(&cdb, &[], PAGE), None, test_nexus(), &test_mgr()).await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn read_truncates_to_data_in_max() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let payload = page_pattern(0xFE);
        let cdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = write(
            &req(&cdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        // Initiator only allocates 256 bytes — handler must clamp.
        let cdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&cdb, &[], 256),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 256);
        assert_eq!(&r.data_in[..], &payload[..256]);
    }

    #[tokio::test]
    async fn synchronize_cache_10_no_op_on_clean_volume() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = sync10_cdb(0, 0);
        let r = synchronize_cache(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert!(r.data_in.is_empty());
    }

    #[tokio::test]
    async fn synchronize_cache_16_no_op_on_clean_volume() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = sync16_cdb(0, SECTORS_PER_PAGE as u32);
        let r = synchronize_cache(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn synchronize_cache_flushes_dirty_pages() {
        // Sub-sector WRITE leaves the page dirty in the cache; a
        // subsequent SYNC must flush it through to the underlying
        // writer (page-index entry becomes set).
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let bytes = vec![0x55u8; SECTOR];
        let cdb = write10_cdb(0, 1);
        let r = write(
            &req(&cdb, &bytes, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        // Page index is still empty before SYNC.
        assert!(cache.writer().page_index().get(0).unwrap().is_none());

        let cdb = sync10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = synchronize_cache(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        // After SYNC the page is durable.
        assert!(cache.writer().page_index().get(0).unwrap().is_some());
    }

    #[tokio::test]
    async fn synchronize_cache_rejects_out_of_range() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // 1024 blocks total; ask for [2000, +10) → out of range.
        let cdb = sync10_cdb(2000, 10);
        let r = synchronize_cache(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn synchronize_cache_zero_blocks_means_to_end_of_medium() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Aligned LBA inside the volume; blocks=0 means "from here
        // to end" — handler should accept.
        let cdb = sync10_cdb(0, 0);
        let r = synchronize_cache(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn synchronize_cache_against_unmapped_lun_check_condition() {
        let cdb = sync10_cdb(0, 0);
        let r = synchronize_cache(&req(&cdb, &[], 0), None, test_nexus(), &test_mgr()).await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn read_after_partial_overwrite_returns_latest_bytes() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let a = page_pattern(0xAA);
        let b = page_pattern(0xBB);

        let w10 = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = write(
            &req(&w10, &a, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        let r = write(
            &req(&w10, &b, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        let r = read(
            &req(&read10_cdb(0, SECTORS_PER_PAGE as u16), &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert_eq!(r.data_in, b);
    }

    // ----------------------------------------------------------------
    // COMPARE AND WRITE (0x89)
    // ----------------------------------------------------------------

    fn caw_cdb(lba: u64, blocks: u8) -> Vec<u8> {
        let mut cdb = vec![0u8; 16];
        cdb[0] = 0x89;
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[13] = blocks;
        cdb
    }

    fn test_caw_locks() -> CawLocks {
        CawLocks::new()
    }

    #[tokio::test]
    async fn caw_succeeds_when_compare_buffer_matches_existing_page() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;

        // Seed page 0 with pattern A.
        let a = page_pattern(0xA0);
        let r = write(
            &req(&write10_cdb(0, SECTORS_PER_PAGE as u16), &a, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        // CAW(compare=A, write=B) over page 0 — should commit B.
        let b = page_pattern(0xB0);
        let mut combined = a.clone();
        combined.extend_from_slice(&b);
        let r = compare_and_write(
            &req(&caw_cdb(0, SECTORS_PER_PAGE as u8), &combined, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        // Read back: page 0 should now hold B.
        let rd = read(
            &req(&read10_cdb(0, SECTORS_PER_PAGE as u16), &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(rd.data_in, b);
    }

    #[tokio::test]
    async fn caw_returns_miscompare_when_page_does_not_match() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;

        let stored = page_pattern(0xAA);
        let r = write(
            &req(&write10_cdb(0, SECTORS_PER_PAGE as u16), &stored, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        let stale = page_pattern(0xCC); // not what's on disk
        let new = page_pattern(0xDD);
        let mut combined = stale;
        combined.extend_from_slice(&new);
        let r = compare_and_write(
            &req(&caw_cdb(0, SECTORS_PER_PAGE as u8), &combined, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::MISCOMPARE));

        // Page 0 must be unchanged after the failed CAW.
        let rd = read(
            &req(&read10_cdb(0, SECTORS_PER_PAGE as u16), &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(rd.data_in, page_pattern(0xAA));
    }

    #[tokio::test]
    async fn caw_against_unallocated_page_succeeds_with_zero_compare() {
        // SBC-3 §5.7: unallocated pages read as zeros. A host-issued
        // CAW with an all-zero compare buffer must succeed and
        // initialize the page.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;

        let zeros = vec![0u8; PAGE];
        let new = page_pattern(0x55);
        let mut combined = zeros;
        combined.extend_from_slice(&new);
        let r = compare_and_write(
            &req(&caw_cdb(0, SECTORS_PER_PAGE as u8), &combined, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        let rd = read(
            &req(&read10_cdb(0, SECTORS_PER_PAGE as u16), &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(rd.data_in, new);
    }

    #[tokio::test]
    async fn sub_page_caw_one_sector_match_commits() {
        // VMFS heartbeat shape: 1-sector CAW. Was rejected by the
        // old aligned-only dispatcher; now succeeds via the cache.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let zeros = vec![0u8; SECTOR];
        let new = vec![0xC3u8; SECTOR];
        let mut combined = zeros;
        combined.extend_from_slice(&new);
        let r = compare_and_write(
            &req(&caw_cdb(0, 1), &combined, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        // Read back the affected sector — must hold the new bytes.
        let r = read(
            &req(&read10_cdb(0, 1), &[], SECTOR),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.data_in, new);
    }

    #[tokio::test]
    async fn sub_page_caw_one_sector_miscompare_leaves_state_unchanged() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Seed sector 0 with 0x77.
        let seed = vec![0x77u8; SECTOR];
        let r = write(
            &req(&write10_cdb(0, 1), &seed, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        // CAW with stale compare bytes.
        let stale = vec![0xAAu8; SECTOR];
        let new = vec![0xDDu8; SECTOR];
        let mut combined = stale;
        combined.extend_from_slice(&new);
        let r = compare_and_write(
            &req(&caw_cdb(0, 1), &combined, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::MISCOMPARE));
        // Seed bytes still in place.
        let r = read(
            &req(&read10_cdb(0, 1), &[], SECTOR),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.data_in, seed);
    }

    #[tokio::test]
    async fn caw_rejects_data_out_length_mismatch() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Should be 2 * page bytes; supply only one.
        let half = vec![0u8; PAGE];
        let r = compare_and_write(
            &req(&caw_cdb(0, SECTORS_PER_PAGE as u8), &half, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn caw_zero_blocks_is_noop() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let r = compare_and_write(
            &req(&caw_cdb(0, 0), &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn caw_refused_on_worm_volume() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), true).await;
        let buf = vec![0u8; 2 * PAGE];
        let r = compare_and_write(
            &req(&caw_cdb(0, SECTORS_PER_PAGE as u8), &buf, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::WRITE_PROTECTED));
    }

    #[tokio::test]
    async fn caw_against_unmapped_lun_check_condition() {
        let buf = vec![0u8; 2 * PAGE];
        let r = compare_and_write(
            &req(&caw_cdb(0, SECTORS_PER_PAGE as u8), &buf, 0),
            None,
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn caw_rejects_lba_past_end_of_volume() {
        let tmp = TempDir::new().unwrap();
        // 4 MiB / 4 KiB = 1024 blocks total.
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let buf = vec![0u8; 2 * PAGE];
        let r = compare_and_write(
            &req(&caw_cdb(1024, SECTORS_PER_PAGE as u8), &buf, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn caw_multi_page_diff_in_second_page_returns_miscompare_first() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;

        // Page 0 = A, page 1 = unallocated (reads as zeros).
        let a = page_pattern(0xA0);
        let r = write(
            &req(&write10_cdb(0, SECTORS_PER_PAGE as u16), &a, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        // Compare buffer says (A, page_pattern(0x99)) — page 1
        // doesn't match because it's actually zero.
        let mut compare = a.clone();
        compare.extend_from_slice(&page_pattern(0x99));
        let mut combined = compare;
        combined.extend_from_slice(&page_pattern(0x11));
        combined.extend_from_slice(&page_pattern(0x22));
        let r = compare_and_write(
            &req(&caw_cdb(0, (2 * SECTORS_PER_PAGE) as u8), &combined, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
            &test_caw_locks(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::MISCOMPARE));

        // Page 0 must still hold the original A — no partial commit.
        let rd = read(
            &req(&read10_cdb(0, SECTORS_PER_PAGE as u16), &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(rd.data_in, a);
    }

    // ----------------------------------------------------------------
    // UNMAP (0x42)
    // ----------------------------------------------------------------

    fn unmap_cdb(parameter_list_length: u16) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x42;
        cdb[7..9].copy_from_slice(&parameter_list_length.to_be_bytes());
        cdb
    }

    /// Build an UNMAP parameter list (header + n descriptors).
    fn unmap_params(descriptors: &[(u64, u32)]) -> Vec<u8> {
        let n = descriptors.len();
        let descriptor_total = (n * 16) as u16;
        let total = 8 + n * 16;
        let mut buf = vec![0u8; total];
        let unmap_data_length = (total - 2) as u16;
        buf[0..2].copy_from_slice(&unmap_data_length.to_be_bytes());
        buf[2..4].copy_from_slice(&descriptor_total.to_be_bytes());
        for (i, (lba, blocks)) in descriptors.iter().enumerate() {
            let off = 8 + i * 16;
            buf[off..off + 8].copy_from_slice(&lba.to_be_bytes());
            buf[off + 8..off + 12].copy_from_slice(&blocks.to_be_bytes());
        }
        buf
    }

    #[tokio::test]
    async fn unmap_clears_allocated_page_so_subsequent_read_returns_zeros() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;

        // Fill page 0 with non-zero pattern; SYNC to commit so the
        // page-index entry is set, then UNMAP it.
        let payload = page_pattern(0x77);
        let r = write(
            &req(&write10_cdb(0, SECTORS_PER_PAGE as u16), &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        let cdb = sync10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = synchronize_cache(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        // Unmap page 0 (16 sectors at LBA 0).
        let params = unmap_params(&[(0, SECTORS_PER_PAGE as u32)]);
        let cdb = unmap_cdb(params.len() as u16);
        let r = unmap(
            &req(&cdb, &params, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        // Underlying page index is cleared.
        assert!(cache.writer().page_index().get(0).unwrap().is_none());

        // Subsequent READ returns the SBC-3 sparse-hole zeros.
        let rd = read(
            &req(&read10_cdb(0, SECTORS_PER_PAGE as u16), &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(rd.sense.is_none());
        assert!(rd.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn unmap_with_zero_parameter_list_is_noop() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = unmap_cdb(0);
        let r = unmap(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn unmap_with_header_only_descriptor_list_is_noop() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = unmap_params(&[]);
        let cdb = unmap_cdb(params.len() as u16);
        let r = unmap(
            &req(&cdb, &params, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn sub_page_unmap_zeros_only_targeted_sectors() {
        // 1-sector UNMAP at LBA 0 — was rejected pre-cache; now
        // zeros sector 0 of page 0 and leaves the remaining 15
        // sectors intact.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Seed the whole page with a non-zero pattern.
        let seed = page_pattern(0x77);
        let r = write(
            &req(&write10_cdb(0, SECTORS_PER_PAGE as u16), &seed, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        let params = unmap_params(&[(0, 1)]);
        let cdb = unmap_cdb(params.len() as u16);
        let r = unmap(
            &req(&cdb, &params, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        // Sector 0 reads as zeros.
        let r = read(
            &req(&read10_cdb(0, 1), &[], SECTOR),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert!(r.data_in.iter().all(|&b| b == 0));

        // Sector 1 still holds the seed pattern.
        let r = read(
            &req(&read10_cdb(1, 1), &[], SECTOR),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        assert_eq!(r.data_in, seed[SECTOR..2 * SECTOR]);
    }

    #[tokio::test]
    async fn unmap_rejects_lba_past_end_of_volume() {
        let tmp = TempDir::new().unwrap();
        // 4 MiB / 4 KiB = 1024 blocks.
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = unmap_params(&[(1024, SECTORS_PER_PAGE as u32)]);
        let cdb = unmap_cdb(params.len() as u16);
        let r = unmap(
            &req(&cdb, &params, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn unmap_validation_failure_in_second_descriptor_leaves_first_untouched() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;

        let payload = page_pattern(0x33);
        let r = write(
            &req(&write10_cdb(0, SECTORS_PER_PAGE as u16), &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        // First descriptor is valid (page 0); second is past EOV.
        let params = unmap_params(&[(0, SECTORS_PER_PAGE as u32), (1024, 1)]);
        let cdb = unmap_cdb(params.len() as u16);
        let r = unmap(
            &req(&cdb, &params, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));

        // Page 0 must still hold the original payload — UNMAP didn't
        // commit any descriptor when the second one was malformed.
        let rd = read(
            &req(&read10_cdb(0, SECTORS_PER_PAGE as u16), &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(rd.data_in, payload);
    }

    #[tokio::test]
    async fn unmap_anchor_bit_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = unmap_params(&[]);
        let mut cdb = unmap_cdb(params.len() as u16);
        cdb[1] |= 0x01; // ANCHOR=1
        let r = unmap(
            &req(&cdb, &params, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn unmap_refused_on_worm_volume() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), true).await;
        let params = unmap_params(&[(0, SECTORS_PER_PAGE as u32)]);
        let cdb = unmap_cdb(params.len() as u16);
        let r = unmap(
            &req(&cdb, &params, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::WRITE_PROTECTED));
    }

    #[tokio::test]
    async fn unmap_against_unmapped_lun_check_condition() {
        let params = unmap_params(&[(0, SECTORS_PER_PAGE as u32)]);
        let cdb = unmap_cdb(params.len() as u16);
        let r = unmap(&req(&cdb, &params, 0), None, test_nexus(), &test_mgr()).await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn unmap_data_out_shorter_than_parameter_list_length_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Claim a 24-byte parameter list but only ship 8 bytes.
        let cdb = unmap_cdb(24);
        let short = vec![0u8; 8];
        let r = unmap(
            &req(&cdb, &short, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    // ---------- VERIFY ----------

    fn verify10_cdb(lba: u32, blocks: u16, bytchk: u8) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x2F;
        cdb[1] = (bytchk & 0x03) << 1;
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn verify16_cdb(lba: u64, blocks: u32, bytchk: u8) -> Vec<u8> {
        let mut cdb = vec![0u8; 16];
        cdb[0] = 0x8F;
        cdb[1] = (bytchk & 0x03) << 1;
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    #[tokio::test]
    async fn verify10_bytchk_zero_passes_on_unallocated_pages() {
        // BYTCHK=00 on a fresh volume reads sparse-hole pages — no
        // medium error; should return GOOD.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = verify10_cdb(0, SECTORS_PER_PAGE as u16, 0);
        let r = verify(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn verify10_bytchk_one_compare_match() {
        // Write a known pattern, then VERIFY BYTCHK=1 with the same
        // bytes — should match.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let payload = page_pattern(0x42);
        let wcdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = write(
            &req(&wcdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());

        let vcdb = verify10_cdb(0, SECTORS_PER_PAGE as u16, 1);
        let r = verify(
            &req(&vcdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn verify10_bytchk_one_compare_miscompare() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let payload = page_pattern(0x42);
        let wcdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let _ = write(
            &req(&wcdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        let mut wrong = payload.clone();
        wrong[0] ^= 0xFF;
        let vcdb = verify10_cdb(0, SECTORS_PER_PAGE as u16, 1);
        let r = verify(
            &req(&vcdb, &wrong, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::MISCOMPARE));
    }

    #[tokio::test]
    async fn verify16_bytchk_zero_out_of_range_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // 4 MiB / 4 KiB = 1024 blocks, last LBA = 1023.
        let cdb = verify16_cdb(2000, 1, 0);
        let r = verify(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn verify_bytchk_three_rejected_no_lbp() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = verify10_cdb(0, 1, 3);
        let r = verify(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn verify_unmapped_lun_check_condition() {
        let cdb = verify10_cdb(0, 1, 0);
        let r = verify(&req(&cdb, &[], 0), None, test_nexus(), &test_mgr()).await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    // ---------- WRITE SAME ----------

    fn write_same_10_cdb(lba: u32, blocks: u16, unmap_bit: bool) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x41;
        if unmap_bit {
            cdb[1] |= 0x08;
        }
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    fn write_same_16_cdb(lba: u64, blocks: u32, unmap_bit: bool, ndob: bool) -> Vec<u8> {
        let mut cdb = vec![0u8; 16];
        cdb[0] = 0x93;
        if unmap_bit {
            cdb[1] |= 0x08;
        }
        if ndob {
            cdb[1] |= 0x01;
        }
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
        cdb
    }

    #[tokio::test]
    async fn write_same_10_zero_pattern_unmap_zero_writes_zeros() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Pre-populate page 0 with a non-zero pattern.
        let payload = page_pattern(0x42);
        let wcdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let _ = write(
            &req(&wcdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        // WRITE SAME 10, UNMAP=0, zero pattern, 16 sectors.
        let cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, false);
        let zeros = vec![0u8; SECTOR];
        let r = write_same(
            &req(&cdb, &zeros, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        // Read back — must be zero.
        let rcdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&rcdb, &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn write_same_10_unmap_with_zero_pattern_route_via_unmap() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let payload = page_pattern(0x55);
        let wcdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let _ = write(
            &req(&wcdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        // UNMAP=1, zero pattern.
        let cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, true);
        let zeros = vec![0u8; SECTOR];
        let r = write_same(
            &req(&cdb, &zeros, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
        // Read back — sparse hole = zeros.
        let rcdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&rcdb, &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn write_same_10_non_zero_pattern_repeats_across_blocks() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Pattern = single sector of 0xAB.
        let pattern = vec![0xAB; SECTOR];
        let cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, false);
        let r = write_same(
            &req(&cdb, &pattern, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        let rcdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&rcdb, &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.data_in.iter().all(|&b| b == 0xAB));
    }

    #[tokio::test]
    async fn write_same_16_ndob_zero_fills_without_data_out() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Pre-populate.
        let payload = page_pattern(0x77);
        let wcdb = write10_cdb(0, SECTORS_PER_PAGE as u16);
        let _ = write(
            &req(&wcdb, &payload, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        // NDOB=1, no data-out, UNMAP=0.
        let cdb = write_same_16_cdb(0, SECTORS_PER_PAGE as u32, false, true);
        let r = write_same(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        let rcdb = read10_cdb(0, SECTORS_PER_PAGE as u16);
        let r = read(
            &req(&rcdb, &[], PAGE),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.data_in.iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn write_same_10_zero_blocks_is_no_op() {
        // SBC-3 §5.49: 10-byte form, NUMBER OF BLOCKS = 0 → no
        // transfer. Must succeed without touching the medium.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = write_same_10_cdb(0, 0, false);
        let zeros = vec![0u8; SECTOR];
        let r = write_same(
            &req(&cdb, &zeros, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn write_same_16_zero_blocks_writes_to_end_of_medium() {
        // 16-byte form, NUMBER OF BLOCKS = 0, NDOB=1 → zero-fill
        // from LBA to end of medium. Smoke-test on a tiny volume.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 64 * 1024, false).await;
        let cdb = write_same_16_cdb(0, 0, false, true);
        let r = write_same(
            &req(&cdb, &[], 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn write_same_anchor_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let mut cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, false);
        cdb[1] |= 0x10; // ANCHOR=1
        let zeros = vec![0u8; SECTOR];
        let r = write_same(
            &req(&cdb, &zeros, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn write_same_wrprotect_nonzero_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let mut cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, false);
        cdb[1] |= 0x20; // WRPROTECT = 0b001
        let zeros = vec![0u8; SECTOR];
        let r = write_same(
            &req(&cdb, &zeros, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn write_same_data_out_wrong_length_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, false);
        let too_short = vec![0u8; SECTOR / 2];
        let r = write_same(
            &req(&cdb, &too_short, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn write_same_worm_volume_refused() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), true).await;
        let cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, false);
        let zeros = vec![0u8; SECTOR];
        let r = write_same(
            &req(&cdb, &zeros, 0),
            Some(cache.as_ref()),
            test_nexus(),
            &test_mgr(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::WRITE_PROTECTED));
    }

    #[tokio::test]
    async fn write_same_unmapped_lun_check_condition() {
        let cdb = write_same_10_cdb(0, SECTORS_PER_PAGE as u16, false);
        let zeros = vec![0u8; SECTOR];
        let r = write_same(&req(&cdb, &zeros, 0), None, test_nexus(), &test_mgr()).await;
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    // -- Property tests for parse_10 / parse_16 ----------------------------
    //
    // The parsers sit on the wire path between an iSCSI initiator and the
    // page cache. Below the proptest budget we cover: round-trip on the
    // valid-length CDB, no-panic + correct rejection on undersize buffers,
    // and no-panic on arbitrary exact-size buffers (any bit pattern in
    // bytes that aren't the LBA / blocks fields is allowed by the
    // parser — the parser only looks at byte ranges 2..6/7..9 for parse_10
    // and 2..10/10..14 for parse_16).

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_10_roundtrip(lba: u32, blocks: u16) {
            let cdb = write10_cdb(lba, blocks);
            let r = parse_10(&cdb).expect("valid CDB must parse");
            prop_assert_eq!(r.lba, u64::from(lba));
            prop_assert_eq!(r.blocks, u64::from(blocks));
        }

        #[test]
        fn parse_10_read_opcode_roundtrip(lba: u32, blocks: u16) {
            // parse_10 is shared across opcodes 0x28 (READ 10), 0x2A (WRITE 10),
            // and 0x35 (SYNC CACHE 10). The opcode byte sits at cdb[0] and the
            // parser ignores it.
            let cdb = read10_cdb(lba, blocks);
            let r = parse_10(&cdb).expect("READ 10 CDB must parse");
            prop_assert_eq!(r.lba, u64::from(lba));
            prop_assert_eq!(r.blocks, u64::from(blocks));
        }

        #[test]
        fn parse_16_roundtrip(lba: u64, blocks: u32) {
            let cdb = write16_cdb(lba, blocks);
            let r = parse_16(&cdb).expect("valid CDB must parse");
            prop_assert_eq!(r.lba, lba);
            prop_assert_eq!(r.blocks, u64::from(blocks));
        }

        #[test]
        fn parse_16_read_opcode_roundtrip(lba: u64, blocks: u32) {
            let cdb = read16_cdb(lba, blocks);
            let r = parse_16(&cdb).expect("READ 16 CDB must parse");
            prop_assert_eq!(r.lba, lba);
            prop_assert_eq!(r.blocks, u64::from(blocks));
        }

        #[test]
        fn parse_10_undersize_rejects_no_panic(cdb in proptest::collection::vec(any::<u8>(), 0..10)) {
            // Any buffer below 10 bytes must return INVALID_FIELD_IN_CDB
            // without panicking on the byte slice access.
            match parse_10(&cdb) {
                Ok(_) => prop_assert!(false, "undersize CDB ({} bytes) must reject", cdb.len()),
                Err(e) => prop_assert_eq!(e, SenseData::INVALID_FIELD_IN_CDB),
            }
        }

        #[test]
        fn parse_16_undersize_rejects_no_panic(cdb in proptest::collection::vec(any::<u8>(), 0..16)) {
            match parse_16(&cdb) {
                Ok(_) => prop_assert!(false, "undersize CDB ({} bytes) must reject", cdb.len()),
                Err(e) => prop_assert_eq!(e, SenseData::INVALID_FIELD_IN_CDB),
            }
        }

        #[test]
        fn parse_10_arbitrary_exact_size_no_panic(cdb in proptest::array::uniform10(any::<u8>())) {
            // Random 10-byte buffer always parses without panic. The
            // computed lba / blocks may be anything; we only assert the
            // parser is total over its declared input size.
            let parsed = match parse_10(&cdb) {
                Ok(r) => r,
                Err(_) => {
                    prop_assert!(false, "exact-size CDB must not error");
                    unreachable!()
                }
            };
            prop_assert!(parsed.lba <= u64::from(u32::MAX));
            prop_assert!(parsed.blocks <= u64::from(u16::MAX));
        }

        #[test]
        fn parse_16_arbitrary_exact_size_no_panic(cdb in proptest::array::uniform16(any::<u8>())) {
            let parsed = match parse_16(&cdb) {
                Ok(r) => r,
                Err(_) => {
                    prop_assert!(false, "exact-size CDB must not error");
                    unreachable!()
                }
            };
            prop_assert!(parsed.blocks <= u64::from(u32::MAX));
            // lba is u64; bounded only by its type.
            let _ = parsed.lba;
        }

        #[test]
        fn parse_10_oversized_buffer_still_parses(extra in 0usize..32, lba: u32, blocks: u16) {
            // Extra trailing bytes beyond the CDB's 10 are ignored —
            // exercises the `.len() < 10` short-circuit's symmetric path.
            let mut cdb = write10_cdb(lba, blocks);
            cdb.extend(std::iter::repeat_n(0xAA_u8, extra));
            let r = parse_10(&cdb).expect("oversize CDB still parses");
            prop_assert_eq!(r.lba, u64::from(lba));
            prop_assert_eq!(r.blocks, u64::from(blocks));
        }

        #[test]
        fn parse_16_oversized_buffer_still_parses(extra in 0usize..32, lba: u64, blocks: u32) {
            let mut cdb = write16_cdb(lba, blocks);
            cdb.extend(std::iter::repeat_n(0xAA_u8, extra));
            let r = parse_16(&cdb).expect("oversize CDB still parses");
            prop_assert_eq!(r.lba, lba);
            prop_assert_eq!(r.blocks, u64::from(blocks));
        }
    }

    // ----------------------------------------------------------------
    // EXTENDED COPY (0x83) / RECEIVE COPY RESULTS (0x84)
    // ----------------------------------------------------------------

    use std::collections::BTreeMap;
    use std::sync::RwLock;

    /// Test-only `VolumeLookup` impl. Mirrors `dispatcher.rs::tests`
    /// so XCOPY can resolve cross-LUN target descriptors.
    #[derive(Default)]
    struct TestRegistry {
        by_lun: RwLock<BTreeMap<u64, Arc<PageCache>>>,
    }

    impl TestRegistry {
        fn new() -> Self {
            Self::default()
        }
        fn register(&self, lun: u64, cache: Arc<PageCache>) {
            self.by_lun.write().unwrap().insert(lun, cache);
        }
    }

    impl VolumeLookup for TestRegistry {
        fn get(&self, lun: u64) -> Option<Arc<PageCache>> {
            self.by_lun.read().unwrap().get(&lun).map(Arc::clone)
        }
        fn luns(&self) -> Vec<u64> {
            self.by_lun.read().unwrap().keys().copied().collect()
        }
        fn name_for_lun(&self, lun: u64) -> Option<String> {
            self.by_lun
                .read()
                .unwrap()
                .get(&lun)
                .map(|c| c.manifest().name.clone())
        }
        fn luns_filtered(&self, allow: Option<&[String]>) -> Vec<u64> {
            let m = self.by_lun.read().unwrap();
            match allow {
                None => m.keys().copied().collect(),
                Some(names) => m
                    .iter()
                    .filter(|(_, c)| names.iter().any(|n| n == &c.manifest().name))
                    .map(|(lun, _)| *lun)
                    .collect(),
            }
        }
    }

    /// Two volumes co-located on one tempdir + LocalBackend with
    /// `DedupScope::Global` so they share the per-backend chunk pool —
    /// the configuration ODX needs to reach the hash-rebind fast path
    /// in `WRITE USING TOKEN`.
    async fn fresh_two_volumes_global(
        a: &str,
        b: &str,
    ) -> (TempDir, Arc<PageCache>, Arc<PageCache>) {
        let tmp = TempDir::new().unwrap();
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        for name in [a, b] {
            VolumeManifest::new(
                name.into(),
                8 * (1u64 << 20),
                DEFAULT_SECTOR_BYTES,
                DEFAULT_PAGE_SIZE_BYTES,
                "primary".into(),
                DedupScope::Global,
                false,
                0,
            )
            .unwrap()
            .create(tmp.path())
            .unwrap();
        }
        let wa = Arc::new(VolumeWriter::open(tmp.path(), a, backend.clone()).unwrap());
        let wb = Arc::new(VolumeWriter::open(tmp.path(), b, backend).unwrap());
        (tmp, PageCache::new(wa), PageCache::new(wb))
    }

    /// One independent volume backed by its own tempdir + LocalBackend.
    async fn fresh_volume(name: &str) -> (TempDir, Arc<PageCache>) {
        let tmp = TempDir::new().unwrap();
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        VolumeManifest::new(
            name.into(),
            8 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(tmp.path())
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(tmp.path(), name, backend).unwrap());
        let cache = PageCache::new(writer);
        (tmp, cache)
    }

    /// Build a 32-byte identification target descriptor (type 0xE4)
    /// carrying the NAA designator the VSA publishes in VPD 0x83.
    fn target_descriptor_for(cache: &PageCache) -> [u8; 32] {
        let designator = naa_locally_assigned(&cache.manifest().uuid);
        let mut desc = [0u8; 32];
        desc[0] = 0xE4;
        // byte 4: CODE SET = 0x01 (binary)
        desc[4] = 0x01;
        // byte 5: DESIGNATOR TYPE = 0x03 (NAA)
        desc[5] = 0x03;
        desc[7] = designator.len() as u8;
        desc[8..8 + designator.len()].copy_from_slice(&designator);
        desc
    }

    /// Build a block-to-block segment descriptor (type 0x02).
    fn block_segment(
        src_idx: u16,
        dst_idx: u16,
        blocks: u16,
        src_lba: u64,
        dst_lba: u64,
    ) -> [u8; 28] {
        let mut sd = [0u8; 28];
        sd[0] = 0x02;
        sd[2..4].copy_from_slice(&0x18u16.to_be_bytes());
        sd[4..6].copy_from_slice(&src_idx.to_be_bytes());
        sd[6..8].copy_from_slice(&dst_idx.to_be_bytes());
        sd[10..12].copy_from_slice(&blocks.to_be_bytes());
        sd[12..20].copy_from_slice(&src_lba.to_be_bytes());
        sd[20..28].copy_from_slice(&dst_lba.to_be_bytes());
        sd
    }

    /// Build an EXTENDED COPY parameter list from target descriptors
    /// and one block-to-block segment descriptor.
    fn build_xcopy_param_list(targets: &[[u8; 32]], segments: &[[u8; 28]]) -> Vec<u8> {
        let tdesc_len = targets.len() * 32;
        let sdesc_len = segments.len() * 28;
        let mut p = vec![0u8; 16 + tdesc_len + sdesc_len];
        p[2..4].copy_from_slice(&(tdesc_len as u16).to_be_bytes());
        p[8..12].copy_from_slice(&(sdesc_len as u32).to_be_bytes());
        let mut off = 16;
        for t in targets {
            p[off..off + 32].copy_from_slice(t);
            off += 32;
        }
        for s in segments {
            p[off..off + 28].copy_from_slice(s);
            off += 28;
        }
        p
    }

    fn xcopy_cdb(param_list_len: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x83;
        cdb[10..14].copy_from_slice(&param_list_len.to_be_bytes());
        cdb
    }

    /// Build a LID4 (sa 0x01) EXTENDED COPY parameter list: 48-byte
    /// header (LIST FORMAT = 0x01, no header CSCDs, no inline data)
    /// then the same 0xE4 target + 0x02 segment descriptor bodies as
    /// LID1.
    fn build_xcopy_lid4_param_list(targets: &[[u8; 32]], segments: &[[u8; 28]]) -> Vec<u8> {
        let tdesc_len = targets.len() * 32;
        let sdesc_len = segments.len() * 28;
        let mut p = vec![0u8; 48 + tdesc_len + sdesc_len];
        p[0] = 0x01; // LIST FORMAT
        p[42..44].copy_from_slice(&(tdesc_len as u16).to_be_bytes());
        p[44..46].copy_from_slice(&(sdesc_len as u16).to_be_bytes());
        let mut off = 48;
        for t in targets {
            p[off..off + 32].copy_from_slice(t);
            off += 32;
        }
        for s in segments {
            p[off..off + 28].copy_from_slice(s);
            off += 28;
        }
        p
    }

    fn xcopy_lid4_cdb(param_list_len: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x83;
        cdb[1] = 0x01; // service action = LID4
        cdb[10..14].copy_from_slice(&param_list_len.to_be_bytes());
        cdb
    }

    fn rcr_cdb(service_action: u8, alloc_len: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x84;
        cdb[1] = service_action & 0x1F;
        cdb[10..14].copy_from_slice(&alloc_len.to_be_bytes());
        cdb
    }

    #[tokio::test]
    async fn xcopy_same_lun_page_aligned_takes_fast_path() {
        // Source page is flushed (clean + Uploaded) so the cache's
        // clone_page_range takes the hash-clone path. Destination
        // should read back identical bytes via the shared chunk.
        let (_tmp, cache) = fresh_volume("vol1").await;
        let payload = page_pattern(0x55);
        cache.write_bytes(0, &payload).await.unwrap();
        cache.flush_all().await.unwrap();

        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        let seg = block_segment(0, 0, SECTORS_PER_PAGE as u16, 0, SECTORS_PER_PAGE as u64);
        let params = build_xcopy_param_list(&[target], &[seg]);
        let cdb = xcopy_cdb(params.len() as u32);
        let request = req(&cdb, &params, 0);

        let r = extended_copy(
            &request,
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        // Destination page reads back the source bytes.
        let dst_off = (SECTORS_PER_PAGE * SECTOR) as u64;
        let read_back = cache.read_bytes(dst_off, PAGE).await.unwrap();
        assert_eq!(read_back, payload);
        // Both page-index entries point at the same chunk hash.
        let src_hash = cache.writer().page_index().get(0).unwrap().unwrap();
        let dst_hash = cache.writer().page_index().get(1).unwrap().unwrap();
        assert_eq!(src_hash, dst_hash);
    }

    #[tokio::test]
    async fn xcopy_lid4_same_lun_page_aligned_round_trips() {
        // LID4 (sa 0x01) reuses the LID1 descriptor + execution path;
        // a same-LUN page-aligned copy takes the hash-clone fast path
        // exactly like LID1, just behind the 48-byte LID4 header.
        let (_tmp, cache) = fresh_volume("vol_lid4").await;
        let payload = page_pattern(0x77);
        cache.write_bytes(0, &payload).await.unwrap();
        cache.flush_all().await.unwrap();

        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        let seg = block_segment(0, 0, SECTORS_PER_PAGE as u16, 0, SECTORS_PER_PAGE as u64);
        let params = build_xcopy_lid4_param_list(&[target], &[seg]);
        let cdb = xcopy_lid4_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        let dst_off = (SECTORS_PER_PAGE * SECTOR) as u64;
        let read_back = cache.read_bytes(dst_off, PAGE).await.unwrap();
        assert_eq!(read_back, payload);
        let src_hash = cache.writer().page_index().get(0).unwrap().unwrap();
        let dst_hash = cache.writer().page_index().get(1).unwrap().unwrap();
        assert_eq!(src_hash, dst_hash, "LID4 took the hash-clone fast path");
    }

    #[tokio::test]
    async fn xcopy_lid4_rejects_bad_list_format() {
        // A LID4 parameter list whose LIST FORMAT byte isn't 0x01
        // selects a header layout we don't model; reject it.
        let (_tmp, cache) = fresh_volume("vol_lid4_fmt").await;
        cache.write_bytes(0, &page_pattern(0x01)).await.unwrap();
        cache.flush_all().await.unwrap();
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        let seg = block_segment(0, 0, SECTORS_PER_PAGE as u16, 0, SECTORS_PER_PAGE as u64);
        let mut params = build_xcopy_lid4_param_list(&[target], &[seg]);
        params[0] = 0x00; // invalid LIST FORMAT
        let cdb = xcopy_lid4_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn xcopy_lid4_rejects_inline_data() {
        // INLINE DATA LENGTH != 0 is rejected (mirrors the LID1 path).
        let (_tmp, cache) = fresh_volume("vol_lid4_inline").await;
        cache.write_bytes(0, &page_pattern(0x02)).await.unwrap();
        cache.flush_all().await.unwrap();
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        let seg = block_segment(0, 0, SECTORS_PER_PAGE as u16, 0, SECTORS_PER_PAGE as u64);
        let mut params = build_xcopy_lid4_param_list(&[target], &[seg]);
        // INLINE DATA LENGTH at bytes 46-47.
        params[46..48].copy_from_slice(&1u16.to_be_bytes());
        let cdb = xcopy_lid4_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn xcopy_cross_lun_routes_through_slow_path() {
        // Different volumes → must use bytes copy; can't share a
        // chunk because pool / namespace differs.
        let (_tmp1, src_cache) = fresh_volume("src").await;
        let (_tmp2, dst_cache) = fresh_volume("dst").await;
        let payload = page_pattern(0xC3);
        src_cache.write_bytes(0, &payload).await.unwrap();

        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, src_cache.clone());
            r.register(1, dst_cache.clone());
            Arc::new(r)
        };
        let targets = [
            target_descriptor_for(src_cache.as_ref()),
            target_descriptor_for(dst_cache.as_ref()),
        ];
        let seg = block_segment(0, 1, SECTORS_PER_PAGE as u16, 0, 0);
        let params = build_xcopy_param_list(&targets, &[seg]);
        let cdb = xcopy_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);

        let read_back = dst_cache.read_bytes(0, PAGE).await.unwrap();
        assert_eq!(read_back, payload);
    }

    #[tokio::test]
    async fn xcopy_zero_parameter_list_is_noop() {
        let (_tmp, cache) = fresh_volume("vol1").await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache);
            Arc::new(r)
        };
        let cdb = xcopy_cdb(0);
        let r = extended_copy(
            &req(&cdb, &[], 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn xcopy_unsupported_service_action_rejected() {
        let (_tmp, cache) = fresh_volume("vol1").await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache);
            Arc::new(r)
        };
        let mut cdb = xcopy_cdb(0);
        cdb[1] = 0x13; // unassigned EXTENDED COPY service action
        let r = extended_copy(
            &req(&cdb, &[], 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn xcopy_rejects_unknown_designator() {
        let (_tmp, cache) = fresh_volume("vol1").await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache);
            Arc::new(r)
        };
        // Forge a target descriptor with a valid NAA designator
        // that doesn't correspond to any registered volume.
        let mut bad_desc = [0u8; 32];
        bad_desc[0] = 0xE4;
        bad_desc[4] = 0x01;
        bad_desc[5] = 0x03; // NAA type
        bad_desc[7] = 8;
        bad_desc[8..16].copy_from_slice(&[0x3D, 0xEA, 0xDB, 0xEE, 0xFD, 0xEA, 0xDB, 0xEE]);
        let seg = block_segment(0, 0, 1, 0, 1);
        let params = build_xcopy_param_list(&[bad_desc], &[seg]);
        let cdb = xcopy_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn xcopy_rejects_unsupported_segment_descriptor_type() {
        let (_tmp, cache) = fresh_volume("vol1").await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        // Build a segment with an unknown descriptor type code.
        let mut sd = [0u8; 28];
        sd[0] = 0x99; // not 0x02
        sd[2..4].copy_from_slice(&0x18u16.to_be_bytes());
        let mut params = vec![0u8; 16 + 32 + 28];
        params[2..4].copy_from_slice(&32u16.to_be_bytes());
        params[8..12].copy_from_slice(&28u32.to_be_bytes());
        params[16..48].copy_from_slice(&target);
        params[48..76].copy_from_slice(&sd);
        let cdb = xcopy_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn xcopy_rejects_lba_past_end_of_destination() {
        let (_tmp, cache) = fresh_volume("vol1").await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        let sz = Sizing::from(cache.as_ref());
        // Destination LBA at the very end of the volume + a copy
        // of one page — runs off the end.
        let seg = block_segment(0, 0, SECTORS_PER_PAGE as u16, 0, sz.total_blocks);
        let params = build_xcopy_param_list(&[target], &[seg]);
        let cdb = xcopy_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::LBA_OUT_OF_RANGE));
    }

    #[tokio::test]
    async fn xcopy_refuses_worm_destination() {
        // Single-volume WORM scenario: source = destination, WORM=1.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 8 * (1u64 << 20), true).await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        let seg = block_segment(0, 0, SECTORS_PER_PAGE as u16, 0, SECTORS_PER_PAGE as u64);
        let params = build_xcopy_param_list(&[target], &[seg]);
        let cdb = xcopy_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert_eq!(r.sense, Some(SenseData::WRITE_PROTECTED));
    }

    #[tokio::test]
    async fn xcopy_unaligned_takes_slow_path_and_round_trips() {
        // Source LBA + length make the segment sub-page aligned —
        // forces the bytes-copy slow path. Result must still match.
        let (_tmp, cache) = fresh_volume("vol1").await;
        let payload = page_pattern(0xAA);
        cache.write_bytes(0, &payload).await.unwrap();
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        // Copy 8 sectors starting at LBA 4 → destination LBA 32
        // (well past the source range; no overlap, but sub-page).
        let seg = block_segment(0, 0, 8, 4, 32);
        let params = build_xcopy_param_list(&[target], &[seg]);
        let cdb = xcopy_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
        // Destination has the source bytes spliced in.
        let dst = cache
            .read_bytes(32 * SECTOR as u64, 8 * SECTOR)
            .await
            .unwrap();
        assert_eq!(dst, &payload[4 * SECTOR..12 * SECTOR]);
    }

    #[tokio::test]
    async fn xcopy_zero_block_segment_completes_without_error() {
        // A segment with NUMBER OF BLOCKS = 0 is a no-op but must
        // not error out the whole copy.
        let (_tmp, cache) = fresh_volume("vol1").await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, cache.clone());
            Arc::new(r)
        };
        let target = target_descriptor_for(cache.as_ref());
        let seg = block_segment(0, 0, 0, 0, 0);
        let params = build_xcopy_param_list(&[target], &[seg]);
        let cdb = xcopy_cdb(params.len() as u32);
        let r = extended_copy(
            &req(&cdb, &params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &test_tokens(),
        )
        .await;
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn receive_copy_results_operating_parameters_advertises_our_limits() {
        let cdb = rcr_cdb(0x03, 256);
        let r = receive_copy_results(&req(&cdb, &[], 256), &test_tokens());
        assert!(r.sense.is_none());
        let d = &r.data_in;
        // bytes 8-9 MAXIMUM TARGET DESCRIPTOR COUNT = 2.
        assert_eq!(u16::from_be_bytes([d[8], d[9]]), 2);
        // bytes 10-11 MAXIMUM SEGMENT DESCRIPTOR COUNT = 1.
        assert_eq!(u16::from_be_bytes([d[10], d[11]]), 1);
        // bytes 12-15 MAXIMUM DESCRIPTOR LIST LENGTH = 128.
        assert_eq!(u32::from_be_bytes([d[12], d[13], d[14], d[15]]), 128);
        // bytes 16-19 MAXIMUM SEGMENT LENGTH = 16 MiB.
        assert_eq!(
            u32::from_be_bytes([d[16], d[17], d[18], d[19]]),
            16u32 << 20
        );
        // byte 43 IMPLEMENTED DESCRIPTOR LIST LENGTH.
        let n = d[43] as usize;
        // bytes 44..44+n are the type codes.
        assert!(d[44..44 + n].contains(&0xE4));
        assert!(d[44..44 + n].contains(&0x02));
    }

    #[tokio::test]
    async fn receive_copy_results_copy_status_reports_completed() {
        let cdb = rcr_cdb(0x00, 256);
        let r = receive_copy_results(&req(&cdb, &[], 256), &test_tokens());
        assert!(r.sense.is_none());
        // COPY MANAGER STATUS byte = 0x02 (completed without errors).
        assert_eq!(r.data_in[4], 0x02);
    }

    #[tokio::test]
    async fn receive_copy_results_unknown_service_action_rejected() {
        // 0x08 is unused — SA 0x07 (RECEIVE ROD TOKEN INFORMATION)
        // is wired for ODX so it no longer rejects.
        let cdb = rcr_cdb(0x08, 256);
        let r = receive_copy_results(&req(&cdb, &[], 256), &test_tokens());
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn receive_copy_results_reserved_sa_05_rejected() {
        // SPC-4 reserves RECEIVE COPY RESULTS service action 0x05 —
        // there is no "operations count" action. It must reject like
        // any other unimplemented SA.
        let cdb = rcr_cdb(0x05, 256);
        let r = receive_copy_results(&req(&cdb, &[], 256), &test_tokens());
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn receive_copy_results_receive_data_is_empty() {
        // We hold no inline / host-bound data, so RECEIVE DATA
        // (SA 0x01) returns the bare AVAILABLE DATA = 0 header.
        let cdb = rcr_cdb(0x01, 256);
        let r = receive_copy_results(&req(&cdb, &[], 256), &test_tokens());
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 4);
        assert_eq!(
            u32::from_be_bytes([r.data_in[0], r.data_in[1], r.data_in[2], r.data_in[3]]),
            0
        );
    }

    #[tokio::test]
    async fn receive_copy_results_failed_segment_details_reports_no_failure() {
        // Synchronous XCOPY retains no per-list failure record, so
        // FAILED SEGMENT DETAILS (SA 0x04) reports command status 0
        // and zero sense data in the 60-byte fixed header.
        let cdb = rcr_cdb(0x04, 256);
        let r = receive_copy_results(&req(&cdb, &[], 256), &test_tokens());
        assert!(r.sense.is_none());
        let d = &r.data_in;
        assert_eq!(d.len(), 60);
        // bytes 0-3 AVAILABLE DATA = 56 (bytes following).
        assert_eq!(u32::from_be_bytes([d[0], d[1], d[2], d[3]]), 56);
        // byte 56 EXTENDED COPY COMMAND STATUS = 0 (no error).
        assert_eq!(d[56], 0);
        // bytes 58-59 SENSE DATA LENGTH = 0.
        assert_eq!(u16::from_be_bytes([d[58], d[59]]), 0);
    }

    #[tokio::test]
    async fn receive_copy_results_alloc_len_truncates_failed_segment_details() {
        // A short allocation length truncates the body, as for every
        // other RCR service action.
        let cdb = rcr_cdb(0x04, 16);
        let r = receive_copy_results(&req(&cdb, &[], 16), &test_tokens());
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 16);
    }

    // ----------------------------------------------------------------
    // ODX (POPULATE TOKEN / WRITE USING TOKEN / RRTI sa 0x07)
    // ----------------------------------------------------------------

    /// 16-byte ODX CDB. The LIST IDENTIFIER offset is opcode-
    /// dependent: 0x83 token operations (POPULATE / WRITE USING /
    /// CANCEL ROD TOKEN) carry it at bytes 6-9 (bytes 2-5 reserved),
    /// while RECEIVE ROD TOKEN INFORMATION (0x84) carries it at bytes
    /// 2-5. PARAMETER LIST / ALLOCATION LENGTH is at bytes 10-13.
    fn odx_cdb(opcode: u8, sa: u8, list_id: u32, plist_len: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = opcode;
        cdb[1] = sa & 0x1F;
        let lid_off = if opcode == 0x84 { 2 } else { 6 };
        cdb[lid_off..lid_off + 4].copy_from_slice(&list_id.to_be_bytes());
        cdb[10..14].copy_from_slice(&plist_len.to_be_bytes());
        cdb
    }

    fn populate_token_param_list(ranges: &[(u64, u32)]) -> Vec<u8> {
        let bdrd_total = ranges.len() * ODX_BDRD_BYTES;
        let total = 16 + bdrd_total;
        let mut plist = vec![0u8; total];
        // bytes 0-1 ROD TOKEN DATA LENGTH = plist_len - 2
        let data_len = (total - 2) as u16;
        plist[0..2].copy_from_slice(&data_len.to_be_bytes());
        // bytes 4-7 INACTIVITY TIMEOUT = 0 (use default)
        // bytes 14-15 BDRD list length
        plist[14..16].copy_from_slice(&(bdrd_total as u16).to_be_bytes());
        let mut off = 16;
        for (lba, blocks) in ranges {
            plist[off..off + 8].copy_from_slice(&lba.to_be_bytes());
            plist[off + 8..off + 12].copy_from_slice(&blocks.to_be_bytes());
            off += ODX_BDRD_BYTES;
        }
        plist
    }

    fn write_using_token_param_list(token: &[u8; ROD_TOKEN_LEN], ranges: &[(u64, u32)]) -> Vec<u8> {
        let bdrd_total = ranges.len() * ODX_BDRD_BYTES;
        let total = 16 + ROD_TOKEN_LEN + 8 + bdrd_total;
        let mut plist = vec![0u8; total];
        // header length
        let data_len = (total - 2) as u16;
        plist[0..2].copy_from_slice(&data_len.to_be_bytes());
        // ROD token bytes 16..528
        plist[16..16 + ROD_TOKEN_LEN].copy_from_slice(token);
        // BDRD list length at bytes 528..530
        plist[16 + ROD_TOKEN_LEN..16 + ROD_TOKEN_LEN + 2]
            .copy_from_slice(&(bdrd_total as u16).to_be_bytes());
        // BDRD list at bytes 536..
        let mut off = 16 + ROD_TOKEN_LEN + 8;
        for (lba, blocks) in ranges {
            plist[off..off + 8].copy_from_slice(&lba.to_be_bytes());
            plist[off + 8..off + 12].copy_from_slice(&blocks.to_be_bytes());
            off += ODX_BDRD_BYTES;
        }
        plist
    }

    /// Extract the 512-byte ROD token from a RRTI sa 0x07 response.
    fn rod_token_from_rrti(body: &[u8]) -> [u8; ROD_TOKEN_LEN] {
        // Header is 32 bytes, then ROD TOKEN DESCRIPTORS LENGTH (BE32)
        // at 32..36, then the descriptor: 2-byte type + 2-byte length
        // + 512-byte token. So token starts at byte 40.
        let mut out = [0u8; ROD_TOKEN_LEN];
        out.copy_from_slice(&body[40..40 + ROD_TOKEN_LEN]);
        out
    }

    #[tokio::test]
    async fn odx_round_trip_populate_then_write_using_token_across_volumes() {
        let (_tmp, src, dst) = fresh_two_volumes_global("src_vol", "dst_vol").await;
        // Seed two pages on the source.
        let pattern_a = page_pattern(0x10);
        let pattern_b = page_pattern(0x20);
        src.write_bytes(0, &pattern_a).await.unwrap();
        src.write_bytes(PAGE as u64, &pattern_b).await.unwrap();
        src.flush_all().await.unwrap();

        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, src.clone());
            r.register(1, dst.clone());
            Arc::new(r)
        };
        let tokens = test_tokens();
        let list_id: u32 = 0xCAFE_BABE;

        // POPULATE TOKEN over LUN 0 (src), 2 pages = 32 sectors.
        let pt_params = populate_token_param_list(&[(0, 2 * SECTORS_PER_PAGE as u32)]);
        let pt_cdb = odx_cdb(0x83, 0x10, list_id, pt_params.len() as u32);
        let r = extended_copy(
            &ScsiRequest {
                lun: 0,
                cdb: &pt_cdb,
                data_out: &pt_params,
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            },
            &registry,
            test_nexus(),
            &test_mgr(),
            &tokens,
        )
        .await;
        assert!(r.sense.is_none(), "POPULATE TOKEN: {:?}", r.sense);

        // RRTI sa 0x07 to fetch the minted token by list_id.
        let rrti_cdb = odx_cdb(0x84, 0x07, list_id, 1024);
        let rrti = receive_copy_results(
            &ScsiRequest {
                lun: 0,
                cdb: &rrti_cdb,
                data_out: &[],
                data_in_max: 1024,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            },
            &tokens,
        );
        assert!(rrti.sense.is_none());
        // RESPONSE TO SERVICE ACTION = 0x10 (POPULATE TOKEN),
        // COPY OPERATION STATUS = 0x02 (completed without errors).
        assert_eq!(rrti.data_in[4], 0x10);
        assert_eq!(rrti.data_in[5], 0x02);
        let token = rod_token_from_rrti(&rrti.data_in);

        // WRITE USING TOKEN onto LUN 1 (dst) at offset of page 4.
        let dst_lba = 4 * SECTORS_PER_PAGE as u64;
        let wut_params =
            write_using_token_param_list(&token, &[(dst_lba, 2 * SECTORS_PER_PAGE as u32)]);
        let wut_cdb = odx_cdb(0x83, 0x11, list_id + 1, wut_params.len() as u32);
        let r = extended_copy(
            &ScsiRequest {
                lun: 1,
                cdb: &wut_cdb,
                data_out: &wut_params,
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            },
            &registry,
            test_nexus(),
            &test_mgr(),
            &tokens,
        )
        .await;
        assert!(r.sense.is_none(), "WRITE USING TOKEN: {:?}", r.sense);

        // Destination reads back the seeded patterns.
        let read_a = dst.read_bytes(4 * PAGE as u64, PAGE).await.unwrap();
        let read_b = dst.read_bytes(5 * PAGE as u64, PAGE).await.unwrap();
        assert_eq!(read_a, pattern_a, "page A bytes round-trip");
        assert_eq!(read_b, pattern_b, "page B bytes round-trip");

        // Both volumes' page-index entries point at the same chunk
        // hashes — the cross-volume hash rebind happened.
        let src_h0 = src.writer().page_index().get(0).unwrap().unwrap();
        let dst_h0 = dst.writer().page_index().get(4).unwrap().unwrap();
        assert_eq!(src_h0, dst_h0, "page 0 rebound to src hash");

        // RRTI on the WRITE USING TOKEN job reports completed + 32 blocks.
        let rrti_w_cdb = odx_cdb(0x84, 0x07, list_id + 1, 1024);
        let rrti_w = receive_copy_results(
            &ScsiRequest {
                lun: 1,
                cdb: &rrti_w_cdb,
                data_out: &[],
                data_in_max: 1024,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            },
            &tokens,
        );
        assert!(rrti_w.sense.is_none());
        assert_eq!(rrti_w.data_in[4], 0x11);
        assert_eq!(rrti_w.data_in[5], 0x02);
        let transfer = u64::from_be_bytes([
            rrti_w.data_in[16],
            rrti_w.data_in[17],
            rrti_w.data_in[18],
            rrti_w.data_in[19],
            rrti_w.data_in[20],
            rrti_w.data_in[21],
            rrti_w.data_in[22],
            rrti_w.data_in[23],
        ]);
        assert_eq!(transfer, 2 * SECTORS_PER_PAGE as u64);
    }

    #[tokio::test]
    async fn odx_cancel_rod_token_invalidates_minted_token() {
        // POPULATE TOKEN → CANCEL ROD TOKEN (same LIST IDENTIFIER) →
        // the minted token is gone: RRTI reports no operation, and a
        // WRITE USING TOKEN carrying that token now rejects TOKEN
        // INVALID. A second CANCEL of the same (now-unknown) list ID
        // is a GOOD no-op.
        let (_tmp, src, dst) = fresh_two_volumes_global("src_cancel", "dst_cancel").await;
        src.write_bytes(0, &page_pattern(0x33)).await.unwrap();
        src.flush_all().await.unwrap();
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, src.clone());
            r.register(1, dst.clone());
            Arc::new(r)
        };
        let tokens = test_tokens();
        let list_id: u32 = 0x0BAD_F00D;

        // POPULATE TOKEN over LUN 0, one page.
        let pt_params = populate_token_param_list(&[(0, SECTORS_PER_PAGE as u32)]);
        let pt_cdb = odx_cdb(0x83, 0x10, list_id, pt_params.len() as u32);
        let r = extended_copy(
            &req_lun(0, &pt_cdb, &pt_params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &tokens,
        )
        .await;
        assert!(r.sense.is_none(), "POPULATE TOKEN: {:?}", r.sense);

        // Fetch the token before cancelling.
        let rrti_cdb = odx_cdb(0x84, 0x07, list_id, 1024);
        let rrti = receive_copy_results(&req_lun(0, &rrti_cdb, &[], 1024), &tokens);
        assert_eq!(rrti.data_in[5], 0x02, "token minted (status Done)");
        let token = rod_token_from_rrti(&rrti.data_in);

        // CANCEL ROD TOKEN for the same list id (zero-length plist).
        let cancel_cdb = odx_cdb(0x83, 0x12, list_id, 0);
        let c = extended_copy(
            &req_lun(0, &cancel_cdb, &[], 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &tokens,
        )
        .await;
        assert!(c.sense.is_none(), "CANCEL ROD TOKEN: {:?}", c.sense);

        // RRTI now reports "no operation in progress" (status 0x00).
        let rrti2 = receive_copy_results(&req_lun(0, &rrti_cdb, &[], 1024), &tokens);
        assert_eq!(rrti2.data_in[5], 0x00, "job forgotten after cancel");

        // WRITE USING TOKEN with the cancelled token rejects TOKEN
        // INVALID (ASC 0x23 / ASCQ 0x07).
        let wut_params = write_using_token_param_list(&token, &[(0, SECTORS_PER_PAGE as u32)]);
        let wut_cdb = odx_cdb(0x83, 0x11, list_id + 1, wut_params.len() as u32);
        let w = extended_copy(
            &req_lun(1, &wut_cdb, &wut_params, 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &tokens,
        )
        .await;
        assert_eq!(
            w.sense,
            Some(SenseData::new(
                scsi_spc::sense::SenseKey::IllegalRequest,
                0x23,
                0x07
            )),
            "cancelled token must read as INVALID"
        );

        // Cancelling the now-unknown list id again is a GOOD no-op.
        let c2 = extended_copy(
            &req_lun(0, &cancel_cdb, &[], 0),
            &registry,
            test_nexus(),
            &test_mgr(),
            &tokens,
        )
        .await;
        assert!(c2.sense.is_none(), "re-CANCEL no-op: {:?}", c2.sense);
    }

    #[tokio::test]
    async fn odx_write_using_token_with_unknown_token_rejects() {
        let (_tmp, src, dst) = fresh_two_volumes_global("s", "d").await;
        let registry: Arc<dyn VolumeLookup> = {
            let r = TestRegistry::new();
            r.register(0, src);
            r.register(1, dst);
            Arc::new(r)
        };
        let tokens = test_tokens();
        let bogus = [0x77u8; ROD_TOKEN_LEN];
        let wut_params = write_using_token_param_list(&bogus, &[(0, SECTORS_PER_PAGE as u32)]);
        let wut_cdb = odx_cdb(0x83, 0x11, 1, wut_params.len() as u32);
        let r = extended_copy(
            &ScsiRequest {
                lun: 1,
                cdb: &wut_cdb,
                data_out: &wut_params,
                data_in_max: 0,
                tsih: 0,
                initiator_iqn: None,
                initiator_isid: [0u8; 6],
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            },
            &registry,
            test_nexus(),
            &test_mgr(),
            &tokens,
        )
        .await;
        let sense = r.sense.expect("must reject unknown token");
        assert_eq!(sense.asc, 0x23);
        assert_eq!(sense.ascq, 0x07);
    }

    #[tokio::test]
    async fn receive_copy_results_rrti_unknown_list_id_emits_no_op_in_progress() {
        // RRTI on a list ID with no minted token / job returns the
        // SPC-4 "no copy operation in progress" header — COPY
        // OPERATION STATUS = 0, no token descriptor, zero
        // TRANSFER COUNT.
        let cdb = rcr_cdb(0x07, 64);
        let r = receive_copy_results(&req(&cdb, &[], 64), &test_tokens());
        assert!(r.sense.is_none(), "{:?}", r.sense);
        let d = &r.data_in;
        // AVAILABLE DATA at bytes 0..4 covers everything after byte 3.
        let available = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
        assert_eq!(available as usize, d.len() - 4);
        // COPY OPERATION STATUS at byte 5 — 0x00 = no op in progress.
        assert_eq!(d[5], 0x00);
        // TRANSFER COUNT (BE64) at bytes 16..24 is zero for the
        // no-operation-in-progress case (UNITS at byte 15 = 0x01
        // blocks regardless).
        let transfer = u64::from_be_bytes([d[16], d[17], d[18], d[19], d[20], d[21], d[22], d[23]]);
        assert_eq!(transfer, 0);
    }
}
