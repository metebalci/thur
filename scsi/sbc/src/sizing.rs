// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Read-only sizing / identity opcodes: TEST UNIT READY, READ
//! CAPACITY 10 / 16, REPORT LUNS.
//!
//! These opcodes don't move user data — they're the surface every
//! initiator hits during discovery and probing. Implementing them
//! before the WRITE / READ data path lets us run real iSCSI
//! conformance tools (`sg3-utils`, `iscsi-inq`) end-to-end and
//! validates the dispatcher shape without committing to an SBC-3
//! data-path design yet.

use core_block::PageCache;

use super::types::{ScsiRequest, ScsiResponse, SenseData};

/// TEST UNIT READY (opcode 0x00). Returns GOOD when the LUN is
/// registered, CHECK CONDITION + LU NOT SUPPORTED otherwise. The
/// real SBC-3 path will surface NOT READY transitions (e.g. a
/// volume in the middle of a re-attach); for now every registered
/// LUN reports ready.
pub(super) fn test_unit_ready(_req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    if cache.is_some() {
        ScsiResponse::good(Vec::new())
    } else {
        ScsiResponse::check(SenseData::LU_NOT_SUPPORTED)
    }
}

/// READ CAPACITY (10) — opcode 0x25. Returns the last LBA + block
/// length in 8 bytes. Volumes whose last LBA exceeds `u32::MAX`
/// return `0xFFFFFFFF` per SBC-3, signalling "use READ CAPACITY
/// 16."
pub(super) fn read_capacity_10(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    if req.cdb.len() < 10 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let manifest = cache.manifest();
    // Live size (issue #76), not the boot-snapshot manifest, so a host
    // re-issuing READ CAPACITY after an online resize sees the new size.
    let total_blocks = cache.size_bytes() / u64::from(manifest.sector_bytes);
    let last_lba_u32 = if total_blocks > u64::from(u32::MAX) {
        u32::MAX
    } else if total_blocks == 0 {
        0
    } else {
        (total_blocks - 1) as u32
    };
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&last_lba_u32.to_be_bytes());
    buf.extend_from_slice(&manifest.sector_bytes.to_be_bytes());
    ScsiResponse::good(buf)
}

/// SERVICE ACTION IN (16) — opcode 0x9E. Today we route only
/// service action 0x10 (READ CAPACITY 16). Anything else gets
/// INVALID FIELD IN CDB; a future GET LBA STATUS handler etc.
/// extends the match.
pub(super) fn service_action_in_16(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
) -> ScsiResponse {
    if req.cdb.len() < 16 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let service_action = req.cdb[1] & 0x1F;
    if service_action != 0x10 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    read_capacity_16(req, cache)
}

fn read_capacity_16(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    let Some(cache) = cache else {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    };
    let manifest = cache.manifest();
    // Live size (issue #76), not the boot-snapshot manifest.
    let total_blocks = cache.size_bytes() / u64::from(manifest.sector_bytes);
    let last_lba = total_blocks.saturating_sub(1);

    let alloc_len =
        u32::from_be_bytes([req.cdb[10], req.cdb[11], req.cdb[12], req.cdb[13]]) as usize;

    let mut buf = vec![0u8; 32];
    buf[0..8].copy_from_slice(&last_lba.to_be_bytes());
    buf[8..12].copy_from_slice(&manifest.sector_bytes.to_be_bytes());
    // byte 14: LBPME (bit 7) | LBPRZ (bit 6) | LOWEST ALIGNED LBA
    // (bits 5-0, top 6 bits of a 14-bit value). LBPME=1 advertises
    // that thin-provisioning management is enabled (initiators that
    // honor it then read VPD 0xB2 for the surface details). LBPRZ=1
    // commits to "unmapped reads return zeros" — matches our sparse
    // -hole semantics on READ. Lowest aligned LBA = 0 (volumes
    // start at LBA 0).
    buf[14] = 0xC0;
    // Bytes 12-13 (PROT_EN, P_TYPE, P_I_EXPONENT, LOGICAL BLOCKS
    // PER PHYSICAL BLOCK EXPONENT) and bytes 15-31 stay zero — no
    // protection info, no read/write protection, no skip mask.

    let truncated: Vec<u8> = buf.into_iter().take(alloc_len).collect();
    ScsiResponse::good(truncated)
}

/// REPORT LUNS — opcode 0xA0. Returns every LUN the dispatcher
/// knows about, encoded SAM-5 style. Honors SELECT REPORT byte:
///   0x00, 0x02, 0x11, 0x12 → full LUN list (we have no admin /
///       well-known LUNs, so all "include admin / well-known"
///       variants are equivalent to the plain list)
///   0x01 → admin LUNs only (empty)
///   anything else → INVALID FIELD IN CDB
///
/// SAM-5 LUN field encoding + 8-byte header framing live in
/// [`scsi_spc::report_luns::build_report_luns`].
pub(super) fn report_luns(req: &ScsiRequest<'_>, luns: &[u64]) -> ScsiResponse {
    if req.cdb.len() < 12 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let select_report = req.cdb[2];
    let alloc_len = u32::from_be_bytes([req.cdb[6], req.cdb[7], req.cdb[8], req.cdb[9]]) as usize;

    let lun_list: &[u64] = match select_report {
        0x00 | 0x02 | 0x11 | 0x12 => luns,
        0x01 | 0x10 => &[],
        _ => return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
    };

    let buf = scsi_spc::report_luns::build_report_luns(lun_list);
    let truncated: Vec<u8> = buf.into_iter().take(alloc_len).collect();
    ScsiResponse::good(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn fixture_cache(data_dir: &std::path::Path, size: u64) -> Arc<PageCache> {
        let storage_root = data_dir.join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = LocalBackend::new(&storage_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        VolumeManifest::new(
            "vol1".into(),
            size,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(data_dir)
        .unwrap();
        let writer = Arc::new(VolumeWriter::open(data_dir, "vol1", backend).unwrap());
        PageCache::new(writer)
    }

    fn req<'a>(cdb: &'a [u8]) -> ScsiRequest<'a> {
        ScsiRequest {
            lun: 0,
            cdb,
            data_out: &[],
            data_in_max: 4096,
            tsih: 0,
            initiator_iqn: None,
            initiator_isid: [0u8; 6],
            cid: 0,
            peer: "",
            session_partition: None,
            session_volumes: None,
        }
    }

    #[tokio::test]
    async fn tur_returns_good_for_registered_lun() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20)).await;
        let cdb = [0x00u8; 6];
        let r = test_unit_ready(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        assert!(r.data_in.is_empty());
    }

    #[tokio::test]
    async fn tur_check_condition_on_unmapped_lun() {
        let cdb = [0x00u8; 6];
        let r = test_unit_ready(&req(&cdb), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn read_capacity_10_small_volume() {
        // 4 MiB / 4 KiB = 1024 blocks, last LBA = 1023.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20)).await;
        let cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let r = read_capacity_10(&req(&cdb), Some(cache.as_ref()));
        let last_lba = u32::from_be_bytes([r.data_in[0], r.data_in[1], r.data_in[2], r.data_in[3]]);
        let block = u32::from_be_bytes([r.data_in[4], r.data_in[5], r.data_in[6], r.data_in[7]]);
        assert_eq!(last_lba, 1023);
        assert_eq!(block, DEFAULT_SECTOR_BYTES);
    }

    #[tokio::test]
    async fn read_capacity_10_caps_at_u32_max_for_huge_volume() {
        // 16 TiB / 4 KiB = 4 294 967 296 blocks, last LBA exceeds
        // u32::MAX → should report 0xFFFFFFFF.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 16 * (1u64 << 40)).await;
        let cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let r = read_capacity_10(&req(&cdb), Some(cache.as_ref()));
        let last_lba = u32::from_be_bytes([r.data_in[0], r.data_in[1], r.data_in[2], r.data_in[3]]);
        assert_eq!(last_lba, u32::MAX);
    }

    #[tokio::test]
    async fn read_capacity_16_advertises_lbpme_and_lbprz() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20)).await;
        let mut cdb = [0u8; 16];
        cdb[0] = 0x9E;
        cdb[1] = 0x10;
        cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
        let r = service_action_in_16(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        // LBPME=bit 7, LBPRZ=bit 6 in byte 14. Both must be set so
        // the host infers thin-provisioning + UNMAP-reads-zero
        // semantics directly off the capacity probe.
        assert_eq!(r.data_in[14] & 0x80, 0x80, "LBPME bit");
        assert_eq!(r.data_in[14] & 0x40, 0x40, "LBPRZ bit");
        // Lowest aligned LBA bits = 0.
        assert_eq!(r.data_in[14] & 0x3F, 0x00);
        assert_eq!(r.data_in[15], 0x00);
    }

    #[tokio::test]
    async fn read_capacity_16_reflects_online_resize() {
        // 4 MiB / 4 KiB = 1024 blocks, last LBA = 1023.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 4 * (1u64 << 20)).await;
        let mut cdb = [0u8; 16];
        cdb[0] = 0x9E;
        cdb[1] = 0x10;
        cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
        let last_lba = |r: &ScsiResponse| {
            u64::from_be_bytes([
                r.data_in[0],
                r.data_in[1],
                r.data_in[2],
                r.data_in[3],
                r.data_in[4],
                r.data_in[5],
                r.data_in[6],
                r.data_in[7],
            ])
        };
        let before = service_action_in_16(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(last_lba(&before), 1023);

        // Grow to 8 MiB: READ CAPACITY must report the new last LBA off
        // the live shadow with no restart / new PageCache (issue #76).
        cache.writer().set_size(8 * (1u64 << 20)).unwrap();
        let after = service_action_in_16(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(last_lba(&after), 2047);
    }

    #[tokio::test]
    async fn read_capacity_16_emits_full_64bit_last_lba() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path(), 16 * (1u64 << 40)).await;
        let mut cdb = [0u8; 16];
        cdb[0] = 0x9E;
        cdb[1] = 0x10; // service action READ CAPACITY 16
        cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
        let r = service_action_in_16(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 32);
        let last_lba = u64::from_be_bytes([
            r.data_in[0],
            r.data_in[1],
            r.data_in[2],
            r.data_in[3],
            r.data_in[4],
            r.data_in[5],
            r.data_in[6],
            r.data_in[7],
        ]);
        let expected = (16 * (1u64 << 40)) / u64::from(DEFAULT_SECTOR_BYTES) - 1;
        assert_eq!(last_lba, expected);
        let block = u32::from_be_bytes([r.data_in[8], r.data_in[9], r.data_in[10], r.data_in[11]]);
        assert_eq!(block, DEFAULT_SECTOR_BYTES);
    }

    #[tokio::test]
    async fn read_capacity_16_rejects_unknown_service_action() {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x9E;
        cdb[1] = 0x11;
        let r = service_action_in_16(&req(&cdb), None);
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[test]
    fn report_luns_returns_full_list_under_select_zero() {
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA0;
        cdb[2] = 0x00;
        cdb[6..10].copy_from_slice(&64u32.to_be_bytes());
        let r = report_luns(&req(&cdb), &[0, 3, 5]);
        let listed_len =
            u32::from_be_bytes([r.data_in[0], r.data_in[1], r.data_in[2], r.data_in[3]]);
        assert_eq!(listed_len, 24);
        // Three LUNs starting at offset 8.
        assert_eq!(r.data_in[8], 0); // LUN 0 byte 0
        assert_eq!(r.data_in[9], 0); // LUN 0 byte 1
        assert_eq!(r.data_in[16], 0); // LUN 3 byte 0
        assert_eq!(r.data_in[17], 3); // LUN 3 byte 1
        assert_eq!(r.data_in[24], 0); // LUN 5 byte 0
        assert_eq!(r.data_in[25], 5);
    }

    #[test]
    fn report_luns_admin_only_is_empty() {
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA0;
        cdb[2] = 0x01;
        cdb[6..10].copy_from_slice(&64u32.to_be_bytes());
        let r = report_luns(&req(&cdb), &[0, 1, 2]);
        let listed_len =
            u32::from_be_bytes([r.data_in[0], r.data_in[1], r.data_in[2], r.data_in[3]]);
        assert_eq!(listed_len, 0);
        assert_eq!(r.data_in.len(), 8);
    }

    #[test]
    fn report_luns_rejects_unknown_select_report() {
        let mut cdb = [0u8; 12];
        cdb[0] = 0xA0;
        cdb[2] = 0xFF;
        let r = report_luns(&req(&cdb), &[]);
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }
}
