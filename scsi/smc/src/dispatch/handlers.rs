// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Six SMC opcode handlers lifted from `thurvtld::iscsi::protocol`.
//!
//! Each consumes `&mut SmcScsiCtx<'_>` — derefs to `scsi_ssc::ScsiCtx`
//! for shared per-command state (CDB / LUN / audit channel / event
//! broadcast tx / UA tracker / drive manager) and adds the
//! SMC-specific `library` mutex + `element_config` topology.

use anyhow::{Result, anyhow};
use core_mediachanger::{AuditResult, TapeEvent};
use scsi_ssc::dispatch::{ScsiResp, ScsiStatus, audit_append, limit_len};
use scsi_ssc::scsi::sense::{
    ASC_MEDIUM_REMOVAL_PREVENTED, AdditionalSenseCode, SenseDataBuilder, SenseKey,
};
use shared_iscsi::unit_attention;

use crate::changer;
use crate::dispatch::types::SmcScsiCtx;

pub fn handle_initialize_element_status(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let lun = ctx.lun;
    let library = ctx.library;
    let data_dir = ctx.data_dir;

    if lun != 0 {
        return Ok(ScsiResp::check_condition());
    }
    tracing::debug!("INITIALIZE ELEMENT STATUS (LUN 0)");
    match changer::handle_initialize_element_status(library, data_dir) {
        Ok(_) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("INITIALIZE ELEMENT STATUS error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_initialize_element_status_with_range(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;

    // INITIALIZE ELEMENT STATUS WITH RANGE (SMC). Like 0x07 but only
    // for a sub-range of elements (start address + count). Our virtual
    // changer's element status is always live in the Library state,
    // so the "scan" is a no-op — we just acknowledge the request.
    if lun != 0 {
        return Ok(ScsiResp::check_condition());
    }
    let start = u16::from_be_bytes([cdb[2], cdb[3]]);
    let count = u16::from_be_bytes([cdb[6], cdb[7]]);
    tracing::debug!(
        "INITIALIZE ELEMENT STATUS WITH RANGE: start={}, count={}",
        start,
        count
    );
    Ok(ScsiResp::good())
}

pub fn handle_read_element_status(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let library = ctx.library;
    let element_config = ctx.element_config;
    // READ ELEMENT STATUS is intentionally NOT partition-filtered: mtx
    // refuses our `descriptor_length = 12` baseline when a per-type
    // page comes back with zero descriptors (the "Transport Element
    // Descriptor Length too short" fatal path in `mtxl.c`). The
    // data-path is still fenced — out-of-partition drive LUNs return
    // the SPC-4 "no logical unit" sentinel on INQUIRY, and MOVE
    // MEDIUM refuses any source/dest outside the session's
    // partition. The leak is purely topological: a tenant's session
    // can see *that* other elements exist via READ ELEMENT STATUS but
    // can't address or move them.
    let session_partition: Option<&str> = None;
    let _ = ctx.session_partition;

    if lun != 0 {
        return Ok(ScsiResp::check_condition());
    }
    let alloc = u32::from_be_bytes([0, cdb[7], cdb[8], cdb[9]]);
    let element_type = cdb[1] & 0x0F;
    let voltag = (cdb[1] & 0x10) != 0;
    let start_address = u16::from_be_bytes([cdb[2], cdb[3]]);
    let num_elements = u16::from_be_bytes([cdb[4], cdb[5]]);
    // SMC-3 CDB byte 6: bit 1 = CURDATA, bit 0 = DVCID. Mixed lives at
    // bit 7 (a vendor-specific bit; SMC-3 reserves it).
    let dvcid = (cdb[6] & 0x01) != 0;
    let mixed = (cdb[6] & 0x80) != 0;

    tracing::debug!(
        "READ ELEMENT STATUS (LUN 0): type=0x{:02x} start={} count={} voltag={} dvcid={} mixed={} alloc={}",
        element_type,
        start_address,
        num_elements,
        voltag,
        dvcid,
        mixed,
        alloc
    );

    let element_type = match changer::ElementType::from_u8(element_type) {
        Some(t) => t,
        None => {
            tracing::warn!("Invalid element type: 0x{:02x}", element_type);
            return Ok(ScsiResp::check_condition());
        }
    };

    let lib = library
        .lock()
        .map_err(|_| anyhow!("library mutex poisoned"))?;
    let opts = changer::ReadElementStatusOpts {
        voltag,
        dvcid,
        mixed,
        lto_generation: lib.lto_generation(),
    };
    match changer::handle_read_element_status(
        &lib,
        element_config,
        element_type,
        start_address,
        num_elements,
        &opts,
        session_partition,
    ) {
        Ok(data) => {
            tracing::debug!("READ ELEMENT STATUS response: {} bytes", data.len());
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        Err(e) => {
            tracing::warn!("READ ELEMENT STATUS error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_move_medium(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let library = ctx.library;
    let ua_tracker = ctx.ua_tracker;
    let element_config = ctx.element_config;
    let event_tx = ctx.event_tx;
    let audit_log = ctx.audit_log;
    let audit_ratelimiter = ctx.audit_ratelimiter;
    let actor = ctx.audit_actor();
    let session_partition = ctx.session_partition;

    if lun != 0 {
        return Ok(ScsiResp::check_condition());
    }
    let transport_address = u16::from_be_bytes([cdb[2], cdb[3]]);
    let source_address = u16::from_be_bytes([cdb[4], cdb[5]]);
    let destination_address = u16::from_be_bytes([cdb[6], cdb[7]]);
    let invert = (cdb[10] & 0x01) != 0;

    tracing::info!(
        "MOVE MEDIUM (LUN 0): transport={}, src={}, dst={}, invert={}",
        transport_address,
        source_address,
        destination_address,
        invert
    );

    let source_type = element_config.element_type_from_address(source_address);
    let dest_type = element_config.element_type_from_address(destination_address);

    let mut lib = library
        .lock()
        .map_err(|_| anyhow!("library mutex poisoned"))?;

    // Partition fence. Source and destination must both belong to the
    // session's bound partition. Refused with ILLEGAL REQUEST + ASC/
    // ASCQ 0x21/0x01.
    if let Some(part_name) = session_partition {
        let in_partition = |addr: u16, kind: Option<changer::ElementType>| -> bool {
            use changer::ElementType;
            match kind {
                Some(ElementType::Storage) => element_config
                    .address_to_storage_id(addr)
                    .map(|id| lib.partition_for_storage_slot(id) == Some(part_name))
                    .unwrap_or(false),
                Some(ElementType::ImportExport) => element_config
                    .address_to_mail_id(addr)
                    .map(|id| lib.partition_for_mail_slot(id) == Some(part_name))
                    .unwrap_or(false),
                Some(ElementType::DataTransfer) => element_config
                    .address_to_drive_id(addr)
                    .map(|id| lib.partition_for_drive(id) == Some(part_name))
                    .unwrap_or(false),
                Some(ElementType::MediumTransport) => true,
                _ => false,
            }
        };
        if !in_partition(source_address, source_type)
            || !in_partition(destination_address, dest_type)
        {
            tracing::warn!(
                "partition fence: session bound to '{}' refused MOVE MEDIUM src={} dst={}",
                part_name,
                source_address,
                destination_address,
            );
            audit_append(
                audit_log,
                audit_ratelimiter,
                "iscsi.move_medium",
                actor,
                serde_json::json!({
                    "transport": transport_address,
                    "src": source_address,
                    "dst": destination_address,
                    "invert": invert,
                    "refused": "partition_fence",
                    "partition": part_name,
                }),
                AuditResult::Error("partition fence".to_string()),
            );
            let sense = SenseDataBuilder::new(
                SenseKey::IllegalRequest,
                AdditionalSenseCode {
                    asc: 0x21,
                    ascq: 0x01,
                },
            )
            .build();
            return Ok(ScsiResp::check_condition_with_sense(sense));
        }
    }

    // Capture the source-drive barcode BEFORE the move clears it,
    // so the unload event can carry the real tape id.
    let unload_source_barcode = if matches!(
        (source_type, dest_type),
        (
            Some(changer::ElementType::DataTransfer),
            Some(changer::ElementType::Storage)
        )
    ) {
        element_config
            .address_to_drive_id(source_address)
            .and_then(|drive_id| lib.get_drive(drive_id).cloned())
            .and_then(|d| d.barcode)
    } else {
        None
    };

    // Compute the audit "action" label from element types so the
    // audit entry says "load" / "unload" / "move" instead of just
    // raw element addresses.
    let action_label = match (source_type, dest_type) {
        (Some(changer::ElementType::Storage), Some(changer::ElementType::DataTransfer)) => "load",
        (Some(changer::ElementType::DataTransfer), Some(changer::ElementType::Storage)) => "unload",
        _ => "move",
    };

    // PREVENT/ALLOW: refuse MOVE MEDIUM when the source element is a
    // drive whose data-transport removal is prevented by any active
    // session.
    if matches!(source_type, Some(changer::ElementType::DataTransfer))
        && let Some(src_drive_id) = element_config.address_to_drive_id(source_address)
        && drive_manager.is_data_transport_prevented(src_drive_id as usize)
    {
        tracing::warn!(
            "MOVE MEDIUM refused: source drive {} (element {}) data-transport removal prevented",
            src_drive_id,
            source_address
        );
        audit_append(
            audit_log,
            audit_ratelimiter,
            "iscsi.move_medium",
            actor.clone(),
            serde_json::json!({
                "action": action_label,
                "transport": transport_address,
                "src": source_address,
                "dst": destination_address,
                "invert": invert,
                "refused": "medium_removal_prevented",
            }),
            AuditResult::Error("medium removal prevented".to_string()),
        );
        let sense =
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_MEDIUM_REMOVAL_PREVENTED).build();
        return Ok(ScsiResp::check_condition_with_sense(sense));
    }

    match changer::handle_move_medium(
        &mut lib,
        element_config,
        transport_address,
        source_address,
        destination_address,
        invert,
    ) {
        Ok(_) => {
            // Mirror the load/unload into drive_manager AND emit
            // CartridgeLoaded/Unloaded events.
            if let (Some(src), Some(dst)) = (source_type, dest_type) {
                use changer::ElementType;
                match (src, dst) {
                    (ElementType::Storage, ElementType::DataTransfer) => {
                        if let Some(drive_id) =
                            element_config.address_to_drive_id(destination_address)
                            && let Some(drive_info) = lib.get_drive(drive_id)
                            && let Some(ref barcode) = drive_info.barcode
                        {
                            if let Err(e) = drive_manager.load_cartridge(drive_id as usize, barcode)
                            {
                                tracing::error!(
                                    "drive_manager.load_cartridge failed for drive {}: {}",
                                    drive_id,
                                    e
                                );
                            }
                            let _ = event_tx.send(TapeEvent::CartridgeLoaded {
                                tape_id: barcode.to_string(),
                                drive_num: drive_id as u8,
                            });
                        }
                    }
                    (ElementType::DataTransfer, ElementType::Storage) => {
                        if let Some(drive_id) = element_config.address_to_drive_id(source_address) {
                            if let Err(e) = drive_manager.unload_cartridge(drive_id as usize) {
                                tracing::warn!(
                                    "drive_manager.unload_cartridge for drive {}: {}",
                                    drive_id,
                                    e
                                );
                            }
                            let tape_id = unload_source_barcode
                                .clone()
                                .unwrap_or_else(|| format!("DRIVE{}", drive_id));
                            let _ = event_tx.send(TapeEvent::CartridgeUnloaded {
                                tape_id,
                                drive_num: drive_id as u8,
                            });
                        }
                    }
                    _ => {}
                }
            }

            // MEDIUM MAY HAVE CHANGED is delivered only to the drive
            // LUN(s) whose cartridge actually changed: the source
            // drive on an unload, the destination drive on a load.
            // Earlier code broadcast the UA across every drive LUN,
            // which preempted unrelated drives' next command — when
            // the host's positioning sequence (e.g. `mt rewind 2>&1`)
            // ignored that CHECK CONDITION, the drive's daemon-side
            // head_lba never got reset, and a follow-up SPACE BLOCKS
            // landed at the wrong LBA, leaving stale filemarks in
            // the block index (issue #37).
            let ua = ua_tracker;
            let mut affected_drives: Vec<u32> = Vec::new();
            if matches!(source_type, Some(changer::ElementType::DataTransfer))
                && let Some(id) = element_config.address_to_drive_id(source_address)
            {
                affected_drives.push(id);
            }
            if matches!(dest_type, Some(changer::ElementType::DataTransfer))
                && let Some(id) = element_config.address_to_drive_id(destination_address)
                && !affected_drives.contains(&id)
            {
                affected_drives.push(id);
            }
            for drive_id in &affected_drives {
                let drive_lun = (*drive_id as u8) + 1;
                ua.add_ua(
                    tsih,
                    drive_lun,
                    unit_attention::UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED,
                );
            }
            tracing::info!(
                "MOVE MEDIUM completed, generated UA for drives {:?}",
                affected_drives
            );
            audit_append(
                audit_log,
                audit_ratelimiter,
                "iscsi.move_medium",
                actor,
                serde_json::json!({
                    "action": action_label,
                    "transport": transport_address,
                    "src": source_address,
                    "dst": destination_address,
                    "invert": invert,
                    "barcode": unload_source_barcode,
                }),
                AuditResult::Ok,
            );
            Ok(ScsiResp::good())
        }
        Err(e) => {
            tracing::warn!("MOVE MEDIUM error: {}", e);
            audit_append(
                audit_log,
                audit_ratelimiter,
                "iscsi.move_medium",
                actor,
                serde_json::json!({
                    "action": action_label,
                    "transport": transport_address,
                    "src": source_address,
                    "dst": destination_address,
                    "invert": invert,
                }),
                AuditResult::Error(e.to_string()),
            );
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_send_volume_tag(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    if ctx.lun != 0 {
        return Ok(ScsiResp::check_condition());
    }
    let cdb = ctx.cdb;
    let element_type = cdb[1] & 0x0F;
    let element_address = u16::from_be_bytes([cdb[2], cdb[3]]);
    let send_action = cdb[5] & 0x1F;
    tracing::debug!(
        "SEND VOLUME TAG (changer) - accepted: element_type=0x{:02x} element=0x{:04x} action=0x{:02x}",
        element_type,
        element_address,
        send_action
    );
    Ok(ScsiResp::good())
}

pub fn handle_exchange_medium(ctx: &mut SmcScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let tsih = ctx.tsih;
    let library = ctx.library;
    let ua_tracker = ctx.ua_tracker;
    let element_config = ctx.element_config;
    let session_partition = ctx.session_partition;

    // EXCHANGE MEDIUM (SMC) — atomic swap. Composed from two MOVE MEDIUM
    // operations executed in the order that doesn't require a temp slot:
    // first move dest1 -> dest2 to free up dest1, then move src -> dest1.
    if lun != 0 {
        return Ok(ScsiResp::check_condition());
    }
    let transport_address = u16::from_be_bytes([cdb[2], cdb[3]]);
    let source_address = u16::from_be_bytes([cdb[4], cdb[5]]);
    let first_dest_address = u16::from_be_bytes([cdb[6], cdb[7]]);
    let second_dest_address = u16::from_be_bytes([cdb[8], cdb[9]]);
    let invert1 = (cdb[10] & 0x01) != 0;
    let invert2 = (cdb[10] & 0x02) != 0;
    let source_type = element_config.element_type_from_address(source_address);
    let first_dest_type = element_config.element_type_from_address(first_dest_address);
    let second_dest_type = element_config.element_type_from_address(second_dest_address);

    tracing::info!(
        "EXCHANGE MEDIUM: transport={}, src={}, dst1={}, dst2={}",
        transport_address,
        source_address,
        first_dest_address,
        second_dest_address
    );

    let mut lib = library
        .lock()
        .map_err(|_| anyhow!("library mutex poisoned"))?;
    if let Some(part_name) = session_partition {
        let in_partition = |addr: u16| -> bool {
            use changer::ElementType;
            match element_config.element_type_from_address(addr) {
                Some(ElementType::Storage) => element_config
                    .address_to_storage_id(addr)
                    .map(|id| lib.partition_for_storage_slot(id) == Some(part_name))
                    .unwrap_or(false),
                Some(ElementType::ImportExport) => element_config
                    .address_to_mail_id(addr)
                    .map(|id| lib.partition_for_mail_slot(id) == Some(part_name))
                    .unwrap_or(false),
                Some(ElementType::DataTransfer) => element_config
                    .address_to_drive_id(addr)
                    .map(|id| lib.partition_for_drive(id) == Some(part_name))
                    .unwrap_or(false),
                Some(ElementType::MediumTransport) => true,
                _ => false,
            }
        };
        if !in_partition(source_address)
            || !in_partition(first_dest_address)
            || !in_partition(second_dest_address)
        {
            tracing::warn!(
                "partition fence: session bound to '{}' refused EXCHANGE MEDIUM src={} dst1={} dst2={}",
                part_name,
                source_address,
                first_dest_address,
                second_dest_address,
            );
            let sense = SenseDataBuilder::new(
                SenseKey::IllegalRequest,
                AdditionalSenseCode {
                    asc: 0x21,
                    ascq: 0x01,
                },
            )
            .build();
            return Ok(ScsiResp::check_condition_with_sense(sense));
        }
    }
    // Step 1: dst1 -> dst2 (frees up dst1)
    if let Err(e) = changer::handle_move_medium(
        &mut lib,
        element_config,
        transport_address,
        first_dest_address,
        second_dest_address,
        invert2,
    ) {
        tracing::warn!("EXCHANGE MEDIUM step 1 (dst1->dst2): {}", e);
        return Ok(ScsiResp::check_condition());
    }
    // Step 2: src -> dst1
    if let Err(e) = changer::handle_move_medium(
        &mut lib,
        element_config,
        transport_address,
        source_address,
        first_dest_address,
        invert1,
    ) {
        tracing::warn!("EXCHANGE MEDIUM step 2 (src->dst1): {}", e);
        return Ok(ScsiResp::check_condition());
    }
    // EXCHANGE MEDIUM moves three cartridges across three elements;
    // raise MEDIUM MAY HAVE CHANGED only on the drive LUN(s) that
    // participated in the swap. Broadcasting across every drive LUN
    // races the host's positioning sequence on unrelated drives (see
    // handle_move_medium for the full rationale + issue #37).
    let ua = ua_tracker;
    let mut affected_drives: Vec<u32> = Vec::new();
    for (addr, kind) in [
        (source_address, source_type),
        (first_dest_address, first_dest_type),
        (second_dest_address, second_dest_type),
    ] {
        if matches!(kind, Some(changer::ElementType::DataTransfer))
            && let Some(id) = element_config.address_to_drive_id(addr)
            && !affected_drives.contains(&id)
        {
            affected_drives.push(id);
        }
    }
    for drive_id in &affected_drives {
        let drive_lun = (*drive_id as u8) + 1;
        ua.add_ua(
            tsih,
            drive_lun,
            unit_attention::UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED,
        );
    }
    Ok(ScsiResp::good())
}
