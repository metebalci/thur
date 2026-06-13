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
    ASC_MEDIUM_REMOVAL_PREVENTED, AdditionalSenseCode, SenseDataBuilder, SenseKey, error_to_sense,
};
use shared_iscsi::unit_attention;

use crate::changer;
use core_mediachanger::ExchangeSlot;
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
    let drive_manager = ctx.drive_manager;
    let library = ctx.library;
    let ua_tracker = ctx.ua_tracker;
    let element_config = ctx.element_config;
    let event_tx = ctx.event_tx;
    let audit_log = ctx.audit_log;
    let audit_ratelimiter = ctx.audit_ratelimiter;
    let actor = ctx.audit_actor();
    let session_partition = ctx.session_partition;
    let session_manager = ctx.session_manager;

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

    // Perform the in-memory inventory move under the lock; everything
    // after this — drive_manager mirroring, events, UA, audit — runs
    // with the Library mutex released (issue #188). load_cartridge /
    // unload_cartridge open cartridge indexes and persist drive state,
    // none of which needs the Library lock; holding it across that work
    // serialized every changer / identity / partition-fence command on
    // all sessions behind one slow cartridge open.
    if let Err(e) = changer::handle_move_medium(
        &mut lib,
        element_config,
        transport_address,
        source_address,
        destination_address,
        invert,
    ) {
        drop(lib);
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
        return Ok(ScsiResp::check_condition());
    }

    // The inventory move succeeded and is persisted. Capture the
    // drive-side mirror work (and the rollback target) while still
    // holding the lock, then drop it before any drive I/O.
    enum Mirror {
        Load {
            drive_id: u32,
            barcode: String,
            src_storage: u32,
        },
        Unload {
            drive_id: u32,
        },
        None,
    }
    let mirror = match (source_type, dest_type) {
        (
            Some(changer::ElementType::Storage),
            Some(changer::ElementType::DataTransfer),
        ) => match (
            element_config.address_to_drive_id(destination_address),
            element_config.address_to_storage_id(source_address),
        ) {
            (Some(drive_id), Some(src_storage)) => {
                match lib.get_drive(drive_id).and_then(|d| d.barcode.clone()) {
                    Some(barcode) => Mirror::Load {
                        drive_id,
                        barcode,
                        src_storage,
                    },
                    None => Mirror::None,
                }
            }
            _ => Mirror::None,
        },
        (
            Some(changer::ElementType::DataTransfer),
            Some(changer::ElementType::Storage),
        ) => match element_config.address_to_drive_id(source_address) {
            Some(drive_id) => Mirror::Unload { drive_id },
            None => Mirror::None,
        },
        _ => Mirror::None,
    };
    drop(lib); // issue #188: release the Library mutex before drive I/O

    // Drive LUN(s) whose cartridge actually changed — the UA targets.
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

    match mirror {
        Mirror::Load {
            drive_id,
            barcode,
            src_storage,
        } => {
            if let Err(e) = drive_manager.load_cartridge(drive_id as usize, &barcode) {
                // issue #189: the drive-side load failed (e.g. keystore
                // unreachable so no cached DEK, corrupt manifest, LTO
                // generation mismatch). Roll the inventory move back so
                // library state doesn't claim the drive is loaded when it
                // isn't, then fail the command with sense reflecting the
                // real cause — rather than returning GOOD over a
                // persistent library/drive desync and a false audit row.
                tracing::error!(
                    "drive_manager.load_cartridge failed for drive {}: {} - rolling back inventory move",
                    drive_id,
                    e
                );
                {
                    let mut lib = library
                        .lock()
                        .map_err(|_| anyhow!("library mutex poisoned"))?;
                    if let Err(re) = lib.unload_from_drive(drive_id, src_storage) {
                        tracing::error!(
                            "MOVE MEDIUM rollback (unload_from_drive drive {} -> slot {}) failed: {}",
                            drive_id,
                            src_storage,
                            re
                        );
                    }
                }
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
                        "refused": "drive_load_failed",
                        "error": e.to_string(),
                    }),
                    AuditResult::Error(e.to_string()),
                );
                return Ok(ScsiResp::check_condition_with_sense(error_to_sense(&e)));
            }
            let _ = event_tx.send(TapeEvent::CartridgeLoaded {
                tape_id: barcode,
                drive_num: drive_id as u8,
            });
        }
        Mirror::Unload { drive_id } => {
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
        Mirror::None => {}
    }

    // MEDIUM MAY HAVE CHANGED on the changed drive LUN(s) — for EVERY
    // live initiator, not just the session that issued the changer
    // command. add_ua is keyed per (tsih, lun), so a per-issuer add
    // silently skips other initiators sharing the drive: host B,
    // mid-read against the now-swapped medium, would never receive the
    // CHECK CONDITION and would read/write the wrong cartridge at a
    // stale position (issue #190). Mirrors the admin-socket changer-move
    // path's add_ua_all_sessions.
    if !affected_drives.is_empty() {
        let tsihs = session_manager.active_tsihs();
        for drive_id in &affected_drives {
            let drive_lun = (*drive_id as u8) + 1;
            ua_tracker.add_ua_all_sessions(
                &tsihs,
                drive_lun,
                unit_attention::UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED,
            );
        }
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
    let library = ctx.library;
    let element_config = ctx.element_config;
    let session_partition = ctx.session_partition;

    // EXCHANGE MEDIUM (SMC): the medium at src moves to dst1, and the
    // medium that was at dst1 moves to dst2. Performed as one atomic
    // Library inventory transaction (issue #191) — the prior
    // two-MOVE composition rejected the canonical swap (dst2 == src,
    // which `mtx exchange A B` issues) because dst2 was still occupied
    // when the first move ran, and left half-applied durable state when
    // the second move failed after the first had already persisted.
    if lun != 0 {
        return Ok(ScsiResp::check_condition());
    }
    let transport_address = u16::from_be_bytes([cdb[2], cdb[3]]);
    let source_address = u16::from_be_bytes([cdb[4], cdb[5]]);
    let first_dest_address = u16::from_be_bytes([cdb[6], cdb[7]]);
    let second_dest_address = u16::from_be_bytes([cdb[8], cdb[9]]);
    let source_type = element_config.element_type_from_address(source_address);
    let first_dest_type = element_config.element_type_from_address(first_dest_address);
    let second_dest_type = element_config.element_type_from_address(second_dest_address);

    // Refuse drive-involving EXCHANGE MEDIUM. This handler swaps bare
    // inventory entries and — unlike MOVE MEDIUM — does not mirror drive
    // loads/unloads into drive_manager, check PREVENT/ALLOW, or emit
    // load/unload events. A drive-involving exchange would leave the data
    // path bound to the wrong Cartridge, silently corrupting backup data
    // (issue #133). Real backup software uses MOVE MEDIUM (load/unload)
    // for drives; EXCHANGE swaps storage / mail cartridges, which stay
    // supported. Because no drive can participate, no drive LUN changes
    // medium here and no MEDIUM MAY HAVE CHANGED UA is raised.
    if [source_type, first_dest_type, second_dest_type]
        .iter()
        .any(|t| matches!(t, Some(changer::ElementType::DataTransfer)))
    {
        tracing::warn!(
            "EXCHANGE MEDIUM refused: drive-involving exchange unsupported (src={} dst1={} dst2={})",
            source_address,
            first_dest_address,
            second_dest_address
        );
        let sense = SenseDataBuilder::new(
            SenseKey::IllegalRequest,
            AdditionalSenseCode {
                asc: 0x24,
                ascq: 0x00,
            },
        )
        .build();
        return Ok(ScsiResp::check_condition_with_sense(sense));
    }

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
    // Resolve the three elements to storage/mail inventory references.
    // Drive elements are refused above; a medium-transport (robot) or
    // unknown address can't take part in an exchange.
    let to_exchange_slot = |addr: u16, kind: Option<changer::ElementType>| -> Option<ExchangeSlot> {
        match kind {
            Some(changer::ElementType::Storage) => {
                element_config.address_to_storage_id(addr).map(ExchangeSlot::Storage)
            }
            Some(changer::ElementType::ImportExport) => {
                element_config.address_to_mail_id(addr).map(ExchangeSlot::Mail)
            }
            _ => None,
        }
    };
    let (src_slot, dst1_slot, dst2_slot) = match (
        to_exchange_slot(source_address, source_type),
        to_exchange_slot(first_dest_address, first_dest_type),
        to_exchange_slot(second_dest_address, second_dest_type),
    ) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => {
            tracing::warn!(
                "EXCHANGE MEDIUM refused: non-storage/mail element (src={} dst1={} dst2={})",
                source_address,
                first_dest_address,
                second_dest_address
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
    };

    if let Err(e) = lib.exchange_medium(src_slot, dst1_slot, dst2_slot) {
        tracing::warn!("EXCHANGE MEDIUM: {}", e);
        return Ok(ScsiResp::check_condition_with_sense(error_to_sense(&e)));
    }
    Ok(ScsiResp::good())
}
