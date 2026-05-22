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
use core_block::uploader::UploaderError;
use tokio::sync::Mutex as AsyncMutex;

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
    let sz = Sizing::from(cache);
    if let Err(s) = validate_in_range(&range, &sz) {
        return ScsiResponse::check(s);
    }
    let want_bytes = (range.blocks * sz.sector) as usize;
    if req.data_out.len() != want_bytes {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let byte_offset = range.lba * sz.sector;
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
    let sz = Sizing::from(cache);
    if let Err(s) = validate_in_range(&range, &sz) {
        return ScsiResponse::check(s);
    }
    let want_bytes = (range.blocks * sz.sector) as usize;
    let byte_offset = range.lba * sz.sector;

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
    let sz = Sizing::from(cache);
    let range = LbaRange { lba, blocks };
    if let Err(s) = validate_in_range(&range, &sz) {
        return ScsiResponse::check(s);
    }
    let want_each = (blocks * sz.sector) as usize;
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

    let byte_offset = range.lba * sz.sector;
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
    let sz = Sizing::from(cache);

    // Two-phase: validate every descriptor first so a malformed
    // entry doesn't leave half the volume cleared. SBC-3 §5.27
    // doesn't require all-or-nothing semantics, but it's the
    // user-friendly behavior — an initiator that retries gets a
    // clean slate to retry against.
    let mut to_clear: Vec<(u64, u64)> = Vec::with_capacity(n);
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
        if let Err(s) = validate_in_range(&range, &sz) {
            return ScsiResponse::check(s);
        }
        to_clear.push((range.lba, range.blocks));
    }

    for (lba, blocks) in to_clear {
        let byte_offset = lba * sz.sector;
        let len_bytes = blocks * sz.sector;
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
    let sz = Sizing::from(cache);
    if let Err(s) = validate_in_range(&range, &sz) {
        return ScsiResponse::check(s);
    }
    let want_bytes = (range.blocks * sz.sector) as usize;
    let byte_offset = range.lba * sz.sector;

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
    if let Err(s) = validate_in_range(&range, &sz) {
        return ScsiResponse::check(s);
    }

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

    let byte_offset = range.lba * sz.sector;
    let len_bytes = range.blocks * sz.sector;

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

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
    use shared_cloud::{CloudBackend, LocalBackend};
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
        let backend: Arc<dyn CloudBackend> = Arc::new(backend);
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
        ScsiRequest {
            lun: 0,
            cdb,
            data_out,
            data_in_max,
            tsih: 0,
            initiator_iqn: None,
            cid: 0,
            peer: "",
            session_partition: None,
        }
    }

    /// An I_T nexus that doesn't hold any registration — every test
    /// in this module exercises the data path against an empty
    /// `ReservationManager`, so the nexus identity doesn't matter.
    fn test_nexus() -> Nexus {
        Nexus {
            tsih: 0,
            initiator_iqn: None,
        }
    }

    fn test_mgr() -> ReservationManager {
        ReservationManager::new()
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
}
