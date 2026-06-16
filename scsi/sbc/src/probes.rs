// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Host-probe stub opcodes that don't touch the page cache:
//! REQUEST SENSE (0x03), START STOP UNIT (0x1B), PREVENT/ALLOW
//! MEDIUM REMOVAL (0x1E), LOG SENSE (0x4D).
//!
//! Linux's `sd_mod` and ESXi's storage stack issue these during
//! attach / hot-plug / shutdown; without handlers they fall
//! through to INVALID OP CODE and clutter the kernel log even
//! though they're functionally no-ops on a virtual direct-access
//! target. Each handler here either returns a structurally-valid
//! empty response (REQUEST SENSE, LOG SENSE) or accepts the
//! command and reports GOOD without modeling state (START STOP
//! UNIT, PREVENT/ALLOW MEDIUM REMOVAL — thurvsa has no power
//! states and no removable media).

use core_block::PageCache;

use super::types::{ScsiRequest, ScsiResponse, SenseData};

/// REQUEST SENSE (opcode 0x03). Hosts issue this when the
/// initiator stack lost track of an autosense'd CHECK CONDITION
/// or simply during attach probing. iSCSI always carries
/// autosense in the SCSI Response PDU, so we have nothing
/// "pending" to report — return NoSense.
///
/// CDB:
///   byte 0  0x03
///   byte 1  bit 0 = DESC (1 → descriptor format, 0 → fixed)
///   byte 4  ALLOCATION LENGTH
///
/// Response is truncated to ALLOCATION LENGTH per SPC-4 §6.39.
/// We honor DESC by emitting the same descriptor-format bytes the
/// rest of the dispatcher uses; for DESC=0 we synthesize an
/// 18-byte fixed-format response with sense key NoSense.
pub(super) fn request_sense(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    if req.cdb.len() < 6 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let desc_format = req.cdb[1] & 0x01 != 0;
    let alloc_len = req.cdb[4] as usize;

    // Per SPC-4 §6.39: if the addressed LUN doesn't exist we still
    // succeed — REQUEST SENSE is one of the handful of opcodes that
    // must complete on an unmapped LUN so the initiator can probe
    // the target's command response shape. Same as INQUIRY.
    let _ = cache;

    let bytes: Vec<u8> = if desc_format {
        // Descriptor format, response code 0x72, no descriptors —
        // matches `SenseData::to_descriptor_bytes` shape but with
        // sense key NoSense.
        vec![0x72, 0x00, 0x00, 0x00, 0, 0, 0, 0]
    } else {
        // Fixed format, response code 0x70. 18-byte canonical
        // length: byte 7 ADDITIONAL SENSE LENGTH = 10 (= 18 - 8).
        let mut buf = vec![0u8; 18];
        buf[0] = 0x70;
        buf[7] = 10;
        buf
    };

    let truncated: Vec<u8> = bytes.into_iter().take(alloc_len).collect();
    ScsiResponse::good(truncated)
}

/// START STOP UNIT (opcode 0x1B). Linux's `sd_mod` issues this
/// during attach / suspend / shutdown to spin up / spin down the
/// device. thurvsa volumes don't model power states — every volume
/// is "always ready" — so we accept-and-GOOD regardless of
/// PowerCondition / NoFlush / LOEJ / START bits.
///
/// CDB:
///   byte 0  0x1B
///   byte 1  bit 0 IMMED (response timing)
///   byte 4  bits 7-4 PowerCondition
///           bit 2 NO_FLUSH
///           bit 1 LOEJ (load/eject — n/a for non-removable)
///           bit 0 START
///   byte 5  control
///
/// Returns CHECK CONDITION + LU NOT SUPPORTED for absent LUNs
/// (matches the rest of the dispatcher's unmapped-LUN behavior).
pub(super) fn start_stop_unit(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    if req.cdb.len() < 6 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if cache.is_none() {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    }
    ScsiResponse::good(Vec::new())
}

/// PREVENT/ALLOW MEDIUM REMOVAL (opcode 0x1E). On a tape or CD-ROM
/// this gates the eject button / drive-empty path; on a
/// direct-access block target the medium isn't removable, so we
/// accept-and-GOOD regardless of byte 4's prevent flags.
///
/// CDB:
///   byte 0  0x1E
///   byte 4  bits 1-0 prevent flags
///   byte 5  control
pub(super) fn prevent_allow_medium_removal(
    req: &ScsiRequest<'_>,
    cache: Option<&PageCache>,
) -> ScsiResponse {
    if req.cdb.len() < 6 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if cache.is_none() {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    }
    ScsiResponse::good(Vec::new())
}

/// LOG SENSE (opcode 0x4D). Hosts probe this during attach to
/// discover supported log pages (page 0x00). thurvsa exposes no log
/// pages today — the surface is for SAS / SES temperature /
/// retry counters that don't apply to a virtual block target — so
/// page 0x00 returns a minimal supported-pages list (page 0x00
/// itself only). Any other page code is rejected with INVALID
/// FIELD IN CDB per SPC-4 §7.2.5.
///
/// CDB:
///   byte 0  0x4D
///   byte 1  bit 0 SP (save parameters — n/a, log not persisted)
///           bit 1 PPC (parameter pointer control — ignored)
///   byte 2  bits 7-6 PC (page control)
///           bits 5-0 PAGE CODE
///   byte 3  SUBPAGE CODE
///   byte 5-6 PARAMETER POINTER
///   byte 7-8 ALLOCATION LENGTH
pub(super) fn log_sense(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    if req.cdb.len() < 10 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    if cache.is_none() {
        return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
    }
    let page_code = req.cdb[2] & 0x3F;
    let subpage_code = req.cdb[3];
    let alloc_len = u16::from_be_bytes([req.cdb[7], req.cdb[8]]) as usize;

    if subpage_code != 0x00 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }

    let body: Vec<u8> = match page_code {
        0x00 => {
            // SUPPORTED LOG PAGES (SPC-4 §7.2.13). 4-byte page header
            // + N parameter codes (one per supported page). We only
            // claim support for page 0x00 itself.
            let supported: &[u8] = &[0x00];
            let mut page = Vec::with_capacity(4 + supported.len());
            page.push(0x00); // PAGE CODE
            page.push(0x00); // SUBPAGE CODE
            page.extend_from_slice(&(supported.len() as u16).to_be_bytes());
            page.extend_from_slice(supported);
            page
        }
        _ => return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
    };

    let truncated: Vec<u8> = body.into_iter().take(alloc_len).collect();
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

    async fn fixture_cache(data_dir: &std::path::Path) -> Arc<PageCache> {
        let storage_root = data_dir.join("storage");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = LocalBackend::new(&storage_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);
        VolumeManifest::new(
            "vol1".into(),
            4 * (1u64 << 20),
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
            data_out: Vec::new(),
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
    async fn request_sense_fixed_format_returns_18_bytes_no_sense() {
        // DESC=0, alloc=18.
        let cdb = [0x03u8, 0x00, 0x00, 0x00, 18, 0];
        let r = request_sense(&req(&cdb), None);
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 18);
        assert_eq!(r.data_in[0], 0x70); // fixed format, current
        assert_eq!(r.data_in[2] & 0x0F, 0x00); // sense key NoSense
        assert_eq!(r.data_in[7], 10); // additional sense length
    }

    #[tokio::test]
    async fn request_sense_descriptor_format_returns_8_bytes() {
        let cdb = [0x03u8, 0x01, 0x00, 0x00, 8, 0];
        let r = request_sense(&req(&cdb), None);
        assert!(r.sense.is_none());
        assert_eq!(r.data_in.len(), 8);
        assert_eq!(r.data_in[0], 0x72); // descriptor format
        assert_eq!(r.data_in[1] & 0x0F, 0x00); // NoSense
    }

    #[tokio::test]
    async fn request_sense_truncates_to_alloc_length() {
        let cdb = [0x03u8, 0x00, 0x00, 0x00, 4, 0];
        let r = request_sense(&req(&cdb), None);
        assert_eq!(r.data_in.len(), 4);
    }

    #[tokio::test]
    async fn request_sense_succeeds_on_absent_lun() {
        // SPC-4: REQUEST SENSE must succeed against unmapped LUNs
        // (used during initiator probing).
        let cdb = [0x03u8, 0x00, 0x00, 0x00, 18, 0];
        let r = request_sense(&req(&cdb), None);
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn start_stop_unit_accepts_any_byte4_combination() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        // PowerCondition=0x02 (Idle) + LOEJ=1 + START=0
        let cdb = [0x1Bu8, 0x00, 0x00, 0x00, 0x22, 0x00];
        let r = start_stop_unit(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        assert!(r.data_in.is_empty());
    }

    #[tokio::test]
    async fn start_stop_unit_unmapped_lun_check_condition() {
        let cdb = [0x1Bu8, 0x00, 0x00, 0x00, 0x01, 0x00];
        let r = start_stop_unit(&req(&cdb), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn prevent_allow_medium_removal_accepts_any_prevent_bits() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x1Eu8, 0x00, 0x00, 0x00, 0x03, 0x00];
        let r = prevent_allow_medium_removal(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
    }

    #[tokio::test]
    async fn prevent_allow_medium_removal_unmapped_lun_check_condition() {
        let cdb = [0x1Eu8, 0x00, 0x00, 0x00, 0x01, 0x00];
        let r = prevent_allow_medium_removal(&req(&cdb), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }

    #[tokio::test]
    async fn log_sense_page_zero_lists_only_page_zero() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        // Page 0x00, alloc 64.
        let cdb = [0x4Du8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00];
        let r = log_sense(&req(&cdb), Some(cache.as_ref()));
        assert!(r.sense.is_none());
        // 4-byte header + 1 entry (0x00) = 5 bytes.
        assert_eq!(r.data_in.len(), 5);
        assert_eq!(r.data_in[0], 0x00); // PAGE CODE
        assert_eq!(r.data_in[1], 0x00); // SUBPAGE CODE
        let body_len = u16::from_be_bytes([r.data_in[2], r.data_in[3]]);
        assert_eq!(body_len, 1);
        assert_eq!(r.data_in[4], 0x00);
    }

    #[tokio::test]
    async fn log_sense_unsupported_page_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x4Du8, 0x00, 0x0D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00];
        let r = log_sense(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn log_sense_subpage_nonzero_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x4Du8, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00];
        let r = log_sense(&req(&cdb), Some(cache.as_ref()));
        assert_eq!(r.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn log_sense_unmapped_lun_check_condition() {
        let cdb = [0x4Du8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00];
        let r = log_sense(&req(&cdb), None);
        assert_eq!(r.sense, Some(SenseData::LU_NOT_SUPPORTED));
    }
}
