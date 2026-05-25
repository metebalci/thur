// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! MODE SENSE 6 / 10 (opcodes 0x1A / 0x5A) and MODE SELECT 6 / 10
//! (opcodes 0x15 / 0x55) for the thurvsa block target. Surfaces the
//! two pages every initiator probes during discovery — Caching
//! (0x08) and Control (0x0A) — plus the all-pages alias (0x3F).
//!
//! thurvsa has no host-tunable mode parameters: writes go through a
//! per-volume in-memory write-back cache that flushes lazily +
//! on SCSI SYNCHRONIZE CACHE. The page bodies advertise that
//! truth — WCE=1 (write-back enabled, host MUST issue SYNC for
//! durability), RCD=1 (no read cache), DRA=1 (no read-ahead) on
//! caching; SBC-3 baseline fields zeroed on control. PC=Changeable
//! returns an all-zero mask (no fields are host-tunable);
//! PC=Current / Default / Saved return the same defaults.
//!
//! MODE SELECT consequently has nothing to mutate. The handlers
//! parse the parameter list, validate every page body byte-for-
//! byte against the current values (since Changeable mask is all-
//! zero, any deviation is a host bug), and reply GOOD on a clean
//! match. Hosts that issue MODE SELECT during disk probe to re-
//! assert the values they just read via MODE SENSE round-trip
//! cleanly; hosts that try to flip WCE / RCD / DRA / D_SENSE get
//! INVALID FIELD IN PARAMETER LIST, telling them the field is
//! fixed. SP=1 (save pages) is rejected with SAVING PARAMETERS
//! NOT SUPPORTED — there's no persistence layer to save into when
//! every bit is fixed anyway.
//!
//! WORM volumes flip WP=1 in the DEVICE-SPECIFIC PARAMETER byte so
//! a host MODE SENSE during the failed-WRITE diagnostic surfaces
//! the protected state. The block descriptor reflects the volume's
//! current `(NUMBER OF LOGICAL BLOCKS, LOGICAL BLOCK LENGTH)`; for
//! volumes whose block count exceeds `u32::MAX` the short
//! descriptor reports `0xFFFFFFFF` (SBC-3 §6.1.3.4) — the LLBAA
//! variant on MS10 carries the full 64-bit count.

use core_block::PageCache;
use scsi_spc::mode::{
    MODE_PARAM_HEADER_6_LEN, MODE_PARAM_HEADER_10_LEN, parse_mode_param_header_6,
    parse_mode_param_header_10, patch_mode_data_length_6, patch_mode_data_length_10,
    write_mode_param_header_6, write_mode_param_header_10,
};

use super::types::{ScsiRequest, ScsiResponse, SenseData};

/// SPC-4 Page Control values (CDB byte 2 bits 7-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum PageControl {
    Current = 0x00,
    Changeable = 0x01,
    Default = 0x02,
    Saved = 0x03,
}

impl PageControl {
    fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0x00 => Self::Current,
            0x01 => Self::Changeable,
            0x02 => Self::Default,
            _ => Self::Saved,
        }
    }
}

/// MODE SENSE (6) — opcode 0x1A.
pub(super) fn mode_sense_6(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if req.cdb.len() < 6 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let dbd = req.cdb[1] & 0x08 != 0;
    let pc = PageControl::from_bits(req.cdb[2] >> 6);
    let page_code = req.cdb[2] & 0x3F;
    let subpage_code = req.cdb[3];
    let alloc_len = req.cdb[4] as usize;

    let pages = match build_pages(page_code, subpage_code, pc) {
        Ok(p) => p,
        Err(s) => return ScsiResponse::check(s),
    };

    let descriptor = if dbd {
        Vec::new()
    } else {
        short_block_descriptor(cache)
    };

    let body_len = pages.len();
    let total = MODE_PARAM_HEADER_6_LEN + descriptor.len() + body_len;
    let mut out = Vec::with_capacity(total);
    // MODE PARAMETER HEADER (4 bytes). medium_type=0 (direct-access).
    // device_specific carries WP for WORM. MODE DATA LENGTH is itself
    // one byte, so initiators that request more than 255 bytes must
    // use MODE SENSE (10) — the patch helper caps at 0xFF.
    write_mode_param_header_6(
        &mut out,
        0x00,
        device_specific_parameter(cache),
        descriptor.len() as u8,
    );
    out.extend_from_slice(&descriptor);
    out.extend_from_slice(&pages);
    patch_mode_data_length_6(&mut out, 0);

    truncate_response(out, alloc_len)
}

/// MODE SENSE (10) — opcode 0x5A.
pub(super) fn mode_sense_10(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if req.cdb.len() < 10 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let llbaa = req.cdb[1] & 0x10 != 0;
    let dbd = req.cdb[1] & 0x08 != 0;
    let pc = PageControl::from_bits(req.cdb[2] >> 6);
    let page_code = req.cdb[2] & 0x3F;
    let subpage_code = req.cdb[3];
    let alloc_len = u16::from_be_bytes([req.cdb[7], req.cdb[8]]) as usize;

    let pages = match build_pages(page_code, subpage_code, pc) {
        Ok(p) => p,
        Err(s) => return ScsiResponse::check(s),
    };

    let descriptor = if dbd {
        Vec::new()
    } else if llbaa {
        long_block_descriptor(cache)
    } else {
        short_block_descriptor(cache)
    };

    let body_len = pages.len();
    let total = MODE_PARAM_HEADER_10_LEN + descriptor.len() + body_len;
    let mut out = Vec::with_capacity(total);
    // MODE PARAMETER HEADER (8 bytes). medium_type=0 (direct-access).
    write_mode_param_header_10(
        &mut out,
        0x00,
        device_specific_parameter(cache),
        llbaa,
        descriptor.len() as u16,
    );
    out.extend_from_slice(&descriptor);
    out.extend_from_slice(&pages);
    patch_mode_data_length_10(&mut out, 0);

    truncate_response(out, alloc_len)
}

/// Build the requested mode page(s). Returns INVALID FIELD IN CDB
/// for unsupported `(page_code, subpage_code)` pairs.
fn build_pages(page_code: u8, subpage_code: u8, pc: PageControl) -> Result<Vec<u8>, SenseData> {
    if subpage_code != 0x00 {
        // thurvsa has no subpages today — every supported page is
        // SPF=0. A non-zero subpage on a known page is "page exists,
        // subpage doesn't" → INVALID FIELD per SPC-4.
        return Err(SenseData::INVALID_FIELD_IN_CDB);
    }
    let mut out = Vec::new();
    match page_code {
        0x08 => out.extend_from_slice(&caching_page(pc)),
        0x0A => out.extend_from_slice(&control_page(pc)),
        0x3F => {
            // SPC-4: page code 0x3F = "all pages." Order is
            // ascending page code so initiators can walk the response
            // by reading byte 1 (PAGE LENGTH) at each page header.
            out.extend_from_slice(&caching_page(pc));
            out.extend_from_slice(&control_page(pc));
        }
        _ => return Err(SenseData::INVALID_FIELD_IN_CDB),
    }
    Ok(out)
}

/// Caching mode page (SBC-3 §6.4.5). 20 bytes total: 2-byte header
/// + 18-byte body (PAGE LENGTH = 0x12).
///
/// thurvsa advertises WCE=1 (write-back enabled), RCD=1 (no read
/// cache), and DRA=1 (no read-ahead). thurvsa's in-memory PageCache
/// is genuinely write-back: WRITE returns GOOD as soon as bytes
/// land in the cache, before the page-index entry + cloud chunk
/// upload commit. SBC-3 §6.4.6.4: WCE=1 tells the host that GOOD
/// status on WRITE does *not* imply durability and that
/// SYNCHRONIZE CACHE is required to fence dirty cache to media.
/// Without WCE=1 the Linux block layer (and any other compliant
/// initiator) elides the SYNCHRONIZE CACHE on `sync(1)` / `umount`
/// — bytes go to cache, host proceeds, daemon dies, data lost.
/// Bug surfaced via `vsa/scripts/test-iscsi-fs-workflow.sh`: tar xf 28
/// files, umount, restart daemon, mount → only `lost+found`. With
/// WCE=1, umount issues SYNCHRONIZE CACHE 16 (LBA 0, NUM 0 = whole
/// medium); thurvsa's `cache.synchronize_bytes` flushes every dirty
/// page through `VolumeWriter::write_page`, persisting the page
/// index + chunks before umount returns.
fn caching_page(pc: PageControl) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0] = 0x08; // PS=0, SPF=0, PAGE CODE
    p[1] = 0x12; // PAGE LENGTH
    match pc {
        PageControl::Changeable => {
            // Every body byte is fixed — no host-tunable fields.
        }
        PageControl::Current | PageControl::Default | PageControl::Saved => {
            p[2] = 0x05; // RCD=1, WCE=1
            p[12] = 0x20; // DRA=1
        }
    }
    p
}

/// Control mode page (SPC-4 §7.5.7). 12 bytes total: 2-byte header
/// + 10-byte body (PAGE LENGTH = 0x0A).
///
/// Body bytes follow SPC-4 defaults: TST=0 (single I_T nexus
/// scope), D_SENSE=0 (fixed sense format default — we still emit
/// descriptor sense, but advertising D_SENSE=0 matches the "host
/// gets fixed unless it explicitly opts in" baseline initiators
/// expect), QUEUE ALGORITHM MODIFIER=0, and every other field
/// zero. thurvsa doesn't queue or reorder commands, so the
/// "restricted reorder" baseline is truthful.
fn control_page(pc: PageControl) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[0] = 0x0A;
    p[1] = 0x0A;
    if pc != PageControl::Changeable {
        // All-zero body matches the SPC-4 defaults. Kept explicit
        // so a future "we now reorder commands" toggle has a
        // single seam to flip.
    }
    p
}

/// DEVICE-SPECIFIC PARAMETER byte (SBC-3 §6.1.3.3). Bit 7 = WP
/// (write protect); bit 4 = DPOFUA (DPO/FUA support). We set WP=1
/// for WORM volumes; DPOFUA stays 0 because we don't honor the
/// DPO/FUA flags on WRITE today.
fn device_specific_parameter(cache: &PageCache) -> u8 {
    if cache.manifest().worm { 0x80 } else { 0x00 }
}

/// Short LBA block descriptor (SBC-3 §6.1.3.4). 8 bytes:
///   bytes 0-3  NUMBER OF LOGICAL BLOCKS (capped at 0xFFFFFFFF)
///   byte  4    reserved
///   bytes 5-7  LOGICAL BLOCK LENGTH (24-bit)
fn short_block_descriptor(cache: &PageCache) -> Vec<u8> {
    let m = cache.manifest();
    let total_blocks = m.size_bytes / u64::from(m.sector_bytes);
    let block_count = if total_blocks > u64::from(u32::MAX) {
        u32::MAX
    } else {
        total_blocks as u32
    };
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&block_count.to_be_bytes());
    buf.push(0x00);
    let len = m.sector_bytes & 0x00FF_FFFF;
    buf.push((len >> 16) as u8);
    buf.push((len >> 8) as u8);
    buf.push(len as u8);
    buf
}

/// Long LBA block descriptor (SBC-3 §6.1.3.5). 16 bytes:
///   bytes 0-7    NUMBER OF LOGICAL BLOCKS (64-bit)
///   bytes 8-11   reserved
///   bytes 12-15  LOGICAL BLOCK LENGTH (32-bit)
fn long_block_descriptor(cache: &PageCache) -> Vec<u8> {
    let m = cache.manifest();
    let total_blocks = m.size_bytes / u64::from(m.sector_bytes);
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&total_blocks.to_be_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&m.sector_bytes.to_be_bytes());
    buf
}

fn truncate_response(buf: Vec<u8>, alloc_len: usize) -> ScsiResponse {
    let truncated: Vec<u8> = buf.into_iter().take(alloc_len).collect();
    ScsiResponse::good(truncated)
}

/// MODE SELECT (6) — opcode 0x15. CDB layout (6 bytes):
///   byte 0     opcode = 0x15
///   byte 1     PF (bit 4) | reserved | SP (bit 0)
///   bytes 2-3  reserved
///   byte 4     PARAMETER LIST LENGTH (8-bit)
///   byte 5     CONTROL
///
/// Parameter list (in `data_out`):
///   bytes 0-3  mode parameter header (matches MS6 response shape)
///   bytes 4..  optional 8-byte block descriptor + 0+ pages
pub(super) fn mode_select_6(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if req.cdb.len() < 6 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if req.cdb[1] & 0x10 == 0 {
        // PF=0 — the host wants the deprecated SCSI-1 vendor-specific
        // mode-page format. We only speak SPC-3+ format.
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if req.cdb[1] & 0x01 != 0 {
        return ScsiResponse::check(SenseData::SAVING_PARAMETERS_NOT_SUPPORTED);
    }
    let parameter_list_length = req.cdb[4] as usize;
    if parameter_list_length == 0 {
        return ScsiResponse::good(Vec::new());
    }
    if req.data_out.len() < parameter_list_length {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    // Header byte 0 (MODE DATA LENGTH) is reserved on MODE SELECT —
    // the host writes 0. Byte 1 (MEDIUM TYPE) is reserved. Byte 2
    // (DEVICE-SPECIFIC PARAMETER) is reserved on direct-access (the
    // WP / DPOFUA bits are read-only); the host writes 0.
    let p = &req.data_out[..parameter_list_length];
    let Some(header) = parse_mode_param_header_6(p) else {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    };
    let block_descriptor_length = header.block_descriptor_length as usize;
    let pages_start = MODE_PARAM_HEADER_6_LEN + block_descriptor_length;
    if pages_start > p.len() {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    if let Err(s) =
        validate_block_descriptor(&p[MODE_PARAM_HEADER_6_LEN..pages_start], false, cache)
    {
        return ScsiResponse::check(s);
    }
    if let Err(s) = validate_pages(&p[pages_start..]) {
        return ScsiResponse::check(s);
    }
    ScsiResponse::good(Vec::new())
}

/// MODE SELECT (10) — opcode 0x55. CDB layout (10 bytes):
///   byte 0     opcode = 0x55
///   byte 1     PF (bit 4) | reserved | SP (bit 0)
///   bytes 2-6  reserved
///   bytes 7-8  PARAMETER LIST LENGTH (16-bit BE)
///   byte 9     CONTROL
///
/// Parameter list shape mirrors MS10's response (8-byte header).
pub(super) fn mode_select_10(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if req.cdb.len() < 10 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if req.cdb[1] & 0x10 == 0 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if req.cdb[1] & 0x01 != 0 {
        return ScsiResponse::check(SenseData::SAVING_PARAMETERS_NOT_SUPPORTED);
    }
    let parameter_list_length = u16::from_be_bytes([req.cdb[7], req.cdb[8]]) as usize;
    if parameter_list_length == 0 {
        return ScsiResponse::good(Vec::new());
    }
    if req.data_out.len() < parameter_list_length {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    // Bytes 0-1 (MODE DATA LENGTH), byte 2 (MEDIUM TYPE), and byte 3
    // (DEVICE-SPECIFIC PARAMETER) are reserved on MODE SELECT. Byte
    // 4 bit 0 carries LONGLBA — describes whether the block
    // descriptor is the 16-byte long form or the 8-byte short form.
    let p = &req.data_out[..parameter_list_length];
    let Some(header) = parse_mode_param_header_10(p) else {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    };
    let llbaa = header.longlba;
    let block_descriptor_length = header.block_descriptor_length as usize;
    let pages_start = MODE_PARAM_HEADER_10_LEN + block_descriptor_length;
    if pages_start > p.len() {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    if let Err(s) =
        validate_block_descriptor(&p[MODE_PARAM_HEADER_10_LEN..pages_start], llbaa, cache)
    {
        return ScsiResponse::check(s);
    }
    if let Err(s) = validate_pages(&p[pages_start..]) {
        return ScsiResponse::check(s);
    }
    ScsiResponse::good(Vec::new())
}

/// Validate a host-supplied block descriptor against the volume's
/// current sizing. Volume capacity and sector size are immutable
/// from the host's perspective; if the descriptor disagrees, that's
/// INVALID FIELD IN PARAMETER LIST. An empty descriptor is fine —
/// the host elected not to send one.
fn validate_block_descriptor(
    bytes: &[u8],
    llbaa: bool,
    cache: &PageCache,
) -> Result<(), SenseData> {
    if bytes.is_empty() {
        return Ok(());
    }
    let expected = if llbaa {
        long_block_descriptor(cache)
    } else {
        short_block_descriptor(cache)
    };
    if bytes != expected.as_slice() {
        return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
    }
    Ok(())
}

/// Walk the page list in a MODE SELECT parameter list, verifying
/// that every page is one we recognize and that its body matches
/// the current page bytes exactly. Returns INVALID FIELD IN
/// PARAMETER LIST on the first deviation. PC=Current is the
/// reference because that's what the host most recently saw via
/// MODE SENSE.
fn validate_pages(bytes: &[u8]) -> Result<(), SenseData> {
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Page header: byte 0 = PS (bit 7) | SPF (bit 6) | PAGE CODE
        // (bits 5-0). Byte 1 = PAGE LENGTH for SPF=0 pages.
        if cursor + 2 > bytes.len() {
            return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        let header = bytes[cursor];
        let spf = (header & 0x40) != 0;
        if spf {
            // SPF=1 pages have a 4-byte header, but thurvsa advertises
            // no subpaged pages today.
            return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        let page_code = header & 0x3F;
        let page_length = bytes[cursor + 1] as usize;
        let total = 2 + page_length;
        if cursor + total > bytes.len() {
            return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        let page = &bytes[cursor..cursor + total];

        // Compare against PC=Current; PS bit (bit 7 of byte 0) is
        // host-writable but reserved-on-write per SPC-4 §6.13 — i.e.
        // we accept either 0 or 1. Mask it before comparison.
        let expected: Vec<u8> = match page_code {
            0x08 => caching_page(PageControl::Current).to_vec(),
            0x0A => control_page(PageControl::Current).to_vec(),
            _ => return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST),
        };
        if page.len() != expected.len() {
            return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        // Mask PS bit on byte 0 before compare.
        if (page[0] & 0x7F) != (expected[0] & 0x7F) {
            return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        if page[1..] != expected[1..] {
            return Err(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }

        cursor += total;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use std::sync::Arc;
    use tempfile::TempDir;

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

    fn ms6_cdb(page_code: u8, subpage: u8, pc: u8, dbd: bool, alloc: u8) -> [u8; 6] {
        let mut cdb = [0u8; 6];
        cdb[0] = 0x1A;
        cdb[1] = if dbd { 0x08 } else { 0x00 };
        cdb[2] = ((pc & 0x03) << 6) | (page_code & 0x3F);
        cdb[3] = subpage;
        cdb[4] = alloc;
        cdb
    }

    fn ms10_cdb(
        page_code: u8,
        subpage: u8,
        pc: u8,
        dbd: bool,
        llbaa: bool,
        alloc: u16,
    ) -> [u8; 10] {
        let mut cdb = [0u8; 10];
        cdb[0] = 0x5A;
        cdb[1] = (if llbaa { 0x10 } else { 0 }) | (if dbd { 0x08 } else { 0 });
        cdb[2] = ((pc & 0x03) << 6) | (page_code & 0x3F);
        cdb[3] = subpage;
        cdb[7..9].copy_from_slice(&alloc.to_be_bytes());
        cdb
    }

    fn req<'a>(cdb: &'a [u8]) -> ScsiRequest<'a> {
        ScsiRequest {
            lun: 0,
            cdb,
            data_out: &[],
            data_in_max: 4096,
            tsih: 0,
            initiator_iqn: None,
            cid: 0,
            peer: "",
            session_partition: None,
        }
    }

    #[tokio::test]
    async fn ms6_caching_page_layout() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_cdb(0x08, 0x00, 0x00, false, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        let d = r.data_in;
        // Header: 4 bytes + 8-byte block descriptor + 20-byte page = 32.
        assert_eq!(d.len(), 4 + 8 + 20);
        assert_eq!(d[0] as usize, d.len() - 1); // mode data length
        assert_eq!(d[1], 0x00); // medium type
        assert_eq!(d[2], 0x00); // device-specific param (no WP)
        assert_eq!(d[3], 8); // block descriptor length

        // Block descriptor: 4 MiB / 4 KiB = 1024 blocks.
        let blocks = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
        assert_eq!(blocks, 1024);
        let block_len = u32::from_be_bytes([0, d[9], d[10], d[11]]);
        assert_eq!(block_len, DEFAULT_SECTOR_BYTES);

        // Caching page header at offset 12.
        assert_eq!(d[12], 0x08); // page code
        assert_eq!(d[13], 0x12); // page length
        assert_eq!(d[14], 0x05); // RCD=1, WCE=1
        assert_eq!(d[24], 0x20); // DRA=1
    }

    #[tokio::test]
    async fn ms6_control_page_layout() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_cdb(0x0A, 0x00, 0x00, true, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        let d = r.data_in;
        // Header (4) + control page (12) = 16, no block descriptor.
        assert_eq!(d.len(), 16);
        assert_eq!(d[3], 0); // block descriptor length
        assert_eq!(d[4], 0x0A); // page code
        assert_eq!(d[5], 0x0A); // page length
        // Body bytes (10): SPC-4 baseline zeros.
        assert!(d[6..16].iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn ms6_all_pages_returns_caching_then_control() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_cdb(0x3F, 0x00, 0x00, true, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        let d = r.data_in;
        // 4 (header) + 20 (caching) + 12 (control) = 36.
        assert_eq!(d.len(), 36);
        assert_eq!(d[4], 0x08); // first page = caching
        assert_eq!(d[24], 0x0A); // second page = control
    }

    #[tokio::test]
    async fn ms6_changeable_page_zeros_tunable_bits() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_cdb(0x08, 0x00, 0x01, true, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        let d = r.data_in;
        // PC=Changeable: every body byte zero (no host-tunable fields).
        assert_eq!(d[4], 0x08);
        assert_eq!(d[5], 0x12);
        assert!(d[6..24].iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn ms6_default_pc_matches_current() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let current = mode_sense_6(
            &req(&ms6_cdb(0x08, 0x00, 0x00, true, 0xFF)),
            Some(cache.as_ref()),
        );
        let default = mode_sense_6(
            &req(&ms6_cdb(0x08, 0x00, 0x02, true, 0xFF)),
            Some(cache.as_ref()),
        );
        let saved = mode_sense_6(
            &req(&ms6_cdb(0x08, 0x00, 0x03, true, 0xFF)),
            Some(cache.as_ref()),
        );
        assert_eq!(current.data_in, default.data_in);
        assert_eq!(current.data_in, saved.data_in);
    }

    #[tokio::test]
    async fn ms6_dbd_omits_block_descriptor() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_cdb(0x08, 0x00, 0x00, true, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        let d = r.data_in;
        assert_eq!(d[3], 0);
        // Right after the 4-byte header is the page (no descriptor).
        assert_eq!(d[4], 0x08);
        assert_eq!(d.len(), 4 + 20);
    }

    #[tokio::test]
    async fn ms6_unknown_page_returns_invalid_field() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_cdb(0x05, 0x00, 0x00, false, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn ms6_subpage_on_supported_page_returns_invalid_field() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // 0x08/0x01 — caching has no subpages.
        let cdb = ms6_cdb(0x08, 0x01, 0x00, true, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn ms6_alloc_len_truncates_response() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_cdb(0x08, 0x00, 0x00, true, 8);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 8);
    }

    #[tokio::test]
    async fn ms6_unmapped_lun_check_condition() {
        let cdb = ms6_cdb(0x08, 0x00, 0x00, false, 0xFF);
        let r = mode_sense_6(&req(&cdb), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn ms6_short_cdb_invalid_field() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = [0x1Au8, 0, 0]; // 3 bytes — too short
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn ms6_worm_volume_advertises_write_protect() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), true).await;
        let cdb = ms6_cdb(0x08, 0x00, 0x00, true, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        let d = r.data_in;
        assert_eq!(d[2], 0x80); // device-specific param: WP=1
    }

    #[tokio::test]
    async fn ms6_huge_volume_caps_block_count_at_u32_max() {
        let tmp = TempDir::new().unwrap();
        // 16 TiB / 4 KiB = 4 294 967 296 blocks > u32::MAX
        let cache = fixture_cache(tmp.path(), 16 * (1u64 << 40), false).await;
        let cdb = ms6_cdb(0x08, 0x00, 0x00, false, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache.as_ref()));
        let d = r.data_in;
        let blocks = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
        assert_eq!(blocks, u32::MAX);
    }

    #[tokio::test]
    async fn ms10_caching_page_layout() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms10_cdb(0x08, 0x00, 0x00, false, false, 4096);
        let r = mode_sense_10(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        let d = r.data_in;
        // Header (8) + short descriptor (8) + caching page (20) = 36.
        assert_eq!(d.len(), 8 + 8 + 20);

        let mode_data_len = u16::from_be_bytes([d[0], d[1]]) as usize;
        assert_eq!(mode_data_len, d.len() - 2);
        assert_eq!(d[2], 0x00); // medium type
        assert_eq!(d[3], 0x00); // device-specific param
        assert_eq!(d[4], 0x00); // LONGLBA=0
        let bdl = u16::from_be_bytes([d[6], d[7]]);
        assert_eq!(bdl, 8);

        // Page header at offset 8 + 8 = 16.
        assert_eq!(d[16], 0x08);
        assert_eq!(d[17], 0x12);
        assert_eq!(d[18], 0x05); // RCD=1, WCE=1
    }

    #[tokio::test]
    async fn ms10_control_page_layout() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms10_cdb(0x0A, 0x00, 0x00, true, false, 4096);
        let r = mode_sense_10(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        let d = r.data_in;
        // Header (8) + control page (12) = 20.
        assert_eq!(d.len(), 20);
        let mode_data_len = u16::from_be_bytes([d[0], d[1]]) as usize;
        assert_eq!(mode_data_len, d.len() - 2);
        assert_eq!(d[8], 0x0A); // page code
        assert_eq!(d[9], 0x0A); // page length
    }

    #[tokio::test]
    async fn ms10_llbaa_emits_long_block_descriptor() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 16 * (1u64 << 40), false).await;
        let cdb = ms10_cdb(0x08, 0x00, 0x00, false, true, 4096);
        let r = mode_sense_10(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        let d = r.data_in;
        // Header (8) + long descriptor (16) + page (20) = 44.
        assert_eq!(d.len(), 8 + 16 + 20);
        assert_eq!(d[4], 0x01); // LONGLBA=1
        let bdl = u16::from_be_bytes([d[6], d[7]]);
        assert_eq!(bdl, 16);
        let blocks = u64::from_be_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]);
        let expected = (16 * (1u64 << 40)) / u64::from(DEFAULT_SECTOR_BYTES);
        assert_eq!(blocks, expected);
        let block_len = u32::from_be_bytes([d[20], d[21], d[22], d[23]]);
        assert_eq!(block_len, DEFAULT_SECTOR_BYTES);
    }

    #[tokio::test]
    async fn ms10_alloc_len_truncates_response() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms10_cdb(0x08, 0x00, 0x00, true, false, 4);
        let r = mode_sense_10(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 4);
    }

    #[tokio::test]
    async fn ms10_unknown_page_returns_invalid_field() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms10_cdb(0x05, 0x00, 0x00, true, false, 4096);
        let r = mode_sense_10(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn ms10_unmapped_lun_check_condition() {
        let cdb = ms10_cdb(0x08, 0x00, 0x00, true, false, 4096);
        let r = mode_sense_10(&req(&cdb), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn ms10_short_cdb_invalid_field() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = [0x5Au8; 5];
        let r = mode_sense_10(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    // ----------------------------------------------------------------
    // MODE SELECT (6) — opcode 0x15
    // MODE SELECT (10) — opcode 0x55
    //
    // thurvsa has no host-tunable bits today: every Changeable mask
    // byte is zero. So MODE SELECT is a "validate-and-no-op" stub.
    // These tests cover the round-trip case (host writes back what
    // it just read), the SP=1 / PF=0 rejections, and the
    // mismatch-on-write case.
    // ----------------------------------------------------------------

    fn ms6_select_cdb(pf: bool, sp: bool, parameter_list_length: u8) -> [u8; 6] {
        let mut cdb = [0u8; 6];
        cdb[0] = 0x15;
        cdb[1] = (if pf { 0x10 } else { 0 }) | (if sp { 0x01 } else { 0 });
        cdb[4] = parameter_list_length;
        cdb
    }

    fn ms10_select_cdb(pf: bool, sp: bool, parameter_list_length: u16) -> [u8; 10] {
        let mut cdb = [0u8; 10];
        cdb[0] = 0x55;
        cdb[1] = (if pf { 0x10 } else { 0 }) | (if sp { 0x01 } else { 0 });
        cdb[7..9].copy_from_slice(&parameter_list_length.to_be_bytes());
        cdb
    }

    fn req_with_data_out<'a>(cdb: &'a [u8], data_out: &'a [u8]) -> ScsiRequest<'a> {
        ScsiRequest {
            lun: 0,
            cdb,
            data_out,
            data_in_max: 0,
            tsih: 0,
            initiator_iqn: None,
            cid: 0,
            peer: "",
            session_partition: None,
        }
    }

    /// Build a MODE SELECT (6) parameter list mirroring what the
    /// host would have just received from MODE SENSE (6) for the
    /// caching page (PC=Current, no DBD).
    async fn round_trip_ms6_caching_params(cache: &PageCache) -> Vec<u8> {
        let cdb = ms6_cdb(0x08, 0x00, 0x00, false, 0xFF);
        let r = mode_sense_6(&req(&cdb), Some(cache));
        let mut buf = r.data_in;
        // Reserved-on-write: zero out MODE DATA LENGTH (byte 0),
        // MEDIUM TYPE (byte 1), DEVICE-SPECIFIC PARAMETER (byte 2).
        buf[0] = 0;
        buf[1] = 0;
        buf[2] = 0;
        buf
    }

    async fn round_trip_ms10_caching_params(cache: &PageCache, llbaa: bool) -> Vec<u8> {
        let cdb = ms10_cdb(0x08, 0x00, 0x00, false, llbaa, 4096);
        let r = mode_sense_10(&req(&cdb), Some(cache));
        let mut buf = r.data_in;
        // Reserved-on-write: MODE DATA LENGTH (bytes 0-1), MEDIUM
        // TYPE (byte 2), DEVICE-SPECIFIC PARAMETER (byte 3). LONGLBA
        // (byte 4 bit 0) stays — we read it on the parse path.
        buf[0] = 0;
        buf[1] = 0;
        buf[2] = 0;
        buf[3] = 0;
        buf
    }

    #[tokio::test]
    async fn ms6_select_round_trips_caching_page() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = round_trip_ms6_caching_params(cache.as_ref()).await;
        let cdb = ms6_select_cdb(true, false, params.len() as u8);
        let r = mode_select_6(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn ms10_select_round_trips_caching_page() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = round_trip_ms10_caching_params(cache.as_ref(), false).await;
        let cdb = ms10_select_cdb(true, false, params.len() as u16);
        let r = mode_select_10(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn ms10_select_round_trips_with_llbaa_block_descriptor() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = round_trip_ms10_caching_params(cache.as_ref(), true).await;
        let cdb = ms10_select_cdb(true, false, params.len() as u16);
        let r = mode_select_10(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert!(r.sense.is_none(), "{:?}", r.sense);
    }

    #[tokio::test]
    async fn ms6_select_rejects_wce_flip_to_zero() {
        // thurvsa Current advertises WCE=1 (write-back enabled) so a
        // host can't flip it OFF — the cache is genuinely write-back
        // and pretending otherwise would mislead the host into
        // skipping SYNCHRONIZE CACHE on umount and losing data.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let mut params = round_trip_ms6_caching_params(cache.as_ref()).await;
        // Find caching page header (page code 0x08) inside the
        // parameter list and clear WCE on body byte 0.
        let header_start = 4 + params[3] as usize;
        assert_eq!(params[header_start], 0x08, "caching page header");
        params[header_start + 2] &= !0x04; // clear WCE bit (byte 2 bit 2)
        let cdb = ms6_select_cdb(true, false, params.len() as u8);
        let r = mode_select_6(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn ms10_select_rejects_dsense_flip() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms10_cdb(0x0A, 0x00, 0x00, true, false, 4096);
        let r = mode_sense_10(&req(&cdb), Some(cache.as_ref()));
        let mut params = r.data_in;
        params[0] = 0;
        params[1] = 0;
        params[2] = 0;
        params[3] = 0;
        // Control page header is right after the 8-byte parameter
        // header. D_SENSE is byte 2 bit 2 of the page body.
        params[8 + 2] |= 0x04;
        let cdb = ms10_select_cdb(true, false, params.len() as u16);
        let r = mode_select_10(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn ms6_select_pf_zero_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = round_trip_ms6_caching_params(cache.as_ref()).await;
        let cdb = ms6_select_cdb(false, false, params.len() as u8);
        let r = mode_select_6(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn ms6_select_sp_one_rejected_with_saving_not_supported() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = round_trip_ms6_caching_params(cache.as_ref()).await;
        let cdb = ms6_select_cdb(true, true, params.len() as u8);
        let r = mode_select_6(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::SAVING_PARAMETERS_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn ms10_select_sp_one_rejected_with_saving_not_supported() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let params = round_trip_ms10_caching_params(cache.as_ref(), false).await;
        let cdb = ms10_select_cdb(true, true, params.len() as u16);
        let r = mode_select_10(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::SAVING_PARAMETERS_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn ms6_select_zero_parameter_list_is_noop_success() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = ms6_select_cdb(true, false, 0);
        let r = mode_select_6(&req_with_data_out(&cdb, &[]), Some(cache.as_ref()));
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn ms6_select_unknown_page_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // 4-byte header + 4-byte fake page (page code 0x05).
        let mut params = vec![0u8; 8];
        // header bytes 0-3 are reserved on write
        params[3] = 0; // BLOCK DESCRIPTOR LENGTH
        params[4] = 0x05; // page code 0x05 — not supported
        params[5] = 0x02; // page length
        params[6] = 0;
        params[7] = 0;
        let cdb = ms6_select_cdb(true, false, params.len() as u8);
        let r = mode_select_6(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn ms6_select_block_descriptor_mismatch_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let mut params = round_trip_ms6_caching_params(cache.as_ref()).await;
        // Flip the block-descriptor block-count bytes — claim a
        // different volume size. Should reject.
        params[4] = 0xFF;
        params[5] = 0xFF;
        let cdb = ms6_select_cdb(true, false, params.len() as u8);
        let r = mode_select_6(&req_with_data_out(&cdb, &params), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }

    #[tokio::test]
    async fn ms6_select_unmapped_lun_check_condition() {
        let cdb = ms6_select_cdb(true, false, 0);
        let r = mode_select_6(&req_with_data_out(&cdb, &[]), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn ms10_select_unmapped_lun_check_condition() {
        let cdb = ms10_select_cdb(true, false, 0);
        let r = mode_select_10(&req_with_data_out(&cdb, &[]), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn ms6_select_short_cdb_invalid_field() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        let cdb = [0x15u8, 0x10, 0]; // 3 bytes — too short
        let r = mode_select_6(&req_with_data_out(&cdb, &[]), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn ms6_select_data_out_shorter_than_parameter_list_length() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20), false).await;
        // Claim a 32-byte parameter list but only ship 4 bytes.
        let cdb = ms6_select_cdb(true, false, 32);
        let r = mode_select_6(&req_with_data_out(&cdb, &[0u8; 4]), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_PARAMETER_LIST));
    }
}
