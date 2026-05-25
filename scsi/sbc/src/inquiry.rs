// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! INQUIRY (opcode 0x12) — standard data + VPD pages 0x00 / 0x80 /
//! 0x83 / 0x8F / 0xB0 / 0xB2.
//!
//! INQUIRY must always succeed against any LUN — that's how the
//! initiator discovers which LUNs are present. For LUNs not in the
//! registry we return the SPC-4 "no LUN" pattern: peripheral
//! qualifier 0b011 + device type 0x1F. For registered LUNs we
//! advertise direct-access block device (type 0x00) with vendor /
//! product identity sourced from [`shared_naming`] (today
//! `VENDOR_INQUIRY` = `MB`, `DISK_PRODUCT` = `THUR VSA`) and a serial
//! number derived from the volume UUID.
//!
//! VPD 0xB0 (Block Limits) advertises MAXIMUM COMPARE AND WRITE
//! LENGTH (= sectors-per-page, so VAAI ATS knows CAW is supported
//! up to that grain) and the UNMAP capability fields (MAXIMUM UNMAP
//! LBA COUNT, descriptor count, optimal granularity = page).
//! Without 0xB0, ESXi VAAI won't issue CAW even though the
//! dispatcher arm is wired.
//!
//! VPD 0xB2 (Logical Block Provisioning) sets LBPU=1 so Linux
//! `discard` / `fstrim` knows UNMAP is honored and LBPRZ=001 so
//! initiators can infer that unmapped LBAs read as zeros (matches
//! our sparse-hole behavior).

use core_block::PageCache;
use scsi_spc::inquiry::{
    Identity, InquiryFlags, PeripheralQualifier, PeripheralType, build_inquiry_std,
    write_padded_ascii,
};
use scsi_spc::vpd::{
    Association, CodeSet, DesignatorType, build_device_identification, build_supported_vpd_pages,
    build_unit_serial_number, finalize_vpd, push_designator, vpd_header,
};
use shared_naming::{DISK_PRODUCT, VENDOR_INQUIRY};

use super::types::{ScsiRequest, ScsiResponse, SenseData};

/// thurvsa daemon revision string surfaced in the standard INQUIRY
/// response. Hardcoded for the skeleton; bump when the SBC-3
/// surface evolves enough that hosts may want to gate on it.
const REVISION: &str = "0001";

/// Wire-shape flags for VSA's standard INQUIRY response: SPC-4,
/// HISUP=1 (we honor SAM-5 LUN structure), CMDQUE=1 (queueable —
/// SBC-3 page cache + sector-level locks support tag queueing).
const VSA_INQUIRY_FLAGS: InquiryFlags = InquiryFlags {
    spc_version: 0x06,
    hisup: true,
    cmdque: true,
};

/// SPC-4 §7.7 VPD header peripheral qualifier + type pair for the
/// two LUN states this dispatcher emits.
fn pq_pt(lun_present: bool) -> (PeripheralQualifier, PeripheralType) {
    if lun_present {
        (PeripheralQualifier::Connected, PeripheralType::DirectAccess)
    } else {
        (PeripheralQualifier::NoDevice, PeripheralType::NoLun)
    }
}

pub(super) fn dispatch(req: &ScsiRequest<'_>, cache: Option<&PageCache>) -> ScsiResponse {
    if req.cdb.len() < 6 {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    }
    let evpd = req.cdb[1] & 0x01 != 0;
    let page_code = req.cdb[2];
    let alloc_len = u16::from_be_bytes([req.cdb[3], req.cdb[4]]) as usize;

    let bytes = if !evpd {
        if page_code != 0 {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        }
        standard_inquiry(cache)
    } else {
        match page_code {
            0x00 => vpd_supported_pages(cache.is_some()),
            0x80 => vpd_unit_serial(cache),
            0x83 => vpd_device_id(cache),
            0x8F => vpd_third_party_copy(cache),
            0xB0 => vpd_block_limits(cache),
            0xB2 => vpd_logical_block_provisioning(cache),
            _ => return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
        }
    };

    let truncated: Vec<u8> = bytes.into_iter().take(alloc_len).collect();
    ScsiResponse::good(truncated)
}

fn standard_inquiry(cache: Option<&PageCache>) -> Vec<u8> {
    let (pq, pt) = pq_pt(cache.is_some());
    build_inquiry_std(
        pq,
        pt,
        false, // not removable
        Identity {
            vendor: VENDOR_INQUIRY,
            product: DISK_PRODUCT,
            revision: REVISION,
        },
        VSA_INQUIRY_FLAGS,
    )
}

/// VPD 0x00 — Supported VPD Pages. Returns the list of pages we
/// implement, in ascending order. For LUNs that aren't registered
/// we still return 0x00 itself (initiators expect it) but no
/// further pages. Framing comes from
/// [`scsi_spc::vpd::build_supported_vpd_pages`] (sorts + dedups,
/// auto-includes page 0x00).
fn vpd_supported_pages(lun_present: bool) -> Vec<u8> {
    let (pq, pt) = pq_pt(lun_present);
    let pages: &[u8] = if lun_present {
        &[0x80, 0x83, 0x8F, 0xB0, 0xB2]
    } else {
        &[]
    };
    build_supported_vpd_pages(pq, pt, pages)
}

/// VPD 0x80 — Unit Serial Number. ASCII serial = hex-encoded
/// volume UUID (32 chars). Empty page for absent LUNs.
fn vpd_unit_serial(cache: Option<&PageCache>) -> Vec<u8> {
    let (pq, pt) = pq_pt(cache.is_some());
    let serial = match cache {
        Some(w) => hex::encode(w.manifest().uuid),
        None => String::new(),
    };
    build_unit_serial_number(pq, pt, &serial, serial.len())
}

/// VPD 0x83 — Device Identification. Two descriptors per LUN:
///
/// 1. T10 vendor ID (designator type 0x01) carrying
///    `"MB      MBD_<uuid_hex>"`. Initiators that gate on T10
///    designators (most Linux SCSI mid-layer code, fio probes)
///    consume this directly.
/// 2. NAA Locally Assigned (designator type 0x03, NAA type 0x3) —
///    an 8-byte binary identifier derived from the first 64 bits
///    of the volume UUID with the top nibble forced to 0x3. The
///    SPC-3 EXTENDED COPY identification target descriptor only
///    has room for a 20-byte designator, so VAAI XCOPY references
///    LUNs by NAA. Without this entry, ESXi can't address VSA
///    volumes as XCOPY targets even with VPD 0x8F advertised.
fn vpd_device_id(cache: Option<&PageCache>) -> Vec<u8> {
    let (pq, pt) = pq_pt(cache.is_some());
    let mut descriptors = Vec::new();
    if let Some(w) = cache {
        // Designator data: 8-byte vendor ID (space-padded) followed by
        // vendor-specific identifier "MBD_<uuid_hex>" (39 bytes).
        let mut designator = vec![b' '; 8];
        write_padded_ascii(&mut designator, VENDOR_INQUIRY);
        let suffix = format!("MBD_{}", hex::encode(w.manifest().uuid));
        designator.extend_from_slice(suffix.as_bytes());
        push_designator(
            &mut descriptors,
            CodeSet::Ascii,
            Association::LogicalUnit,
            DesignatorType::T10VendorId,
            &designator,
        );
        // NAA Locally Assigned. 8 bytes: top nibble = NAA type 0x3,
        // remaining 60 bits derived from the volume UUID's first
        // 8 bytes. Stable across daemon restarts because the UUID
        // is creation-frozen in the manifest.
        let naa = naa_locally_assigned(&w.manifest().uuid);
        push_designator(
            &mut descriptors,
            CodeSet::Binary,
            Association::LogicalUnit,
            DesignatorType::Naa,
            &naa,
        );
    }
    build_device_identification(pq, pt, &descriptors)
}

/// Derive the 8-byte NAA Locally Assigned identifier from a volume
/// UUID. Top nibble forced to NAA type 0x3 (locally assigned),
/// remaining 60 bits taken from the UUID's first 8 bytes. Stable
/// across daemon restarts because the UUID is creation-frozen.
///
/// `pub(crate)` so [`crate::data_path`]'s XCOPY descriptor matcher
/// can rebuild the same identifier when resolving an incoming
/// target descriptor against the registered LUN set.
pub(crate) fn naa_locally_assigned(uuid: &[u8; 16]) -> [u8; 8] {
    let mut naa = [0u8; 8];
    naa.copy_from_slice(&uuid[..8]);
    // NAA type = 0x3 in bits 63-60 (top nibble of byte 0).
    naa[0] = 0x30 | (naa[0] & 0x0F);
    naa
}

/// VPD 0x8F — Third Party Copy (SPC-4 §7.7.18). Advertises the
/// SPC-3 EXTENDED COPY (LID1) surface VAAI / VAAI-like initiators
/// gate on before issuing offloaded copy. The page is a wrapper
/// around a sequence of typed sub-descriptors; we publish four:
///
///   0x0001 SUPPORTED COMMANDS — declares opcode 0x83 (EXTENDED
///          COPY) with service action 0x00 (LID1) and opcode 0x84
///          (RECEIVE COPY RESULTS) with service actions 0x00
///          (COPY STATUS) and 0x03 (OPERATING PARAMETERS). Without
///          this, ESXi won't try the offload at all.
///   0x0004 PARAMETER DATA — per-XCOPY limits matching what
///          RECEIVE COPY RESULTS / OPERATING PARAMETERS reports:
///          max target descriptors = 2, max segment descriptors = 1,
///          max descriptor list length = 128, max inline = 0.
///   0x0008 SUPPORTED DESCRIPTORS — the descriptor type codes we
///          accept (target 0xE4, segment 0x02).
///   0x8001 GENERAL COPY OPERATIONS — total bytes per XCOPY cap
///          (16 MiB), concurrent-copy hints (1).
///
/// SPC-4 layout: each descriptor is a 2-byte descriptor type code
/// followed by a 2-byte length and that many body bytes.
fn vpd_third_party_copy(cache: Option<&PageCache>) -> Vec<u8> {
    let (pq, pt) = pq_pt(cache.is_some());
    let mut page = vpd_header(pq, pt, 0x8F, 0);
    let Some(cache) = cache else {
        finalize_vpd(&mut page);
        return page;
    };
    // SPC-4 §7.7.18 requires a 4-byte reserved block at the start
    // of the body (offsets 4-7 inclusive in the page-relative
    // numbering). After that the descriptor list begins.
    page.extend_from_slice(&[0u8; 4]);

    // 0x0001 SUPPORTED COMMANDS (per SPC-4 §7.7.18.2). One entry
    // per (opcode, service action) pair we honor. Body layout:
    //   byte 0     COMMANDS SUPPORTED LIST LENGTH
    //   per entry (variable):
    //     byte 0   OPERATION CODE
    //     bytes 1-2  SERVICE ACTION (BE16) — 0xFFFF when no SA
    //     byte 3   reserved
    //     bytes 4-7 CDB SIZES (4 bytes; we publish only the most
    //              common SA size and reserve the rest as 0)
    //
    // Initiators only inspect the (opcode, SA) presence; the
    // body layout details vary across vendors. We publish a
    // compact form (one byte length, then a 4-byte (op, SA, SA,
    // reserved) shape) that LIO interoperates with.
    let mut commands: Vec<u8> = Vec::new();
    // (0x83, 0x00) — EXTENDED COPY (LID1)
    commands.extend_from_slice(&[0x83, 0x00, 0x00, 0x00]);
    // (0x84, 0x00) — RECEIVE COPY RESULTS / COPY STATUS
    commands.extend_from_slice(&[0x84, 0x00, 0x00, 0x00]);
    // (0x84, 0x03) — RECEIVE COPY RESULTS / OPERATING PARAMETERS
    commands.extend_from_slice(&[0x84, 0x00, 0x03, 0x00]);
    push_tpc_descriptor(&mut page, 0x0001, &{
        let mut body = Vec::with_capacity(1 + commands.len());
        body.push(commands.len() as u8);
        body.extend_from_slice(&commands);
        body
    });

    // 0x0004 PARAMETER DATA (SPC-4 §7.7.18.6).
    //   bytes 0-1   MAXIMUM CSCD DESCRIPTOR COUNT
    //   bytes 2-3   MAXIMUM SEGMENT DESCRIPTOR COUNT
    //   bytes 4-7   MAXIMUM DESCRIPTOR LIST LENGTH
    //   bytes 8-11  MAXIMUM INLINE DATA LENGTH
    let mut param_body = vec![0u8; 12];
    param_body[0..2].copy_from_slice(&2u16.to_be_bytes());
    param_body[2..4].copy_from_slice(&1u16.to_be_bytes());
    param_body[4..8].copy_from_slice(&128u32.to_be_bytes());
    // inline data length stays 0 — inline data unsupported.
    push_tpc_descriptor(&mut page, 0x0004, &param_body);

    // 0x0008 SUPPORTED DESCRIPTORS (SPC-4 §7.7.18.9).
    //   byte 0      SUPPORTED DESCRIPTOR IDS LIST LENGTH
    //   bytes 1..   descriptor type codes (one byte each)
    let supported_descs = [0xE4u8, 0x02u8];
    let mut sd_body = Vec::with_capacity(1 + supported_descs.len());
    sd_body.push(supported_descs.len() as u8);
    sd_body.extend_from_slice(&supported_descs);
    push_tpc_descriptor(&mut page, 0x0008, &sd_body);

    // 0x8001 GENERAL COPY OPERATIONS (SPC-4 §7.7.18.13).
    //   bytes 0-3   TOTAL CONCURRENT COPIES
    //   bytes 4-7   MAXIMUM IDENTIFIED CONCURRENT COPIES
    //   bytes 8-11  MAXIMUM SEGMENT LENGTH (bytes)
    //   byte 12     DATA SEGMENT GRANULARITY (log2(bytes))
    //   byte 13     INLINE DATA GRANULARITY (log2(bytes))
    //   bytes 14-15 reserved
    let mut gen_body = vec![0u8; 16];
    gen_body[0..4].copy_from_slice(&1u32.to_be_bytes());
    gen_body[4..8].copy_from_slice(&1u32.to_be_bytes());
    gen_body[8..12].copy_from_slice(&(16u32 << 20).to_be_bytes()); // 16 MiB
    let page_log2 = u64::from(cache.manifest().page_size_bytes).trailing_zeros() as u8;
    gen_body[12] = page_log2;
    push_tpc_descriptor(&mut page, 0x8001, &gen_body);

    finalize_vpd(&mut page);
    page
}

/// Append one VPD 0x8F sub-descriptor: 4-byte header (2-byte type
/// code + 2-byte length) followed by `body`.
fn push_tpc_descriptor(page: &mut Vec<u8>, type_code: u16, body: &[u8]) {
    page.extend_from_slice(&type_code.to_be_bytes());
    page.extend_from_slice(&(body.len() as u16).to_be_bytes());
    page.extend_from_slice(body);
}

/// VPD 0xB0 — Block Limits (SBC-3 §6.6.4). 64-byte response
/// advertising the per-volume thin-provisioning + atomic-CAW
/// capability surface. Without this page, ESXi VAAI ATS won't
/// issue CAW and Linux SCSI mid-layer won't issue UNMAP, even
/// though both arms are wired in the dispatcher. For absent LUNs
/// we still return a structurally-valid header — initiators don't
/// query 0xB0 against an absent LUN, but the wire surface should
/// match VPD 0x80 / 0x83's "empty body" pattern.
fn vpd_block_limits(cache: Option<&PageCache>) -> Vec<u8> {
    let (pq, pt) = pq_pt(cache.is_some());
    // SBC-3 mandates exactly 60 bytes of body (page length 60, n=63
    // with the spec's 0-indexed `n - 3` formula). Always emit the full
    // 64-byte page even when the LUN is absent, so the wire surface
    // stays structurally valid.
    let mut page = vpd_header(pq, pt, 0xB0, 60);
    page.resize(64, 0);
    page[2..4].copy_from_slice(&60u16.to_be_bytes());

    let Some(cache) = cache else {
        return page;
    };
    let m = cache.manifest();
    let sector = u64::from(m.sector_bytes);
    let sectors_per_page = u64::from(m.page_size_bytes) / sector;
    // The CAW length field is 1 byte (max 255). Our page-aligned
    // constraint forces multiples of `sectors_per_page`; advertise
    // exactly one page so VAAI sees support without us promising a
    // larger atomic window than we honor today.
    let max_caw = sectors_per_page.min(255) as u8;
    // Optimal transfer granularity = one page. Initiators that
    // honor this self-align to our constraint.
    let optimal_granularity = sectors_per_page.min(u64::from(u16::MAX)) as u16;
    // UNMAP descriptor cap = max number of 16-byte descriptors that
    // fit in a 16-bit parameter list length: floor((65535-8)/16) =
    // 4095. Use that as the truthful limit.
    let max_unmap_descriptors: u32 = 4095;
    // No software cap on the total LBA count an UNMAP can sweep.
    let max_unmap_lba_count: u32 = u32::MAX;
    let optimal_unmap_granularity = sectors_per_page.min(u64::from(u32::MAX)) as u32;

    // WSNZ (bit 0 of byte 4): set to 0 — we accept WRITE SAME with
    // either zero or non-zero data patterns. WSNZ=1 would mean
    // "device only honors zero patterns and only when LBPRZ=1",
    // which is more restrictive than what we implement.
    page[4] = 0x00;
    page[5] = max_caw;
    page[6..8].copy_from_slice(&optimal_granularity.to_be_bytes());
    // bytes 8-11 MAXIMUM TRANSFER LENGTH = 0 (no specific limit
    // beyond what the CDB encoding allows)
    // bytes 12-15 OPTIMAL TRANSFER LENGTH = 0 (no preference)
    // bytes 16-19 MAXIMUM PREFETCH LENGTH = 0 (PREFETCH not wired)
    page[20..24].copy_from_slice(&max_unmap_lba_count.to_be_bytes());
    page[24..28].copy_from_slice(&max_unmap_descriptors.to_be_bytes());
    page[28..32].copy_from_slice(&optimal_unmap_granularity.to_be_bytes());
    // UNMAP GRANULARITY ALIGNMENT (bytes 32-35): UGAVALID at bit 31
    // of byte 32, alignment LBA in bytes 32-35 low. Volumes start at
    // LBA 0 so the alignment is just 0; UGAVALID=1 advertises that
    // the alignment hint is meaningful.
    page[32] = 0x80;
    // bytes 36-43 MAXIMUM WRITE SAME LENGTH = 0 (no specific limit;
    // host-side block layer or VAAI module will chunk according to
    // its own knobs). Setting a non-zero cap here would force hosts
    // to split transfers below their own preferred granularity for
    // no clear benefit — we route through the cache, and the cache
    // already chunks pattern expansion internally.
    // bytes 44-63 reserved
    page
}

/// VPD 0xB2 — Logical Block Provisioning (SBC-3 §6.6.5). Tells
/// initiators that UNMAP is honored (LBPU=1), unmapped LBAs read
/// zeros (LBPRZ=001 — matches our sparse-hole behavior on READ),
/// and the volume is thin-provisioned (PROVISIONING TYPE=010 —
/// pages allocate on first WRITE).
fn vpd_logical_block_provisioning(cache: Option<&PageCache>) -> Vec<u8> {
    let (pq, pt) = pq_pt(cache.is_some());
    // PAGE LENGTH = 4 (header excluded) — minimum body, no
    // provisioning group descriptor (DP=0). 8-byte page total.
    let mut page = vpd_header(pq, pt, 0xB2, 4);
    page.resize(8, 0);
    page[2..4].copy_from_slice(&4u16.to_be_bytes());

    if cache.is_none() {
        return page;
    }
    page[4] = 0x00; // THRESHOLD EXPONENT = 0 (no soft-threshold)
    // byte 5: LBPU(7)=1 | LBPWS(6)=0 | LBPWS10(5)=0 | LBPRZ(4-2)=001 | ANC_SUP(1)=0 | DP(0)=0
    // LBPU=1   -> UNMAP supported
    // LBPRZ=001 -> unmapped LBAs read as zeros (we sparse-hole)
    // ANC_SUP=0 -> anchored UNMAP not supported (matches dispatcher reject)
    page[5] = 0x80 | (0b001 << 2);
    // byte 6: PROVISIONING TYPE in bits 2-0. 010 = thin
    // provisioned (sparse on first write).
    page[6] = 0b010;
    // byte 7 reserved
    page
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
        let cloud_root = data_dir.join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
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

    fn req<'a>(cdb: &'a [u8], lun: u64) -> ScsiRequest<'a> {
        ScsiRequest {
            lun,
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
    async fn standard_inquiry_for_registered_lun() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0, 0, 0x00, 0x60, 0]; // alloc 96
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));

        assert!(resp.sense.is_none());
        let d = &resp.data_in;
        // Direct-access, qualifier Connected: byte 0 = 0x00.
        assert_eq!(d[0], 0x00);
        assert_eq!(d[2], 0x06); // SPC-4
        assert_eq!(d[3], 0x12); // HISUP + format 2
        assert_eq!(d[7], 0x02); // CMDQUE=1
        assert_eq!(&d[8..16], b"MB      ");
        assert_eq!(&d[16..32], b"THUR VSA        ");
        assert_eq!(&d[32..36], b"0001");
    }

    #[tokio::test]
    async fn standard_inquiry_for_absent_lun() {
        let cdb = [0x12u8, 0, 0, 0x00, 0x60, 0];
        let resp = dispatch(&req(&cdb, 99), None);
        assert!(resp.sense.is_none()); // INQUIRY must succeed
        // SPC-4 §6.4.2 "no LUN" pattern: qualifier 0b011 + type 0x1F.
        assert_eq!(resp.data_in[0], 0x7F);
    }

    #[tokio::test]
    async fn vpd_page_zero_lists_supported_pages() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0x00, 0x00, 0x40, 0]; // EVPD + page 0
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        assert_eq!(d[1], 0x00);
        assert_eq!(d[3] as usize, d.len() - 4);
        assert!(d[4..].contains(&0x00));
        assert!(d[4..].contains(&0x80));
        assert!(d[4..].contains(&0x83));
    }

    #[tokio::test]
    async fn vpd_unit_serial_carries_volume_uuid() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0x80, 0x00, 0x60, 0];
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        assert_eq!(d[1], 0x80);
        let serial_len = d[3] as usize;
        let serial = std::str::from_utf8(&d[4..4 + serial_len]).unwrap();
        assert_eq!(serial, hex::encode(cache.manifest().uuid));
    }

    #[tokio::test]
    async fn vpd_device_id_t10_starts_with_vendor() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0x83, 0x00, 0x60, 0];
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        assert_eq!(d[1], 0x83);
        // descriptor at offset 4: code set + type, len, then designator.
        assert_eq!(d[4], 0x02); // ASCII code set
        assert_eq!(d[5], 0x01); // T10 vendor-id designator
        let designator_len = d[7] as usize;
        let designator = &d[8..8 + designator_len];
        assert_eq!(&designator[0..8], b"MB      ");
        let suffix = std::str::from_utf8(&designator[8..]).unwrap();
        assert!(suffix.starts_with("MBD_"));
    }

    #[tokio::test]
    async fn vpd_unknown_page_rejected() {
        let cdb = [0x12u8, 0x01, 0x42, 0x00, 0x60, 0];
        let resp = dispatch(&req(&cdb, 0), None);
        assert_eq!(resp.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn standard_inquiry_with_evpd_zero_and_page_set_rejected() {
        let cdb = [0x12u8, 0x00, 0x42, 0x00, 0x60, 0];
        let resp = dispatch(&req(&cdb, 0), None);
        assert_eq!(resp.sense, Some(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[tokio::test]
    async fn alloc_length_truncates_response() {
        let cdb = [0x12u8, 0, 0, 0x00, 0x10, 0]; // alloc only 16 bytes
        let resp = dispatch(&req(&cdb, 99), None);
        assert_eq!(resp.data_in.len(), 16);
    }

    #[tokio::test]
    async fn vpd_page_zero_includes_b0_and_b2() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0x00, 0x00, 0x40, 0];
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        let n = d[3] as usize;
        let pages = &d[4..4 + n];
        assert!(pages.contains(&0xB0));
        assert!(pages.contains(&0xB2));
    }

    #[tokio::test]
    async fn vpd_block_limits_advertises_caw_and_unmap() {
        // 4 MiB / 4 KiB sector / 64 KiB page ⇒ 16 sectors per page.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0xB0, 0x00, 0x40, 0]; // alloc 64
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        assert!(resp.sense.is_none());
        assert_eq!(d.len(), 64);
        assert_eq!(d[1], 0xB0);
        let page_len = u16::from_be_bytes([d[2], d[3]]);
        assert_eq!(page_len, 60);
        // MAXIMUM COMPARE AND WRITE LENGTH = sectors_per_page = 16.
        assert_eq!(d[5], 16);
        // OPTIMAL TRANSFER LENGTH GRANULARITY = 16.
        let granularity = u16::from_be_bytes([d[6], d[7]]);
        assert_eq!(granularity, 16);
        // MAXIMUM UNMAP LBA COUNT = u32::MAX.
        let max_unmap_lba = u32::from_be_bytes([d[20], d[21], d[22], d[23]]);
        assert_eq!(max_unmap_lba, u32::MAX);
        // MAXIMUM UNMAP BLOCK DESCRIPTOR COUNT = 4095.
        let max_unmap_descs = u32::from_be_bytes([d[24], d[25], d[26], d[27]]);
        assert_eq!(max_unmap_descs, 4095);
        // OPTIMAL UNMAP GRANULARITY = 16.
        let optimal_unmap = u32::from_be_bytes([d[28], d[29], d[30], d[31]]);
        assert_eq!(optimal_unmap, 16);
        // UGAVALID=1 with alignment LBA = 0.
        assert_eq!(d[32], 0x80);
    }

    #[tokio::test]
    async fn vpd_logical_block_provisioning_advertises_unmap_and_zeros() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0xB2, 0x00, 0x40, 0];
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        assert!(resp.sense.is_none());
        assert_eq!(d.len(), 8);
        assert_eq!(d[1], 0xB2);
        let page_len = u16::from_be_bytes([d[2], d[3]]);
        assert_eq!(page_len, 4);
        // byte 5: LBPU=1, LBPWS=0, LBPWS10=0, LBPRZ=001, ANC_SUP=0, DP=0
        assert_eq!(d[5] & 0x80, 0x80, "LBPU bit"); // LBPU
        assert_eq!(d[5] & 0x40, 0x00, "LBPWS bit");
        assert_eq!((d[5] >> 2) & 0x07, 0b001, "LBPRZ field"); // unmapped reads zero
        assert_eq!(d[5] & 0x02, 0x00, "ANC_SUP bit");
        assert_eq!(d[5] & 0x01, 0x00, "DP bit");
        // byte 6 PROVISIONING TYPE = 010 (thin).
        assert_eq!(d[6] & 0x07, 0b010);
    }

    #[tokio::test]
    async fn vpd_page_zero_includes_8f_for_third_party_copy() {
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0x00, 0x00, 0x40, 0];
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        let n = d[3] as usize;
        let pages = &d[4..4 + n];
        assert!(pages.contains(&0x8F));
    }

    #[tokio::test]
    async fn vpd_third_party_copy_publishes_required_descriptors() {
        // ESXi gates VAAI XCOPY on this page. The descriptor sequence
        // must include 0x0001 SUPPORTED COMMANDS (with opcodes 0x83
        // and 0x84), 0x0004 PARAMETER DATA, 0x0008 SUPPORTED
        // DESCRIPTORS (with type codes 0xE4 and 0x02), and 0x8001
        // GENERAL COPY OPERATIONS.
        let tmp = TempDir::new().unwrap();
        let cache = fixture_cache(tmp.path()).await;
        let cdb = [0x12u8, 0x01, 0x8F, 0x00, 0x80, 0];
        let resp = dispatch(&req(&cdb, 0), Some(cache.as_ref()));
        let d = &resp.data_in;
        assert!(resp.sense.is_none(), "{:?}", resp.sense);
        assert_eq!(d[1], 0x8F);
        let page_len = u16::from_be_bytes([d[2], d[3]]) as usize;
        assert_eq!(d.len(), 4 + page_len);
        // Skip the reserved 4 bytes after the header.
        let mut cur = 4 + 4;
        let mut found_codes = std::collections::BTreeSet::new();
        let mut commands_body: Option<Vec<u8>> = None;
        let mut supported_descs_body: Option<Vec<u8>> = None;
        while cur + 4 <= d.len() {
            let type_code = u16::from_be_bytes([d[cur], d[cur + 1]]);
            let body_len = u16::from_be_bytes([d[cur + 2], d[cur + 3]]) as usize;
            let body = &d[cur + 4..cur + 4 + body_len];
            found_codes.insert(type_code);
            if type_code == 0x0001 {
                commands_body = Some(body.to_vec());
            }
            if type_code == 0x0008 {
                supported_descs_body = Some(body.to_vec());
            }
            cur += 4 + body_len;
        }
        for code in [0x0001u16, 0x0004, 0x0008, 0x8001] {
            assert!(
                found_codes.contains(&code),
                "descriptor {code:#06x} missing"
            );
        }
        let commands = commands_body.expect("SUPPORTED COMMANDS body present");
        // First byte is list length; the list of opcodes follows in
        // 4-byte tuples (op, sa_hi, sa_lo, reserved).
        let len = commands[0] as usize;
        assert!(len >= 12);
        let entries = &commands[1..1 + len];
        let ops: Vec<u8> = entries.chunks(4).map(|e| e[0]).collect();
        assert!(ops.contains(&0x83), "EXTENDED COPY not advertised");
        assert!(ops.contains(&0x84), "RECEIVE COPY RESULTS not advertised");
        let supported = supported_descs_body.expect("SUPPORTED DESCRIPTORS body present");
        // First byte is the list length, then the type codes.
        let n = supported[0] as usize;
        let types = &supported[1..1 + n];
        assert!(types.contains(&0xE4), "identification descriptor missing");
        assert!(types.contains(&0x02), "block-to-block segment missing");
    }
}
