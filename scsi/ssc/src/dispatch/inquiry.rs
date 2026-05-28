// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Drive-LUN INQUIRY handling — standard INQUIRY plus per-VPD-page
//! response builders. The eight VPD pages we advertise on a tape
//! drive (0x00, 0x80, 0x83, 0xB0, 0xB1, 0xB2, 0xB3, 0xC0) each have
//! their own private helper in this module; [`handle_inquiry`] is a
//! flat dispatcher that parses the CDB header and forwards to the
//! right builder.

use anyhow::Result;

use super::types::{ScsiCtx, ScsiResp, ScsiStatus, drive_mfg_serial_fallback, limit_len};

use scsi_spc::inquiry::{
    Identity, InquiryFlags, PeripheralQualifier, PeripheralType, build_inquiry_std,
};
use scsi_spc::vpd::{
    Association, CodeSet, DesignatorType, TpgsField, build_device_identification,
    build_extended_inquiry, build_supported_vpd_pages, build_unit_serial_number, push_designator,
};
use shared_iscsi::alua::AluaTopology;

/// Wire-shape flags for the drive-LUN standard INQUIRY: SPC-3 (the
/// version SSC-4 / SMC-3 reference; real LTO drives advertise this),
/// HISUP=1, CMDQUE=0 (per-drive serialization in the daemon),
/// TPGS=01b (implicit-only ALUA — REPORT TARGET PORT GROUPS honored
/// since #43).
const DRIVE_INQUIRY_FLAGS: InquiryFlags = InquiryFlags {
    spc_version: 0x05,
    hisup: true,
    tpgs: TpgsField::Implicit,
    cmdque: false,
};

pub fn handle_inquiry(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let device_type = ctx.device_type;
    let drive_manager = ctx.drive_manager;
    let facade = ctx.facade;

    let evpd = (cdb[1] & 0x01) != 0;
    let page_code = cdb[2];
    let alloc = u16::from_be_bytes([cdb[3], cdb[4]]) as u32;
    tracing::debug!(
        "INQUIRY (drive LUN {}): EVPD={} page_code=0x{:02x} allocation_length={}",
        lun,
        evpd,
        page_code,
        alloc
    );

    if !evpd {
        debug_assert_eq!(
            device_type, 0x01,
            "drive-LUN standard INQUIRY runs only for sequential-access LUNs"
        );
        let d = build_standard_inquiry(facade);
        return Ok(ScsiResp {
            status: ScsiStatus::Good,
            data_out: limit_len(d, alloc),
            sense: None,
        });
    }

    let data_out = match page_code {
        0x00 => build_supported_pages(),
        0x80 => build_unit_serial(facade, lun, drive_id),
        0x83 => build_device_id(facade, lun, ctx.session_partition, ctx.alua),
        0x86 => build_extended_inquiry_drive(),
        0xB0 => build_seq_access_chars(drive_manager, drive_id, device_type),
        0xB1 => build_mfg_serial(facade, lun, drive_id, device_type),
        0xB2 => build_tapealert_supported(device_type),
        0xB3 => build_auto_serial(facade, device_type),
        0xC0 => build_firmware_info(facade, device_type),
        _ => {
            // VPD 0xB4 (Data Transfer Device Element Address) is
            // thurvtl-specific — the wrapper handles it before
            // delegating here. Any other VPD is unsupported.
            tracing::debug!(
                "INQUIRY VPD 0x{:02x} not supported by shared drive dispatcher (LUN {})",
                page_code,
                lun,
            );
            return Ok(ScsiResp::check_condition());
        }
    };

    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(data_out, alloc),
        sense: None,
    })
}

/// Standard INQUIRY — generic LTO tape drive. Both LTO generation and
/// firmware revision come from the facade so the response tracks
/// `library init --lto-generation N --firmware CODE`. Vendor string
/// from shared_naming; "Ultrium N-SCSI" product is the LTO Consortium
/// spec-defined family naming (not vendor-branded).
fn build_standard_inquiry(facade: &dyn core_mediachanger::TapeDeviceFacade) -> Vec<u8> {
    let lto_gen = facade.lto_generation();
    let revision = facade.drive_firmware();
    let product_id = format!("Ultrium {}-SCSI", lto_gen);
    let d = build_inquiry_std(
        PeripheralQualifier::Connected,
        PeripheralType::SequentialAccess,
        true, // RMB: tape cartridges are removable
        Identity {
            vendor: shared_naming::VENDOR_INQUIRY,
            product: &product_id,
            revision: &revision,
        },
        DRIVE_INQUIRY_FLAGS,
    );
    tracing::debug!(
        "INQUIRY standard response: {} bytes ({} Ultrium {} rev {})",
        d.len(),
        shared_naming::VENDOR_INQUIRY,
        lto_gen,
        revision,
    );
    d
}

/// Supported VPD pages for a tape drive. Mirrors the historical
/// thurvtl list (a real LTO drive advertises the automation-related
/// pages 0xB3/0xB4 even on standalone topologies). VPD 0xB4 is
/// thurvtl-specific (element address); the shared dispatcher returns
/// CHECK CONDITION for it and the wrapper in thurvtl handles it
/// before delegating here.
fn build_supported_pages() -> Vec<u8> {
    let d = build_supported_vpd_pages(
        PeripheralQualifier::Connected,
        PeripheralType::SequentialAccess,
        &[0x80, 0x83, 0x86, 0xB0, 0xB1, 0xB2, 0xB3, 0xB4],
    );
    tracing::debug!("INQUIRY VPD 0x00 response: {} bytes", d.len());
    d
}

/// VPD 0x86 — Extended INQUIRY Data (SPC-4 §7.7.10). All capability
/// bits cleared — the actual TPGS field initiators read lives in
/// INQUIRY standard data byte 5 (`DRIVE_INQUIRY_FLAGS.tpgs`); this
/// page exists so VPD-page enumeration sees a contiguous list.
fn build_extended_inquiry_drive() -> Vec<u8> {
    let d = build_extended_inquiry(
        PeripheralQualifier::Connected,
        PeripheralType::SequentialAccess,
    );
    tracing::debug!("INQUIRY VPD 0x86 response: {} bytes", d.len());
    d
}

/// Unit Serial Number — per-drive manufacturer serial, kept stable
/// across reboots (persisted in inventory.json on thurvtl). Falls
/// back to a synthetic literal for pre-field deployments without the
/// field.
fn build_unit_serial(
    facade: &dyn core_mediachanger::TapeDeviceFacade,
    lun: u8,
    drive_id: usize,
) -> Vec<u8> {
    let serial = facade
        .drive_mfg_serial(drive_id as u32)
        .unwrap_or_else(|| format!("THUR-DRV-{:03}", lun));
    let d = build_unit_serial_number(
        PeripheralQualifier::Connected,
        PeripheralType::SequentialAccess,
        &serial,
        serial.len(),
    );
    tracing::debug!(
        "INQUIRY VPD 0x80 response: {} bytes (serial={:?})",
        d.len(),
        serial
    );
    d
}

/// Device Identification (SPC-4 §7.8.6). Three descriptors:
///   1. NAA-binary (Type 3, Locally Assigned) for stable multipath
///      identity, derived from BLAKE3(chassis || lun || partition).
///   2. T10 vendor-ID-based (Type 1).
///   3. Logical Unit Group (Type 6) — backup software auto-correlates
///      LUNs to the same logical library across partitions.
fn build_device_id(
    facade: &dyn core_mediachanger::TapeDeviceFacade,
    lun: u8,
    session_partition: Option<&str>,
    alua: Option<&AluaTopology>,
) -> Vec<u8> {
    let chassis_ser = facade.chassis_serial();
    let lug_id = scsi_spc::naa::logical_unit_group(&chassis_ser, session_partition);
    let naa = scsi_spc::naa::naa3_locally_assigned(&chassis_ser, lun, session_partition);

    let mut vendor_id = [b' '; 8];
    let v = shared_naming::VENDOR_INQUIRY.as_bytes();
    vendor_id[..v.len()].copy_from_slice(v);
    let mut product_id = format!("DRV-{:03}", lun).into_bytes();
    product_id.resize(16, b' ');
    let mut t10_value = Vec::with_capacity(vendor_id.len() + product_id.len());
    t10_value.extend_from_slice(&vendor_id);
    t10_value.extend_from_slice(&product_id);

    let mut descriptors = Vec::new();
    push_designator(
        &mut descriptors,
        CodeSet::Binary,
        Association::LogicalUnit,
        DesignatorType::Naa,
        &naa,
    );
    push_designator(
        &mut descriptors,
        CodeSet::Ascii,
        Association::LogicalUnit,
        DesignatorType::T10VendorId,
        &t10_value,
    );
    push_designator(
        &mut descriptors,
        CodeSet::Binary,
        Association::TargetDevice,
        DesignatorType::LogicalUnitGroup,
        &lug_id,
    );
    // ALUA target-port descriptors — one (NAA + RelativeTargetPort +
    // TargetPortGroup) trio per advertised iSCSI portal. Absent for
    // non-iSCSI / synthetic call sites (`ctx.alua == None`).
    if let Some(topo) = alua {
        topo.push_vpd83_target_port_descriptors(&mut descriptors);
    }
    let d = build_device_identification(
        PeripheralQualifier::Connected,
        PeripheralType::SequentialAccess,
        &descriptors,
    );

    tracing::debug!(
        "INQUIRY VPD 0x83 response: {} bytes (NAA + T10 + LUG{})",
        d.len(),
        if alua.is_some() { " + TP" } else { "" }
    );
    d
}

/// Sequential-Access Device Characteristics (SSC-5 §8.5.4). Carries
/// the WORMM bit (byte 4 bit 7) reflecting whether the loaded
/// cartridge is WORM, plus block-size hints (left at vendor default).
fn build_seq_access_chars(
    drive_manager: &crate::drive_manager::DriveManager,
    drive_id: usize,
    device_type: u8,
) -> Vec<u8> {
    let worm = drive_manager
        .is_loaded_cartridge_worm(drive_id)
        .unwrap_or(false);
    let mut d = vec![0u8; 60];
    d[0] = device_type;
    d[1] = 0xB0;
    d[2] = 0x00;
    d[3] = 0x3C; // Page length = 56
    d[4] = if worm { 0x80 } else { 0x00 };
    tracing::debug!(
        "INQUIRY VPD 0xB0 response: WORMM={}, {} bytes",
        worm,
        d.len()
    );
    d
}

/// Manufacturer-Assigned Serial Number VPD page. Real LTO drives emit
/// a fixed-at-production serial here, distinct from the per-unit
/// serial on VPD 0x80. The same string is also reported by LOG SENSE
/// page 0x14 parameter 0x0040.
fn build_mfg_serial(
    facade: &dyn core_mediachanger::TapeDeviceFacade,
    lun: u8,
    drive_id: usize,
    device_type: u8,
) -> Vec<u8> {
    let serial = facade
        .drive_mfg_serial(drive_id as u32)
        .unwrap_or_else(|| drive_mfg_serial_fallback(lun));
    let mut payload = vec![b' '; 32];
    let len = serial.len().min(payload.len());
    payload[..len].copy_from_slice(&serial.as_bytes()[..len]);
    let mut d = vec![0u8; 4 + payload.len()];
    d[0] = device_type;
    d[1] = 0xB1;
    d[2] = 0x00;
    d[3] = payload.len() as u8;
    d[4..].copy_from_slice(&payload);
    tracing::debug!(
        "INQUIRY VPD 0xB1 response: {} bytes (serial={:?})",
        d.len(),
        serial
    );
    d
}

/// TapeAlert Supported Flags VPD page. 8-byte bitmap covering flags
/// 1..=64. We advertise all 64 flags so the bitmap matches what LOG
/// SENSE 0x2E exposes.
fn build_tapealert_supported(device_type: u8) -> Vec<u8> {
    let mut d = vec![0u8; 4 + 8];
    d[0] = device_type;
    d[1] = 0xB2;
    d[2] = 0x00;
    d[3] = 0x08;
    for byte in d[4..12].iter_mut() {
        *byte = 0xFF;
    }
    tracing::debug!(
        "INQUIRY VPD 0xB2 response: {} bytes (TapeAlert flags 1..=64 advertised)",
        d.len()
    );
    d
}

/// Automation Device Serial Number VPD page (SSC-5 §8.5.6) — chassis
/// (automation device) serial without the `_LLNN` partition suffix
/// that VPD 0x80 carries on the changer LUN.
fn build_auto_serial(facade: &dyn core_mediachanger::TapeDeviceFacade, device_type: u8) -> Vec<u8> {
    let serial = facade.chassis_serial();
    let mut payload = vec![b' '; 32];
    let len = serial.len().min(payload.len());
    payload[..len].copy_from_slice(&serial.as_bytes()[..len]);
    let mut d = vec![0u8; 4 + payload.len()];
    d[0] = device_type;
    d[1] = 0xB3;
    d[2] = 0x00;
    d[3] = payload.len() as u8;
    d[4..].copy_from_slice(&payload);
    tracing::debug!(
        "INQUIRY VPD 0xB3 response: {} bytes (serial={:?})",
        d.len(),
        serial
    );
    d
}

/// Firmware Build Information. The on-wire format isn't fully
/// documented (image-only tables); emit an ASCII identity string
/// padded to a fixed 64-byte build descriptor.
fn build_firmware_info(
    facade: &dyn core_mediachanger::TapeDeviceFacade,
    device_type: u8,
) -> Vec<u8> {
    let fw = facade.drive_firmware();
    let body = format!("thurvtl {}", fw);
    let mut payload = body.into_bytes();
    payload.resize(64, b' ');
    let page_len = payload.len();
    let mut d = vec![0u8; 4 + page_len];
    d[0] = device_type;
    d[1] = 0xC0;
    d[2] = ((page_len >> 8) & 0xFF) as u8;
    d[3] = (page_len & 0xFF) as u8;
    d[4..].copy_from_slice(&payload);
    d
}
